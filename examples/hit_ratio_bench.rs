//! Hit-ratio benchmark for FerrumKV's eviction policies.
//!
//! This is the benchmark the raw `redis-benchmark` QPS numbers cannot give
//! you: under a realistic, non-stationary access pattern, *how much of the
//! working set actually stays cached*. That hit ratio is the metric an
//! eviction algorithm is ultimately judged on, and it is exactly what was
//! missing from the performance tables.
//!
//! Why end-to-end instead of an in-process microbenchmark? FerrumKV's LRU and
//! AHE scores are time-aware: recency is measured against a 600-second
//! horizon and the AHE controller feeds on the observed hit ratio. A tight
//! in-process loop finishes in milliseconds, flattening every recency signal
//! to ~0 and disabling the adaptive loop — so AHE would silently collapse to
//! plain LFU and the comparison would be meaningless. Driving a *live server*
//! with realistic inter-request spacing lets recency and the adaptive loop
//! behave exactly as they do in production.
//!
//! For every `(policy, pattern)` pair the benchmark spawns a fresh server
//! with a fixed memory cap, replays the workload as a read-through client
//! (a GET miss populates the key with a SET, exactly like a real cache
//! fill), then reads the server's own `keyspace_hits` / `keyspace_misses`
//! counters and reports the hit ratio.
//!
//! ```bash
//! cargo build --release
//! cargo run --release --example hit_ratio_bench
//! ```

use std::env;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

/// Default per-entry footprint. Keys are formatted `k{id:08}` (9 bytes);
/// `PER_ENTRY_OVERHEAD` in the engine is 48. Keep this in sync with
/// `src/storage/engine/mod.rs::PER_ENTRY_OVERHEAD`.
fn bytes_per_entry(value_size: usize) -> u64 {
    (9 + value_size + 48) as u64
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Pattern {
    Zipf,
    Shift,
    Mixed,
    Scan,
}

impl Pattern {
    fn as_str(self) -> &'static str {
        match self {
            Pattern::Zipf => "zipf",
            Pattern::Shift => "shift",
            Pattern::Mixed => "mixed",
            Pattern::Scan => "scan",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Policy {
    Lru,
    Lfu,
    Ahe,
    Random,
}

impl Policy {
    fn name(self) -> &'static str {
        match self {
            Policy::Lru => "allkeys-lru",
            Policy::Lfu => "allkeys-lfu",
            Policy::Ahe => "allkeys-ahe",
            Policy::Random => "allkeys-random",
        }
    }
}

/// Deterministic xorshift64 RNG so every run is reproducible from a seed.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed | 1)
    }
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn next_f64(&mut self) -> f64 {
        // 53-bit fraction in [0, 1).
        ((self.next_u64() >> 11) as f64) / ((1u64 << 53) as f64)
    }
}

/// Cumulative distribution for Zipf sampling over `n` ranks with exponent `s`.
fn build_zipf_cdf(n: u64, s: f64) -> Vec<f64> {
    let mut cdf = Vec::with_capacity(n as usize);
    let mut sum = 0.0f64;
    for i in 1..=n {
        sum += 1.0 / (i as f64).powf(s);
        cdf.push(sum);
    }
    let inv = 1.0 / sum;
    for v in &mut cdf {
        *v *= inv;
    }
    cdf
}

/// Inverse-CDF sample: first rank whose cumulative mass reaches `u`.
fn sample_zipf(cdf: &[f64], u: f64) -> u64 {
    let idx = cdf.partition_point(|&v| v < u);
    idx.min(cdf.len() - 1) as u64
}

struct Params {
    working_set: u64,
    capacity: u64,
    ops: u64,
    pace_ms: f64,
    value_size: usize,
    seed: u64,
    epochs: u64,
    zipf_s: f64,
    hot_frac: f64,
    base_port: u16,
}

struct Cdfs {
    whole: Vec<f64>,
    band: Vec<f64>,
    hot: Vec<f64>,
}

