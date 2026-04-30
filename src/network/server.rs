use std::io::{ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

use crate::error::FerrumError;
use crate::network::shutdown::Shutdown;
use crate::protocol::encoder;
use crate::protocol::parser::{self, Command, FrameParse};
use crate::storage::engine::KvEngine;

/// Initial capacity of each per-connection read buffer.
///
/// Most Redis commands fit into well under 1 KiB; starting small keeps idle
/// connections cheap while letting the buffer grow naturally for bulk values.
const READ_BUF_INITIAL: usize = 1024;

/// Upper bound on how much unparsed data we are willing to buffer per client.
///
/// If the buffer exceeds this size without a complete frame the client is
/// sending something malformed or abusive; we reply with an error and close.
const READ_BUF_MAX: usize = 16 * 1024 * 1024;

/// Size of each `read()` scratch buffer.
const READ_CHUNK: usize = 8 * 1024;

/// Tunable per-server runtime knobs.
///
/// Defaults are intentionally permissive so that unit tests and casual users
/// do not need to supply a config at all. Production deployments override the
/// fields they care about through CLI flags or the configuration file.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// Per-connection idle timeout. When `Some(d)`, a client that neither
    /// sends nor receives any bytes for `d` is closed. `None` disables the
    /// timeout, matching Redis' `timeout 0` semantics.
    pub client_timeout: Option<Duration>,
    /// Maximum number of concurrently accepted client connections.
    ///
    /// `0` disables the limit entirely. Matches Redis' `maxclients` default.
    pub max_clients: usize,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            client_timeout: None,
            max_clients: 10_000,
        }
    }
}

impl ServerConfig {
    /// Convenience constructor used by tests that want explicit defaults.
    pub fn new() -> Self {
        Self::default()
    }
}

/// RAII counter that tracks the number of in-flight client connections.
///
/// The accept loop calls [`ConnCounter::try_acquire`] for every newly accepted
/// socket; on success it hands the returned guard to the worker thread. When
/// the worker finishes (normally or via panic) the guard is dropped and the
/// count is decremented — so even a panicking handler cannot leak a slot.
#[derive(Clone, Default)]
struct ConnCounter {
    active: Arc<AtomicUsize>,
}

impl ConnCounter {
    fn new() -> Self {
        Self::default()
    }

    /// Attempts to reserve a slot; returns `None` when `max_clients` would be
    /// exceeded. `max == 0` disables the limit.
    fn try_acquire(&self, max: usize) -> Option<ConnGuard> {
        if max == 0 {
            self.active.fetch_add(1, Ordering::SeqCst);
            return Some(ConnGuard {
                active: Arc::clone(&self.active),
            });
        }
        // Compare-and-swap loop: only bump the counter when it stays within
        // the cap, so a racing acceptor cannot push us over the limit.
        let mut current = self.active.load(Ordering::SeqCst);
        loop {
            if current >= max {
                return None;
            }
            match self.active.compare_exchange(
                current,
                current + 1,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => {
                    return Some(ConnGuard {
                        active: Arc::clone(&self.active),
                    });
                }
                Err(observed) => current = observed,
            }
        }
    }
}

struct ConnGuard {
    active: Arc<AtomicUsize>,
}

