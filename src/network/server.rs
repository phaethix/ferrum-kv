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
use crate::network::shutdown::Shutdown;
use crate::protocol::encoder;
use crate::protocol::parser::{self, Command, FrameParse};
use crate::storage::engine::{KvEngine, SlowLogEntry, TtlStatus, current_epoch_ms};
use crate::storage::eviction::EvictionPolicy;

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
    let mut outbuf: Vec<u8> = Vec::with_capacity(256);

    // Per-connection authentication flag. Starts `false`; a successful `AUTH`
    // flips it, after which every other command runs. We keep this state in the
    // connection handler (it is connection-local) while the configured password
    // itself lives in the shared engine. Declared once, outside the read loop,
    // so that authentication survives across packet boundaries and pipelined
    // commands.
    let mut authed = false;

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

        // Drain as many complete frames as the buffer currently holds.
        loop {
            match parser::parse_frame(&inbuf) {
                Ok(FrameParse::Complete { command, consumed }) => {
                    outbuf.clear();
                    // When a password is configured, every command except
                    // `AUTH` is rejected until the connection authenticates.
                    let requires_auth = engine.requirepass().map(|p| p.is_some()).unwrap_or(false);
                    if let Command::Auth { password } = &command {
                        if auth_reply(&engine, password, &mut outbuf) {
                            authed = true;
                        }
                    } else if requires_auth && !authed {
                        encoder::encode_error(&mut outbuf, "NOAUTH Authentication required.");
                    } else {
                        execute_command(command, &engine, Some(peer), &mut outbuf);
                    }
                    if let Err(e) = stream.write_all(&outbuf).await {
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
                    if let Err(e) = stream.write_all(&outbuf).await {
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
                    let _ = stream.write_all(&outbuf).await;
                    warn!("protocol error from {peer}: {e}");
                    return Ok(());
                }
            }
        }

        if inbuf.len() > READ_BUF_MAX {
            outbuf.clear();
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
) {
    // Snapshot the command's RESP args *before* the consuming `match` (NLL
    // lets a borrow precede the move), and time the whole dispatch. The
    // snapshot is taken only when the slow-log is active, so a disabled log
    // leaves the hot path allocation-free.
    let slow_args = if engine.slowlog_active() {
        Some(cmd.args())
    } else {
        None
    };
    // SLOWLOG is a meta/observability command; it must not pollute its
    // own ring (a RESET would otherwise re-log itself and never clear).
    let is_slowlog_meta = matches!(cmd, Command::SlowLog { .. });
    let start = Instant::now();
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
        Command::Exists { keys } => match engine.exists_many(&keys) {
            Ok(n) => encoder::encode_integer(out, n as i64),
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
        Command::Config { sub, args } => {
            if sub.eq_ignore_ascii_case(b"GET") {
                config_get(engine, &args[0], out);
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
    }
    // Slow-log: record the command only if it crossed the threshold. The
    // snapshot is `None` (and no clock was read) when logging is disabled.
    // SLOWLOG never observes itself.
    if let Some(args) = slow_args
        && !is_slowlog_meta
    {
        let elapsed_us = start.elapsed().as_micros() as u64;
        engine.maybe_push_slowlog(args, client, elapsed_us);
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
            let (expires, avg_ttl) = engine.expire_stats().unwrap_or((0, 0));
            out.push_str(&format!(
                "db0:keys={keys},expires={expires},avg_ttl={avg_ttl}\r\n"
            ));
        }
        out.push_str("\r\n");
    }
    out
}

/// Produces the reply for `CONFIG GET <param|*>`.
///
/// Mirrors Redis' flat-array layout: every matched parameter is emitted as a
/// pair of bulk strings `(name, value)`, interleaved in the same array.
/// `CONFIG GET *` returns every recognised parameter; a specific name returns
/// just that pair, or an empty array (`*0`) when the name is unknown — matching
/// Redis' behaviour so clients that probe for an unsupported parameter get a
/// clean "nothing matched" rather than an error.
fn config_get(engine: &KvEngine, pattern: &[u8], out: &mut Vec<u8>) {
    let cfg = match engine.eviction_config() {
        Ok(c) => c,
        Err(e) => return write_ferrum_error(out, &e),
    };
    // The configured password (or empty when none is set), cloned
    // once so the pairs array below can own its bytes.
    let requirepass = engine.requirepass().unwrap_or_default().unwrap_or_default();
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
    if pattern.eq_ignore_ascii_case(b"*") {
        encoder::encode_array_header(out, pairs.len() * 2);
        for (name, value) in &pairs {
            encoder::encode_bulk_string(out, name.as_bytes());
            encoder::encode_bulk_string(out, value);
        }
        return;
    }
    let requested = match std::str::from_utf8(pattern) {
        Ok(s) => s,
        Err(_) => return encoder::encode_array_header(out, 0),
    };
    if let Some((name, value)) = pairs
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(requested))
    {
        encoder::encode_array_header(out, 2);
        encoder::encode_bulk_string(out, name.as_bytes());
        encoder::encode_bulk_string(out, value);
    } else {
        encoder::encode_array_header(out, 0);
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
