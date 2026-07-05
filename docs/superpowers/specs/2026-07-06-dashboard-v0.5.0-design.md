# FerrumKV Dashboard v0.5.0 — Engineering Design

> Date: 2026-07-06
> Owner: Engineering
> Status: design complete, pending implementation plan
> Source spec: `docs/dashboard-design.md` v2.1 (2026-07-05)
> Issue: FERRUM-012

---

## 0. Scope

This design covers the **v0.5.0** deliverable only: Level 1 Live Overview
dashboard. v0.5.1 (sparklines, EPS breakdown, animations) and v0.6.0
(Sandbox, SIEVE) are explicitly out of scope and tracked in the product spec's
roadmap table.

The design is the output of the `brainstorming` skill. The next step is
`writing-plans` to produce an implementation plan; no code is written under
this design.

## 1. Product Summary (decomposed from spec v2.1)

A localhost-only HTTP dashboard running in-process alongside the RESP2 server,
surfacing live cache internals via Server-Sent Events. Four metric cards, an
AHE adaptive-alpha gauge, a simplified eviction log, a live RESP2 wire stream,
and a sampled keyspace distribution. Zero new crate dependencies. Dark theme,
single HTML file with embedded CSS and vanilla JS.

## 2. Locked Design Decisions

These were resolved during brainstorming. Each is binding for the
implementation plan.

| # | Decision | Choice | Rationale |
|---|----------|--------|-----------|
| 1 | Dashboard listener mount | `tokio::spawn` inside `run_listener`'s `block_on`, sharing the existing `MultiThread` runtime + `Shutdown` | Spec §7 "sharing the existing tokio runtime". Bind failure logs `warn`, does not abort RESP2 server. |
| 2 | Dashboard address config | Full CLI + config directive flow: `--dashboard-addr` flag, `dashboard-bind` + `dashboard-port` directives, default `127.0.0.1:6381` | Project convention is Redis-style directives; RESP2 port already flows this way. `0.0.0.0` bind triggers spec §7 warning. |
| 3 | Keyspace distribution sampling | Sampled, fixed `DISTRIBUTION_SAMPLES = 200` keys, new method `keyspace_distribution_sampled(k)` | Spec mockup labels "sampled, ~200 keys". O(200) lock hold vs O(n) full scan protects the §7 2% throughput NFR on large datasets. |
| 4 | RESP2 tap shape | `Entry { dir: Direction, bytes: Vec<u8> }` in a fixed-200 ring buffer; `tap.snapshot()` polled by SSE endpoint | Decouples hot path (tap push, µs) from cold path (SSE push, 500ms). `Vec<u8>` avoids new crate. `Direction` enum is required — frontend cannot reliably infer direction from wire bytes alone (pipeline/inline/bulk-data edge cases). |
| 5 | Eviction log ownership | Independent `storage/eviction_log.rs` module; `KvEngine` holds `Option<Arc<EvictionLog>>`; `Mutex<VecDeque>` (write-heavy: evict writes, SSE reads) | Matches the per-concern submodule split from commit 5cba310. `Option` means zero allocation when `maxmemory == 0` (no evictions possible). |
| 6 | Frontend inline | Single `index.html` with embedded `<style>` + `<script>`, `include_str!` direct, no `build.rs`, no escape concatenation | Spec §10 "single HTML file, vanilla JS". Avoids `</script>` escape bugs of multi-file concat. ~400 lines manageable inline. |
| 7 | HTTP routes | `/` → HTML, `/events` → SSE stream, `/metrics` → JSON snapshot | Three routes matched in `dashboard::serve`. `/metrics` is unauthenticated JSON for future automation; not advertised in UI. |
| 8 | SSE frame format | Standard `event: metrics\ndata: {json}\n\n` at 500ms tick; `EventSource` consumer | One combined payload per tick: metrics + eviction log snapshot + keyspace dist + RESP2 tap snapshot. Bounded payload size (ring buffers cap all). |
| 9 | Dashboard spawn timing | Inside `block_on`, before `accept_loop`; bind failure → `warn!` + return, accept_loop runs normally | Ops-friendly: misconfigured dashboard port must not break the RESP2 server. |
| 10 | `0.0.0.0` warning trigger | In `main.rs`, after resolving `CliArgs::dashboard_addr`, before `TcpListener::bind`: `if addr.ip().is_unspecified() { warn!("Dashboard bound to {} — no authentication is enforced", addr); }` | Spec §7. Lives in `main.rs` (not `dashboard::serve`) so it fires even when the dashboard is disabled by config — the user explicitly chose `0.0.0.0`, so warning regardless of listener spawn is correct ops behavior. §4.6 documents the same call site. |
| 11 | Uptime source | `KvEngine` records `Instant::now()` at construction into an `Arc<OnceLock<Instant>>` (monotonic clock, immune to NTP/wall-clock jumps). `uptime_secs = start.get().unwrap().elapsed().as_secs()`. `Arc`-shared so all clones read the same Instant. | ops/s card computes `total_commands / uptime_secs`. Lifetime average per spec §3. Not `AtomicU64` epoch millis — `Instant` is not atomically-storable, and wall-clock epoch would jump on NTP adjusts. |
| 12 | Command counter tick | `execute_command` (server.rs:272, free fn) entry: `engine.tick_command();`. Includes invalid commands (an op is an op). | Spec §3 "ops/s". Atomic, lock-free. `tick_command` is a thin `&self` method on `KvEngine` wrapping `self.commands.fetch_add(1, Relaxed)` — see §4.7. `execute_command` is not a method so cannot use `self.` directly. |
| 13 | Eviction counter tick | `enforce_memory_limit`: after the bounded loop, single `self.evictions.fetch_add(evicted, Relaxed)` with the loop's local `evicted` count. | One atomic per sweep, not per victim. Avoids hot-path atomic contention inside the eviction loop. |
| 14 | Bench gate | `resp2_bench.rs`: `dashboard_tap_inactive_zero_overhead` group. Control = no dashboard spawn. Experiment = dashboard spawned, no connection. Assert `delta < 1%`. | Spec §9 success metric "±1%". Criterion group, runs in CI via `cargo bench --no-run` + local full bench. |
| 15 | Dashboard module layout | `src/network/dashboard/{mod.rs, serve.rs, http.rs, sse.rs, tap.rs}` | Per-concern split, parallel to `network/server.rs`. `tap.rs` is the shared RESP2 byte mirror (also holds the `AtomicBool active` flag). |
| 16 | `keyspace_distribution_sampled` return type | `KeyspaceDistribution { ttl_buckets: [u32; 6], size_buckets: [u32; 4], lfu_heat: [u32; 5] }` | Fixed arrays, no heap. Bucket boundaries are constants in the method. |

