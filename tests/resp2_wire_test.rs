//! End-to-end wire-level tests for the RESP2 network layer.
//!
//! These tests exercise the full client → TCP → server → TCP → client path:
//! a real listener is bound to an ephemeral port, the accept loop runs in a
//! background thread, and requests are hand-crafted as RESP2 bytes so that
//! the reply can be asserted byte for byte. The goal is to catch regressions
//! that unit tests on the parser/encoder in isolation cannot, such as frame
//! boundaries, pipelining, and recovery after a command-level error.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;
use std::time::Duration;

use ferrum_kv::network::server;
use ferrum_kv::protocol::encoder;
use ferrum_kv::storage::engine::KvEngine;

/// Handle returned by [`spawn_server`]; keeps the server thread alive for the
/// duration of a test and exposes the address to connect to.
struct ServerGuard {
    addr: String,
    _thread: thread::JoinHandle<()>,
}

/// Binds a listener on `127.0.0.1:0`, spawns the RESP2 accept loop on a
/// background thread, and returns the address the caller should connect to.
///
/// The accept loop exits when the listener is dropped, which happens when the
/// server thread finishes. Tests rely on OS-level socket cleanup rather than
/// an explicit shutdown because no graceful-shutdown API exists yet — the
/// background thread is simply detached at test end.
fn spawn_server() -> ServerGuard {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local_addr").to_string();
    let engine = KvEngine::new();
    let handle = thread::spawn(move || {
        // `run_listener` only returns on listener failure; in tests we simply
        // let it run until the process exits.
        let _ = server::run_listener(listener, engine);
    });
    ServerGuard {
        addr,
        _thread: handle,
    }
}

/// Opens a TCP connection to the server with sensible read/write timeouts so
/// that a bug in the server cannot wedge the test suite indefinitely.
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

/// Builds a RESP2 `*N` array frame out of the provided bulk-string arguments.
fn build_request(args: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(format!("*{}\r\n", args.len()).as_bytes());
    for arg in args {
        encoder::encode_bulk_string(&mut out, arg);
    }
    out
}

/// Sends `request` and reads until `expected.len()` bytes have arrived, then
/// asserts byte-for-byte equality. Any short read surfaces as a test failure
/// instead of hanging.
fn round_trip(stream: &mut TcpStream, request: &[u8], expected: &[u8]) {
    stream.write_all(request).expect("write request");
    let mut reply = vec![0u8; expected.len()];
    stream.read_exact(&mut reply).expect("read reply");
    assert_eq!(
        reply,
        expected,
        "unexpected reply\n  got:      {:?}\n  expected: {:?}",
        String::from_utf8_lossy(&reply),
        String::from_utf8_lossy(expected),
    );
}

#[test]
fn ping_returns_simple_pong() {
    let server = spawn_server();
    let mut s = connect(&server.addr);
    round_trip(&mut s, &build_request(&[b"PING"]), b"+PONG\r\n");
}

#[test]
fn ping_with_message_returns_bulk_echo() {
    let server = spawn_server();
    let mut s = connect(&server.addr);
    round_trip(
        &mut s,
        &build_request(&[b"PING", b"hello world"]),
        b"$11\r\nhello world\r\n",
    );
}

#[test]
fn set_get_del_exists_roundtrip() {
    let server = spawn_server();
    let mut s = connect(&server.addr);

    round_trip(
        &mut s,
        &build_request(&[b"SET", b"name", b"ferrum"]),
        b"+OK\r\n",
    );
    round_trip(
        &mut s,
        &build_request(&[b"GET", b"name"]),
        b"$6\r\nferrum\r\n",
    );
    round_trip(&mut s, &build_request(&[b"EXISTS", b"name"]), b":1\r\n");
    round_trip(&mut s, &build_request(&[b"DEL", b"name"]), b":1\r\n");
    round_trip(&mut s, &build_request(&[b"EXISTS", b"name"]), b":0\r\n");
    // A second DEL on a missing key still succeeds but returns 0.
    round_trip(&mut s, &build_request(&[b"DEL", b"name"]), b":0\r\n");
}

#[test]
fn del_with_multiple_keys_returns_removed_count() {
    let server = spawn_server();
    let mut s = connect(&server.addr);

    round_trip(&mut s, &build_request(&[b"SET", b"a", b"1"]), b"+OK\r\n");
    round_trip(&mut s, &build_request(&[b"SET", b"b", b"2"]), b"+OK\r\n");
    // Two of the three keys exist, so DEL returns :2 and the missing key is
    // ignored without affecting the tally.
    round_trip(
        &mut s,
        &build_request(&[b"DEL", b"a", b"missing", b"b"]),
        b":2\r\n",
    );
    round_trip(&mut s, &build_request(&[b"DBSIZE"]), b":0\r\n");
}

