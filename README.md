<p align="center">
  <img src="assets/hero.svg" width="100%" alt="FerrumKV — Eviction Algorithm Laboratory" />
</p>

<p align="center">
  <a href="https://github.com/phaethix/ferrum-kv/releases"><img src="https://img.shields.io/badge/release-v0.6.0-blue" alt="Release" /></a>
  <a href="./LICENSE"><img src="https://img.shields.io/badge/license-MIT-green" alt="License" /></a>
  <a href="https://github.com/phaethix/ferrum-kv/actions/workflows/ci.yml"><img src="https://github.com/phaethix/ferrum-kv/actions/workflows/ci.yml/badge.svg" alt="CI" /></a>
  <img src="https://img.shields.io/badge/rust-1.85%2B-orange" alt="Rust 1.85+" />
</p>

---

## What is FerrumKV?

FerrumKV is an **eviction algorithm laboratory** — a place to study,
benchmark, and experiment with cache eviction strategies. It wraps 16
policies (from classic LRU to the self-tuning AHE) in a working,
RESP2-compatible KV store you can drive with `redis-cli`.

It is **not** a Redis replacement. It is a research vehicle: every
algorithm ships with reproducible benchmarks, every code path is
commented, and the entire ~12,000-line codebase is designed to be read
in an afternoon.

Three principles guide the project:

