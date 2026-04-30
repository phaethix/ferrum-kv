//! Integration tests for the per-connection idle timeout.
//!
//! A listener is bound, the accept loop is spawned with a tight
//! `client_timeout`, and an idle client is kept open past the timeout.
//! The server is expected to close the socket, which the test observes as a
//! clean EOF on the client side.

use std::io::Read;
use std::net::{TcpListener, TcpStream};
use std::thread;
use std::time::{Duration, Instant};

use ferrum_kv::network::server::{self, ServerConfig};
use ferrum_kv::network::shutdown::Shutdown;
use ferrum_kv::storage::engine::KvEngine;

#[test]
fn idle_connection_is_closed_after_client_timeout() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local_addr");
    let engine = KvEngine::new();

    let config = ServerConfig {
        client_timeout: Some(Duration::from_millis(300)),
    };

    let _server = thread::spawn(move || {
        let _ = server::run_listener(listener, engine, Shutdown::new(), config);
    });

    // Give the accept loop a moment to be ready.
    thread::sleep(Duration::from_millis(50));

    let mut stream = TcpStream::connect(addr).expect("connect");
    // Client-side read timeout is longer than the server's idle timeout so we
    // are sure it is the server that closes first.
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();

    // Send nothing; wait for the server to close the connection.
    let start = Instant::now();
    let mut buf = [0u8; 16];
    let n = stream
        .read(&mut buf)
        .expect("read should return, not error");
    let elapsed = start.elapsed();

    assert_eq!(n, 0, "expected orderly EOF from server-side close");
    assert!(
        elapsed >= Duration::from_millis(250),
        "server closed too early: {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_millis(1500),
        "server took too long to close idle connection: {elapsed:?}"
    );
}
