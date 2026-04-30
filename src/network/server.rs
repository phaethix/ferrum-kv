use std::io::{ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

use log::{debug, error, info, warn};

use crate::error::FerrumError;
use crate::network::shutdown::Shutdown;
use crate::protocol::encoder;
use crate::protocol::parser::{self, Command, FrameParse};
use crate::storage::engine::{KvEngine, TtlStatus, current_epoch_ms};

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
    debug!("client connected: {peer}");

    // Apply idle timeouts, if configured. A failure here is logged but not
    // fatal: the connection can still function, just without the timeout.
    if let Some(timeout) = config.client_timeout {
        if let Err(e) = stream.set_read_timeout(Some(timeout)) {
            warn!("set_read_timeout failed for {peer}: {e}");
        }
        if let Err(e) = stream.set_write_timeout(Some(timeout)) {
            warn!("set_write_timeout failed for {peer}: {e}");
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
                info!("{peer} idle timeout, closing connection");
                return Ok(());
            }
            Err(e) => {
                error!("read failed for {peer}: {e}");
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
                        error!("write failed for {peer}: {e}");
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
                        error!("write failed for {peer}: {e}");
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
                    warn!("protocol error from {peer}: {e}");
                    return Ok(());
                }
            }
        }

        if inbuf.len() > READ_BUF_MAX {
            outbuf.clear();
            encoder::encode_error(&mut outbuf, "ERR request too large");
            let _ = stream.write_all(&outbuf);
            warn!("request buffer overflow from {peer}");
            return Ok(());
        }
    }

    debug!("client disconnected: {peer}");
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
        Command::Expire { key, seconds } => {
            let reply = expire_reply(engine, &key, checked_seconds_to_ms(seconds));
            write_bool_integer(out, reply);
        }
        Command::PExpire { key, millis } => {
            let reply = expire_reply(engine, &key, Some(millis));
            write_bool_integer(out, reply);
        }
        Command::PExpireAt { key, abs_epoch_ms } => match engine.expire_at_ms(&key, abs_epoch_ms) {
            Ok(true) => encoder::encode_integer(out, 1),
            Ok(false) => encoder::encode_integer(out, 0),
            Err(e) => write_ferrum_error(out, &e),
        },
        Command::Persist { key } => match engine.persist(&key) {
            Ok(true) => encoder::encode_integer(out, 1),
            Ok(false) => encoder::encode_integer(out, 0),
            Err(e) => write_ferrum_error(out, &e),
        },
        Command::Ttl { key } => match engine.ttl_ms(&key) {
            Ok(TtlStatus::Missing) => encoder::encode_integer(out, -2),
            Ok(TtlStatus::NoExpire) => encoder::encode_integer(out, -1),
            Ok(TtlStatus::Millis(ms)) => encoder::encode_integer(out, (ms + 999) / 1000),
            Err(e) => write_ferrum_error(out, &e),
        },
        Command::PTtl { key } => match engine.ttl_ms(&key) {
            Ok(TtlStatus::Missing) => encoder::encode_integer(out, -2),
            Ok(TtlStatus::NoExpire) => encoder::encode_integer(out, -1),
            Ok(TtlStatus::Millis(ms)) => encoder::encode_integer(out, ms),
            Err(e) => write_ferrum_error(out, &e),
        },
        Command::MemoryUsage { key } => match engine.memory_usage(&key) {
            Ok(Some(bytes)) => encoder::encode_integer(out, bytes as i64),
            Ok(None) => encoder::encode_null_bulk(out),
            Err(e) => write_ferrum_error(out, &e),
        },
        Command::Info { section } => {
            let body = render_info(engine, section.as_deref());
            encoder::encode_bulk_string(out, body.as_bytes());
        }
    }
}

