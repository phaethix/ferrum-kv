//! High-concurrency smoke tests for the tokio-backed server.
//!
//! These tests exercise the async accept loop under workloads that used to
//! spawn one OS thread per connection in the synchronous version. With the
//! tokio runtime a handful of worker threads should multiplex all of them
//! without breaking a sweat.
//!
//! They are deliberately lightweight (small keyspace, short values) so they
//! stay fast enough for the default `cargo test` run while still stressing
//! the multiplexed I/O path.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use ferrum_kv::network::server::{self, ServerConfig};
use ferrum_kv::network::shutdown::Shutdown;
use ferrum_kv::storage::engine::KvEngine;

/// Number of concurrent clients in the PING fan-out test.
///
/// Chosen to clearly exceed the number of logical CPUs on typical CI boxes
/// so we know the runtime is actually multiplexing connections rather than
/// pinning one per worker thread.
const FAN_OUT_CLIENTS: usize = 500;

/// Drives a FerrumKV server with a fixed, tiny worker pool and asks
/// `FAN_OUT_CLIENTS` clients to each send a PING and read back `+PONG`.
///
/// Passing asserts that:
/// 1. the accept loop can absorb hundreds of concurrent incoming connections,
/// 2. the shared engine correctly replies on each of them, and
/// 3. the server winds down cleanly after the shared shutdown flag flips.
#[test]
fn handles_many_concurrent_ping_clients() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local_addr");
    let engine = KvEngine::new();
    let shutdown = Shutdown::new();

    let shutdown_for_thread = shutdown.clone();
    let config = ServerConfig {
        worker_threads: 2,
        max_clients: FAN_OUT_CLIENTS * 2,
        ..ServerConfig::default()
    };
    let server = thread::spawn(move || {
        server::run_listener(listener, engine, shutdown_for_thread, config).expect("run_listener");
    });

    // Give the accept loop a moment to come up before we stampede it.
    thread::sleep(Duration::from_millis(100));

    let ok = Arc::new(AtomicUsize::new(0));
    let mut handles = Vec::with_capacity(FAN_OUT_CLIENTS);
    let started = Instant::now();
    for _ in 0..FAN_OUT_CLIENTS {
        let ok = Arc::clone(&ok);
        handles.push(thread::spawn(move || {
            let mut stream = TcpStream::connect(addr).expect("connect");
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            stream.write_all(b"*1\r\n$4\r\nPING\r\n").unwrap();
            let mut buf = [0u8; 16];
            let n = stream.read(&mut buf).unwrap();
            // The reply is `+PONG\r\n` (7 bytes); we only look at the prefix
            // so we stay resilient to any future PING payload variants.
            if n >= 5 && &buf[..5] == b"+PONG" {
                ok.fetch_add(1, Ordering::SeqCst);
            }
        }));
    }
    for h in handles {
        h.join().expect("client thread panicked");
    }
    let elapsed = started.elapsed();

    assert_eq!(
        ok.load(Ordering::SeqCst),
        FAN_OUT_CLIENTS,
        "some concurrent clients did not get a PONG back (elapsed: {elapsed:?})",
    );

    shutdown.trigger();
    server.join().expect("server thread panicked");
}
