use std::net::{SocketAddr, TcpListener as StdTcpListener};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use log::{debug, error, info, warn};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::runtime::Builder;
use tokio::time::timeout;

use crate::error::FerrumError;
use crate::network::monitor::MonitorMessage;
use crate::network::shutdown::Shutdown;
use crate::protocol::ProtocolVersion;
use crate::protocol::encoder;
use crate::protocol::parser::{self, Command, FrameParse};
use crate::storage::engine::{
    KvEngine, SlowLogEntry, TtlStatus, VALUE_MAX_BYTES, current_epoch_ms,
};
use crate::storage::eviction::EvictionPolicy;

/// Initial capacity of each per-connection read buffer.
///
/// Most Redis commands fit into well under 1 KiB; starting small keeps idle
/// connections cheap while letting the buffer grow naturally for bulk values.
const READ_BUF_INITIAL: usize = 1024;

/// Upper bound on how much unparsed data we are willing to buffer per client.
///
/// Must exceed [`VALUE_MAX_BYTES`] plus the RESP framing (command name, key,
/// length prefixes): the old limit was exactly 16 MiB, the same as the largest
/// value the engine accepts, so the biggest legal value could never be
/// received. The extra 1 MiB covers framing for a 64 KiB key.
const READ_BUF_MAX: usize = VALUE_MAX_BYTES + 1024 * 1024;

/// Size of each `read()` scratch buffer.
const READ_CHUNK: usize = 8 * 1024;

