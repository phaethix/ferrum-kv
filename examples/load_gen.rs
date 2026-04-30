//! Minimal load generator for ferrum-kv.
//!
//! Spawns N worker threads, each opening a single TCP connection to the
//! server and issuing a fixed number of `SET`, `GET`, and `INCR` commands
//! in sequence. Commands are written as raw RESP2 arrays — no compatibility
//! handshake is performed — and replies are parsed just enough to confirm
//! correctness before moving on.
//!
//! The point is not to be a fully-featured benchmark (that is what
//! `redis-benchmark` is for, once we implement enough commands for its
//! handshake), but to give us a reproducible baseline for `SET / GET / INCR`
//! throughput that lives inside this repo and keeps working as the server
//! evolves.
//!
//! Build with `cargo build --release --example load_gen` and drive it via
//! `scripts/bench-smoke.sh`, or invoke directly:
//!
//! ```bash
//! cargo run --release --example load_gen -- \
//!     --addr 127.0.0.1:6399 --clients 50 --requests 100000
//! ```

use std::env;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

struct Config {
    addr: String,
    clients: usize,
    requests: usize,
    value_size: usize,
}

fn parse_args() -> Result<Config, String> {
    let mut cfg = Config {
        addr: "127.0.0.1:6399".to_string(),
        clients: 50,
        requests: 100_000,
        value_size: 64,
    };
    let mut iter = env::args().skip(1);
    while let Some(flag) = iter.next() {
        match flag.as_str() {
            "--addr" => {
                cfg.addr = iter
                    .next()
                    .ok_or_else(|| "--addr requires a value".to_string())?;
            }
            "--clients" => {
                cfg.clients = iter
                    .next()
                    .ok_or_else(|| "--clients requires a value".to_string())?
                    .parse()
                    .map_err(|e| format!("--clients: {e}"))?;
            }
            "--requests" => {
                cfg.requests = iter
                    .next()
                    .ok_or_else(|| "--requests requires a value".to_string())?
                    .parse()
                    .map_err(|e| format!("--requests: {e}"))?;
            }
            "--value-size" => {
                cfg.value_size = iter
                    .next()
                    .ok_or_else(|| "--value-size requires a value".to_string())?
                    .parse()
                    .map_err(|e| format!("--value-size: {e}"))?;
            }
            "-h" | "--help" => {
                print_usage();
                std::process::exit(0);
            }
            other => return Err(format!("unknown flag: {other}")),
        }
    }
    if cfg.clients == 0 {
        return Err("--clients must be > 0".into());
    }
    if cfg.requests == 0 {
        return Err("--requests must be > 0".into());
    }
    Ok(cfg)
}

fn print_usage() {
    eprintln!(
        "usage: load_gen [--addr HOST:PORT] [--clients N] [--requests N] [--value-size BYTES]"
    );
}

fn connect(addr: &str) -> std::io::Result<TcpStream> {
    let s = TcpStream::connect(addr)?;
    s.set_nodelay(true)?;
    s.set_read_timeout(Some(Duration::from_secs(10)))?;
    s.set_write_timeout(Some(Duration::from_secs(10)))?;
    Ok(s)
}

/// Reads one RESP2 reply from the stream, returning just the first byte so
/// the caller can tell apart `+OK`, `$N`, `:N`, and `-ERR`.
fn read_reply_kind(stream: &mut TcpStream) -> std::io::Result<u8> {
    let mut header = Vec::with_capacity(16);
    let mut byte = [0u8; 1];
    loop {
        stream.read_exact(&mut byte)?;
        header.push(byte[0]);
        if header.len() >= 2 && header.ends_with(b"\r\n") {
            break;
        }
    }
    match header[0] {
        b'+' | b'-' | b':' => Ok(header[0]),
        b'$' => {
            let len: i64 = std::str::from_utf8(&header[1..header.len() - 2])
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?
                .parse()
                .map_err(|e: std::num::ParseIntError| {
                    std::io::Error::new(std::io::ErrorKind::InvalidData, e)
                })?;
            if len >= 0 {
                let mut body = vec![0u8; len as usize + 2];
                stream.read_exact(&mut body)?;
            }
            Ok(b'$')
        }
        other => Err(std::io::Error::other(format!(
            "unexpected reply prefix: {other:#x}"
        ))),
    }
}

fn append_bulk(out: &mut Vec<u8>, payload: &[u8]) {
    out.extend_from_slice(format!("${}\r\n", payload.len()).as_bytes());
    out.extend_from_slice(payload);
    out.extend_from_slice(b"\r\n");
}

