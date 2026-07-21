//! End-to-end tests for the F-04 `BGREWRITEAOF` command.
//!
//! Boots a real server with AOF persistence enabled, drives it over a TCP
//! socket, and asserts that:
//! * a rewrite compacts the on-disk AOF to exactly one `SET` per live key
//!   (no redundant overwrite frames),
//! * the compact AOF replays back to the full pre-rewrite keyspace on restart,
//! * a write issued *during* the rewrite survives the restart.
//!
//! A `#[ignore]` test exercises the 1M-key scale target from the roadmap.

use std::collections::HashSet;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ferrum_kv::network::server::{self, ServerConfig};
use ferrum_kv::network::shutdown::Shutdown;
use ferrum_kv::persistence::AofWriter;
use ferrum_kv::persistence::config::{AofConfig, FsyncPolicy};
use ferrum_kv::persistence::replay;
use ferrum_kv::protocol::encoder;
use ferrum_kv::storage::engine::KvEngine;

/// Number of keys used by the fast correctness cases.
const KEY_COUNT: u32 = 200;

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn tmp_path(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    std::env::temp_dir().join(format!("ferrum-rewrite-it-{label}-{nanos}-{n}.aof"))
}

/// Keeps the background server thread alive and lets the test shut it down.
struct ServerGuard {
    addr: String,
    shutdown: Shutdown,
    _thread: thread::JoinHandle<()>,
}

impl ServerGuard {
    fn shutdown(self) {
        self.shutdown.trigger();
        let _ = self._thread.join();
    }
}

/// Starts a server with AOF persistence and `FsyncPolicy::Always`.
fn spawn_aof_server(path: &Path) -> ServerGuard {
    spawn_aof_server_with_policy(path, FsyncPolicy::Always)
}

/// Starts a server with AOF persistence using the given fsync policy.
fn spawn_aof_server_with_policy(path: &Path, fsync: FsyncPolicy) -> ServerGuard {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local_addr").to_string();
    let cfg = AofConfig::new(path.to_path_buf(), fsync);
    let writer = Arc::new(AofWriter::open(&cfg).expect("open aof"));
    let engine = KvEngine::new().with_aof(Arc::clone(&writer));
    let shutdown = Shutdown::new();
    let shutdown_for_thread = shutdown.clone();
    let handle = thread::spawn(move || {
        let _ = server::run_listener(
            listener,
            engine,
            shutdown_for_thread,
            ServerConfig::default(),
        );
    });
    // Give the accept loop a moment to come up.
    thread::sleep(Duration::from_millis(200));
    ServerGuard {
        addr,
        shutdown,
        _thread: handle,
    }
}

/// Starts a fresh server that *replays* `path` on boot (mirrors production
/// startup: replay existing AOF, then attach a writer).
fn spawn_replayed_server(path: &Path) -> ServerGuard {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local_addr").to_string();
    let engine = KvEngine::new();
    replay(path, &engine).expect("replay aof");
    let cfg = AofConfig::new(path.to_path_buf(), FsyncPolicy::Always);
    let writer = Arc::new(AofWriter::open(&cfg).expect("open aof"));
    let engine = engine.with_aof(Arc::clone(&writer));
    let shutdown = Shutdown::new();
    let shutdown_for_thread = shutdown.clone();
    let handle = thread::spawn(move || {
        let _ = server::run_listener(
            listener,
            engine,
            shutdown_for_thread,
            ServerConfig::default(),
        );
    });
    thread::sleep(Duration::from_millis(200));
    ServerGuard {
        addr,
        shutdown,
        _thread: handle,
    }
}

fn connect(addr: &str) -> TcpStream {
    let stream = TcpStream::connect(addr).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("set read timeout");
    stream
        .set_write_timeout(Some(Duration::from_secs(2)))
        .expect("set write timeout");
    stream
}

fn build_request(args: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(format!("*{}\r\n", args.len()).as_bytes());
    for arg in args {
        encoder::encode_bulk_string(&mut out, arg);
    }
    out
}

fn send(stream: &mut TcpStream, args: &[&[u8]]) -> Vec<u8> {
    send_with_timeout(stream, args, Duration::from_millis(300))
}

fn send_with_timeout(stream: &mut TcpStream, args: &[&[u8]], timeout_dur: Duration) -> Vec<u8> {
    let req = build_request(args);
    stream.write_all(&req).expect("write request");
    read_reply_timeout(stream, timeout_dur)
}

fn read_reply_timeout(stream: &mut TcpStream, timeout_dur: Duration) -> Vec<u8> {
    stream
        .set_read_timeout(Some(timeout_dur))
        .expect("set read timeout");
    read_bytes(stream)
}