/// Reply-buffer watermark used while draining a pipelined batch.
///
/// Replies are accumulated and flushed once per read batch; if a client
/// pipelines aggressively the buffer is flushed whenever it crosses this
/// watermark so memory stays bounded.
const WRITE_BATCH_MAX: usize = 64 * 1024;

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
    /// Number of worker threads backing the tokio runtime when the server
    /// spins one up internally. `0` asks tokio to pick a sensible default
    /// (usually the CPU core count).
    pub worker_threads: usize,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            client_timeout: None,
            max_clients: 10_000,
            worker_threads: 0,
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
/// socket; on success it hands the returned guard to the worker task. When
/// the task finishes (normally or via panic) the guard is dropped and the
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
async fn handle_client(
    mut stream: TcpStream,
    engine: KvEngine,
    shutdown: Shutdown,
    config: ServerConfig,
) -> Result<(), FerrumError> {
    let peer = stream.peer_addr()?;
    debug!("client connected: {peer}");

    let mut inbuf: Vec<u8> = Vec::with_capacity(READ_BUF_INITIAL);
    let mut chunk = [0u8; READ_CHUNK];
    let mut outbuf: Vec<u8> = Vec::with_capacity(READ_BUF_INITIAL);

    // Per-connection authentication flag. Starts `false`; a successful `AUTH`
    // flips it, after which every other command runs. We keep this state in the
    // connection handler (it is connection-local) while the configured password
    // itself lives in the shared engine. Declared once, outside the read loop,
    // so that authentication survives across packet boundaries and pipelined
    // commands.
    let mut authed = false;
    // Per-connection protocol version. Defaults to RESP2; upgraded to
    // RESP3 when the client issues a successful `HELLO 3` handshake.
    let mut version = ProtocolVersion::default();

    loop {
        if shutdown.is_triggered() {
            break;
        }

        // Read the next chunk, racing three concurrent wake-up sources:
        // 1. bytes arriving from the peer,
        // 2. the idle timeout (only when configured),
        // 3. the shared shutdown signal.
        let read_result = tokio::select! {
            biased;
            _ = shutdown.notified() => break,
            r = read_with_optional_timeout(&mut stream, &mut chunk, config.client_timeout) => r,
        };

        let n = match read_result {
            ReadOutcome::Bytes(n) => n,
            ReadOutcome::Eof => break,
            ReadOutcome::IdleTimeout => {
                info!("{peer} idle timeout, closing connection");
                return Ok(());
            }
            ReadOutcome::Err(e) => {
                error!("read failed for {peer}: {e}");
                return Err(e.into());
            }
        };
        inbuf.extend_from_slice(&chunk[..n]);

        // Drain as many complete frames as the buffer holds.
        //
        // Two changes keep pipelined clients efficient: frames are consumed by
        // advancing `start` (the buffer is compacted once per read batch, so a
        // deep pipeline no longer pays an O(remaining) memmove per command),
        // and replies are accumulated in `outbuf` and flushed in batches
        // instead of one `write_all` syscall per command.
        let mut start = 0usize;
        loop {
            match parser::parse_frame(&inbuf[start..]) {
                Ok(FrameParse::Complete { command, consumed }) => {
                    // When a password is configured, every command except
                    // `AUTH`/`HELLO` is rejected until the connection
                    // authenticates. `auth_required` is a lock-free flag, so
                    // the hot path no longer takes the password RwLock.
                    if let Command::Auth { password } = &command {
                        if auth_reply(&engine, password, &mut outbuf) {
                            authed = true;
                        }
                    } else if let Command::Hello {
                        version: proto_version,
                    } = &command
                    {
                        if let Some(v) = proto_version {
                            version = match v {
                                2 => ProtocolVersion::Resp2,
                                3 => ProtocolVersion::Resp3,
                                _ => version,
                            };
                        }
                        hello_reply(&mut outbuf, version);
                    } else if matches!(&command, Command::Monitor) {
                        // MONITOR bypasses the normal execute→write cycle
                        // and enters a dedicated event loop. Flush anything
                        // already buffered (including the +OK) first.
                        encoder::encode_simple_string(&mut outbuf, "OK");
                        if let Err(e) = stream.write_all(&outbuf).await {
                            error!("write failed for {peer}: {e}");
                            return Err(e.into());
                        }
                        let mut rx = engine.monitor_registry.subscribe();
                        return run_monitor_loop(stream, peer, &mut rx, shutdown).await;
                    } else if engine.auth_required() && !authed {
                        encoder::encode_error(&mut outbuf, "NOAUTH Authentication required.");
                    } else if engine.aof_barrier_blocks() {
                        // `appendfsync always` waits for a disk round-trip
                        // inside `execute_command`. Running it on an async
                        // worker would let a handful of writers occupy the
                        // whole pool (measured: reads stalled to double-digit
                        // rps). The blocking pool absorbs the wait.
                        let engine_bg = engine.clone();
                        match tokio::task::spawn_blocking(move || {
                            let mut out = Vec::with_capacity(256);
                            execute_command(command, &engine_bg, Some(peer), &mut out, version);
                            out
                        })
                        .await
                        {
                            Ok(reply) => outbuf.extend_from_slice(&reply),
                            Err(e) => write_ferrum_error(
                                &mut outbuf,
                                &FerrumError::Internal(format!("command task failed: {e}")),
                            ),
                        }
                    } else {
                        execute_command(command, &engine, Some(peer), &mut outbuf, version);
                    }
                    start += consumed;
                }
                Ok(FrameParse::Invalid { error, consumed }) => {
                    // The frame was well-formed; the command itself was
                    // invalid (unknown, wrong arity, etc). Reply with -ERR
                    // and keep the connection open so the client can retry.
                    write_ferrum_error(&mut outbuf, &error);
                    start += consumed;
                }
                Ok(FrameParse::Incomplete) => break,
                Err(e) => {
                    // Protocol-level error: the byte stream itself cannot be
                    // resynchronised, so reply with -ERR and close.
                    write_ferrum_error(&mut outbuf, &e);
                    let _ = stream.write_all(&outbuf).await;
                    warn!("protocol error from {peer}: {e}");
                    return Ok(());
                }
            }

            if outbuf.len() >= WRITE_BATCH_MAX {
                if let Err(e) = stream.write_all(&outbuf).await {
                    error!("write failed for {peer}: {e}");
                    return Err(e.into());
                }
                outbuf.clear();
            }
        }

        if !outbuf.is_empty() {
            if let Err(e) = stream.write_all(&outbuf).await {
                error!("write failed for {peer}: {e}");
                return Err(e.into());
            }
            outbuf.clear();
        }
        if start > 0 {
            inbuf.drain(..start);
        }

        if inbuf.len() > READ_BUF_MAX {
            encoder::encode_error(&mut outbuf, "ERR request too large");
            let _ = stream.write_all(&outbuf).await;
            warn!("request buffer overflow from {peer}");
            return Ok(());
        }
    }

    debug!("client disconnected: {peer}");
    Ok(())
}

/// Result of a single async read attempt, flattened so the caller can match
/// on it without juggling nested `Option<Result<...>>`.
enum ReadOutcome {
    Bytes(usize),
    Eof,
    IdleTimeout,
    Err(std::io::Error),
}

