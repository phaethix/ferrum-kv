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

use ferrum_kv::network::server::{self, ServerConfig};
use ferrum_kv::network::shutdown::Shutdown;
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
        // `run_listener` only returns on listener failure or shutdown; in
        // tests we never trigger shutdown and let it run until process exit.
        let _ = server::run_listener(listener, engine, Shutdown::new(), ServerConfig::default());
    });
    ServerGuard {
        addr,
        _thread: handle,
    }
}

/// Like [`spawn_server`] but boots the engine with `requirepass` enabled, so
/// the AUTH-gating behaviour can be exercised end to end.
fn spawn_server_with_requirepass(password: &[u8]) -> ServerGuard {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local_addr").to_string();
    let engine = KvEngine::new();
    engine
        .set_requirepass(Some(password.to_vec()))
        .expect("set requirepass");
    let handle = thread::spawn(move || {
        let _ = server::run_listener(listener, engine, Shutdown::new(), ServerConfig::default());
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
fn exists_multi_key_returns_total_count() {
    let server = spawn_server();
    let mut s = connect(&server.addr);

    round_trip(&mut s, &build_request(&[b"SET", b"k1", b"v"]), b"+OK\r\n");
    round_trip(&mut s, &build_request(&[b"SET", b"k3", b"v"]), b"+OK\r\n");
    // k1 and k3 exist; k2 missing; k1 duplicated → Redis counts 3.
    round_trip(
        &mut s,
        &build_request(&[b"EXISTS", b"k1", b"k2", b"k3", b"k1"]),
        b":3\r\n",
    );
    // Single key still works (backward compatible).
    round_trip(&mut s, &build_request(&[b"EXISTS", b"k3"]), b":1\r\n");
    round_trip(&mut s, &build_request(&[b"EXISTS", b"k2"]), b":0\r\n");
}

#[test]
fn exists_with_zero_keys_is_wrong_arity() {
    let server = spawn_server();
    let mut s = connect(&server.addr);

    let request = build_request(&[b"EXISTS"]);
    s.write_all(&request).expect("write");
    let mut buf = [0u8; 64];
    let n = s.read(&mut buf).expect("read");
    let reply = std::str::from_utf8(&buf[..n]).unwrap();
    assert!(
        reply.starts_with("-ERR") && reply.contains("wrong number of arguments"),
        "expected arity error, got: {reply}"
    );
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
fn incr_and_decr_round_trip_on_integer_keys() {
    let server = spawn_server();
    let mut s = connect(&server.addr);

    round_trip(&mut s, &build_request(&[b"INCR", b"counter"]), b":1\r\n");
    round_trip(&mut s, &build_request(&[b"INCR", b"counter"]), b":2\r\n");
    round_trip(
        &mut s,
        &build_request(&[b"INCRBY", b"counter", b"8"]),
        b":10\r\n",
    );
    round_trip(
        &mut s,
        &build_request(&[b"DECRBY", b"counter", b"3"]),
        b":7\r\n",
    );
    round_trip(&mut s, &build_request(&[b"DECR", b"counter"]), b":6\r\n");
    round_trip(
        &mut s,
        &build_request(&[b"GET", b"counter"]),
        b"$1\r\n6\r\n",
    );
}

#[test]
fn incr_on_non_integer_value_returns_err_and_keeps_connection() {
    let server = spawn_server();
    let mut s = connect(&server.addr);

    round_trip(
        &mut s,
        &build_request(&[b"SET", b"k", b"hello"]),
        b"+OK\r\n",
    );
    round_trip(
        &mut s,
        &build_request(&[b"INCR", b"k"]),
        b"-ERR value is not an integer or out of range\r\n",
    );
    // Connection must still be usable after a command-level error.
    round_trip(&mut s, &build_request(&[b"PING"]), b"+PONG\r\n");
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

#[test]
fn config_get_wildcard_returns_all_eviction_params() {
    let server = spawn_server();
    let mut s = connect(&server.addr);

    // Default engine: maxmemory=0, maxmemory-policy=noeviction,
    // maxmemory-samples=5, requirepass unset (empty), plus the two
    // slow-log tunables. `CONFIG GET *` returns a flat array of
    // (name, value) pairs — 6 pairs since F-03 added the slowlog
    // parameters alongside the eviction ones.
    round_trip(
        &mut s,
        &build_request(&[b"CONFIG", b"GET", b"*"]),
        b"*12\r\n$9\r\nmaxmemory\r\n$1\r\n0\r\n$16\r\nmaxmemory-policy\r\n$10\r\nnoeviction\r\n$17\r\nmaxmemory-samples\r\n$1\r\n5\r\n$11\r\nrequirepass\r\n$0\r\n\r\n$23\r\nslowlog-log-slower-than\r\n$5\r\n10000\r\n$15\r\nslowlog-max-len\r\n$3\r\n128\r\n",
    );
}

#[test]
fn config_get_single_param_returns_one_pair() {
    let server = spawn_server();
    let mut s = connect(&server.addr);

    round_trip(
        &mut s,
        &build_request(&[b"CONFIG", b"GET", b"maxmemory"]),
        b"*2\r\n$9\r\nmaxmemory\r\n$1\r\n0\r\n",
    );
}

#[test]
fn config_get_unknown_param_returns_empty_array() {
    let server = spawn_server();
    let mut s = connect(&server.addr);

    round_trip(
        &mut s,
        &build_request(&[b"CONFIG", b"GET", b"nonexistent"]),
        b"*0\r\n",
    );
}

#[test]
fn config_set_policy_then_get_reflects_change() {
    let server = spawn_server();
    let mut s = connect(&server.addr);

    round_trip(
        &mut s,
        &build_request(&[b"CONFIG", b"SET", b"maxmemory-policy", b"allkeys-lru"]),
        b"+OK\r\n",
    );
    // The change must be visible immediately to a subsequent GET.
    round_trip(
        &mut s,
        &build_request(&[b"CONFIG", b"GET", b"maxmemory-policy"]),
        b"*2\r\n$16\r\nmaxmemory-policy\r\n$11\r\nallkeys-lru\r\n",
    );
}

#[test]
fn config_set_maxmemory_with_byte_suffix() {
    let server = spawn_server();
    let mut s = connect(&server.addr);

    // `1mb` parses to 1_048_576 bytes via the same grammar as the config file.
    round_trip(
        &mut s,
        &build_request(&[b"CONFIG", b"SET", b"maxmemory", b"1mb"]),
        b"+OK\r\n",
    );
    round_trip(
        &mut s,
        &build_request(&[b"CONFIG", b"GET", b"maxmemory"]),
        b"*2\r\n$9\r\nmaxmemory\r\n$7\r\n1048576\r\n",
    );
}

#[test]
fn config_set_maxmemory_samples_then_get() {
    let server = spawn_server();
    let mut s = connect(&server.addr);

    round_trip(
        &mut s,
        &build_request(&[b"CONFIG", b"SET", b"maxmemory-samples", b"12"]),
        b"+OK\r\n",
    );
    round_trip(
        &mut s,
        &build_request(&[b"CONFIG", b"GET", b"maxmemory-samples"]),
        b"*2\r\n$17\r\nmaxmemory-samples\r\n$2\r\n12\r\n",
    );
}

#[test]
fn config_set_unknown_param_is_rejected() {
    let server = spawn_server();
    let mut s = connect(&server.addr);

    round_trip(
        &mut s,
        &build_request(&[b"CONFIG", b"SET", b"bogus", b"1"]),
        b"-ERR Unknown parameter 'bogus'\r\n",
    );
    // The connection must stay usable after the error.
    round_trip(&mut s, &build_request(&[b"PING"]), b"+PONG\r\n");
}

#[test]
fn config_set_invalid_policy_is_rejected() {
    let server = spawn_server();
    let mut s = connect(&server.addr);

    round_trip(
        &mut s,
        &build_request(&[b"CONFIG", b"SET", b"maxmemory-policy", b"not-a-policy"]),
        b"-ERR Invalid argument 'not-a-policy' for CONFIG SET 'maxmemory-policy'\r\n",
    );
}

/// Boots a server with `requirepass` configured and returns the shared
/// password so tests can authenticate against it.
fn auth_password() -> &'static [u8] {
    b"secret"
}

#[test]
fn auth_blocks_unauthenticated_command_with_noauth() {
    let server = spawn_server_with_requirepass(auth_password());
    let mut s = connect(&server.addr);

    // Before AUTH, every command except AUTH is rejected with -NOAUTH.
    round_trip(
        &mut s,
        &build_request(&[b"SET", b"k", b"v"]),
        b"-NOAUTH Authentication required.\r\n",
    );
    // The connection must remain usable after the gatekeeping error.
    round_trip(
        &mut s,
        &build_request(&[b"PING"]),
        b"-NOAUTH Authentication required.\r\n",
    );
}

#[test]
fn auth_success_unlocks_subsequent_commands() {
    let server = spawn_server_with_requirepass(auth_password());
    let mut s = connect(&server.addr);

    round_trip(
        &mut s,
        &build_request(&[b"AUTH", auth_password()]),
        b"+OK\r\n",
    );
    // Once authenticated, ordinary commands run normally.
    round_trip(&mut s, &build_request(&[b"SET", b"k", b"v"]), b"+OK\r\n");
    round_trip(&mut s, &build_request(&[b"GET", b"k"]), b"$1\r\nv\r\n");
}

#[test]
fn auth_wrong_password_keeps_connection_blocked() {
    let server = spawn_server_with_requirepass(auth_password());
    let mut s = connect(&server.addr);

    // A mismatched password yields -WRONGPASS and leaves the connection
    // unauthenticated.
    round_trip(
        &mut s,
        &build_request(&[b"AUTH", b"wrong"]),
        b"-WRONGPASS invalid username-password pair or user is disabled.\r\n",
    );
    // Still blocked, because the failed AUTH did not flip the flag.
    round_trip(
        &mut s,
        &build_request(&[b"GET", b"k"]),
        b"-NOAUTH Authentication required.\r\n",
    );
    // A correct AUTH afterwards succeeds and unlocks the connection.
    round_trip(
        &mut s,
        &build_request(&[b"AUTH", auth_password()]),
        b"+OK\r\n",
    );
    round_trip(&mut s, &build_request(&[b"PING"]), b"+PONG\r\n");
}

#[test]
fn auth_without_configured_password_returns_err() {
    let server = spawn_server();
    let mut s = connect(&server.addr);

    // On a server with no requirepass, AUTH is itself an error.
    round_trip(
        &mut s,
        &build_request(&[b"AUTH", b"anything"]),
        b"-ERR Client sent AUTH, but no password is set\r\n",
    );
    // The connection is unaffected and ordinary commands work.
    round_trip(&mut s, &build_request(&[b"PING"]), b"+PONG\r\n");
}

#[test]
fn auth_survives_across_pipelined_commands() {
    let server = spawn_server_with_requirepass(auth_password());
    let mut s = connect(&server.addr);

    // AUTH and three following commands written in one batch; all must be
    // processed in order, proving the authenticated flag persists across the
    // frame-draining loop and survives the read boundary.
    let mut batched = Vec::new();
    batched.extend_from_slice(&build_request(&[b"AUTH", auth_password()]));
    batched.extend_from_slice(&build_request(&[b"SET", b"k", b"v"]));
    batched.extend_from_slice(&build_request(&[b"GET", b"k"]));
    batched.extend_from_slice(&build_request(&[b"PING"]));
    s.write_all(&batched).expect("write batched");

    let expected = b"+OK\r\n+OK\r\n$1\r\nv\r\n+PONG\r\n";
    let mut reply = vec![0u8; expected.len()];
    s.read_exact(&mut reply).expect("read batched reply");
    assert_eq!(&reply[..], &expected[..]);
}

#[test]
fn config_get_requirepass_returns_set_password() {
    let server = spawn_server_with_requirepass(auth_password());
    let mut s = connect(&server.addr);

    // The server requires auth, so AUTH first, then read the password back.
    round_trip(
        &mut s,
        &build_request(&[b"AUTH", auth_password()]),
        b"+OK\r\n",
    );
    round_trip(
        &mut s,
        &build_request(&[b"CONFIG", b"GET", b"requirepass"]),
        b"*2\r\n$11\r\nrequirepass\r\n$6\r\nsecret\r\n",
    );
}

#[test]
fn config_set_requirepass_enables_auth_mid_connection() {
    let server = spawn_server();
    let mut s = connect(&server.addr);

    // With no password initially, AUTH is an error.
    round_trip(
        &mut s,
        &build_request(&[b"AUTH", b"secret"]),
        b"-ERR Client sent AUTH, but no password is set\r\n",
    );
    // Enabling requirepass at runtime starts gating new commands.
    round_trip(
        &mut s,
        &build_request(&[b"CONFIG", b"SET", b"requirepass", b"secret"]),
        b"+OK\r\n",
    );
    // The already-open connection is now gated (config is shared via Arc).
    round_trip(
        &mut s,
        &build_request(&[b"SET", b"k", b"v"]),
        b"-NOAUTH Authentication required.\r\n",
    );
    // Authenticate and confirm the gate opens for this connection.
    round_trip(&mut s, &build_request(&[b"AUTH", b"secret"]), b"+OK\r\n");
    round_trip(&mut s, &build_request(&[b"SET", b"k", b"v"]), b"+OK\r\n");
}