impl Drop for ConnGuard {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Handles a single client connection using the RESP2 protocol.
///
/// Bytes are read into a per-connection buffer and fed to [`parser::parse_frame`]
/// in a loop: every complete frame is executed against the shared engine and
/// its reply is written back using the RESP2 encoders. Partial frames remain
/// in the buffer until the next `read` fills them in, so requests that span
/// multiple packets are handled transparently.
fn handle_client(
    mut stream: TcpStream,
    engine: KvEngine,
    shutdown: Shutdown,
    config: ServerConfig,
) -> Result<(), FerrumError> {
    let peer = stream.peer_addr()?;
    eprintln!("[INFO] Client connected: {peer}");

    // Apply idle timeouts, if configured. A failure here is logged but not
    // fatal: the connection can still function, just without the timeout.
    if let Some(timeout) = config.client_timeout {
        if let Err(e) = stream.set_read_timeout(Some(timeout)) {
            eprintln!("[WARN] set_read_timeout failed for {peer}: {e}");
        }
        if let Err(e) = stream.set_write_timeout(Some(timeout)) {
            eprintln!("[WARN] set_write_timeout failed for {peer}: {e}");
        }
    }

    let mut inbuf: Vec<u8> = Vec::with_capacity(READ_BUF_INITIAL);
    let mut chunk = [0u8; READ_CHUNK];
    let mut outbuf: Vec<u8> = Vec::with_capacity(256);

    loop {
        if shutdown.is_triggered() {
            break;
        }
        let n = match stream.read(&mut chunk) {
            Ok(0) => break, // Orderly EOF: client closed the connection.
            Ok(n) => n,
            Err(e) if is_timeout(&e) => {
                eprintln!("[INFO] {peer} idle timeout, closing connection");
                return Ok(());
            }
            Err(e) => {
                eprintln!("[ERROR] read failed for {peer}: {e}");
                return Err(e.into());
            }
        };
        inbuf.extend_from_slice(&chunk[..n]);

        // Drain as many complete frames as the buffer currently holds.
        loop {
            match parser::parse_frame(&inbuf) {
                Ok(FrameParse::Complete { command, consumed }) => {
                    outbuf.clear();
                    execute_command(command, &engine, &mut outbuf);
                    if let Err(e) = stream.write_all(&outbuf) {
                        eprintln!("[ERROR] write failed for {peer}: {e}");
                        return Err(e.into());
                    }
                    inbuf.drain(..consumed);
                }
                Ok(FrameParse::Invalid { error, consumed }) => {
                    // The frame was well-formed; the command itself was
                    // invalid (unknown, wrong arity, etc). Reply with -ERR
                    // and keep the connection open so the client can retry.
                    outbuf.clear();
                    write_ferrum_error(&mut outbuf, &error);
                    if let Err(e) = stream.write_all(&outbuf) {
                        eprintln!("[ERROR] write failed for {peer}: {e}");
                        return Err(e.into());
                    }
                    inbuf.drain(..consumed);
                }
                Ok(FrameParse::Incomplete) => break,
                Err(e) => {
                    // Protocol-level error: the byte stream itself cannot be
                    // resynchronised, so reply with -ERR and close.
                    outbuf.clear();
                    write_ferrum_error(&mut outbuf, &e);
                    let _ = stream.write_all(&outbuf);
                    eprintln!("[WARN] protocol error from {peer}: {e}");
                    return Ok(());
                }
            }
        }

        if inbuf.len() > READ_BUF_MAX {
            outbuf.clear();
            encoder::encode_error(&mut outbuf, "ERR request too large");
            let _ = stream.write_all(&outbuf);
            eprintln!("[WARN] request buffer overflow from {peer}");
            return Ok(());
        }
    }

    eprintln!("[INFO] Client disconnected: {peer}");
    Ok(())
}

/// Returns `true` when a `read`/`write` I/O error is the kind produced by an
/// expired [`TcpStream`] timeout.
///
/// Platforms disagree on which `ErrorKind` a blocking-socket timeout surfaces
/// as: Linux reports `WouldBlock`, macOS and Windows prefer `TimedOut`. We
/// treat both as the same signal so callers do not have to care.
fn is_timeout(err: &std::io::Error) -> bool {
    matches!(err.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut)
}

/// Executes a parsed command against the engine and appends its RESP2 reply
/// to `out`.
///
/// Replies follow the Redis conventions:
///
/// - `SET`, `FLUSHDB`                         → `+OK`
/// - `GET` (hit)                              → bulk string
/// - `GET` (miss)                             → null bulk (`$-1`)
/// - `DEL`, `EXISTS`, `DBSIZE`                → integer
/// - `PING` (no arg)                          → `+PONG`
/// - `PING msg`                               → bulk string echoing `msg`
/// - engine/persistence failures              → `-ERR` / `-OOM`
pub fn execute_command(cmd: Command, engine: &KvEngine, out: &mut Vec<u8>) {
    match cmd {
        Command::Set { key, value } => match engine.set(key, value) {
            Ok(_) => encoder::encode_simple_string(out, "OK"),
            Err(e) => write_ferrum_error(out, &e),
        },
        Command::SetNx { key, value } => match engine.set_nx(key, value) {
            Ok(true) => encoder::encode_integer(out, 1),
            Ok(false) => encoder::encode_integer(out, 0),
            Err(e) => write_ferrum_error(out, &e),
        },
        Command::MSet { pairs } => match engine.mset(pairs) {
            Ok(()) => encoder::encode_simple_string(out, "OK"),
            Err(e) => write_ferrum_error(out, &e),
        },
        Command::MGet { keys } => match engine.mget(&keys) {
            Ok(values) => {
                encoder::encode_array_header(out, values.len());
                for value in values {
                    match value {
                        Some(v) => encoder::encode_bulk_string(out, &v),
                        None => encoder::encode_null_bulk(out),
                    }
                }
            }
            Err(e) => write_ferrum_error(out, &e),
        },
        Command::IncrBy { key, delta } => match engine.incr_by(key, delta) {
            Ok(n) => encoder::encode_integer(out, n),
            Err(e) => write_ferrum_error(out, &e),
        },
        Command::Get { key } => match engine.get(&key) {
            Ok(Some(v)) => encoder::encode_bulk_string(out, &v),
            Ok(None) => encoder::encode_null_bulk(out),
            Err(e) => write_ferrum_error(out, &e),
        },
        Command::Del { keys } => match engine.del_many(&keys) {
            Ok(n) => encoder::encode_integer(out, n as i64),
            Err(e) => write_ferrum_error(out, &e),
        },
        Command::Exists { key } => match engine.exists(&key) {
            Ok(true) => encoder::encode_integer(out, 1),
            Ok(false) => encoder::encode_integer(out, 0),
            Err(e) => write_ferrum_error(out, &e),
        },
        Command::Ping { msg } => match msg {
            None => encoder::encode_simple_string(out, "PONG"),
            Some(m) => encoder::encode_bulk_string(out, &m),
        },
        Command::Append { key, value } => match engine.append(key, value) {
            Ok(n) => encoder::encode_integer(out, n as i64),
            Err(e) => write_ferrum_error(out, &e),
        },
        Command::StrLen { key } => match engine.strlen(&key) {
            Ok(n) => encoder::encode_integer(out, n as i64),
            Err(e) => write_ferrum_error(out, &e),
        },
        Command::DbSize => match engine.dbsize() {
            Ok(n) => encoder::encode_integer(out, n as i64),
            Err(e) => write_ferrum_error(out, &e),
        },
        Command::FlushDb => match engine.flushdb() {
            Ok(()) => encoder::encode_simple_string(out, "OK"),
            Err(e) => write_ferrum_error(out, &e),
        },
    }
}

/// Serialises a [`FerrumError`] into a RESP2 error reply.
///
/// The `OOM` variant is emitted with the `OOM` prefix mandated by the Redis
/// protocol; everything else uses `ERR`. CR/LF characters inside error
/// messages are replaced with spaces because Simple errors are single-line.
fn write_ferrum_error(out: &mut Vec<u8>, err: &FerrumError) {
    let (prefix, detail) = match err {
        FerrumError::OutOfMemory => ("OOM", err.to_string()),
        _ => ("ERR", err.to_string()),
    };
    let mut sanitised = String::with_capacity(prefix.len() + 1 + detail.len());
    sanitised.push_str(prefix);
    sanitised.push(' ');
    for ch in detail.chars() {
        if ch == '\r' || ch == '\n' {
            sanitised.push(' ');
        } else {
            sanitised.push(ch);
        }
    }
    encoder::encode_error(out, &sanitised);
}

/// Starts the TCP server and listens for incoming connections.
///
/// Each accepted connection is handled on a separate thread that shares the
/// same [`KvEngine`] instance.
pub fn start(
    addr: &str,
    engine: KvEngine,
    shutdown: Shutdown,
    config: ServerConfig,
) -> Result<(), FerrumError> {
    let listener = TcpListener::bind(addr)?;
    let local = listener.local_addr()?;
    eprintln!("[INFO] FerrumKV listening on {local}");
    run_listener(listener, engine, shutdown, config)
}

/// Runs the accept loop on an already-bound [`TcpListener`].
///
/// Split out from [`start`] so that tests (and future embeddings) can bind
/// their own listener — for example to port `0` to obtain an OS-assigned
/// ephemeral port — and drive the server from there.
pub fn run_listener(
    listener: TcpListener,
    engine: KvEngine,
    shutdown: Shutdown,
    config: ServerConfig,
) -> Result<(), FerrumError> {
    let counter = ConnCounter::new();

    for stream in listener.incoming() {
        if shutdown.is_triggered() {
            break;
        }
        match stream {
            Ok(mut stream) => {
                if shutdown.is_triggered() {
                    break;
                }
                let Some(guard) = counter.try_acquire(config.max_clients) else {
                    let peer = stream
                        .peer_addr()
                        .map(|a| a.to_string())
                        .unwrap_or_else(|_| "<unknown>".into());
                    eprintln!(
                        "[WARN] rejecting {peer}: max_clients={} reached",
                        config.max_clients
                    );
                    let mut out = Vec::with_capacity(64);
                    encoder::encode_error(&mut out, "ERR max number of clients reached");
                    let _ = stream.write_all(&out);
                    continue;
                };
                let engine = engine.clone();
                let shutdown = shutdown.clone();
                let config = config.clone();
                thread::spawn(move || {
                    if let Err(e) = handle_client(stream, engine, shutdown, config) {
                        eprintln!("[ERROR] client handler error: {e}");
                    }
                    drop(guard);
                });
            }
            Err(e) => {
                eprintln!("[ERROR] connection failed: {e}");
            }
        }
    }

    eprintln!("[INFO] accept loop exiting");
    Ok(())
}