**🔬 Algorithms first.**
[AHE](./docs/reference/ahe.md) (Adaptive Hybrid Eviction) fuses recency,
frequency, and TTL into a single *Eviction Priority Score* and self-tunes
its weights from live hit-ratio feedback. No knobs, no tuning — swap it
in with `--maxmemory-policy allkeys-ahe` and it adapts. SIEVE (NSDI'24),
SIEVE-S, AdaptiveClimb, and 10 more policies ship alongside it, all
benchmarked against the same workload suite.

**📖 Readable end-to-end.**
~12,000 lines of layered, well-commented Rust — no macro magic, no custom
allocators. TCP → RESP2 parsing → engine → eviction → AOF → Tokio async:
the whole pipeline is followable in an afternoon.

**🔧 Self-contained and Redis-flavoured.**
A single static binary, RESP2-compatible, driven by `redis-cli` and
`redis-benchmark`. 16 eviction policies you can swap at runtime, plus a
zero-dependency web dashboard.

<p align="center">
  <img src="assets/section-divider.svg" width="80%" alt="" />
</p>

<p align="center">
  🎓 FerrumKV is built by the author of
  <a href="https://shipthatcode.com/courses/build-redis"><b>Build Redis from Scratch</b></a>
  — a step-by-step course on writing a RESP-compatible key-value store.
</p>

## Quick Start

```bash
cargo build --release

# In-memory (default: :6380, dashboard on :6381)
./target/release/ferrum-kv

# With AOF persistence + AHE eviction
./target/release/ferrum-kv \
  --aof-path /tmp/ferrum.aof \
  --maxmemory 256mb \
  --maxmemory-policy allkeys-ahe
```

```text
$ redis-cli -p 6380
redis-cli> SET user:1000 '{"name":"Alice"}'
OK
redis-cli> GET user:1000
{"name":"Alice"}
redis-cli> INFO memory
# Memory
used_memory:184
maxmemory:268435456
...
```

Open **http://127.0.0.1:6381** for the built-in dashboard. It is bound to
loopback by default and refuses a non-loopback address unless
`--dashboard-token` is supplied.

## The Eviction Laboratory

Cache eviction has seen a renaissance since 2024. SIEVE (NSDI'24)
showed that a simple FIFO-derived algorithm can beat 9 state-of-the-art
policies on nearly half of 1,559 production traces. AdaptiveClimb
(arXiv:2511.21235) brought control theory to cache replacement. Cold-RL
put reinforcement learning on the table.

FerrumKV tracks this frontier. Every policy ships with a shared benchmark
harness that measures hit ratio across four workload patterns — Zipfian
(stable skew), shifting hot set, OLTP-mixed, and sequential scan. Switch
policies at runtime with `CONFIG SET maxmemory-policy` and compare results
immediately. No restart, no redeploy.

**AHE** is FerrumKV's flagship: it blends recency, frequency, and TTL into
a self-tuning *Eviction Priority Score*. On every workload pattern it
tracks the better of LRU and LFU, and it never hits either policy's worst
case. LFU's sticky frequency counters collapse to **51.1%** on a shifting
hot set while AHE holds at **52.3%**; under a scan-heavy mix AHE
(**56.8%**) stays clear of LRU's dip to **56.3%**.

📎 [AHE algorithm reference](./docs/reference/ahe.md) ·
[Benchmark methodology](./docs/reference/benchmarks.md) ·
[Reproduce: `scripts/bench-hit-ratio.sh`](scripts/bench-hit-ratio.sh)

<p align="center">
  <img src="assets/section-divider.svg" width="80%" alt="" />
</p>

## Dashboard

![Dashboard](assets/dashboard-preview.png)

| Feature | Description |
|---------|-------------|
| Key browser | Glob search (`user:*`, `session?`), paginated list, TTL badges |
| Inline editor | View, edit, set TTL, delete keys — all in-browser |
| Live stats | Keys, memory, hit ratio, eviction policy — auto-refresh every 2s |
| Command console | Run any RESP command (`GET`, `SET`, `INFO`, …) with syntax highlighting |

## Eviction Policies

| Policy | Type | Recency | Frequency | TTL | Self-Tuning |
|--------|------|:-------:|:---------:|:---:|:-----------:|
| `noeviction` | — | | | | |
| `allkeys-lru` / `volatile-lru` | LRU | ✓ | | ✓ | |
| `allkeys-lfu` / `volatile-lfu` | LFU | | ✓ | ✓ | |
| `allkeys-random` / `volatile-random` | Random | | | ✓ | |
| `volatile-ttl` | TTL | | | ✓ | |
| `allkeys-sieve` / `volatile-sieve` | SIEVE (NSDI'24) | ✓ | | | |
| **`allkeys-sieves`** / **`volatile-sieves`** | SIEVE-S (FerrumKV) | ✓ | | ✓ | |
| **`allkeys-adaptiveclimb`** / **`volatile-adaptiveclimb`** | AdaptiveClimb (arXiv:2511.21235) | ✓ | ✓ | | ✓ |
| **`allkeys-ahe`** / **`volatile-ahe`** | Adaptive | ✓ | ✓ | ✓ | ✓ |

Policies marked **bold** are unique to FerrumKV or adapted from recent
research. All policies are benchmarked against the same four-pattern
workload suite. See [benchmarks](#benchmarks) below.

<p align="center">
  <img src="assets/section-divider.svg" width="80%" alt="" />
</p>

## Benchmarks

Apple M5 (10 cores), loopback, `redis-benchmark -n 100000 -c 50`:

| Scenario | SET QPS | GET QPS | p50 Latency |
|----------|--------:|--------:|------------:|
| Baseline (no eviction) | 62,189 | 65,231 | 0.42ms |
| Pipelined `-P 16` | 350,877 | 378,787 | 1.06ms |
| LFU (16MB cap) | 57,339 | 61,690 | 0.42ms |
| AHE (16MB cap) | 59,559 | 50,787 | 0.42ms |

Full report: [`benches/redis-benchmark.md`](benches/redis-benchmark.md)

### Hit ratio — the metric that matters

Throughput tells you how fast the engine serves requests. Hit ratio
tells you how well the eviction algorithm keeps the working set cached.
Measured end-to-end against a live, memory-capped server (working set
**100,000 keys**, cache capped at **5,000 entries ≈ 590 KiB**):

| Policy | `zipf` (stable skew) | `shift` (rotating hot set) | `mixed` (OLTP-like) | `scan` (sequential) |
|--------|---------------------:|---------------------------:|---------------------:|--------------------:|
| `allkeys-lru` | 59.5% | 52.4% | 56.3% | 0.0% |
| `allkeys-lfu` | 59.4% | 51.1% | 58.0% | 0.0% |
| `allkeys-ahe` | 59.5% | 52.3% | 56.8% | 0.0% |
| `allkeys-random` | 57.1% | 52.4% | 54.5% | 0.0% |

**AHE is the no-regret choice.** On every pattern it tracks the better of
LRU and LFU and never hits either policy's worst case: LFU's sticky
frequency counters collapse to **51.1%** on a shifting hot set while AHE
holds at **52.3%**, and under a scan-heavy mix AHE (**56.8%**) stays clear
of LRU's dip to **56.3%** and `random`'s **54.5%** floor.

Method and a TTL-intensive pattern: [`docs/reference/benchmarks.md`](docs/reference/benchmarks.md).
Reproduce: `scripts/bench-hit-ratio.sh` (harness: [`examples/hit_ratio_bench.rs`](examples/hit_ratio_bench.rs)).
Figures vary ~±1 pp across runs (internal LFU/LRU sampling RNG seeded from wall clock).

## Architecture

<p align="center">
  <img src="assets/architecture.svg" width="100%"
       alt="FerrumKV runtime architecture: RESP and browser clients, tokio server process, shared KvEngine with eviction and expiry, and AOF persistence" />
</p>

One process, three tiers. The **tokio server** (`src/network`,
`src/protocol`) accepts RESP2 clients on `:6380` and serves the built-in
dashboard on `:6381`; both paths run through the same `execute_command`.
The shared **`KvEngine`** (`src/storage`) owns the single
`RwLock<HashMap>` keyspace and dispatches over 16 eviction policies.
**AOF** (`src/persistence`) is optional (`--aof-path`) and appends RESP2
frames inside the store write lock — the `fsync` barrier runs after the
lock is released — and is replayed on the next boot. The expire sweeper, the AOF fsync thread (`--appendfsync
everysec`), and the dashboard each run outside the main tokio runtime.

## CLI Flags

| Flag | Default | Description |
|------|---------|-------------|
| `--config PATH` | *(none)* | Load a config file |
| `--addr HOST:PORT` | `127.0.0.1:6380` | RESP listening address |
| `--dashboard-addr ADDR\|off` | `127.0.0.1:6381` | Web dashboard address, or `off` to disable |
| `--dashboard-token SECRET` | *(none)* | Require this token on every `/api/*` dashboard request; mandatory for non-loopback binds |
| `--aof-path PATH` | *(disabled)* | Enable AOF persistence |
| `--appendfsync POLICY` | `everysec` | `always` / `everysec` / `no` |
| `--client-timeout SECONDS` | `0` (disabled) | Per-connection idle timeout |
| `--maxclients N` | *(unlimited)* | Max concurrent client connections |
| `--maxmemory BYTES` | `0` (unlimited) | Memory cap (`512b` / `64kb` / `256mb` / `1gb`) |
| `--maxmemory-policy POLICY` | `noeviction` | Any of the 16 policies |
| `--maxmemory-samples N` | `5` | Keys sampled per eviction round |
| `--io-threads N` | `0` (auto) | Tokio worker threads |
| `--loglevel LEVEL` | `info` | `off` / `error` / `warn` / `info` / `debug` / `trace` |

Config file: [`ferrum.conf.example`](ferrum.conf.example). All flags: `ferrum-kv --help`.

## Contributing

```bash
git clone https://github.com/phaethix/ferrum-kv.git
cd ferrum-kv

cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test --all-targets
```

See [CONTRIBUTING.md](CONTRIBUTING.md) for conventions and review process.

## Roadmap

| Version | Focus |
|---------|-------|
| v0.5.1 | CONFIG SET/GET, AUTH requirepass; SIEVE, SIEVE-S, AdaptiveClimb, AHE TTL-aware eviction, benchmark suite |
| v0.5.2 | SLOWLOG, AOF REWRITE / BGREWRITEAOF |
| v0.5.3 | MONITOR, INFO fields expansion, WriteGuard refactor |
| v0.6.0 | RESP3 protocol: HELLO handshake, typed replies (null/boolean/double/map), INCRBYFLOAT |
| v0.6.1 | Security and performance hardening: dashboard token/CSRF, AUTH redaction, shared-lock reads, scoped eviction indexes |
| v0.7 | List, Hash, Set data types |
| v0.8 | MULTI/EXEC with WATCH (needs a per-key version) |
| v0.9 | ACL users and per-command permissions |
| — | TLS is intentionally out of scope: terminate it in front of FerrumKV (stunnel, Envoy, cloud LB). See [SECURITY.md](./SECURITY.md). |

Full roadmap: [`docs/design/product-strategy.md`](docs/design/product-strategy.md)

---

<p align="center">
  <sub>MIT License · <a href="./LICENSE">LICENSE</a></sub>
</p>
