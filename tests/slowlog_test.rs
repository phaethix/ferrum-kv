//! End-to-end tests for the F-03 SLOWLOG command and its CONFIG wiring.
//!
//! Boots a real server on an ephemeral port and drives it over a TCP socket,
//! asserting the RESP2 bytes the server emits for `SLOWLOG GET/LEN/RESET`
//! and `CONFIG GET/SET slowlog-*`. Mirrors the harness in
//! `resp2_wire_test.rs` (real listener, background accept loop, hand-crafted
//! frames) so wire-level regressions surface byte-for-byte.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;
use std::time::Duration;

use ferrum_kv::network::server::{self, ServerConfig};
use ferrum_kv::network::shutdown::Shutdown;
use ferrum_kv::protocol::encoder;
use ferrum_kv::storage::engine::KvEngine;

/// Keeps the background server thread alive for the duration of a test and
/// exposes the address to connect to.
struct ServerGuard {
    addr: String,
    _thread: thread::JoinHandle<()>,
}

fn spawn_server() -> ServerGuard {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local_addr").to_string();
    let engine = KvEngine::new();
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
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("set_read_timeout");
    stream
        .set_write_timeout(Some(Duration::from_secs(2)))
        .expect("set_write_timeout");
    stream
}

/// Builds a RESP2 `*N` array frame from the provided bulk-string arguments.
fn build_request(args: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(format!("*{}\r\n", args.len()).as_bytes());
    for arg in args {
        encoder::encode_bulk_string(&mut out, arg);
    }
    out
}

/// Sends `request` and reads the entire RESP2 reply (until the server stops
/// sending or the read times out), returning the raw bytes.
fn read_reply(stream: &mut TcpStream) -> Vec<u8> {
    stream
        .set_read_timeout(Some(Duration::from_millis(300)))
        .expect("set read timeout");
    let mut out = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => out.extend_from_slice(&buf[..n]),
            // Timeout / would-block: the reply is complete.
            Err(_) => break,
        }
    }
    out
}

fn send(stream: &mut TcpStream, args: &[&[u8]]) -> Vec<u8> {
    let req = build_request(args);
    stream.write_all(&req).expect("write request");
    read_reply(stream)
}

#[test]
fn config_set_enables_and_disables_slowlog() {
    let server = spawn_server();
    let mut s = connect(&server.addr);

    // Disabled by default (threshold 0): a SET is not logged.
    assert_eq!(
        send(
            &mut s,
            &[b"CONFIG", b"SET", b"slowlog-log-slower-than", b"0"]
        ),
        b"+OK\r\n"
    );
    assert_eq!(send(&mut s, &[b"SET", b"foo", b"bar"]), b"+OK\r\n");
    assert_eq!(send(&mut s, &[b"SLOWLOG", b"LEN"]), b":0\r\n");

    // Force-on via a negative threshold: everything is logged.
    assert_eq!(
        send(
            &mut s,
            &[b"CONFIG", b"SET", b"slowlog-log-slower-than", b"-1"]
        ),
        b"+OK\r\n"
    );
    assert_eq!(send(&mut s, &[b"SET", b"baz", b"qux"]), b"+OK\r\n");
    assert_eq!(send(&mut s, &[b"SLOWLOG", b"LEN"]), b":1\r\n");
}

#[test]
fn slowlog_records_args_and_client_addr() {
    let server = spawn_server();
    let mut s = connect(&server.addr);

    assert_eq!(
        send(
            &mut s,
            &[b"CONFIG", b"SET", b"slowlog-log-slower-than", b"-1"]
        ),
        b"+OK\r\n"
    );
    assert_eq!(send(&mut s, &[b"SET", b"k", b"v"]), b"+OK\r\n");

    // Note: with threshold -1 *every* command is logged, including the
    // `CONFIG SET` and the `SLOWLOG GET` themselves, so the ring is
    // not exactly one entry. We therefore assert on the GET reply's
    // *contents* (the SET entry is present) rather than its length.
    let reply = send(&mut s, &[b"SLOWLOG", b"GET"]);
    // The SET entry's args were reconstructed: ["SET","k","v"].
    assert!(
        reply
            .windows(b"$3\r\nSET\r\n".len())
            .any(|w| w == b"$3\r\nSET\r\n"),
        "missing SET in slow-log args"
    );
    assert!(
        reply
            .windows(b"$1\r\nk\r\n".len())
            .any(|w| w == b"$1\r\nk\r\n"),
        "missing key k"
    );
    assert!(
        reply
            .windows(b"$1\r\nv\r\n".len())
            .any(|w| w == b"$1\r\nv\r\n"),
        "missing value v"
    );
    // Client address is the loopback peer.
    let text = String::from_utf8_lossy(&reply);
    assert!(
        text.contains("127.0.0.1:"),
        "expected client addr, got: {text}"
    );
    // Client name is an empty bulk (no CLIENT SETNAME yet).
    assert!(
        reply
            .windows(b"$0\r\n\r\n".len())
            .any(|w| w == b"$0\r\n\r\n"),
        "expected empty client-name bulk"
    );
}

#[test]
fn slowlog_reset_clears_ring() {
    let server = spawn_server();
    let mut s = connect(&server.addr);

    assert_eq!(
        send(
            &mut s,
            &[b"CONFIG", b"SET", b"slowlog-log-slower-than", b"-1"]
        ),
        b"+OK\r\n"
    );
    // `SET a b` is recorded (alongside the already-logged `CONFIG
    // SET` and any later commands), then RESET empties the ring.
    assert_eq!(send(&mut s, &[b"SET", b"a", b"b"]), b"+OK\r\n");
    assert_eq!(send(&mut s, &[b"SLOWLOG", b"RESET"]), b"+OK\r\n");
    assert_eq!(send(&mut s, &[b"SLOWLOG", b"LEN"]), b":0\r\n");
}

#[test]
fn config_get_round_trips_slowlog_params() {
    let server = spawn_server();
    let mut s = connect(&server.addr);

    // Defaults: threshold 10000µs, max-len 128.
    let reply = send(&mut s, &[b"CONFIG", b"GET", b"slowlog-log-slower-than"]);
    assert!(
        reply
            .windows(b"$5\r\n10000\r\n".len())
            .any(|w| w == b"$5\r\n10000\r\n"),
        "expected default threshold 10000"
    );

    let reply = send(&mut s, &[b"CONFIG", b"GET", b"slowlog-max-len"]);
    assert!(
        reply
            .windows(b"$3\r\n128\r\n".len())
            .any(|w| w == b"$3\r\n128\r\n"),
        "expected default max-len 128"
    );

    // Wildcard GET includes both parameters.
    let reply = send(&mut s, &[b"CONFIG", b"GET", b"*"]);
    let text = String::from_utf8_lossy(&reply);
    assert!(text.contains("slowlog-log-slower-than"));
    assert!(text.contains("slowlog-max-len"));
}

#[test]
fn config_set_max_len_rejects_zero() {
    let server = spawn_server();
    let mut s = connect(&server.addr);
    let reply = send(&mut s, &[b"CONFIG", b"SET", b"slowlog-max-len", b"0"]);
    assert!(
        reply.starts_with(b"-ERR"),
        "expected ERR for max-len 0, got: {:?}",
        String::from_utf8_lossy(&reply)
    );
}