## 3. Architecture

### 3.1 Process Topology

```
┌── ferrum-kv process ───────────────────────────────────────────┐
│                                                                │
│  main.rs                                                       │
│    ├─ build_engine → KvEngine (Arc-cloneable shared state)     │
│    ├─ install_signal_handlers → Shutdown flag                  │
│    ├─ spawn expire sweeper (dedicated OS thread, unchanged)    │
│    └ run_listener(listener, engine, shutdown, config)         │
│         └ block_on(async {                                    │
│              ├─ tokio::spawn(dashboard::serve(                 │
│              │      dashboard_listener, engine.clone(),        │
│              │      shutdown.clone()))                         │
│              └ accept_loop(listener, engine, shutdown, config)│
│           })                                                   │
└────────────────────────────────────────────────────────────────┘
```

The dashboard listener is bound in `main.rs` (parallel to the RESP2 listener,
resolved before signal handler install so `--dashboard-addr :0` resolves too)
and passed into `run_listener` alongside the RESP2 listener. `run_listener`
spawns `dashboard::serve` inside `block_on` before entering `accept_loop`.
Both share the single `MultiThread` tokio runtime; shutdown is observed by
both via `Shutdown::notified()`.

### 3.2 Module Map (new + changed)

```
src/
├─ main.rs                          [CHG] bind dashboard listener, pass into run_listener
├─ cli.rs                           [CHG] +--dashboard-addr flag, +dashboard-bind/+dashboard-port directives
├─ network/
│  ├─ server.rs                     [CHG] run_listener spawns dashboard::serve; execute_command ticks command counter
│  └ dashboard/
│     ├─ mod.rs                     [NEW] pub use, DashboardConfig, re-exports
│     ├─ serve.rs                   [NEW] serve(listener, engine, shutdown) — accept loop + 0.0.0.0 warning
│     ├─ http.rs                    [NEW] route match: / / /events / /metrics; HTML responder
│     ├─ sse.rs                     [NEW] SSE stream loop, 500ms tick, payload assembly
│     └ tap.rs                      [NEW] Resp2Tap { active: AtomicBool, ring: Mutex<VecDeque<Entry>> }, Direction enum
├─ storage/
│  ├─ engine/mod.rs                 [CHG] +evictions: AtomicU64, +commands: AtomicU64, +epoch: AtomicU64,
│  │                                       +eviction_log: Option<Arc<EvictionLog>>, +keyspace_distribution_sampled,
│  │                                       enforce_memory_limit ticks evictions + pushes EvictionEvent
│  └ eviction_log.rs               [NEW] EvictionLog { Arc<Mutex<VecDeque<EvictionEvent>>> }, EvictionEvent { key, policy, ttl_remaining },
│                                       snapshot(), push(), capacity 200
ferrum.conf.example                 [CHG] +dashboard-bind / +dashboard-port example directives
benches/
└─ resp2_bench.rs                   [CHG] +dashboard_tap_inactive_zero_overhead group
src/network/dashboard/index.html    [NEW] single-file frontend, include_str! in http.rs
```

