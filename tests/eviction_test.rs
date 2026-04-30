//! End-to-end tests for memory accounting, MEMORY USAGE, INFO, and
//! maxmemory eviction.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;
use std::time::Duration;

use ferrum_kv::network::server::{self, ServerConfig};
use ferrum_kv::network::shutdown::Shutdown;
use ferrum_kv::protocol::encoder;
use ferrum_kv::storage::engine::KvEngine;
use ferrum_kv::storage::eviction::{EvictionConfig, EvictionPolicy};

struct ServerGuard {
    addr: String,
    shutdown: Shutdown,
    _server: thread::JoinHandle<()>,
}

impl Drop for ServerGuard {
    fn drop(&mut self) {
        self.shutdown.trigger();
        let _ = TcpStream::connect_timeout(&self.addr.parse().unwrap(), Duration::from_millis(200));
    }
}

fn spawn_server_with_engine(engine: KvEngine) -> ServerGuard {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local_addr").to_string();
    let shutdown = Shutdown::new();
    let shutdown_clone = shutdown.clone();
    let handle = thread::spawn(move || {
        let _ = server::run_listener(listener, engine, shutdown_clone, ServerConfig::default());
    });
    ServerGuard {
        addr,
        shutdown,
        _server: handle,
    }
}

fn connect(addr: &str) -> TcpStream {
    let stream = TcpStream::connect(addr).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(2)))
        .unwrap();
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

fn send(stream: &mut TcpStream, request: &[u8]) -> Vec<u8> {
    stream.write_all(request).expect("write request");
    read_one_reply(stream)
}

fn read_one_reply(stream: &mut TcpStream) -> Vec<u8> {
    let mut out = Vec::new();
    let mut byte = [0u8; 1];
    stream.read_exact(&mut byte).expect("read type byte");
    out.push(byte[0]);
    match byte[0] {
        b'+' | b'-' | b':' => read_until_crlf(stream, &mut out),
        b'$' => {
            let mut header = Vec::new();
            read_until_crlf(stream, &mut header);
            let header_str =
                std::str::from_utf8(&header[..header.len() - 2]).expect("ascii length");
            let len: i64 = header_str.parse().expect("integer length");
            out.extend_from_slice(&header);
            if len >= 0 {
                let mut body = vec![0u8; len as usize + 2];
                stream.read_exact(&mut body).expect("read bulk body");
                out.extend_from_slice(&body);
            }
        }
        other => panic!("unexpected RESP type byte {other:#x}"),
    }
    out
}

fn read_until_crlf(stream: &mut TcpStream, out: &mut Vec<u8>) {
    let mut byte = [0u8; 1];
    loop {
        stream.read_exact(&mut byte).expect("read byte");
        out.push(byte[0]);
        if out.len() >= 2 && out[out.len() - 2] == b'\r' && out[out.len() - 1] == b'\n' {
            return;
        }
    }
}

#[test]
fn memory_usage_reports_integer_for_known_key() {
    let engine = KvEngine::new();
    let guard = spawn_server_with_engine(engine);
    let mut stream = connect(&guard.addr);

    send(&mut stream, &build_request(&[b"SET", b"k", b"hello"]));
    let reply = send(&mut stream, &build_request(&[b"MEMORY", b"USAGE", b"k"]));
    // Payload is `:<N>\r\n`.
    let text = std::str::from_utf8(&reply).unwrap();
    assert!(text.starts_with(':'), "reply: {text:?}");
    let n: u64 = text[1..text.len() - 2].parse().unwrap();
    assert!(n >= (1 + 5), "memory usage should cover key+value: {n}");
}

#[test]
fn memory_usage_returns_null_bulk_for_missing_key() {
    let engine = KvEngine::new();
    let guard = spawn_server_with_engine(engine);
    let mut stream = connect(&guard.addr);

    let reply = send(
        &mut stream,
        &build_request(&[b"MEMORY", b"USAGE", b"absent"]),
    );
    assert_eq!(reply, b"$-1\r\n");
}

#[test]
fn info_memory_contains_used_memory_and_policy() {
    let engine = KvEngine::new();
    engine
        .set_eviction_config(EvictionConfig {
            max_memory: 1024,
            policy: EvictionPolicy::AllKeysLru,
            samples: 7,
        })
        .unwrap();
    let guard = spawn_server_with_engine(engine);
    let mut stream = connect(&guard.addr);

    let reply = send(&mut stream, &build_request(&[b"INFO", b"memory"]));
    let text = String::from_utf8_lossy(&reply);
    assert!(text.starts_with('$'), "reply: {text}");
    assert!(text.contains("used_memory:"));
    assert!(text.contains("maxmemory:1024"));
    assert!(text.contains("maxmemory_policy:allkeys-lru"));
    assert!(text.contains("maxmemory_samples:7"));
}

#[test]
fn noeviction_returns_oom_error_on_overflow() {
    let engine = KvEngine::new();
    engine
        .set_eviction_config(EvictionConfig {
            max_memory: 64, // tight
            policy: EvictionPolicy::NoEviction,
            samples: 5,
        })
        .unwrap();
    let guard = spawn_server_with_engine(engine);
    let mut stream = connect(&guard.addr);

    // First SET fits.
    send(&mut stream, &build_request(&[b"SET", b"k", b"v"]));
    // Second SET with a large value should bust the cap.
    let payload = vec![b'x'; 256];
    let reply = send(&mut stream, &build_request(&[b"SET", b"big", &payload]));
    let text = String::from_utf8_lossy(&reply);
    assert!(text.starts_with('-'), "expected error reply, got {text:?}");
}

