//! Integration tests for the max-clients backpressure mechanism.
//!
//! Starts a server capped at a single concurrent client, opens the first
//! connection and holds it, then verifies that a second connection is
//! immediately refused with a Redis-style error reply.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;
use std::time::Duration;

use ferrum_kv::network::server::{self, ServerConfig};
use ferrum_kv::network::shutdown::Shutdown;
use ferrum_kv::storage::engine::KvEngine;

#[test]
fn second_client_is_rejected_when_maxclients_is_one() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local_addr");
    let engine = KvEngine::new();

    let config = ServerConfig {
        client_timeout: None,
        max_clients: 1,
    };

    let _server = thread::spawn(move || {
        let _ = server::run_listener(listener, engine, Shutdown::new(), config);
    });

    thread::sleep(Duration::from_millis(50));

    // First client holds the only slot and completes a PING round-trip so we
    // are sure it has entered `handle_client` (and therefore occupies a slot).
    let mut first = TcpStream::connect(addr).expect("connect first");
    first
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    first.write_all(b"*1\r\n$4\r\nPING\r\n").unwrap();
    let mut pong = [0u8; 7];
    first.read_exact(&mut pong).unwrap();
    assert_eq!(&pong, b"+PONG\r\n");

    // Second client must receive the rejection and then EOF.
    let mut second = TcpStream::connect(addr).expect("connect second");
    second
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut reply = Vec::new();
    second.read_to_end(&mut reply).unwrap();
    let text = String::from_utf8_lossy(&reply).into_owned();
    assert!(
        text.starts_with("-ERR max number of clients reached"),
        "unexpected reply to rejected client: {text:?}"
    );

    // Now release the first slot and confirm a new client can take over.
    drop(first);
    thread::sleep(Duration::from_millis(50));

    let mut third = TcpStream::connect(addr).expect("connect third");
    third
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    third.write_all(b"*1\r\n$4\r\nPING\r\n").unwrap();
    let mut pong2 = [0u8; 7];
    third.read_exact(&mut pong2).unwrap();
    assert_eq!(&pong2, b"+PONG\r\n");
}
