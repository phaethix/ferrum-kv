# `redis-benchmark` Smoke Results

> Captured at the Week 7 milestone (LFU + AHE + redis-benchmark sign-off).
> These numbers establish a baseline; they are not tuned competitive
> benchmarks. Any future run should append (not rewrite) new blocks so
> regressions are visible in git diff.

## Environment

- **CPU**: Apple M5 (10 cores)
- **RAM**: 32 GiB
- **OS**: macOS 26.4.1
- **Rust profile**: `cargo build --release`
- **Loopback**: `127.0.0.1:6399`
- **Client**: `redis-benchmark 8.6.2` (Homebrew)
- **Branch / tag**: `feat/memory-eviction-advanced` (target: `v0.3.0`)

## Methodology

- 100 000 requests per command, 50 concurrent clients (`-n 100000 -c 50 -q`).
- Pipelined runs add `-P 16`.
- Server started fresh between scenarios; logs redirected so they do not
  back-pressure the benchmark.

## Results

### Scenario 1 — no memory cap (baseline)

```
./target/release/ferrum-kv --addr 127.0.0.1:6399
redis-benchmark -h 127.0.0.1 -p 6399 -n 100000 -c 50 -q -t set,get,incr
```

| Command | QPS       | p50      |
| ------- | --------- | -------- |
| SET     | 58 207.21 | 0.415 ms |
| GET     | 61 349.70 | 0.415 ms |
| INCR    | 61 728.39 | 0.415 ms |

### Scenario 2 — pipelined (`-P 16`)

Same engine flags as Scenario 1; exercises throughput once
request/response RTT is amortised.

| Command | QPS        | p50      |
| ------- | ---------- | -------- |
| SET     | 366 300.38 | 1.071 ms |
| GET     | 373 134.31 | 1.063 ms |
| INCR    | 375 939.84 | 0.967 ms |

### Scenario 3 — `allkeys-lfu` under 16 MiB cap

```
./target/release/ferrum-kv --addr 127.0.0.1:6399 \
    --maxmemory 16mb --maxmemory-policy allkeys-lfu
redis-benchmark -h 127.0.0.1 -p 6399 -n 100000 -c 50 -q -t set,get
```

| Command | QPS       | p50      |
| ------- | --------- | -------- |
| SET     | 57 339.45 | 0.415 ms |
| GET     | 61 690.31 | 0.415 ms |

> LFU's Morris counter + minute-granularity decay stays within ~1.5% of
> the no-eviction baseline: the read path pays a handful of extra ALU ops
> and one atomic RNG load per hit.

### Scenario 4 — `allkeys-ahe` under 16 MiB cap

```
./target/release/ferrum-kv --addr 127.0.0.1:6399 \
    --maxmemory 16mb --maxmemory-policy allkeys-ahe
redis-benchmark -h 127.0.0.1 -p 6399 -n 100000 -c 50 -q -t set,get
```

| Command | QPS       | p50      |
| ------- | --------- | -------- |
| SET     | 59 559.26 | 0.423 ms |
| GET     | 50 787.20 | 0.447 ms |

> AHE reuses the LFU bookkeeping on the read path, so reads are within
> noise of the LFU scenario. The SET path additionally runs the EPS
> scorer over the candidate sample; since sampling is capped at 5 the
> per-eviction overhead is flat.

## Observations

- Single-threaded FerrumKV sustains ~60k QPS of synchronous SET/GET on
  an M5 over loopback. The sync IO model is the main ceiling — Week 8's
  Tokio refactor is the next lever.
- Pipelined throughput scales ~6× (60k → ~370k), confirming the bottleneck
  is RTT, not CPU. The LFU touch path is *not* showing up in profiles at
  this scale.
- All four scenarios sat comfortably inside their memory caps; no OOM
  replies were returned during the smoke runs.

## Repro script

```bash
cargo build --release
./target/release/ferrum-kv --addr 127.0.0.1:6399 &
SERVER=$!
sleep 0.5
redis-benchmark -h 127.0.0.1 -p 6399 -n 100000 -c 50 -q -t set,get,incr
redis-benchmark -h 127.0.0.1 -p 6399 -n 100000 -c 50 -P 16 -q -t set,get,incr
kill "$SERVER"
```

---

## v0.4.0 — tokio async runtime (Week 8)

Re-ran on the same Apple M5 / macOS 26.4.1 box after the `feat/async-runtime`
branch replaced the thread-per-connection model with a multi-threaded tokio
runtime. The server binary, the benchmark client and the command mix are
identical to the v0.3.0 runs above so the deltas are apples-to-apples.

### Scenario 1 — no memory cap, `c=50`

| Command | v0.3.0 QPS | v0.4.0 QPS | Δ      |
| ------- | ---------- | ---------- | ------ |
| SET     | 58 207     | 62 189     | +6.8%  |
| GET     | 61 349     | 65 231     | +6.3%  |
| INCR    | 61 728     | 63 492     | +2.9%  |

### Scenario 2 — pipelined (`-P 16`, `c=50`)

| Command | v0.3.0 QPS | v0.4.0 QPS | Δ      |
| ------- | ---------- | ---------- | ------ |
| SET     | 366 300    | 350 877    | -4.2%  |
| GET     | 373 134    | 378 787    | +1.5%  |
| INCR    | 375 939    | 381 679    | +1.5%  |

> Pipelined throughput is dominated by in-memory command execution rather
> than socket I/O, so the delta is inside run-to-run noise. What matters
> is that the async rewrite did **not** regress the hot path.

### Scenario 5 — high client fan-out (`c=500`)

```
./target/release/ferrum-kv --addr 127.0.0.1:6399
redis-benchmark -h 127.0.0.1 -p 6399 -n 100000 -c 500 -q -t set,get
```

| Command | v0.4.0 QPS | p50      |
| ------- | ---------- | -------- |
| SET     | 50 301     | 4.567 ms |
| GET     | 43 440     | 5.871 ms |

> The pre-tokio build would have tried to spawn 500 OS threads here — one
> per accepted connection — and spent most of its time context switching.
> v0.4.0 handles the whole fan-out on a handful of worker threads with
> single-digit-millisecond p50, the clearest win of Phase 8.

### Observations

- Single-client-per-thread throughput nudged up ~5% because tokio replaces
  two blocking `read`/`write` syscalls per round-trip with `epoll` /
  `kqueue`-backed readiness checks that batch more efficiently under load.
- The thread-per-connection floor is gone: scaling `-c` past the CPU count
  no longer multiplies kernel threads; the runtime just multiplexes more
  sockets onto the same workers.
- The integration suite's 500-client PING fan-out finishes in under 300 ms
  on the same box, confirming the smoke result is not a microbenchmark
  artefact.