/// Picks the next key id for the given pattern at position `op`.
fn gen_id(pattern: Pattern, op: u64, ops: u64, p: &Params, cdfs: &Cdfs, rng: &mut Rng) -> u64 {
    match pattern {
        Pattern::Scan => op % p.working_set,
        Pattern::Zipf => sample_zipf(&cdfs.whole, rng.next_f64()),
        Pattern::Shift => {
            // Working set is split into `epochs` equal bands; the hot band
            // rotates every `ops / epochs` requests. This is the canonical
            // non-stationary workload: a frequency policy clings to the
            // previous band while a recency/adaptive policy can follow.
            let band = (p.working_set / p.epochs).max(1);
            let epoch = op * p.epochs / ops.max(1);
            let base = epoch * band;
            let within = sample_zipf(&cdfs.band, rng.next_f64());
            (base + within) % p.working_set
        }
        Pattern::Mixed => {
            // 70% of traffic lands on a small stable hot set (size == cache
            // capacity) that any good policy should keep resident, while 30%
            // scans the full keyspace — an OLTP-ish mix where a pure-LRU policy
            // would let the scan thrash the hot set out.
            if rng.next_f64() < p.hot_frac {
                sample_zipf(&cdfs.hot, rng.next_f64())
            } else {
                op % p.working_set
            }
        }
    }
}

#[derive(Debug)]
enum Reply {
    Simple,
    Error(String),
    Bulk(Vec<u8>),
    Null,
}

fn read_exact(s: &mut TcpStream, buf: &mut [u8]) -> std::io::Result<()> {
    s.read_exact(buf)
}

fn read_line(s: &mut TcpStream) -> std::io::Result<String> {
    let mut out = Vec::new();
    let mut b = [0u8; 1];
    loop {
        read_exact(s, &mut b)?;
        if b[0] == b'\n' {
            break;
        }
        if b[0] == b'\r' {
            continue;
        }
        out.push(b[0]);
    }
    Ok(String::from_utf8_lossy(&out).into_owned())
}

fn read_reply(s: &mut TcpStream) -> std::io::Result<Reply> {
    let mut head = [0u8; 1];
    read_exact(s, &mut head)?;
    match head[0] {
        b'+' => {
            read_line(s)?;
            Ok(Reply::Simple)
        }
        b'-' => Ok(Reply::Error(read_line(s)?)),
        b'$' => {
            let line = read_line(s)?;
            let len: i64 = line
                .trim()
                .parse()
                .map_err(|_| std::io::Error::other("bad bulk length"))?;
            if len < 0 {
                return Ok(Reply::Null);
            }
            let mut body = vec![0u8; len as usize];
            read_exact(s, &mut body)?;
            let mut crlf = [0u8; 2];
            read_exact(s, &mut crlf)?;
            Ok(Reply::Bulk(body))
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

fn build_get(key: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(16 + key.len());
    out.extend_from_slice(b"*2\r\n");
    append_bulk(&mut out, b"GET");
    append_bulk(&mut out, key);
    out
}

fn build_set(key: &[u8], value: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(32 + key.len() + value.len());
    out.extend_from_slice(b"*3\r\n");
    append_bulk(&mut out, b"SET");
    append_bulk(&mut out, key);
    append_bulk(&mut out, value);
    out
}

fn connect(addr: &str) -> std::io::Result<TcpStream> {
    let s = TcpStream::connect(addr)?;
    s.set_nodelay(true).ok();
    s.set_read_timeout(Some(Duration::from_secs(10))).ok();
    s.set_write_timeout(Some(Duration::from_secs(10))).ok();
    Ok(s)
}

/// Runs one workload against a live server and returns the hit ratio.
fn run_workload(addr: &str, pattern: Pattern, p: &Params, cdfs: &Cdfs) -> std::io::Result<f64> {
    let mut s = connect(addr)?;
    let value = vec![b'v'; p.value_size];
    let pace = if p.pace_ms > 0.0 {
        Duration::from_secs_f64(p.pace_ms / 1000.0)
    } else {
        Duration::ZERO
    };
    let mut rng = Rng::new(p.seed ^ (pattern as u64).wrapping_mul(0x9E37_79B9));
    let mut key = Vec::with_capacity(16);

    for op in 0..p.ops {
        let id = gen_id(pattern, op, p.ops, p, cdfs, &mut rng);
        key.clear();
        key.extend_from_slice(format!("k{:08}", id).as_bytes());

        s.write_all(&build_get(&key))?;
        match read_reply(&mut s)? {
            Reply::Null => {
                // Cache miss -> read-through populate, which may trigger eviction.
                s.write_all(&build_set(&key, &value))?;
                let _ = read_reply(&mut s);
            }
            Reply::Bulk(_) => {}
            Reply::Error(e) => {
                return Err(std::io::Error::other(format!("GET error: {e}")));
            }
            _ => {}
        }
        if pace != Duration::ZERO {
            sleep(pace);
        }
    }

    s.write_all(b"*2\r\n$4\r\nINFO\r\n$5\r\nstats\r\n")?;
    match read_reply(&mut s)? {
        Reply::Bulk(body) => parse_hit_ratio(&body),
        _ => Err(std::io::Error::other("INFO did not return a bulk reply")),
    }
}

fn parse_hit_ratio(body: &[u8]) -> std::io::Result<f64> {
    let text = String::from_utf8_lossy(body);
    let mut hits: Option<u64> = None;
    let mut misses: Option<u64> = None;
    for line in text.lines() {
        if let Some(v) = line.strip_prefix("keyspace_hits:") {
            hits = v.trim().parse().ok();
        } else if let Some(v) = line.strip_prefix("keyspace_misses:") {
            misses = v.trim().parse().ok();
        }
    }
    match (hits, misses) {
        (Some(h), Some(m)) => {
            let total = h + m;
            if total == 0 {
                Ok(0.0)
            } else {
                Ok(h as f64 / total as f64)
            }
        }
        _ => Err(std::io::Error::other(
            "missing keyspace_hits/misses in INFO",
        )),
    }
}

fn find_binary() -> String {
    let manifest = env!("CARGO_MANIFEST_DIR");
    for profile in ["release", "debug"] {
        let candidate = format!("{manifest}/target/{profile}/ferrum-kv");
        if std::path::Path::new(&candidate).exists() {
            return candidate;
        }
    }
    "target/release/ferrum-kv".to_string()
}

fn start_server(bin: &str, policy: &str, max_memory: u64, addr: &str) -> std::io::Result<Child> {
    Command::new(bin)
        .args([
            "--addr",
            addr,
            "--maxmemory",
            &max_memory.to_string(),
            "--maxmemory-policy",
            policy,
            "--dashboard-addr",
            "off",
        ])
        .env("RUST_LOG", "warn")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
}

fn wait_ready(addr: &str, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if TcpStream::connect(addr).is_ok() {
            return true;
        }
        sleep(Duration::from_millis(50));
    }
    false
}

struct Config {
    patterns: Vec<Pattern>,
    policies: Vec<Policy>,
    params: Params,
}

fn parse_patterns(s: &str) -> Vec<Pattern> {
    s.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| match s {
            "zipf" => Pattern::Zipf,
            "shift" => Pattern::Shift,
            "mixed" => Pattern::Mixed,
            "scan" => Pattern::Scan,
            other => panic!("unknown pattern: {other}"),
        })
        .collect()
}

fn parse_policies(s: &str) -> Vec<Policy> {
    s.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| match s {
            "lru" => Policy::Lru,
            "lfu" => Policy::Lfu,
            "ahe" => Policy::Ahe,
            "random" => Policy::Random,
            other => panic!("unknown policy: {other}"),
        })
        .collect()
}