#[test]
fn allkeys_lru_evicts_over_the_wire() {
    let engine = KvEngine::new();
    // Room for two small entries.
    engine
        .set_eviction_config(EvictionConfig {
            max_memory: 2 * (1 + 1 + 48),
            policy: EvictionPolicy::AllKeysLru,
            samples: 10,
        })
        .unwrap();
    let guard = spawn_server_with_engine(engine);
    let mut stream = connect(&guard.addr);

    send(&mut stream, &build_request(&[b"SET", b"a", b"1"]));
    thread::sleep(Duration::from_millis(5));
    send(&mut stream, &build_request(&[b"SET", b"b", b"2"]));
    thread::sleep(Duration::from_millis(5));
    // Touch `a` so `b` becomes the LRU victim.
    send(&mut stream, &build_request(&[b"GET", b"a"]));
    thread::sleep(Duration::from_millis(5));
    send(&mut stream, &build_request(&[b"SET", b"c", b"3"]));

    // `b` should have been evicted; both `a` and `c` should be alive.
    let reply_b = send(&mut stream, &build_request(&[b"EXISTS", b"b"]));
    assert_eq!(reply_b, b":0\r\n");
    let reply_a = send(&mut stream, &build_request(&[b"EXISTS", b"a"]));
    assert_eq!(reply_a, b":1\r\n");
    let reply_c = send(&mut stream, &build_request(&[b"EXISTS", b"c"]));
    assert_eq!(reply_c, b":1\r\n");
}

#[test]
fn allkeys_lfu_evicts_cold_key_over_the_wire() {
    let engine = KvEngine::new();
    // Room for two small entries.
    engine
        .set_eviction_config(EvictionConfig {
            max_memory: 2 * (1 + 1 + 48),
            policy: EvictionPolicy::AllKeysLfu,
            samples: 10,
        })
        .unwrap();
    let guard = spawn_server_with_engine(engine);
    let mut stream = connect(&guard.addr);

    send(&mut stream, &build_request(&[b"SET", b"a", b"1"]));
    send(&mut stream, &build_request(&[b"SET", b"b", b"2"]));
    // Warm `a` with many reads so its LFU counter dwarfs `b`'s.
    for _ in 0..64 {
        send(&mut stream, &build_request(&[b"GET", b"a"]));
    }
    // Insert a third key so eviction has to pick between `a` and `b`.
    send(&mut stream, &build_request(&[b"SET", b"c", b"3"]));

    let reply_b = send(&mut stream, &build_request(&[b"EXISTS", b"b"]));
    assert_eq!(reply_b, b":0\r\n", "cold key `b` should be evicted");
    let reply_a = send(&mut stream, &build_request(&[b"EXISTS", b"a"]));
    assert_eq!(reply_a, b":1\r\n", "hot key `a` must survive");
    let reply_c = send(&mut stream, &build_request(&[b"EXISTS", b"c"]));
    assert_eq!(reply_c, b":1\r\n");
}

#[test]
fn allkeys_ahe_evicts_and_publishes_alpha_in_info() {
    let engine = KvEngine::new();
    engine
        .set_eviction_config(EvictionConfig {
            max_memory: 2 * (1 + 1 + 48),
            policy: EvictionPolicy::AllKeysAhe,
            samples: 10,
        })
        .unwrap();
    let guard = spawn_server_with_engine(engine);
    let mut stream = connect(&guard.addr);

    // Fill + warm so AHE has both recency and frequency signal.
    send(&mut stream, &build_request(&[b"SET", b"a", b"1"]));
    send(&mut stream, &build_request(&[b"SET", b"b", b"2"]));
    for _ in 0..32 {
        send(&mut stream, &build_request(&[b"GET", b"a"]));
    }
    // Force at least one eviction pass.
    send(&mut stream, &build_request(&[b"SET", b"c", b"3"]));

    // Cold key must be gone; hot key must live.
    let reply_b = send(&mut stream, &build_request(&[b"EXISTS", b"b"]));
    assert_eq!(reply_b, b":0\r\n");
    let reply_a = send(&mut stream, &build_request(&[b"EXISTS", b"a"]));
    assert_eq!(reply_a, b":1\r\n");

    let reply = send(&mut stream, &build_request(&[b"INFO", b"memory"]));
    let text = String::from_utf8_lossy(&reply);
    assert!(text.contains("maxmemory_policy:allkeys-ahe"));
    assert!(text.contains("ahe_alpha:"));
}

#[test]
fn info_stats_reports_keyspace_hits_and_misses() {
    let engine = KvEngine::new();
    let guard = spawn_server_with_engine(engine);
    let mut stream = connect(&guard.addr);

    send(&mut stream, &build_request(&[b"SET", b"k", b"v"]));
    // One hit, one miss.
    send(&mut stream, &build_request(&[b"GET", b"k"]));
    send(&mut stream, &build_request(&[b"GET", b"absent"]));

    let reply = send(&mut stream, &build_request(&[b"INFO", b"stats"]));
    let text = String::from_utf8_lossy(&reply);
    assert!(text.contains("keyspace_hits:1"), "reply: {text}");
    assert!(text.contains("keyspace_misses:1"), "reply: {text}");
}
