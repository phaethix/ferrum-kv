//! Concurrency and stress integration tests.
//!
//! These tests push many clients at a real TCP listener in parallel to flush
//! out races that single-threaded tests cannot reach:
//!
//! * disjoint-key workloads must not lose writes or corrupt the index;
//! * shared-key `INCR` must be exactly atomic across all clients;
//! * when AOF is enabled, a replay into a fresh engine must reproduce the
//!   running engine's state byte-for-byte, proving the log is serialised
//!   correctly under contention.
//!
//! Volumes are deliberately modest (tens of clients × hundreds of ops) so the
//! suite stays fast enough for `cargo test`. The `redis-benchmark` script
//! under `scripts/` is the right tool for pushing real load.

use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ferrum_kv::network::server::{self, ServerConfig};
use ferrum_kv::network::shutdown::Shutdown;
use ferrum_kv::persistence::AofWriter;
use ferrum_kv::persistence::config::{AofConfig, FsyncPolicy};
use ferrum_kv::persistence::replay;
use ferrum_kv::protocol::encoder;
use ferrum_kv::storage::engine::KvEngine;

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn tmp_aof_path(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    std::env::temp_dir().join(format!("ferrum-stress-{label}-{nanos}-{n}.aof"))
}

struct ServerGuard {
    addr: String,
    _thread: thread::JoinHandle<()>,
}

fn spawn_server(engine: KvEngine) -> ServerGuard {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local_addr").to_string();
    let handle = thread::spawn(move || {
        let _ = server::run_listener(listener, engine, Shutdown::new(), ServerConfig::default());
    });
    ServerGuard {
        addr,
        _thread: handle,
    }
}

fn connect(addr: &str) -> TcpStream {
    let stream = TcpStream::connect(addr).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("set_read_timeout");
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .expect("set_write_timeout");
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

/// Reads a single RESP2 reply off the wire. Only the reply kinds produced by
/// the server in these tests are recognised; anything else is an error.
fn read_reply(stream: &mut TcpStream) -> Vec<u8> {
    let mut header = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        stream.read_exact(&mut byte).expect("read reply header");
        header.push(byte[0]);
        if header.len() >= 2 && header.ends_with(b"\r\n") {
            break;
        }
    }
    match header[0] {
        b'+' | b'-' | b':' => header,
        b'$' => {
            let len: i64 = std::str::from_utf8(&header[1..header.len() - 2])
                .expect("bulk len utf8")
                .parse()
                .expect("bulk len parse");
            if len < 0 {
                return header; // null bulk
            }
            let mut body = vec![0u8; len as usize + 2];
            stream.read_exact(&mut body).expect("read bulk body");
            header.extend_from_slice(&body);
            header
        }
        other => panic!("unexpected reply prefix: {other:#x}"),
    }
}

fn expect_simple_ok(stream: &mut TcpStream) {
    assert_eq!(read_reply(stream), b"+OK\r\n");
}

fn expect_integer(stream: &mut TcpStream) -> i64 {
    let reply = read_reply(stream);
    assert_eq!(reply[0], b':', "expected integer reply, got {reply:?}");
    std::str::from_utf8(&reply[1..reply.len() - 2])
        .expect("int utf8")
        .parse()
        .expect("int parse")
}

#[test]
fn many_clients_with_disjoint_keys_do_not_lose_writes() {
    const CLIENTS: usize = 16;
    const OPS_PER_CLIENT: usize = 200;

    let engine = KvEngine::new();
    let server = spawn_server(engine.clone());

    let mut handles = Vec::with_capacity(CLIENTS);
    for cid in 0..CLIENTS {
        let addr = server.addr.clone();
        handles.push(thread::spawn(move || {
            let mut s = connect(&addr);
            for op in 0..OPS_PER_CLIENT {
                let key = format!("c{cid}:k{op}");
                let value = format!("c{cid}:v{op}");
                s.write_all(&build_request(&[b"SET", key.as_bytes(), value.as_bytes()]))
                    .expect("write SET");
                expect_simple_ok(&mut s);
            }
        }));
    }
    for h in handles {
        h.join().expect("client thread");
    }

    assert_eq!(engine.dbsize().unwrap(), CLIENTS * OPS_PER_CLIENT);
    for cid in 0..CLIENTS {
        for op in 0..OPS_PER_CLIENT {
            let key = format!("c{cid}:k{op}");
            let expected = format!("c{cid}:v{op}");
            assert_eq!(
                engine.get(key.as_bytes()).unwrap(),
                Some(expected.into_bytes()),
                "client {cid} op {op} lost"
            );
        }
    }
}

