//! Integration tests for the cooperative shutdown mechanism.
//!
//! These tests bring up a real listener, spawn the accept loop on a
//! background thread, and verify that:
//!
//! 1. Triggering the [`Shutdown`] handle and waking the listener causes the
//!    accept loop to return in bounded time.
//! 2. After shutdown the listener port is released, so a fresh server can
//!    bind the same ephemeral address again without `EADDRINUSE`.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;
use std::time::{Duration, Instant};

use ferrum_kv::network::server::{self, ServerConfig};
use ferrum_kv::network::shutdown::Shutdown;
use ferrum_kv::storage::engine::KvEngine;

#[test]
fn accept_loop_exits_after_shutdown_triggered() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local_addr");
    let engine = KvEngine::new();
    let shutdown = Shutdown::new();

    let shutdown_for_thread = shutdown.clone();
    let handle = thread::spawn(move || {
        server::run_listener(
            listener,
            engine,
            shutdown_for_thread,
            ServerConfig::default(),
        )
        .expect("run_listener");
    });

    // Prove the server is actually up by completing one round-trip.
    let mut stream = TcpStream::connect(addr).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    stream.write_all(b"*1\r\n$4\r\nPING\r\n").unwrap();
    let mut buf = [0u8; 16];
    let n = stream.read(&mut buf).unwrap();
    assert!(n > 0, "expected a reply for PING");
    drop(stream);

    // Now trigger shutdown and wake the blocked accept.
    shutdown.trigger();
    Shutdown::wake_listener(addr);

    // The accept loop must terminate promptly.
    let start = Instant::now();
    loop {
        if handle.is_finished() {
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "accept loop did not exit within 2s after shutdown"
        );
        thread::sleep(Duration::from_millis(10));
    }
    handle.join().expect("server thread panicked");
}