### 3.3 Data Flow

```
   ┌──────────────┐
   │  RESP2 client │
   └──────┬───────┘
          │ wire bytes
          ▼
   ┌──────────────────────────────────────────┐
   │ handle_client (server.rs)                │
   │   read → execute_command → write         │
   │     │            │             │         │
   │     │            ▼             │         │
   │     │     commands.fetch_add   │         │
   │     │            │             │         │
   │     ▼            │             ▼         │
   │  tap.push(Request, bytes)   tap.push(Response, bytes)  ◄─ gated by tap.active AtomicBool
   └────────┬─────────────────────────────────┘
            │
            │ (enforce_memory_limit, on eviction)
            ▼
   ┌──────────────────────────────────────────┐
   │ evictions.fetch_add(evicted)             │
   │ eviction_log.push(EvictionEvent { ... }) │
   └─────────────┬────────────────────────────┘
                 │
                 │  (SSE endpoint, 500ms tick)
                 ▼
   ┌──────────────────────────────────────────────────────┐
   │ dashboard::sse                                         │
   │   payload = {                                         │
   │     memory:    engine.used_memory() / eviction_config,│
   │     hit_rate:  engine.keyspace_stats(),               │
   │     evictions: engine.evictions.load(),               │
   │     ops_s:     engine.commands.load() / uptime_secs,  │
   │     ahe:       engine.ahe_snapshot(),                 │
   │     eviction_log:  engine.eviction_log.snapshot(),    │
   │     keyspace:  engine.keyspace_distribution_sampled(200),│
   │     resp2_stream: tap.snapshot()                      │
   │   }                                                   │
   │   write "event: metrics\ndata: {json}\n\n"            │
   └──────────────────┬────────────────────────────────────┘
                      │ SSE
                      ▼
                ┌──────────┐
                │ browser  │ EventSource('/events')
                └──────────┘
```

## 4. Component Contracts

Each unit answers: what it does, how to use it, what it depends on.

### 4.1 `storage/eviction_log.rs` — `EvictionLog`

**Purpose**: bounded ring buffer of recent eviction events for the dashboard.