fn parse_args() -> Config {
    let mut params = Params {
        working_set: 100_000,
        capacity: 5_000,
        ops: 3_000,
        pace_ms: 20.0,
        value_size: 64,
        seed: 0x1234_5678,
        epochs: 8,
        zipf_s: 1.0,
        hot_frac: 0.7,
        base_port: 6391,
    };
    let mut patterns = parse_patterns("zipf,shift,mixed,scan");
    let mut policies = parse_policies("lru,lfu,ahe,random");

    let mut iter = env::args().skip(1);
    while let Some(flag) = iter.next() {
        let mut val = || -> String {
            iter.next()
                .unwrap_or_else(|| panic!("flag {flag} requires a value"))
        };
        match flag.as_str() {
            "--working-set" => params.working_set = val().parse().expect("working-set"),
            "--capacity" => params.capacity = val().parse().expect("capacity"),
            "--ops" => params.ops = val().parse().expect("ops"),
            "--pace-ms" => params.pace_ms = val().parse().expect("pace-ms"),
            "--value-size" => params.value_size = val().parse().expect("value-size"),
            "--seed" => params.seed = val().parse().expect("seed"),
            "--epochs" => params.epochs = val().parse().expect("epochs"),
            "--zipf-s" => params.zipf_s = val().parse().expect("zipf-s"),
            "--hot-frac" => params.hot_frac = val().parse().expect("hot-frac"),
            "--base-port" => params.base_port = val().parse().expect("base-port"),
            "--patterns" => patterns = parse_patterns(&val()),
            "--policies" => policies = parse_policies(&val()),
            "-h" | "--help" => {
                print_help();
                std::process::exit(0);
            }
            other => panic!("unknown flag: {other}"),
        }
    }
    Config {
        patterns,
        policies,
        params,
    }
}