#[test]
fn append_creates_and_extends_value() {
    let server = spawn_server();
    let mut s = connect(&server.addr);

    round_trip(
        &mut s,
        &build_request(&[b"APPEND", b"k", b"hello "]),
        b":6\r\n",
    );
    round_trip(
        &mut s,
        &build_request(&[b"APPEND", b"k", b"world"]),
        b":11\r\n",
    );
    round_trip(
        &mut s,
        &build_request(&[b"GET", b"k"]),
        b"$11\r\nhello world\r\n",
    );
}

#[test]
fn strlen_returns_value_byte_length() {
    let server = spawn_server();
    let mut s = connect(&server.addr);

    round_trip(&mut s, &build_request(&[b"STRLEN", b"k"]), b":0\r\n");
    round_trip(
        &mut s,
        &build_request(&[b"SET", b"k", b"hello"]),
        b"+OK\r\n",
    );
    round_trip(&mut s, &build_request(&[b"STRLEN", b"k"]), b":5\r\n");
}

#[test]
fn setnx_inserts_only_when_key_is_absent() {
    let server = spawn_server();
    let mut s = connect(&server.addr);

    // First SETNX succeeds: key did not exist, reply is :1.
    round_trip(
        &mut s,
        &build_request(&[b"SETNX", b"k", b"first"]),
        b":1\r\n",
    );
    // Second SETNX on the same key is a no-op: reply is :0 and the
    // original value must survive unchanged.
    round_trip(
        &mut s,
        &build_request(&[b"SETNX", b"k", b"second"]),
        b":0\r\n",
    );
    round_trip(&mut s, &build_request(&[b"GET", b"k"]), b"$5\r\nfirst\r\n");
}

#[test]
fn mset_then_mget_returns_array_with_order_preserved() {
    let server = spawn_server();
    let mut s = connect(&server.addr);

    round_trip(
        &mut s,
        &build_request(&[b"MSET", b"a", b"1", b"b", b"2"]),
        b"+OK\r\n",
    );
    // MGET on two keys that exist and one that does not: the missing entry
    // must serialise as a null bulk (`$-1`) while the others are ordinary
    // bulks, preserving the request order.
    round_trip(
        &mut s,
        &build_request(&[b"MGET", b"a", b"missing", b"b"]),
        b"*3\r\n$1\r\n1\r\n$-1\r\n$1\r\n2\r\n",
    );
}

#[test]
fn get_missing_key_returns_null_bulk() {
    let server = spawn_server();
    let mut s = connect(&server.addr);
    round_trip(&mut s, &build_request(&[b"GET", b"ghost"]), b"$-1\r\n");
}

#[test]
fn dbsize_and_flushdb_reflect_state() {
    let server = spawn_server();
    let mut s = connect(&server.addr);

    round_trip(&mut s, &build_request(&[b"DBSIZE"]), b":0\r\n");
    round_trip(&mut s, &build_request(&[b"SET", b"a", b"1"]), b"+OK\r\n");
    round_trip(&mut s, &build_request(&[b"SET", b"b", b"2"]), b"+OK\r\n");
    round_trip(&mut s, &build_request(&[b"DBSIZE"]), b":2\r\n");
    round_trip(&mut s, &build_request(&[b"FLUSHDB"]), b"+OK\r\n");
    round_trip(&mut s, &build_request(&[b"DBSIZE"]), b":0\r\n");
}

#[test]
fn embedded_crlf_in_value_does_not_confuse_framing() {
    let server = spawn_server();
    let mut s = connect(&server.addr);

    // The key point of this test is that CRLF inside a bulk payload must be
    // treated as ordinary bytes, not as a frame delimiter. A naive
    // line-oriented parser would split the payload on `\r\n` and lose data;
    // the RESP2 parser reads exactly `$<len>` bytes regardless of content.
    let payload: &[u8] = b"line1\r\nline2\r\nline3 with trailing CRLF\r\n";
    round_trip(&mut s, &build_request(&[b"SET", b"k", payload]), b"+OK\r\n");

    let request = build_request(&[b"GET", b"k"]);
    s.write_all(&request).expect("write");
    let expected_header = format!("${}\r\n", payload.len());
    let expected_total = expected_header.len() + payload.len() + 2;
    let mut reply = vec![0u8; expected_total];
    s.read_exact(&mut reply).expect("read");

    let mut expected = Vec::with_capacity(expected_total);
    expected.extend_from_slice(expected_header.as_bytes());
    expected.extend_from_slice(payload);
    expected.extend_from_slice(b"\r\n");
    assert_eq!(reply, expected);
}