**API**:
```rust
pub struct EvictionEvent {
    pub key: Vec<u8>,           // binary-safe per project rule
    pub policy: &'static str,   // EvictionPolicy::name() — 'static via enum
    pub ttl_remaining_ms: Option<u64>,  // None = no TTL set
    pub ts: u64,                // epoch millis, Instant::now().duration_since(epoch)
}

pub struct EvictionLog {
    buf: Mutex<VecDeque<EvictionEvent>>,
}

impl EvictionLog {
    pub fn new(capacity: usize) -> Self;        // capacity = 200, pre-allocated
    pub fn push(&self, event: EvictionEvent);   // pops front if at capacity
    pub fn snapshot(&self) -> Vec<EvictionEvent>; // clones current contents
}
```

**Depends on**: nothing (std only). `&'static str` for policy avoids allocating
the name per event.

**Tests**: capacity bound (push 201, snapshot returns 200 newest), FIFO order,
binary key round-trip (`Nul`, non-UTF-8).

### 4.2 `network/dashboard/tap.rs` — `Resp2Tap`

**Purpose**: optional mirror of RESP2 wire bytes for the live stream panel.

**API**:
```rust
pub enum Direction { Request, Response }

pub struct TapEntry {
    pub dir: Direction,
    pub bytes: Vec<u8>,
}

pub struct Resp2Tap {
    active: AtomicBool,
    buf: Mutex<VecDeque<TapEntry>>,
}

impl Resp2Tap {
    pub fn new(capacity: usize) -> Self;        // capacity = 200, pre-allocated
    pub fn activate(&self);                      // dashboard connected → active = true
    pub fn deactivate(&self);                    // dashboard disconnected → active = false
    pub fn is_active(&self) -> bool;
    pub fn push(&self, dir: Direction, bytes: &[u8]);  // no-op if !active (hot path)
    pub fn snapshot(&self) -> Vec<TapEntry>;     // clones + clears consumed
}

impl Default for Resp2Tap { capacity: 200 }
```

**Hot-path contract**: `push` is `if self.active.load(Relaxed) { lock + push }`.
When inactive: one relaxed atomic load + predicted-not-taken branch. Zero
allocation. This is the path benchmarked by decision #14.

**Depends on**: nothing. Shared via `Arc<Resp2Tap>` between `handle_client`
(writer) and `dashboard::sse` (reader). The same `Arc` is cloned into each
`handle_client` task; `activate`/`deactivate` are called by `dashboard::sse`
on SSE connect/disconnect.

**Tests**: inactive push is no-op (snapshot empty after push), active push
round-trips binary bytes, capacity bound, snapshot-then-clear semantics.

### 4.3 `network/dashboard/serve.rs` — `serve`

**Purpose**: accept loop for dashboard HTTP connections.

**API**:
```rust
pub async fn serve(
    listener: TcpListener,
    engine: KvEngine,
    tap: Arc<Resp2Tap>,
    shutdown: Shutdown,
) -> Result<(), FerrumError>;
```

Loop mirrors `accept_loop`: `tokio::select!` between `shutdown.notified()` and
`listener.accept()`. Each connection spawns `handle_http(stream, engine, tap.clone())`.