fn print_help() {
    eprintln!(
        "usage: hit_ratio_bench [--working-set N] [--capacity N] [--ops N] \
         [--pace-ms F] [--value-size B] [--seed N] [--epochs N] \
         [--zipf-s F] [--hot-frac F] [--base-port P] \
         [--patterns zipf,shift,mixed,scan] [--policies lru,lfu,ahe,random]"
    );
}

fn main() {
    let cfg = parse_args();
    let p = &cfg.params;
    let bin = find_binary();
    let max_memory = p.capacity * bytes_per_entry(p.value_size);

    let cdfs = Cdfs {
        whole: build_zipf_cdf(p.working_set, p.zipf_s),
        band: build_zipf_cdf((p.working_set / p.epochs).max(1), p.zipf_s),
        hot: build_zipf_cdf(p.capacity, p.zipf_s),
    };

    println!(
        "FerrumKV hit-ratio benchmark | working_set={} capacity={} ({} KiB cap) \
         ops={} pace={}ms patterns={:?} policies={:?}",
        p.working_set,
        p.capacity,
        max_memory / 1024,
        p.ops,
        p.pace_ms,
        cfg.patterns.iter().map(|x| x.as_str()).collect::<Vec<_>>(),
        cfg.policies.iter().map(|x| x.name()).collect::<Vec<_>>(),
    );

    let mut results = vec![vec![0.0f64; cfg.patterns.len()]; cfg.policies.len()];
    let mut ok = vec![vec![false; cfg.patterns.len()]; cfg.policies.len()];

    let mut port = p.base_port;
    for (pi, &policy) in cfg.policies.iter().enumerate() {
        for (ti, &pattern) in cfg.patterns.iter().enumerate() {
            let addr = format!("127.0.0.1:{}", port);
            port += 1;
            let mut child = match start_server(&bin, policy.name(), max_memory, &addr) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!(
                        "!! {} {}: failed to start server: {e}",
                        policy.name(),
                        pattern.as_str()
                    );
                    continue;
                }
            };
            if !wait_ready(&addr, Duration::from_secs(10)) {
                eprintln!(
                    "!! {} {}: server not ready",
                    policy.name(),
                    pattern.as_str()
                );
                let _ = child.kill();
                continue;
            }
            match run_workload(&addr, pattern, p, &cdfs) {
                Ok(ratio) => {
                    results[pi][ti] = ratio;
                    ok[pi][ti] = true;
                    println!(
                        "  {:<14} {:<6} hit_ratio={:.2}%",
                        policy.name(),
                        pattern.as_str(),
                        ratio * 100.0
                    );
                }
                Err(e) => {
                    eprintln!(
                        "!! {} {}: workload error: {e}",
                        policy.name(),
                        pattern.as_str()
                    );
                }
            }
            let _ = child.kill();
        }
    }

    print_table(&cfg, &results, &ok, max_memory);
}

fn print_table(cfg: &Config, results: &[Vec<f64>], ok: &[Vec<bool>], max_memory: u64) {
    let p = &cfg.params;
    println!();
    println!(
        "## Hit ratio by eviction policy (working set {} keys, cache cap {} KiB)",
        p.working_set,
        max_memory / 1024
    );
    println!();
    // Header
    let mut header = String::from("| Policy");
    for pat in &cfg.patterns {
        header.push_str(&format!(" | {}", pat.as_str()));
    }
    header.push_str(" |");
    println!("{header}");
    let mut sep = String::from("|--------");
    for _ in &cfg.patterns {
        sep.push_str("|--------:");
    }
    sep.push('|');
    println!("{sep}");
    for (pi, &policy) in cfg.policies.iter().enumerate() {
        let mut row = format!("| `{}`", policy.name());
        for ti in 0..cfg.patterns.len() {
            if ok[pi][ti] {
                row.push_str(&format!(" | {:.1}%", results[pi][ti] * 100.0));
            } else {
                row.push_str(" | —");
            }
        }
        row.push_str(" |");
        println!("{row}");
    }
    println!();
    println!(
        "Higher is better. Each cell replays the same seeded workload against a \
         fresh server pinned to that policy; the value is the server's own \
         `keyspace_hits / (keyspace_hits + keyspace_misses)` over the whole run \
         (cold start included). Reproduce with `scripts/bench-hit-ratio.sh`."
    );
}