/// Reads into `chunk`, optionally enforcing a per-read idle timeout that
/// mirrors the synchronous `SO_RCVTIMEO` behaviour from the pre-tokio era.
async fn read_with_optional_timeout(
    stream: &mut TcpStream,
    chunk: &mut [u8],
    idle: Option<Duration>,
) -> ReadOutcome {
    let fut = stream.read(chunk);
    let read_result = match idle {
        Some(d) => match timeout(d, fut).await {
            Ok(r) => r,
            Err(_) => return ReadOutcome::IdleTimeout,
        },
        None => fut.await,
    };
    match read_result {
        Ok(0) => ReadOutcome::Eof,
        Ok(n) => ReadOutcome::Bytes(n),
        Err(e) => ReadOutcome::Err(e),
    }
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
pub fn execute_command(
    cmd: Command,
    engine: &KvEngine,
    client: Option<SocketAddr>,
    out: &mut Vec<u8>,
    version: ProtocolVersion,
) {
    // Helper closures for version-dispatch encoding. The common RESP2
    // functions (simple_string, error, integer, bulk_string, array_header)
    // are identical across versions and use encoder::* directly.
    let enc_null = |out: &mut Vec<u8>| match version {
        ProtocolVersion::Resp2 => encoder::encode_null_bulk(out),
        ProtocolVersion::Resp3 => encoder::encode_null(out),
    };
    let enc_bool = |out: &mut Vec<u8>, v: bool| match version {
        ProtocolVersion::Resp2 => encoder::encode_integer(out, v as i64),
        ProtocolVersion::Resp3 => encoder::encode_boolean(out, v),
    };
    // SLOWLOG is a meta/observability command; it must not pollute its
    // own ring (a RESET would otherwise re-log itself and never clear).
    let is_slowlog_meta = matches!(&cmd, Command::SlowLog { .. } | Command::RewriteAof);
    engine
        .total_commands
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    // Broadcast to MONITOR clients — skip meta/observability commands that
    // would create noise loops (SLOWLOG, MONITOR itself), and skip the argv
    // clone entirely when nobody is subscribed.
    let monitor_meta = matches!(
        &cmd,
        Command::SlowLog { .. } | Command::RewriteAof | Command::Monitor
    );
    if !monitor_meta && engine.monitor_registry.has_subscribers() {
        engine.monitor_registry.send(MonitorMessage {
            timestamp_ms: current_epoch_ms(),
            peer: client.unwrap_or(SocketAddr::from(([0, 0, 0, 0], 0))),
            // `AUTH` secrets are masked; MONITOR must not leak the password.
            args: cmd.args_redacted(),
        });
    }
    // Sample the slow-log switch *before* dispatch: a command that turns the
    // log on must not be recorded by the very setting it installs.
    let slowlog_armed = engine.slowlog_armed();
    let start = Instant::now();
    match &cmd {
        Command::Set { key, value } => match engine.set(key.clone(), value.clone()) {
            Ok(_) => encoder::encode_simple_string(out, "OK"),
            Err(e) => write_ferrum_error(out, &e),
        },
        Command::SetNx { key, value } => match engine.set_nx(key.clone(), value.clone()) {
            Ok(true) => enc_bool(out, true),
            Ok(false) => enc_bool(out, false),
            Err(e) => write_ferrum_error(out, &e),
        },
        Command::MSet { pairs } => match engine.mset(pairs.clone()) {
            Ok(()) => encoder::encode_simple_string(out, "OK"),
            Err(e) => write_ferrum_error(out, &e),
        },
        Command::MGet { keys } => match engine.mget(keys) {
            Ok(values) => {
                encoder::encode_array_header(out, values.len());
                for value in values {
                    match value {
                        Some(v) => encoder::encode_bulk_string(out, &v),
                        None => enc_null(out),
                    }
                }
            }
            Err(e) => write_ferrum_error(out, &e),
        },
        Command::IncrBy { key, delta } => match engine.incr_by(key.clone(), *delta) {
            Ok(n) => encoder::encode_integer(out, n),
            Err(e) => write_ferrum_error(out, &e),
        },
        Command::IncrByFloat { key, delta } => match engine.incr_by_float(key.clone(), *delta) {
            // RESP2: bulk string (Redis-compatible). RESP3: native double.
            Ok(n) => match version {
                ProtocolVersion::Resp2 => {
                    encoder::encode_bulk_string(out, n.to_string().as_bytes())
                }
                ProtocolVersion::Resp3 => encoder::encode_double(out, n),
            },
            Err(e) => write_ferrum_error(out, &e),
        },
        Command::Get { key } => match engine.get(key) {
            Ok(Some(v)) => encoder::encode_bulk_string(out, &v),
            Ok(None) => enc_null(out),
            Err(e) => write_ferrum_error(out, &e),
        },
        Command::Del { keys } => match engine.del_many(keys) {
            Ok(n) => encoder::encode_integer(out, n as i64),
            Err(e) => write_ferrum_error(out, &e),
        },
        Command::Exists { keys } => match engine.exists_many(keys) {
            Ok(n) => encoder::encode_integer(out, n as i64),
            Err(e) => write_ferrum_error(out, &e),
        },
        Command::Ping { msg } => match msg {
            None => encoder::encode_simple_string(out, "PONG"),
            Some(m) => encoder::encode_bulk_string(out, m),
        },
        Command::Append { key, value } => match engine.append(key.clone(), value.clone()) {
            Ok(n) => encoder::encode_integer(out, n as i64),
            Err(e) => write_ferrum_error(out, &e),
        },
        Command::StrLen { key } => match engine.strlen(key) {
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
            let reply = expire_reply(engine, key, checked_seconds_to_ms(*seconds));
            write_bool_integer(out, reply, version);
        }
        Command::PExpire { key, millis } => {
            let reply = expire_reply(engine, key, Some(*millis));
            write_bool_integer(out, reply, version);
        }
        Command::PExpireAt { key, abs_epoch_ms } => match engine.expire_at_ms(key, *abs_epoch_ms) {
            Ok(true) => enc_bool(out, true),
            Ok(false) => enc_bool(out, false),
            Err(e) => write_ferrum_error(out, &e),
        },
        Command::Persist { key } => match engine.persist(key) {
            Ok(true) => enc_bool(out, true),
            Ok(false) => enc_bool(out, false),
            Err(e) => write_ferrum_error(out, &e),
        },
        Command::Ttl { key } => match engine.ttl_ms(key) {
            Ok(TtlStatus::Missing) => encoder::encode_integer(out, -2),
            Ok(TtlStatus::NoExpire) => encoder::encode_integer(out, -1),
            Ok(TtlStatus::Millis(ms)) => encoder::encode_integer(out, (ms + 999) / 1000),
            Err(e) => write_ferrum_error(out, &e),
        },
        Command::PTtl { key } => match engine.ttl_ms(key) {
            Ok(TtlStatus::Missing) => encoder::encode_integer(out, -2),
            Ok(TtlStatus::NoExpire) => encoder::encode_integer(out, -1),
            Ok(TtlStatus::Millis(ms)) => encoder::encode_integer(out, ms),
            Err(e) => write_ferrum_error(out, &e),
        },
        Command::MemoryUsage { key } => match engine.memory_usage(key) {
            Ok(Some(bytes)) => encoder::encode_integer(out, bytes as i64),
            Ok(None) => enc_null(out),
            Err(e) => write_ferrum_error(out, &e),
        },
        Command::Info { section } => {
            let sections = render_info_sections(engine, section.as_deref());
            match version {
                // RESP2: single concatenated bulk string (Redis-compatible).
                ProtocolVersion::Resp2 => {
                    let mut body = String::new();
                    for (_, text) in &sections {
                        body.push_str(text);
                    }
                    encoder::encode_bulk_string(out, body.as_bytes());
                }
                // RESP3: native map of section-name → section-text.
                ProtocolVersion::Resp3 => {
                    encoder::encode_map_header(out, sections.len());
                    for (name, text) in &sections {
                        encoder::encode_bulk_string(out, name.as_bytes());
                        encoder::encode_bulk_string(out, text.as_bytes());
                    }
                }
            }
        }
        Command::Config { sub, args } => {
            if sub.eq_ignore_ascii_case(b"GET") {
                config_get(engine, &args[0], out, version);
            } else if sub.eq_ignore_ascii_case(b"SET") {
                config_set(engine, &args[0], &args[1], out);
            } else {
                // Unreachable: the parser only emits `GET`/`SET` subs.
                encoder::encode_error(out, "ERR unknown config subcommand");
            }
        }
        Command::SlowLog { sub, args } => {
            if sub.eq_ignore_ascii_case(b"GET") {
                // Optional `count` argument; parse it exactly like INCRBY's
                // delta so a non-integer yields the Redis-standard error.
                let count = if args.is_empty() {
                    None
                } else {
                    match std::str::from_utf8(&args[0])
                        .ok()
                        .and_then(|s| s.trim().parse::<usize>().ok())
                    {
                        Some(n) => Some(n),
                        None => {
                            encoder::encode_error(
                                out,
                                "ERR value is not an integer or out of range",
                            );
                            return;
                        }
                    }
                };
                let entries = engine.slowlog_get(count);
                encode_slowlog_entries(out, &entries);
            } else if sub.eq_ignore_ascii_case(b"LEN") {
                encoder::encode_integer(out, engine.slowlog_len() as i64);
            } else if sub.eq_ignore_ascii_case(b"RESET") {
                engine.slowlog_reset();
                encoder::encode_simple_string(out, "OK");
            } else {
                encoder::encode_error(out, "ERR unknown slowlog subcommand");
            }
        }
        Command::Auth { .. } => {
            // Handled in `handle_client` so it can maintain the
            // per-connection `authenticated` flag; this arm exists only
            // to keep the command match exhaustive.
            encoder::encode_error(out, "ERR internal: AUTH handled by connection layer");
        }
        Command::RewriteAof => match engine.rewrite_aof() {
            Ok(()) => encoder::encode_simple_string(out, "OK"),
            Err(e) => write_ferrum_error(out, &e),
        },
        // Handled in `handle_client`; never reaches here.
        Command::Monitor => {
            encoder::encode_error(out, "ERR internal: MONITOR handled by connection layer");
        }
        Command::Hello { .. } => {
            encoder::encode_error(out, "ERR internal: HELLO handled by connection layer");
        }
    }
    // Slow-log: serialise the argv only *after* the command ran and only when
    // it actually crossed the threshold. `cmd` is still owned here because the
    // dispatcher borrows it, so no snapshot is needed on the fast path.
    if !is_slowlog_meta && slowlog_armed {
        let elapsed_us = start.elapsed().as_micros() as u64;
        if engine.slowlog_would_log(elapsed_us) {
            engine.maybe_push_slowlog(cmd.args_redacted(), client, elapsed_us);
        }
    }
}

/// Serialises a list of slow-log entries (already newest-first) into the
/// RESP2 reply for `SLOWLOG GET`.
///
/// Each entry is a 6-element array: `(id, ts_secs, duration_us, args[],
/// client_addr, client_name)`. The `args` sub-array mirrors the wire frame;
/// `client_name` is an empty bulk because FerrumKV has no `CLIENT
/// SETNAME` yet (YAGNI for v0.5).
fn encode_slowlog_entries(out: &mut Vec<u8>, entries: &[SlowLogEntry]) {
    encoder::encode_array_header(out, entries.len());
    for e in entries {
        encoder::encode_array_header(out, 6);
        encoder::encode_integer(out, e.id as i64);
        encoder::encode_integer(out, e.ts_secs as i64);
        encoder::encode_integer(out, e.duration_us as i64);
        encoder::encode_array_header(out, e.args.len());
        for arg in &e.args {
            encoder::encode_bulk_string(out, arg);
        }
        encoder::encode_bulk_string(out, e.peer.to_string().as_bytes());
        encoder::encode_bulk_string(out, b"");
    }
}

/// Produces the payload for `INFO [section]`.
///
/// Only the `server` and `memory` sections are currently populated, plus a
/// small `keyspace` summary that mirrors Redis' `db0` line. Unknown sections
/// return an empty body, matching Redis' behaviour.
/// Dedicated loop for a MONITOR client. Receives command snapshots from
/// the registry and writes them as RESP simple strings to the socket.
/// Exits when the shutdown signal fires or the client disconnects.
async fn run_monitor_loop(
    mut stream: TcpStream,
    _peer: SocketAddr,
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<MonitorMessage>,
    shutdown: Shutdown,
) -> Result<(), FerrumError> {
    let mut out = Vec::new();
    loop {
        tokio::select! {
            biased;
            _ = shutdown.notified() => break,
            msg = rx.recv() => {
                let Some(msg) = msg else { break }; // sender dropped
                out.clear();
                // Format: +<timestamp_ms> [<peer>] "<cmd>" "<arg>"...\r\n
                let ts = msg.timestamp_ms;
                let peer_str = msg.peer.to_string();
                let args_quoted: Vec<String> = msg
                    .args
                    .iter()
                    .map(|a| format!("\"{}\"", String::from_utf8_lossy(a)))
                    .collect();
                let line = format!("+{ts} [{peer_str}] {}\r\n", args_quoted.join(" "));
                out.extend_from_slice(line.as_bytes());
                if stream.write_all(&out).await.is_err() {
                    break;
                }
            }
        }
    }
    Ok(())
}

/// Encodes the `HELLO` reply — RESP2 array or RESP3 map depending on version.
fn hello_reply(out: &mut Vec<u8>, version: ProtocolVersion) {
    let proto_num: u8 = match version {
        ProtocolVersion::Resp2 => 2,
        ProtocolVersion::Resp3 => 3,
    };
    match version {
        ProtocolVersion::Resp2 => {
            encoder::encode_array_header(out, 8);
        }
        ProtocolVersion::Resp3 => {
            // RESP3 map with 4 key-value pairs
            encoder::encode_map_header(out, 4);
        }
    }
    encoder::encode_bulk_string(out, b"server");
    encoder::encode_bulk_string(out, b"ferrum");
    encoder::encode_bulk_string(out, b"version");
    encoder::encode_bulk_string(out, env!("CARGO_PKG_VERSION").as_bytes());
    encoder::encode_bulk_string(out, b"proto");
    encoder::encode_integer(out, proto_num as i64);
    encoder::encode_bulk_string(out, b"mode");
    encoder::encode_bulk_string(out, b"standalone");
}

/// Renders the `INFO` reply as an ordered list of `(section_name, text)`
/// pairs. RESP2 callers concatenate the texts into one bulk string (the
/// Redis layout); RESP3 callers emit a native map keyed by section name.
fn render_info_sections(engine: &KvEngine, section: Option<&[u8]>) -> Vec<(&'static str, String)> {
    let wants = |name: &str| match section {
        None => true,
        Some(s) => s.eq_ignore_ascii_case(name.as_bytes()),
    };

    let mut sections: Vec<(&'static str, String)> = Vec::new();

    if wants("server") {
        let mut out = String::new();
        out.push_str("# Server\r\n");
        out.push_str(concat!(
            "ferrum_version:",
            env!("CARGO_PKG_VERSION"),
            "\r\n"
        ));
        out.push_str("\r\n");
        sections.push(("server", out));
    }
    if wants("memory") {
        let used = engine.used_memory();
        let cfg = engine.eviction_config().unwrap_or_default();
        let mut out = String::new();
        out.push_str("# Memory\r\n");
        out.push_str(&format!("used_memory:{used}\r\n"));
        out.push_str(&format!("maxmemory:{}\r\n", cfg.max_memory));
        out.push_str(&format!("maxmemory_policy:{}\r\n", cfg.policy.name()));
        out.push_str(&format!("maxmemory_samples:{}\r\n", cfg.samples));
        // Bytes held by the SIEVE / AdaptiveClimb index. Not part of
        // `used_memory` (which only accounts for the keyspace), but operators
        // sizing a container need to see it.
        out.push_str(&format!(
            "used_memory_policy_indexes:{}\r\n",
            engine.policy_index_memory()
        ));
        // Surface the AHE controller state so operators can see the
        // self-tuning converge. Redis has no exact analogue; we keep the
        // `ahe_` prefix so tooling can filter it out easily.
        let ahe = engine.ahe_snapshot();
        out.push_str(&format!("ahe_alpha:{:.3}\r\n", ahe.alpha));
        out.push_str(&format!("ahe_last_hit_ratio:{:.3}\r\n", ahe.last_hit_ratio));
        out.push_str("\r\n");
        sections.push(("memory", out));
    }
    if wants("stats") {
        let (hits, misses) = engine.keyspace_stats();
        let mut out = String::new();
        out.push_str("# Stats\r\n");
        out.push_str(&format!(
            "total_commands_processed:{}\r\n",
            engine.total_commands_processed()
        ));
        out.push_str(&format!("keyspace_hits:{hits}\r\n"));
        out.push_str(&format!("keyspace_misses:{misses}\r\n"));
        out.push_str(&format!("evicted_keys:{}\r\n", engine.evicted_keys()));
        out.push_str(&format!("expired_keys:{}\r\n", engine.expired_keys()));
        // Persistence failures are logged as warnings and do not fail the
        // client command, so this counter is the only operator-visible signal
        // that acknowledged writes are not reaching the disk.
        out.push_str(&format!(
            "aof_write_errors:{}\r\n",
            engine.aof_write_errors()
        ));
        out.push_str(&format!("aof_fsyncs:{}\r\n", engine.aof_fsync_count()));
        out.push_str(&format!("aof_fsync_skips:{}\r\n", engine.aof_fsync_skips()));
        out.push_str(&format!(
            "aof_append_batches:{}\r\n",
            engine.aof_append_batches()
        ));
        out.push_str("\r\n");
        sections.push(("stats", out));
    }
    if wants("keyspace") {
        let keys = engine.dbsize().unwrap_or(0);
        let mut out = String::new();
        out.push_str("# Keyspace\r\n");
        if keys > 0 {
            let (expires, avg_ttl) = engine.expire_stats().unwrap_or((0, 0));
            out.push_str(&format!(
                "db0:keys={keys},expires={expires},avg_ttl={avg_ttl}\r\n"
            ));
        }
        out.push_str("\r\n");
        sections.push(("keyspace", out));
    }
    sections
}

/// Produces the reply for `CONFIG GET <param|*>`.
///
/// Mirrors Redis' flat-array layout: every matched parameter is emitted as a
/// pair of bulk strings `(name, value)`, interleaved in the same array.
/// `CONFIG GET *` returns every recognised parameter; a specific name returns
/// just that pair, or an empty array (`*0`) when the name is unknown — matching
/// Redis' behaviour so clients that probe for an unsupported parameter get a
/// clean "nothing matched" rather than an error.
///
/// RESP3 clients receive a native map (`%N`) instead of the flat array, the
/// same layout Redis uses after a `HELLO 3` handshake.
fn config_get(engine: &KvEngine, pattern: &[u8], out: &mut Vec<u8>, version: ProtocolVersion) {
    let cfg = match engine.eviction_config() {
        Ok(c) => c,
        Err(e) => return write_ferrum_error(out, &e),
    };
    // Never echo the password itself: `CONFIG GET requirepass` used to return
    // it in clear text to any authenticated client (and to the dashboard).
    // A non-empty mask still lets tooling tell "set" from "not set".
    let requirepass: Vec<u8> = match engine.requirepass() {
        Ok(Some(_)) => b"(redacted)".to_vec(),
        _ => Vec::new(),
    };
    // (name, value) pairs in display order. Only the eviction-related
    // parameters and `requirepass` are exposed.
    let pairs: Vec<(&str, Vec<u8>)> = vec![
        ("maxmemory", cfg.max_memory.to_string().into_bytes()),
        ("maxmemory-policy", cfg.policy.name().as_bytes().to_vec()),
        ("maxmemory-samples", cfg.samples.to_string().into_bytes()),
        ("requirepass", requirepass),
        (
            "slowlog-log-slower-than",
            engine.slowlog_threshold_us().to_string().into_bytes(),
        ),
        (
            "slowlog-max-len",
            engine.slowlog_max_len().to_string().into_bytes(),
        ),
    ];
    // Emits the container header: RESP2 array of 2N elements, or RESP3 map
    // of N pairs.
    let header = |out: &mut Vec<u8>, pairs: usize| match version {
        ProtocolVersion::Resp2 => encoder::encode_array_header(out, pairs * 2),
        ProtocolVersion::Resp3 => encoder::encode_map_header(out, pairs),
    };
    if pattern.eq_ignore_ascii_case(b"*") {
        header(out, pairs.len());
        for (name, value) in &pairs {
            encoder::encode_bulk_string(out, name.as_bytes());
            encoder::encode_bulk_string(out, value);
        }
        return;
    }
    let requested = match std::str::from_utf8(pattern) {
        Ok(s) => s,
        Err(_) => return header(out, 0),
    };
    if let Some((name, value)) = pairs
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(requested))
    {
        header(out, 1);
        encoder::encode_bulk_string(out, name.as_bytes());
        encoder::encode_bulk_string(out, value);
    } else {
        header(out, 0);
    }
}

/// Applies `CONFIG SET <param> <value>` to the live engine configuration.
///
/// The change is validated and, on success, installed via
/// [`KvEngine::set_eviction_config`] so it takes effect for all subsequent
/// writes. Only the eviction-related parameters are mutable; unknown
/// parameters and malformed values are rejected with a Redis-style
/// `-ERR Unknown parameter '…'` / `-ERR Invalid argument …` reply. The runtime
/// change is not persisted to the AOF (consistent with Redis' semantics for
/// the parameters we support).
fn config_set(engine: &KvEngine, param: &[u8], value: &[u8], out: &mut Vec<u8>) {
    let name = match std::str::from_utf8(param) {
        Ok(s) => s.to_ascii_lowercase(),
        Err(_) => {
            return encoder::encode_error(out, "ERR Unknown parameter (non-UTF8 parameter name)");
        }
    };
    let value_str = match std::str::from_utf8(value) {
        Ok(s) => s,
        Err(_) => return encoder::encode_error(out, "ERR Invalid argument (non-UTF8 value)"),
    };

    // `requirepass` is stored separately from the eviction config, so it
    // gets its own early return and never falls through to the eviction path.
    if name == "requirepass" {
        match engine.set_requirepass(Some(value_str.as_bytes().to_vec())) {
            Ok(()) => return encoder::encode_simple_string(out, "OK"),
            Err(e) => return write_ferrum_error(out, &e),
        }
    }

    let mut cfg = match engine.eviction_config() {
        Ok(c) => c,
        Err(e) => return write_ferrum_error(out, &e),
    };

    match name.as_str() {
        "maxmemory" => match crate::config::file::parse_bytes(value_str, "maxmemory") {
            Ok(bytes) => cfg.max_memory = bytes,
            Err(msg) => return encoder::encode_error(out, &format!("ERR {msg}")),
        },
        "maxmemory-policy" => match EvictionPolicy::from_name(&value_str.to_ascii_lowercase()) {
            Some(p) => cfg.policy = p,
            None => {
                return encoder::encode_error(
                    out,
                    &format!(
                        "ERR Invalid argument '{value_str}' for CONFIG SET 'maxmemory-policy'"
                    ),
                );
            }
        },
        "maxmemory-samples" => match value_str.trim().parse::<usize>() {
            Ok(s) if s > 0 => cfg.samples = s,
            Ok(_) => {
                return encoder::encode_error(
                    out,
                    "ERR Invalid argument for CONFIG SET 'maxmemory-samples' (must be > 0)",
                );
            }
            Err(_) => {
                return encoder::encode_error(
                    out,
                    "ERR Invalid argument for CONFIG SET 'maxmemory-samples' (not an integer)",
                );
            }
        },
        // Slow-log tunables live on their own atomics, not `cfg`, so they
        // get their own early `OK` return and never fall through to the
        // eviction path.
        "slowlog-log-slower-than" => {
            match value_str.trim().parse::<i64>() {
                Ok(v) => engine.set_slowlog_threshold_us(v),
                Err(_) => {
                    return encoder::encode_error(
                        out,
                        "ERR Invalid argument for CONFIG SET 'slowlog-log-slower-than' (not an integer)",
                    );
                }
            }
            return encoder::encode_simple_string(out, "OK");
        }
        "slowlog-max-len" => {
            match value_str.trim().parse::<u64>() {
                Ok(v) if v > 0 => engine.set_slowlog_max_len(v),
                Ok(_) => {
                    return encoder::encode_error(
                        out,
                        "ERR Invalid argument for CONFIG SET 'slowlog-max-len' (must be > 0)",
                    );
                }
                Err(_) => {
                    return encoder::encode_error(
                        out,
                        "ERR Invalid argument for CONFIG SET 'slowlog-max-len' (not an integer)",
                    );
                }
            }
            return encoder::encode_simple_string(out, "OK");
        }
        _ => {
            return encoder::encode_error(out, &format!("ERR Unknown parameter '{name}'"));
        }
    }

    match engine.set_eviction_config(cfg) {
        Ok(()) => encoder::encode_simple_string(out, "OK"),
        Err(e) => write_ferrum_error(out, &e),
    }
}

/// Runs an `AUTH` attempt and writes the RESP reply.
///
/// Returns `true` when authentication succeeded so the connection
/// handler can flip its per-connection "authenticated" flag. The
/// password is compared as raw bytes so binary-safe secrets work.
fn auth_reply(engine: &KvEngine, password: &[u8], out: &mut Vec<u8>) -> bool {
    match engine.authenticate(password) {
        Ok(true) => {
            encoder::encode_simple_string(out, "OK");
            true
        }
        Ok(false) => {
            encoder::encode_error(
                out,
                "WRONGPASS invalid username-password pair or user is disabled.",
            );
            false
        }
        Err(e) => {
            write_ferrum_error(out, &e);
            false
        }
    }
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

/// Writes a `Result<bool, _>` as a boolean reply — RESP2 integer (`0`/`1`)
/// or RESP3 boolean (`#t`/`#f`) — or as an error.
fn write_bool_integer(
    out: &mut Vec<u8>,
    reply: Result<bool, FerrumError>,
    version: ProtocolVersion,
) {
    match reply {
        Ok(v) => match version {
            ProtocolVersion::Resp2 => encoder::encode_integer(out, v as i64),
            ProtocolVersion::Resp3 => encoder::encode_boolean(out, v),
        },
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
/// Binds a fresh [`std::net::TcpListener`] on `addr` and hands control over
/// to [`run_listener`]. The call blocks until the shared [`Shutdown`] flag
/// flips.
pub fn start(
    addr: &str,
    engine: KvEngine,
    shutdown: Shutdown,
    config: ServerConfig,
) -> Result<(), FerrumError> {
    let listener = StdTcpListener::bind(addr)?;
    let local = listener.local_addr()?;
    info!("FerrumKV listening on {local}");
    run_listener(listener, engine, shutdown, config)
}

/// Runs the accept loop on an already-bound [`std::net::TcpListener`].
///
/// The function is intentionally synchronous so every existing integration
/// test — which binds its own ephemeral listener and then calls into this
/// function from a dedicated thread — keeps working unchanged. Internally
/// we build a multi-threaded tokio runtime, register the listener with it,
/// and drive the async accept loop until [`Shutdown::trigger`] fires.
pub fn run_listener(
    listener: StdTcpListener,
    engine: KvEngine,
    shutdown: Shutdown,
    config: ServerConfig,
) -> Result<(), FerrumError> {
    raise_listen_backlog(&listener);
    listener.set_nonblocking(true)?;

    let mut builder = Builder::new_multi_thread();
    builder.enable_all().thread_name("ferrum-worker");
    if config.worker_threads > 0 {
        builder.worker_threads(config.worker_threads);
    }
    let runtime = builder
        .build()
        .map_err(|e| FerrumError::Internal(format!("failed to build tokio runtime: {e}")))?;

    let result = runtime.block_on(async move {
        let listener = TcpListener::from_std(listener)
            .map_err(|e| FerrumError::Internal(format!("TcpListener::from_std failed: {e}")))?;
        accept_loop(listener, engine, shutdown, config).await
    });

    // Drop the runtime in a tight window: any handler still holding a socket
    // gets a brief grace period before forced teardown. `shutdown_timeout`
    // bounds the wait so a stuck task cannot block process exit indefinitely.
    runtime.shutdown_timeout(Duration::from_millis(500));
    result
}

/// Raises the kernel's accept queue for `listener`.
///
/// `std::net::TcpListener::bind` hardcodes a backlog of 128. When hundreds of
/// clients connect simultaneously (a benchmark start, a reconnect storm, the
/// concurrency smoke test) the queue overflows and the kernel resets otherwise
/// valid connections — the client sees `ECONNRESET` on its first read even
/// though the server never returned an error. Redis listens with 511.
fn raise_listen_backlog(listener: &StdTcpListener) {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        // SAFETY: `listener` owns a valid, open socket for the duration of the
        // call, and `listen(2)` only updates the backlog for that socket.
        let rc = unsafe { libc::listen(listener.as_raw_fd(), 1024) };
        if rc != 0 {
            warn!(
                "failed to raise listen backlog: {}",
                std::io::Error::last_os_error()
            );
        }
    }
    #[cfg(not(unix))]
    {
        let _ = listener;
    }
}

/// Async accept loop running on the tokio runtime.
async fn accept_loop(
    listener: TcpListener,
    engine: KvEngine,
    shutdown: Shutdown,
    config: ServerConfig,
) -> Result<(), FerrumError> {
    let counter = ConnCounter::new();

    loop {
        tokio::select! {
            biased;
            _ = shutdown.notified() => break,
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _)) => {
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
                            // Best-effort refusal reply; we do not wait for
                            // the client to drain it before closing.
                            let mut out = Vec::with_capacity(64);
                            encoder::encode_error(&mut out, "ERR max number of clients reached");
                            let mut stream = stream;
                            let _ = stream.write_all(&out).await;
                            continue;
                        };
                        let engine = engine.clone();
                        let shutdown = shutdown.clone();
                        let config = config.clone();
                        tokio::spawn(async move {
                            if let Err(e) = handle_client(stream, engine, shutdown, config).await {
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
        }
    }

    info!("accept loop exiting");
    Ok(())
}