/// Encodes a single bulk string and returns the bytes (unlike
/// [`encoder::encode_bulk_string`], which writes into a caller-supplied
/// buffer and returns `()`).
fn bulk_bytes(value: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    encoder::encode_bulk_string(&mut out, value);
    out
}

fn read_bytes(stream: &mut TcpStream) -> Vec<u8> {
    let mut out = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(_) => break,
        }
    }
    out
}

/// Extracts every `SET` key from a compact/raw AOF byte stream.
///
/// Walks the bytes looking for the SET record prefix and decodes the key bulk
/// string that follows. `PEXPIREAT` records use a different bulk-string
/// length (`$9` vs `$3`) so they are never mistaken for `SET`.
fn parse_set_keys(bytes: &[u8]) -> Vec<Vec<u8>> {
    let prefix = b"*3\r\n$3\r\nSET\r\n";
    let mut keys = Vec::new();
    let mut i = 0usize;
    while i + prefix.len() <= bytes.len() {
        if &bytes[i..i + prefix.len()] == prefix {
            let mut j = i + prefix.len(); // now at '$'
            // Parse the key length (`$<len>\r\n`).
            let mut num = 0usize;
            j += 1; // skip '$'
            while bytes[j] != b'\r' {
                num = num * 10 + (bytes[j] - b'0') as usize;
                j += 1;
            }
            j += 2; // skip CRLF
            keys.push(bytes[j..j + num].to_vec());
            i = j + num + 2;
        } else {
            i += 1;
        }
    }
    keys
}

/// Polls `path` until the on-disk AOF holds exactly `n` distinct `SET` keys
/// (i.e. the rewrite has finished and compacted), or the timeout elapses.
fn wait_for_compact(path: &PathBuf, n: usize, timeout: Duration) -> bool {
    let start = Instant::now();
    loop {
        if let Ok(bytes) = std::fs::read(path) {
            let keys = parse_set_keys(&bytes);
            let distinct: HashSet<Vec<u8>> = keys.iter().cloned().collect();
            if keys.len() == n && distinct.len() == n {
                return true;
            }
        }
        if start.elapsed() > timeout {
            return false;
        }
        thread::sleep(Duration::from_millis(50));
    }
}