#[test]
fn concurrent_incr_on_shared_counter_is_atomic() {
    const CLIENTS: usize = 16;
    const INCRS_PER_CLIENT: usize = 500;

    let engine = KvEngine::new();
    let server = spawn_server(engine.clone());

    let mut handles = Vec::with_capacity(CLIENTS);
    for _ in 0..CLIENTS {
        let addr = server.addr.clone();
        handles.push(thread::spawn(move || {
            let mut s = connect(&addr);
            for _ in 0..INCRS_PER_CLIENT {
                s.write_all(&build_request(&[b"INCR", b"counter"]))
                    .expect("write INCR");
                let _ = expect_integer(&mut s);
            }
        }));
    }
    for h in handles {
        h.join().expect("client thread");
    }

    let expected = (CLIENTS * INCRS_PER_CLIENT) as i64;
    let stored = engine.get(b"counter").unwrap().expect("counter must exist");
    let as_int: i64 = std::str::from_utf8(&stored)
        .expect("int utf8")
        .parse()
        .expect("int parse");
    assert_eq!(as_int, expected, "lost INCR under contention");
}

#[test]
fn concurrent_writes_replay_into_identical_state() {
    const CLIENTS: usize = 8;
    const OPS_PER_CLIENT: usize = 100;

    let path = tmp_aof_path("replay");
    let cfg = AofConfig::new(&path, FsyncPolicy::Always);
    let writer = Arc::new(AofWriter::open(&cfg).expect("open aof"));
    let engine = KvEngine::new().with_aof(Arc::clone(&writer));
    let server = spawn_server(engine.clone());

    let mut handles = Vec::with_capacity(CLIENTS);
    for cid in 0..CLIENTS {
        let addr = server.addr.clone();
        handles.push(thread::spawn(move || {
            let mut s = connect(&addr);
            for op in 0..OPS_PER_CLIENT {
                let key = format!("c{cid}:k{op}");
                let value = format!("v{op}");
                s.write_all(&build_request(&[b"SET", key.as_bytes(), value.as_bytes()]))
                    .expect("write SET");
                expect_simple_ok(&mut s);

                s.write_all(&build_request(&[b"INCR", b"shared:hits"]))
                    .expect("write INCR");
                let _ = expect_integer(&mut s);
            }
        }));
    }
    for h in handles {
        h.join().expect("client thread");
    }

    // Drop the writer so buffers flush before we replay.
    let running_dbsize = engine.dbsize().unwrap();
    let running_counter = engine.get(b"shared:hits").unwrap();
    drop(engine);
    drop(writer);

    let restored = KvEngine::new();
    let stats = replay(&path, &restored).expect("replay");
    assert_eq!(
        stats.skipped, 0,
        "replay must not drop records written under contention"
    );
    assert!(
        !stats.truncated_tail,
        "clean shutdown must not leave a truncated tail"
    );

    assert_eq!(restored.dbsize().unwrap(), running_dbsize);
    assert_eq!(restored.get(b"shared:hits").unwrap(), running_counter);

    for cid in 0..CLIENTS {
        for op in 0..OPS_PER_CLIENT {
            let key = format!("c{cid}:k{op}");
            let expected = format!("v{op}");
            assert_eq!(
                restored.get(key.as_bytes()).unwrap(),
                Some(expected.into_bytes()),
                "replay lost c{cid} op {op}"
            );
        }
    }

    let _ = fs::remove_file(&path);
}