fn build_set(key: &[u8], value: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(32 + key.len() + value.len());
    out.extend_from_slice(b"*3\r\n");
    append_bulk(&mut out, b"SET");
    append_bulk(&mut out, key);
    append_bulk(&mut out, value);
    out
}

fn build_get(key: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(16 + key.len());
    out.extend_from_slice(b"*2\r\n");
    append_bulk(&mut out, b"GET");
    append_bulk(&mut out, key);
    out
}

fn build_incr(key: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(16 + key.len());
    out.extend_from_slice(b"*2\r\n");
    append_bulk(&mut out, b"INCR");
    append_bulk(&mut out, key);
    out
}

struct Totals {
    set: AtomicU64,
    get: AtomicU64,
    incr: AtomicU64,
    errors: AtomicU64,
}

fn run_phase<F>(label: &str, cfg: &Config, totals: &Arc<Totals>, build: F)
where
    F: Fn(usize, usize) -> Vec<u8> + Send + Sync + 'static + Clone,
{
    let per_client = cfg.requests / cfg.clients;
    let remainder = cfg.requests - per_client * cfg.clients;

    let start = Instant::now();
    let mut handles = Vec::with_capacity(cfg.clients);
    for cid in 0..cfg.clients {
        let count = per_client + if cid < remainder { 1 } else { 0 };
        let addr = cfg.addr.clone();
        let totals = Arc::clone(totals);
        let label = label.to_string();
        let build = build.clone();
        handles.push(thread::spawn(move || {
            let mut stream = match connect(&addr) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("client {cid}: connect failed: {e}");
                    totals.errors.fetch_add(count as u64, Ordering::Relaxed);
                    return;
                }
            };
            for op in 0..count {
                let frame = build(cid, op);
                if stream.write_all(&frame).is_err() {
                    totals.errors.fetch_add(1, Ordering::Relaxed);
                    break;
                }
                match read_reply_kind(&mut stream) {
                    Ok(b'-') => {
                        totals.errors.fetch_add(1, Ordering::Relaxed);
                    }
                    Ok(_) => match label.as_str() {
                        "SET" => {
                            totals.set.fetch_add(1, Ordering::Relaxed);
                        }
                        "GET" => {
                            totals.get.fetch_add(1, Ordering::Relaxed);
                        }
                        "INCR" => {
                            totals.incr.fetch_add(1, Ordering::Relaxed);
                        }
                        _ => {}
                    },
                    Err(_) => {
                        totals.errors.fetch_add(1, Ordering::Relaxed);
                        break;
                    }
                }
            }
        }));
    }
    for h in handles {
        let _ = h.join();
    }
    let elapsed = start.elapsed();

    let done = match label {
        "SET" => totals.set.load(Ordering::Relaxed),
        "GET" => totals.get.load(Ordering::Relaxed),
        "INCR" => totals.incr.load(Ordering::Relaxed),
        _ => 0,
    };
    let secs = elapsed.as_secs_f64().max(1e-9);
    let qps = done as f64 / secs;
    println!(
        "{label:<5}  {done:>10} ops  {secs:>7.3} s  {qps:>10.0} ops/s",
        label = label,
        done = done,
        secs = secs,
        qps = qps
    );
}

fn main() -> ExitCode {
    let cfg = match parse_args() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {e}");
            print_usage();
            return ExitCode::from(2);
        }
    };

    println!(
        "addr={} clients={} requests={} value_size={}",
        cfg.addr, cfg.clients, cfg.requests, cfg.value_size
    );

    let totals = Arc::new(Totals {
        set: AtomicU64::new(0),
        get: AtomicU64::new(0),
        incr: AtomicU64::new(0),
        errors: AtomicU64::new(0),
    });

    let value_size = cfg.value_size;
    run_phase("SET", &cfg, &totals, move |cid, op| {
        let key = format!("c{cid}:k{op}").into_bytes();
        let value = vec![b'x'; value_size];
        build_set(&key, &value)
    });
    run_phase("GET", &cfg, &totals, move |cid, op| {
        let key = format!("c{cid}:k{op}").into_bytes();
        build_get(&key)
    });
    run_phase("INCR", &cfg, &totals, move |_cid, _op| {
        build_incr(b"counter")
    });

    let errors = totals.errors.load(Ordering::Relaxed);
    if errors > 0 {
        eprintln!("warning: {errors} command errors during run");
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}