#[test]
fn pipelined_requests_receive_ordered_replies() {
    let server = spawn_server();
    let mut s = connect(&server.addr);

    // Three requests written back-to-back in a single syscall; the server
    // must reply to each one in the same order without losing framing.
    let mut batched = Vec::new();
    batched.extend_from_slice(&build_request(&[b"SET", b"k1", b"v1"]));
    batched.extend_from_slice(&build_request(&[b"SET", b"k2", b"v2"]));
    batched.extend_from_slice(&build_request(&[b"GET", b"k1"]));
    s.write_all(&batched).expect("write batched");

    let expected = b"+OK\r\n+OK\r\n$2\r\nv1\r\n";
    let mut reply = vec![0u8; expected.len()];
    s.read_exact(&mut reply).expect("read batched reply");
    assert_eq!(&reply[..], &expected[..]);
}

#[test]
fn command_level_error_keeps_connection_alive() {
    let server = spawn_server();
    let mut s = connect(&server.addr);

    // Unknown command → `-ERR unknown command: 'FOOBAR'\r\n`.
    let request = build_request(&[b"FOOBAR"]);
    s.write_all(&request).expect("write");
    let mut first_byte = [0u8; 1];
    s.read_exact(&mut first_byte).expect("read first byte");
    assert_eq!(first_byte[0], b'-', "expected RESP2 error reply");

    // Drain the rest of the error line so it doesn't pollute the next read.
    let mut rest = Vec::new();
    loop {
        let mut b = [0u8; 1];
        s.read_exact(&mut b).expect("drain error line");
        rest.push(b[0]);
        if rest.ends_with(b"\r\n") {
            break;
        }
    }

    // The connection must still be usable after a business-level error.
    round_trip(&mut s, &build_request(&[b"PING"]), b"+PONG\r\n");
}

#[test]
fn wrong_arity_reports_err_and_connection_survives() {
    let server = spawn_server();
    let mut s = connect(&server.addr);

    // SET with only a key is a well-formed frame but an invalid command.
    let request = build_request(&[b"SET", b"only-key"]);
    s.write_all(&request).expect("write");

    let mut head = [0u8; 1];
    s.read_exact(&mut head).expect("read head byte");
    assert_eq!(head[0], b'-');
    // Drain until CRLF so the stream is aligned for the follow-up command.
    loop {
        let mut b = [0u8; 1];
        s.read_exact(&mut b).expect("drain");
        if b[0] == b'\n' {
            break;
        }
    }

    round_trip(&mut s, &build_request(&[b"SET", b"k", b"v"]), b"+OK\r\n");
}

#[test]
fn fragmented_frame_is_reassembled() {
    let server = spawn_server();
    let mut s = connect(&server.addr);

    // Send a SET command one byte at a time to stress the incremental parser.
    let request = build_request(&[b"SET", b"key", b"value"]);
    for byte in &request {
        s.write_all(std::slice::from_ref(byte)).expect("write byte");
    }

    let mut reply = [0u8; 5];
    s.read_exact(&mut reply).expect("read +OK");
    assert_eq!(&reply, b"+OK\r\n");

    round_trip(
        &mut s,
        &build_request(&[b"GET", b"key"]),
        b"$5\r\nvalue\r\n",
    );
}

#[test]
fn multiple_clients_share_engine_state() {
    let server = spawn_server();

    // Writer sets a value and closes; reader on a fresh connection must see
    // it, proving that the engine is shared across accepted connections.
    {
        let mut writer = connect(&server.addr);
        round_trip(
            &mut writer,
            &build_request(&[b"SET", b"shared", b"42"]),
            b"+OK\r\n",
        );
    }

    let mut reader = connect(&server.addr);
    round_trip(
        &mut reader,
        &build_request(&[b"GET", b"shared"]),
        b"$2\r\n42\r\n",
    );
}

#[test]
fn non_utf8_key_and_value_round_trip_end_to_end() {
    let server = spawn_server();
    let mut s = connect(&server.addr);

    // Arbitrary binary payloads: NUL, bytes >= 0x80, and an invalid UTF-8
    // sequence (`0xc3 0x28`). The engine must preserve them byte-for-byte.
    let key: &[u8] = &[0x00, 0xff, 0x01, b'\r', b'\n'];
    let value: &[u8] = &[0x80, 0x00, 0xc3, 0x28, 0xfe, 0x01, 0x02];

    round_trip(&mut s, &build_request(&[b"SET", key, value]), b"+OK\r\n");

    let request = build_request(&[b"GET", key]);
    s.write_all(&request).expect("write");
    let header = format!("${}\r\n", value.len());
    let total = header.len() + value.len() + 2;
    let mut reply = vec![0u8; total];
    s.read_exact(&mut reply).expect("read");

    let mut expected = Vec::with_capacity(total);
    expected.extend_from_slice(header.as_bytes());
    expected.extend_from_slice(value);
    expected.extend_from_slice(b"\r\n");
    assert_eq!(reply, expected);
}
