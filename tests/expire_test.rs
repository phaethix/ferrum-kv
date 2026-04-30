//! End-to-end tests for the TTL / expiration commands.
//!
//! Drives the full server path (RESP2 parser, engine, active sweeper) through
//! a real TCP connection so regressions in wire format, command dispatch, or
//! background expiration all surface here.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;
use std::time::{Duration, Instant};

use ferrum_kv::network::server::{self, ServerConfig};
use ferrum_kv::network::shutdown::Shutdown;
use ferrum_kv::protocol::encoder;
use ferrum_kv::storage::engine::KvEngine;
use ferrum_kv::storage::expire;

struct ServerGuard {
    addr: String,
    shutdown: Shutdown,
    _server: thread::JoinHandle<()>,
}

impl Drop for ServerGuard {
    fn drop(&mut self) {
        self.shutdown.trigger();
        // Self-connect to unblock accept() so the thread can wind down.
        let _ = TcpStream::connect_timeout(&self.addr.parse().unwrap(), Duration::from_millis(200));
    }
}

/// Spawns a server with an active expiration sweeper that ticks frequently
/// so tests do not need to wait a full 100 ms per iteration.
fn spawn_server_with_sweeper() -> ServerGuard {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local_addr").to_string();
    let engine = KvEngine::new();
    let shutdown = Shutdown::new();

    // Keep the sweeper handle alive for the server thread's lifetime.
    let sweeper_engine = engine.clone();
    let sweeper_shutdown = shutdown.clone();
    let handle = thread::spawn(move || {
        let sweeper = expire::spawn_with(
            sweeper_engine,
            sweeper_shutdown.clone(),
            16,
            Duration::from_millis(10),
        );
        let _ = server::run_listener(listener, engine, sweeper_shutdown, ServerConfig::default());
        sweeper.shutdown();
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
        .expect("set_read_timeout");
    stream
        .set_write_timeout(Some(Duration::from_secs(2)))
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

fn send(stream: &mut TcpStream, request: &[u8]) -> Vec<u8> {
    stream.write_all(request).expect("write request");
    read_one_reply(stream)
}

/// Reads exactly one RESP2 reply from `stream`.
///
/// Supports simple strings, errors, integers, and bulk strings, which is
/// enough for the TTL command family.
fn read_one_reply(stream: &mut TcpStream) -> Vec<u8> {
    let mut out = Vec::new();
    let mut byte = [0u8; 1];
    stream.read_exact(&mut byte).expect("read type byte");
    out.push(byte[0]);
    match byte[0] {
        b'+' | b'-' | b':' => {
            read_until_crlf(stream, &mut out);
        }
        b'$' => {
            let mut header = Vec::new();
            read_until_crlf(stream, &mut header);
            // header is "<len>\r\n"; parse it to know how many body bytes follow.
            let header_str =
                std::str::from_utf8(&header[..header.len() - 2]).expect("ascii length prefix");
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
fn ttl_returns_minus_two_for_missing_key() {
    let guard = spawn_server_with_sweeper();
    let mut stream = connect(&guard.addr);

    let reply = send(&mut stream, &build_request(&[b"TTL", b"missing"]));
    assert_eq!(reply, b":-2\r\n");
}

#[test]
fn ttl_returns_minus_one_for_persistent_key() {
    let guard = spawn_server_with_sweeper();
    let mut stream = connect(&guard.addr);

    send(&mut stream, &build_request(&[b"SET", b"k", b"v"]));
    let reply = send(&mut stream, &build_request(&[b"TTL", b"k"]));
    assert_eq!(reply, b":-1\r\n");
}

#[test]
fn expire_and_ttl_roundtrip() {
    let guard = spawn_server_with_sweeper();
    let mut stream = connect(&guard.addr);

    send(&mut stream, &build_request(&[b"SET", b"k", b"v"]));
    let r = send(&mut stream, &build_request(&[b"EXPIRE", b"k", b"100"]));
    assert_eq!(r, b":1\r\n");

    let ttl = send(&mut stream, &build_request(&[b"TTL", b"k"]));
    let text = std::str::from_utf8(&ttl).unwrap();
    assert!(text.starts_with(':') && text.ends_with("\r\n"));
    let n: i64 = text[1..text.len() - 2].parse().unwrap();
    assert!((1..=100).contains(&n), "TTL out of range: {n}");
}

#[test]
fn persist_removes_ttl() {
    let guard = spawn_server_with_sweeper();
    let mut stream = connect(&guard.addr);

    send(&mut stream, &build_request(&[b"SET", b"k", b"v"]));
    send(&mut stream, &build_request(&[b"EXPIRE", b"k", b"60"]));
    let r = send(&mut stream, &build_request(&[b"PERSIST", b"k"]));
    assert_eq!(r, b":1\r\n");
    let ttl = send(&mut stream, &build_request(&[b"TTL", b"k"]));
    assert_eq!(ttl, b":-1\r\n");
}

#[test]
fn pexpire_triggers_background_eviction() {
    let guard = spawn_server_with_sweeper();
    let mut stream = connect(&guard.addr);

    send(&mut stream, &build_request(&[b"SET", b"k", b"v"]));
    send(&mut stream, &build_request(&[b"PEXPIRE", b"k", b"50"]));

    // Poll EXISTS until the sweeper removes the key, with a generous ceiling
    // so CI jitter does not flake the test.
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let reply = send(&mut stream, &build_request(&[b"EXISTS", b"k"]));
        if reply == b":0\r\n" {
            break;
        }
        if Instant::now() >= deadline {
            panic!("key was not expired within 2s");
        }
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn expire_with_negative_seconds_deletes_key() {
    let guard = spawn_server_with_sweeper();
    let mut stream = connect(&guard.addr);

    send(&mut stream, &build_request(&[b"SET", b"k", b"v"]));
    let r = send(&mut stream, &build_request(&[b"EXPIRE", b"k", b"-1"]));
    assert_eq!(r, b":1\r\n");
    let reply = send(&mut stream, &build_request(&[b"GET", b"k"]));
    assert_eq!(reply, b"$-1\r\n");
}

#[test]
fn set_overwrite_clears_ttl_on_wire() {
    let guard = spawn_server_with_sweeper();
    let mut stream = connect(&guard.addr);

    send(&mut stream, &build_request(&[b"SET", b"k", b"v"]));
    send(&mut stream, &build_request(&[b"EXPIRE", b"k", b"60"]));
    send(&mut stream, &build_request(&[b"SET", b"k", b"v2"]));
    let ttl = send(&mut stream, &build_request(&[b"TTL", b"k"]));
    assert_eq!(ttl, b":-1\r\n");
}