/// Polls `path` until the on-disk AOF file size stops changing for a brief
/// period, indicating the rewrite (including any delta replay) has completed.
fn wait_for_stable_aof(path: &PathBuf, timeout: Duration) -> bool {
    let start = Instant::now();
    let mut last_size = 0u64;
    let mut stable_ms = 0u64;
    const STABLE_THRESHOLD_MS: u64 = 300;
    loop {
        let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        if size > 0 && size == last_size {
            stable_ms += 50;
            if stable_ms >= STABLE_THRESHOLD_MS {
                return true;
            }
        } else {
            stable_ms = 0;
        }
        last_size = size;
        if start.elapsed() > timeout {
            return false;
        }
        thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn bgrewriteaof_produces_compact_aof_and_restores_on_restart() {
    let path = tmp_path("compact-restart");
    let server = spawn_aof_server(&path);
    let mut s = connect(&server.addr);

    // Seed the keyspace with two passes to force on-disk redundancy.
    for pass in 0..2u32 {
        for i in 0..KEY_COUNT {
            let val = format!("v{pass}-{i}");
            assert_eq!(
                send(
                    &mut s,
                    &[b"SET", format!("k{i}").as_bytes(), val.as_bytes()]
                ),
                b"+OK\r\n"
            );
        }
    }

    // Trigger the background rewrite; the reply is immediate.
    assert_eq!(send(&mut s, &[b"BGREWRITEAOF"]), b"+OK\r\n");

    // The on-disk AOF must compact to exactly one SET per distinct key.
    assert!(
        wait_for_compact(&path, KEY_COUNT as usize, Duration::from_secs(10)),
        "rewrite did not compact the AOF within the timeout"
    );
    let bytes = std::fs::read(&path).unwrap();
    let keys = parse_set_keys(&bytes);
    assert_eq!(
        keys.len(),
        KEY_COUNT as usize,
        "compact AOF must have exactly one SET per key (no redundant overwrites)"
    );
    let distinct: HashSet<Vec<u8>> = keys.iter().cloned().collect();
    assert_eq!(distinct.len(), KEY_COUNT as usize, "no duplicate SET keys");

    server.shutdown();

    // Restart from the compact AOF and confirm the full keyspace is restored.
    let server2 = spawn_replayed_server(&path);
    let mut s2 = connect(&server2.addr);
    for i in 0..KEY_COUNT {
        let reply = send(&mut s2, &[b"GET", format!("k{i}").as_bytes()]);
        // Last pass used value "v1-{i}".
        let expected = format!("v1-{i}");
        assert_eq!(reply, bulk_bytes(expected.as_bytes()));
    }
    server2.shutdown();
    let _ = std::fs::remove_file(&path);
}

#[test]
fn write_during_rewrite_survives_restart() {
    let path = tmp_path("during-restart");
    let server = spawn_aof_server(&path);
    let mut s = connect(&server.addr);

    for i in 0..KEY_COUNT {
        assert_eq!(
            send(
                &mut s,
                &[
                    b"SET",
                    format!("k{i}").as_bytes(),
                    format!("v{i}").as_bytes()
                ]
            ),
            b"+OK\r\n"
        );
    }

    // Kick off the rewrite.
    assert_eq!(send(&mut s, &[b"BGREWRITEAOF"]), b"+OK\r\n");

    // Issue a write while the rewrite is running so it lands in the delta
    // buffer (or as a post-swap append if the rewrite already finished).
    let mut wrote_during = false;
    for _ in 0..200 {
        // Peek at the rewrite state by checking the file periodically is
        // racy, so just keep writing; the engine serialises correctly either
        // way. We flag "during" if the compact file is not yet stable.
        let compact_now = std::fs::read(&path)
            .map(|b| parse_set_keys(&b).len() == KEY_COUNT as usize)
            .unwrap_or(false);
        if compact_now {
            break;
        }
        assert_eq!(send(&mut s, &[b"SET", b"during", b"yes"]), b"+OK\r\n");
        wrote_during = true;
    }
    if !wrote_during {
        // Rewrite finished before we wrote; still assert the write survives.
        assert_eq!(send(&mut s, &[b"SET", b"during", b"yes"]), b"+OK\r\n");
    }

    // Wait for the rewrite to complete. The delta replay appends the
    // "during" SETs after the compact snapshot, so the final AOF has more
    // than KEY_COUNT SET records. Use a file-stability check instead of
    // an exact record count.
    assert!(
        wait_for_stable_aof(&path, Duration::from_secs(10)),
        "rewrite did not complete"
    );

    server.shutdown();

    let server2 = spawn_replayed_server(&path);
    let mut s2 = connect(&server2.addr);
    assert_eq!(send(&mut s2, &[b"GET", b"during"]), bulk_bytes(b"yes"));
    for i in 0..KEY_COUNT {
        assert_eq!(
            send(&mut s2, &[b"GET", format!("k{i}").as_bytes()]),
            bulk_bytes(format!("v{i}").as_bytes())
        );
    }
    server2.shutdown();
    let _ = std::fs::remove_file(&path);
}

/// Scale test for the "AOF rewrite tested with 1M keys" roadmap criterion.
///
/// Ignored by default so the normal `cargo test` run stays fast; run it with
/// `cargo test --ignored aof_rewrite`.
///
/// Uses `FsyncPolicy::No` to avoid 1M synchronous fsync calls during data
/// seeding; the test validates rewrite correctness and AOF replay, not fsync
/// latency.
#[test]
#[ignore]
fn rewrite_with_1m_keys_stays_correct() {
    let path = tmp_path("1m");
    let server = spawn_aof_server_with_policy(&path, FsyncPolicy::No);
    let mut s = connect(&server.addr);

    let n: u32 = 1_000_000;
    let scale_timeout = Duration::from_secs(30);
    let report_every = 100_000u32;
    for i in 0..n {
        assert_eq!(
            send_with_timeout(
                &mut s,
                &[
                    b"SET",
                    format!("k{i}").as_bytes(),
                    format!("v{i}").as_bytes()
                ],
                scale_timeout,
            ),
            b"+OK\r\n"
        );
        if i > 0 && i % report_every == 0 {
            eprintln!("  seeded {i}/{n} keys");
        }
    }
    eprintln!("seeding complete ({n} keys), triggering BGREWRITEAOF");
    assert_eq!(
        send_with_timeout(&mut s, &[b"BGREWRITEAOF"], scale_timeout),
        b"+OK\r\n"
    );
    assert!(
        wait_for_compact(&path, n as usize, Duration::from_secs(60)),
        "1M-key rewrite did not compact within the timeout"
    );

    // Replay the compact file and confirm the keyspace is fully restored.
    server.shutdown();
    let restored = KvEngine::new();
    let stats = replay(&path, &restored).unwrap();
    assert_eq!(
        stats.applied, n as usize,
        "compact AOF must replay exactly one command per key"
    );
    assert_eq!(restored.dbsize().unwrap(), n as usize);
    assert_eq!(restored.get(b"k0").unwrap(), Some(b"v0".to_vec()));
    assert_eq!(
        restored.get(format!("k{}", n - 1).as_bytes()).unwrap(),
        Some(format!("v{}", n - 1).into_bytes())
    );
    let _ = std::fs::remove_file(&path);
}