/// Produces the payload for `INFO [section]`.
///
/// Only the `server` and `memory` sections are currently populated, plus a
/// small `keyspace` summary that mirrors Redis' `db0` line. Unknown sections
/// return an empty body, matching Redis' behaviour.
fn render_info(engine: &KvEngine, section: Option<&[u8]>) -> String {
    let wants = |name: &str| match section {
        None => true,
        Some(s) => s.eq_ignore_ascii_case(name.as_bytes()),
    };

    let mut out = String::new();
    if wants("server") {
        out.push_str("# Server\r\n");
        out.push_str(concat!(
            "ferrum_version:",
            env!("CARGO_PKG_VERSION"),
            "\r\n"
        ));
        out.push_str("\r\n");
    }
    if wants("memory") {
        let used = engine.used_memory();
        let cfg = engine.eviction_config().unwrap_or_default();
        out.push_str("# Memory\r\n");
        out.push_str(&format!("used_memory:{used}\r\n"));
        out.push_str(&format!("maxmemory:{}\r\n", cfg.max_memory));
        out.push_str(&format!("maxmemory_policy:{}\r\n", cfg.policy.name()));
        out.push_str(&format!("maxmemory_samples:{}\r\n", cfg.samples));
        // Surface the AHE controller state so operators can see the
        // self-tuning converge. Redis has no exact analogue; we keep the
        // `ahe_` prefix so tooling can filter it out easily.
        let ahe = engine.ahe_snapshot();
        out.push_str(&format!("ahe_alpha:{:.3}\r\n", ahe.alpha));
        out.push_str(&format!("ahe_last_hit_ratio:{:.3}\r\n", ahe.last_hit_ratio));
        out.push_str("\r\n");
    }
    if wants("stats") {
        let (hits, misses) = engine.keyspace_stats();
        out.push_str("# Stats\r\n");
        out.push_str(&format!("keyspace_hits:{hits}\r\n"));
        out.push_str(&format!("keyspace_misses:{misses}\r\n"));
        out.push_str("\r\n");
    }
    if wants("keyspace") {
        let keys = engine.dbsize().unwrap_or(0);
        out.push_str("# Keyspace\r\n");
        if keys > 0 {
            out.push_str(&format!("db0:keys={keys},expires=0,avg_ttl=0\r\n"));
        }
        out.push_str("\r\n");
    }
    out
}

/// Converts an `EXPIRE` second delta to milliseconds, saturating on overflow.
///
/// A `None` return means the caller should treat the request as "delete this
/// key right now" — which is how [`KvEngine::expire_at_ms`] interprets an
/// already-past absolute timestamp.
fn checked_seconds_to_ms(seconds: i64) -> Option<i64> {
    seconds.checked_mul(1_000)
}

/// Computes the absolute epoch-millisecond deadline for `EXPIRE`/`PEXPIRE`
/// and forwards it to the engine.
///
/// A delta that overflows `i64` when expressed in milliseconds (only possible
/// with `EXPIRE` and astronomically large second counts) is treated the same
/// way as an in-the-past deadline: the key is dropped on the spot. That keeps
/// the wire contract simple — either the key existed and was updated
/// (`:1`), or it did not (`:0`).
fn expire_reply(engine: &KvEngine, key: &[u8], delta_ms: Option<i64>) -> Result<bool, FerrumError> {
    let now_ms = current_epoch_ms();
    let abs_ms = match delta_ms {
        Some(d) => now_ms.saturating_add(d),
        None => now_ms, // treat overflow as "expire immediately"
    };
    engine.expire_at_ms(key, abs_ms)
}

/// Writes a `Result<bool, _>` as a RESP integer (`0`/`1`) or as an error.
fn write_bool_integer(out: &mut Vec<u8>, reply: Result<bool, FerrumError>) {
    match reply {
        Ok(true) => encoder::encode_integer(out, 1),
        Ok(false) => encoder::encode_integer(out, 0),
        Err(e) => write_ferrum_error(out, &e),
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
    info!("FerrumKV listening on {local}");
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
                    warn!(
                        "rejecting {peer}: max_clients={} reached",
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
                        error!("client handler error: {e}");
                    }
                    drop(guard);
                });
            }
            Err(e) => {
                error!("connection failed: {e}");
            }
        }
    }

    info!("accept loop exiting");
    Ok(())
}