On bind: caller (`main.rs`) already bound. `0.0.0.0` warning is emitted by
caller before bind (decision #10) so it fires even if dashboard is disabled.

**Depends on**: `http`, `sse`, `tap`, `KvEngine`, `Shutdown`.

### 4.4 `network/dashboard/http.rs` — routing

**Purpose**: route HTTP requests to the right responder.

**API**:
```rust
pub const DASHBOARD_HTML: &str = include_str!("index.html");

pub async fn handle_http(
    stream: TcpStream,
    engine: KvEngine,
    tap: Arc<Resp2Tap>,
);
```

Parses the request line (minimal: `GET <path> HTTP/1.1`). Routes:

| Path | Responder | Content-Type |
|------|-----------|--------------|
| `/` | `DASHBOARD_HTML` body | `text/html; charset=utf-8` |
| `/events` | `sse::stream(stream, engine, tap)` — upgrades to SSE | `text/event-stream` |
| `/metrics` | JSON snapshot (same payload schema as SSE tick — §4.5, one-shot; single source of truth for the schema, both endpoints call the same `sse::build_payload(&engine, &tap)` fn) | `application/json` |
| other | 404 | `text/plain` |

Malformed request → 400. Non-GET → 405.

**Depends on**: `sse`, `KvEngine`, `tap`, `index.html`.

### 4.5 `network/dashboard/sse.rs` — `stream`

**Purpose**: push live metrics to the browser at 2Hz.

**API**:
```rust
pub async fn stream(
    stream: TcpStream,
    engine: KvEngine,
    tap: Arc<Resp2Tap>,
);
```

On enter: `tap.activate()`. On exit (stream closed/error): `tap.deactivate()`.

Loop: `tokio::time::interval(Duration::from_millis(500))`, each tick assemble
the JSON payload (§3.3), write `event: metrics\ndata: {json}\n\n`. On write
error (client gone): break, deactivate, return.

**Payload schema** (JSON, camelCase keys for JS ergonomics):
```json
{
  "version": "0.5.0-dev",
  "uptime_secs": 8064,
  "memory": { "used": 47520240, "max": 268435456 },
  "hit_rate": { "hits": 7830, "misses": 2170 },
  "evictions": 1247,
  "ops_per_sec": 6241.5,
  "ahe": { "alpha": 0.62, "last_hit_ratio": 0.78 },
  "policy": "allkeys-ahe",
  "eviction_log": [
    { "key_b64": "dXNlcjo5MjE=", "policy": "allkeys-ahe", "ttl_remaining_ms": 12000, "ts": 1751900401 }
  ],
  "keyspace": {
    "ttl_buckets": [3, 12, 45, 80, 30, 30],
    "size_buckets": [120, 40, 25, 15],
    "lfu_heat": [40, 60, 50, 30, 20]
  },
  "resp2_stream": [
    { "dir": "req", "bytes_b64": "KjMN..." },
    { "dir": "res", "bytes_b64": "K09L..." }
  ]
}
```

**Binary safety**: keys and wire bytes are **base64-encoded** in JSON. This
keeps the payload valid UTF-8 JSON regardless of `Nul`/non-UTF-8 content, and
matches the project's "binary safety is sacred" rule — the dashboard is a
read-only viewer, not a data path participant, but it must not corrupt or
misrepresent binary content. Base64 with the std library only (no new crate).

**Depends on**: `KvEngine`, `tap`, `serde_json`? **No** — hand-roll JSON
serialization via `write!` into a `String` to avoid adding `serde_json` as a
dependency. The payload schema is fixed and small; manual formatting is
~30 lines and keeps the dependency count at 6.

**Tests**: payload validity (valid JSON via parse in test using a minimal
checker or `serde_json` dev-dependency — dev-only is allowed since it doesn't
ship), deactivate-on-disconnect, tick interval.

### 4.6 `cli.rs` — dashboard address config

**New flag**: `--dashboard-addr <addr>` (e.g. `127.0.0.1:6381`, `:6381`).

**New directives** (in `ferrum.conf`):
- `dashboard-bind 127.0.0.1` (default)
- `dashboard-port 6381` (default)

Merge precedence (matches existing `--addr` / `bind`+`port` pattern):
CLI flag > config file > default. If `--dashboard-addr` given, it overrides
both `dashboard-bind` and `dashboard-port`. Otherwise bind+port compose.

**New getter**: `CliArgs::dashboard_addr(&self) -> SocketAddr`.

**`0.0.0.0` warning**: emitted in `main.rs` after resolving the address, before
bind: `if addr.ip().is_unspecified() { warn!("Dashboard bound to {} — no authentication is enforced", addr); }`.

**Tests**: default is `127.0.0.1:6381`, CLI overrides config, config composes
bind+port, `0.0.0.0` parses without error (warning is caller's job).

### 4.7 `storage/engine/mod.rs` — new fields and methods

**New fields on `KvEngine`** (all `Arc<AtomicU64>` to match existing `used_memory`/`hits`/`misses`/`rng` shape — `KvEngine::clone()` relies on `Arc`-shared interior mutability, bare atomics would break it. Exception: `start` is `OnceLock<Instant>` because `Instant` is not atomically-storable and we want a monotonic clock for uptime, immune to wall-clock jumps like NTP adjusts):
```rust
commands: Arc<AtomicU64>,      // ticked via engine.tick_command() from execute_command
evictions: Arc<AtomicU64>,     // ticked in enforce_memory_limit
start: Arc<OnceLock<Instant>>, // set once in KvEngine::new; uptime_secs = start.get().unwrap().elapsed().as_secs(). Arc-shared so all clones read the same Instant.
eviction_log: Option<Arc<EvictionLog>>,  // None when maxmemory == 0
```

**New methods**:
```rust
pub fn tick_command(&self);            // self.commands.fetch_add(1, Relaxed) — called from execute_command in server.rs
pub fn commands(&self) -> u64;
pub fn evictions(&self) -> u64;
pub fn uptime_secs(&self) -> u64;      // (now_ms - epoch_ms) / 1000
pub fn eviction_log_snapshot(&self) -> Vec<EvictionEvent>;  // empty if None
pub fn keyspace_distribution_sampled(&self, k: usize) -> KeyspaceDistribution;
```

**`tick_command` rationale**: `execute_command` (server.rs:272) is a free
function `fn execute_command(cmd, engine: &KvEngine, out)`, not a `KvEngine`
method — it cannot touch `self.commands`. The thin `engine.tick_command()`
public method keeps the tick at the call site (one line) without exposing
the raw `Arc<AtomicU64>` as `pub`.

**`KeyspaceDistribution`** (in `engine/mod.rs`):
```rust
pub struct KeyspaceDistribution {
    pub ttl_buckets: [u32; 6],   // [≤1s, ≤10s, ≤1m, ≤5m, ≤1h, ∞/no-TTL]
    pub size_buckets: [u32; 4],  // [≤64B, ≤256B, ≤1KB, >1KB]
    pub lfu_heat: [u32; 5],      // [0-25, 25-50, 50-100, 100-200, >200] LFU counter ranges
}
```

Sampling: `next_rand01` (already private) walks the `indexmap` with random
offset, collects `k` keys, buckets each. Read lock held for the duration;
O(200) under read lock is bounded and cheap.

**`enforce_memory_limit` change**: before `track_remove`, push
`EvictionEvent { key: victim.key.clone(), policy: cfg.policy.name(), ttl_remaining_ms: ..., ts: ... }`
to `self.eviction_log` (if `Some`). After loop: `self.evictions.fetch_add(evicted, Relaxed)`.

**Tests**: new counter accuracy, distribution sampling hits all buckets with
synthetic data, eviction log populated by a forced-eviction test (already
exists in `eviction_test.rs` — extend).

### 4.8 `network/server.rs` — run_listener + execute_command

**`run_listener` change**: signature gains `dashboard_listener: Option<TcpListener>`.
Inside `block_on`, before `accept_loop`: if `Some`, `tokio::spawn(dashboard::serve(...))`.
The `Resp2Tap` is `Arc::new(Resp2Tap::default())`, shared into both `accept_loop`
(passed to `handle_client`) and `dashboard::serve`.

**`accept_loop` change**: clone `tap: Arc<Resp2Tap>` into each spawned
`handle_client` task.

**`handle_client` change**: after each successful read, `tap.push(Direction::Request, &read_buf)`;
after each successful write, `tap.push(Direction::Response, &write_buf)`. These
are the hot-path taps benchmarked by decision #14.

**`execute_command` change**: first line of the free function (server.rs:272, signature `fn execute_command(cmd: Command, engine: &KvEngine, out: &mut Vec<u8>)`), call `engine.tick_command();`. This is a `&KvEngine` method call (not `self.` — `execute_command` is not a method), tick lands via the new `tick_command` public wrapper documented in §4.7. Includes invalid commands (an op is an op, per decision #12).

**Tests**: `dashboard_tap_inactive_zero_overhead` bench group (decision #14);
existing wire tests unchanged (tap inactive = no-op, behavior identical).

## 5. Error Handling

All fallible ops return `Result<_, FerrumError>`. New error variants: none
needed — dashboard bind failure in `main.rs` uses the existing `io::Error` path
(logged, not propagated as `FerrumError` since dashboard is non-critical).

`FerrumError::Display` text for dashboard-specific errors (e.g. malformed HTTP
request) is written for operators: `"malformed HTTP request line"` not a debug
dump.

## 6. Testing Strategy

| Layer | Test type | Location |
|-------|-----------|----------|
| `EvictionLog` | unit | `src/storage/eviction_log.rs` inline `mod tests` |
| `Resp2Tap` | unit | `src/network/dashboard/tap.rs` inline `mod tests` |
| `keyspace_distribution_sampled` | unit | `src/storage/engine/tests.rs` (existing file) |
| `execute_command` counter | unit | extend existing `engine/tests.rs` |
| `enforce_memory_limit` eviction log + counter | integration | extend `tests/eviction_test.rs` |
| CLI `--dashboard-addr` | unit | extend `src/cli.rs` inline `mod tests` |
| Dashboard HTTP routing | integration | new `tests/dashboard_test.rs` — bind `127.0.0.1:0`, GET `/`, assert HTML body; GET `/metrics`, assert valid JSON |
| SSE endpoint | integration | `tests/dashboard_test.rs` — connect `EventSource`-style raw TCP, assert first frame arrives within 1s |
| Tap zero-overhead | bench | `benches/resp2_bench.rs` new group |
| Binary safety in JSON | unit | `sse.rs` tests — `Nul` and non-UTF-8 in keys/bytes round-trip via base64 |

**Integration test note**: `tests/dashboard_test.rs` spawns a real KvEngine +
dashboard listener (mirrors `tests/resp2_wire_test.rs`'s real-TCP approach per
project testing conventions). No mocking.

## 7. Performance Verification

- `resp2_bench.rs`: `dashboard_tap_inactive_zero_overhead` — Criterion group
  comparing RESP2 ops/s with no dashboard vs dashboard spawned-but-idle.
  Assert delta < 1% (spec §9).
- Hot-path `tap.push` when inactive: one `Relaxed` atomic load + branch. This
  is the only per-operation overhead added to the RESP2 data path.
- SSE payload size: bounded by ring buffer capacities (200 entries × ~200 bytes
  = ~40KB worst case for RESP2 stream, ~12KB for eviction log). At 2Hz that's
  ~80KB/s peak bandwidth — negligible on localhost.

## 8. Security

- Default bind `127.0.0.1` — no network exposure.
- `0.0.0.0` explicit bind → `warn!` in logs (decision #10).
- Read-only: no POST/PUT/DELETE routes, no command execution against the engine.
- No auth: documented in spec §10 as a scope boundary. The `0.0.0.0` warning is
  the only guardrail.

## 9. Out of Scope (v0.5.0)

Per product spec §0 roadmap:
- Sparklines (v0.5.1)
- Eviction Log EPS breakdown (v0.5.1)
- Animations (v0.5.1)
- Sandbox (v0.6.0)
- SIEVE (v0.6.0, blocked by FERRUM-009)

## 10. Open Implementation Risks

1. **`run_listener` signature change** is a public API break. The only caller
   is `main.rs`. The change is internal to the binary, no library consumers
   affected. Low risk.

2. **`handle_client` data-path edit** touches the hot loop. Must verify with
   the new bench group that inactive tap adds <1% overhead. If the branch
   prediction assumption fails (unlikely on modern CPUs for a always-false
   branch), fallback: move the `if` into a separate `#[inline(always)]` fn
   that the compiler can sink more aggressively.

3. **Manual JSON serialization** (decision to avoid `serde_json`) is ~30 lines
   but error-prone for escaping. Mitigation: restrict the payload to numeric
   values + base64 strings (no free-text fields), so the only escaping concern
   is base64 alphabet (no `"`/`\` in base64). Test with a JSON validator in
   the dev-dependencies.

4. **`cli.rs` is 701 lines** (project issue FERRUM-005 flags it exceeding 2000
   — it's not there yet but growing). Adding `--dashboard-addr` + 2 directives
   is ~30 lines. Acceptable for v0.5.0; if the file later crosses 2000, the
   dashboard config can be split out then.

---

*End of engineering design. Next step: `writing-plans` skill to produce the
implementation plan.*
