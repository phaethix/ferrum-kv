# Dashboard v0.5.0 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship a localhost HTTP dashboard that surfaces FerrumKV's live cache internals via SSE — four metric cards, AHE adaptive-alpha gauge, simplified eviction log, live RESP2 wire stream, and sampled keyspace distribution.

**Architecture:** A second tokio listener spawned inside `run_listener`'s `block_on`, sharing the existing `MultiThread` runtime, `KvEngine` (via `Arc`-clone), and `Shutdown`. SSE pushes a combined JSON payload at 2Hz. Frontend is a single `index.html` with embedded CSS + vanilla JS, compiled in via `include_str!`. Zero new crate dependencies.

**Tech Stack:** Rust 2024, tokio 1 (existing features), std-only for JSON/base64, Criterion 0.8 (dev).

## Global Constraints

Copied verbatim from project `.atomcode.md` + design spec:

- **Rust edition 2024**, `cargo fmt`/`clippy -D warnings`/`cargo check`/`cargo test` all must pass before any task is declared done.
- **Binary safety is sacred**: keys/values are `Vec<u8>`, never assume UTF-8. `Nul`/`\r\n`/non-UTF-8 must round-trip through network + AOF + dashboard JSON unchanged. Dashboard JSON base64-encodes all binary fields.
- **Zero new crate dependencies**: `Cargo.toml` stays at 6 deps. `serde_json` is forbidden in prod deps; dev-dep OK for test assertions only.
- **`KvEngine` interior state**: new shared state defaults to `Arc<AtomicU64>`; `Mutex`/`RwLock` only when atomics can't express the invariant. All new fields must be `Arc`-wrapped to keep `KvEngine::clone()` cheap-clone semantics.
- **RESP2 only**: dashboard HTTP is a separate listener; RESP2 server behavior is untouched except for the gated tap (inactive = no-op).
- **4-space indent**, `cargo fmt` is authoritative. Public items have `///` doc comments.
- **Conventional Commits**: `feat:`, `fix:`, `test:`, `docs:`, `refactor:`, `bench:`, `chore:`, `ci:`.
- **No `Co-Authored-By` trailers** in commits unless the user explicitly asks.

---

## File Structure

**New files:**
| File | Responsibility |
|------|----------------|
| `src/storage/eviction_log.rs` | `EvictionLog` ring buffer + `EvictionEvent` struct |
| `src/network/dashboard/mod.rs` | Module re-exports, `DashboardConfig` |
| `src/network/dashboard/serve.rs` | `serve()` accept loop + `0.0.0.0` warning hook |
| `src/network/dashboard/http.rs` | `handle_http()` routing + `DASHBOARD_HTML` const |
| `src/network/dashboard/sse.rs` | `stream()` SSE loop + `build_payload()` |
| `src/network/dashboard/tap.rs` | `Resp2Tap` + `TapEntry` + `Direction` |
| `src/network/dashboard/index.html` | Single-file frontend (HTML + CSS + JS inline) |
| `tests/dashboard_test.rs` | Integration tests for HTTP + SSE endpoints |

**Modified files:**
| File | Changes |
|------|---------|
| `src/storage/mod.rs` | `pub mod eviction_log;` |
| `src/storage/engine/mod.rs` | +4 fields, +5 methods, `new()` init, `enforce_memory_limit` tick+push |
| `src/network/mod.rs` | `pub mod dashboard;` |
| `src/network/server.rs` | `run_listener` spawns dashboard; `accept_loop`+`handle_client` tap; `execute_command` tick |
| `src/cli.rs` | `--dashboard-addr` flag, `RawFlags`+`CliArgs`+`merge`+`scan_argv` extensions |
| `src/config/file.rs` | `FileConfig` +2 fields (`dashboard_bind`, `dashboard_port`) |
| `src/main.rs` | Bind dashboard listener, emit `0.0.0.0` warning, pass into `run_listener` |
| `ferrum.conf.example` | +`dashboard-bind`/`dashboard-port` example directives |
| `benches/resp2_bench.rs` | +`dashboard_tap_inactive_zero_overhead` group |
| `tests/eviction_test.rs` | Extend forced-eviction test to assert eviction log + counter |

---

## Task 1: `EvictionLog` ring buffer

**Files:**
- Create: `src/storage/eviction_log.rs`
- Modify: `src/storage/mod.rs` (add `pub mod eviction_log;`)

**Interfaces:**
- Consumes: nothing (std only)
- Produces: `pub struct EvictionEvent { pub key: Vec<u8>, pub policy: &'static str, pub ttl_remaining_ms: Option<u64>, pub ts: u64 }`, `pub struct EvictionLog`, `impl EvictionLog { pub fn new(capacity: usize) -> Self; pub fn push(&self, event: EvictionEvent); pub fn snapshot(&self) -> Vec<EvictionEvent>; }`

- [ ] **Step 1: Write the failing tests**

Create `src/storage/eviction_log.rs` with the test module first. The implementation will be filled in Step 3.

```rust
//! Bounded ring buffer of recent eviction events for the dashboard.
//!
//! The dashboard's Eviction Log panel consumes this via `snapshot()`.
//! Capacity is fixed at construction; `push` drops the oldest entry when
//! full. All operations are `O(1)` amortised.

use std::collections::VecDeque;
use std::sync::Mutex;

/// One eviction decision, captured at the moment `enforce_memory_limit`
/// removed a key. Designed for the dashboard's "show your work" panel.
///
/// `policy` is `&'static str` because `EvictionPolicy::name()` returns a
/// `'static` slice via the enum — no allocation per event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvictionEvent {
    /// The key that was evicted. Binary-safe: may contain `Nul` or non-UTF-8.
    pub key: Vec<u8>,
    /// Name of the eviction policy that made the decision (e.g. "allkeys-ahe").
    pub policy: &'static str,
    /// Remaining TTL at eviction time, in milliseconds. `None` = no TTL set.
    pub ttl_remaining_ms: Option<u64>,
    /// Wall-clock millis at eviction (for the dashboard's timestamp display).
    pub ts: u64,
}

/// Fixed-capacity ring buffer of `EvictionEvent`s. Thread-safe via a single
/// `Mutex` (write-heavy: every eviction pushes; the dashboard SSE tick reads
/// at 2Hz). Pre-allocated at construction; no allocation after init.
pub struct EvictionLog {
    buf: Mutex<VecDeque<EvictionEvent>>,
    capacity: usize,
}

impl EvictionLog {
    /// Creates an empty log with the given fixed capacity. The internal
    /// `VecDeque` is pre-allocated to `capacity` so `push` never allocates.
    pub fn new(capacity: usize) -> Self {
        let mut buf = VecDeque::with_capacity(capacity);
        buf.reserve(capacity);
        Self {
            buf: Mutex::new(buf),
            capacity,
        }
    }

    /// Appends an event. If at capacity, drops the oldest entry (FIFO eviction
    /// from the log itself — the most recent `capacity` events are kept).
    pub fn push(&self, event: EvictionEvent) {
        let mut buf = self.buf.lock().expect("eviction log mutex poisoned");
        if buf.len() >= self.capacity {
            buf.pop_front();
        }
        buf.push_back(event);
    }

    /// Returns a clone of all current events. The dashboard calls this at
    /// 2Hz; the lock is held only for the clone duration.
    pub fn snapshot(&self) -> Vec<EvictionEvent> {
        let buf = self.buf.lock().expect("eviction log mutex poisoned");
        buf.iter().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn evt(key: &[u8], policy: &'static str, ttl: Option<u64>, ts: u64) -> EvictionEvent {
        EvictionEvent {
            key: key.to_vec(),
            policy,
            ttl_remaining_ms: ttl,
            ts,
        }
    }

    #[test]
    fn snapshot_empty_when_no_pushes() {
        let log = EvictionLog::new(200);
        assert!(log.snapshot().is_empty());
    }

    #[test]
    fn push_then_snapshot_returns_event() {
        let log = EvictionLog::new(200);
        log.push(evt(b"user:921", "allkeys-ahe", Some(12_000), 1));
        let snap = log.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].key, b"user:921");
        assert_eq!(snap[0].policy, "allkeys-ahe");
        assert_eq!(snap[0].ttl_remaining_ms, Some(12_000));
        assert_eq!(snap[0].ts, 1);
    }

    #[test]
    fn capacity_bound_drops_oldest() {
        let log = EvictionLog::new(3);
        log.push(evt(b"a", "p", None, 1));
        log.push(evt(b"b", "p", None, 2));
        log.push(evt(b"c", "p", None, 3));
        log.push(evt(b"d", "p", None, 4)); // exceeds capacity, drops "a"
        let snap = log.snapshot();
        assert_eq!(snap.len(), 3);
        assert_eq!(snap[0].key, b"b");
        assert_eq!(snap[1].key, b"c");
        assert_eq!(snap[2].key, b"d");
    }

    #[test]
    fn fifo_order_preserved() {
        let log = EvictionLog::new(200);
        for i in 0..10u64 {
            log.push(evt(&[i as u8], "p", None, i));
        }
        let snap = log.snapshot();
        assert_eq!(snap.len(), 10);
        for (i, e) in snap.iter().enumerate() {
            assert_eq!(e.ts, i as u64);
        }
    }

    #[test]
    fn binary_key_round_trips() {
        // Keys with Nul, \r\n, and non-UTF-8 must survive unchanged.
        // Guards the project's "binary safety is sacred" rule.
        let key = vec![0u8, 0x0d, 0x0a, 0xff, 0xfe];
        let log = EvictionLog::new(200);
        log.push(evt(&key, "p", None, 0));
        let snap = log.snapshot();
        assert_eq!(snap[0].key, key);
    }

    #[test]
    fn none_ttl_preserved() {
        let log = EvictionLog::new(200);
        log.push(evt(b"k", "p", None, 0));
        assert!(log.snapshot()[0].ttl_remaining_ms.is_none());
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib storage::eviction_log 2>&1 | head -20`
Expected: FAIL with "could not find module" or compile error — the module isn't wired into `src/storage/mod.rs` yet.

- [ ] **Step 3: Wire the module into `src/storage/mod.rs`**

Read `src/storage/mod.rs` and add `pub mod eviction_log;` alongside the existing `pub mod` declarations. Match the existing declaration order (the file currently has `engine`, `eviction`, `expire` — add `eviction_log` after `eviction`).

```rust
// In src/storage/mod.rs, after the `pub mod eviction;` line:
pub mod eviction_log;
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib storage::eviction_log 2>&1 | tail -20`
Expected: PASS — all 6 tests green.

Then run the full gate:
```bash
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo check && cargo test --lib
```
Expected: all green, zero warnings.

- [ ] **Step 5: Commit**

```bash
git add src/storage/eviction_log.rs src/storage/mod.rs
git commit -m "feat(storage): add EvictionLog ring buffer for dashboard (FERRUM-012)"
```

---

## Task 2: `Resp2Tap` — gated wire-byte mirror

**Files:**
- Create: `src/network/dashboard/tap.rs`
- Create: `src/network/dashboard/mod.rs` (minimal, just re-exports for this task)

**Interfaces:**
- Consumes: nothing (std only)
- Produces: `pub enum Direction { Request, Response }`, `pub struct TapEntry { pub dir: Direction, pub bytes: Vec<u8> }`, `pub struct Resp2Tap { ... }`, `impl Resp2Tap { pub fn new(capacity: usize) -> Self; pub fn activate(&self); pub fn deactivate(&self); pub fn is_active(&self) -> bool; pub fn push(&self, dir: Direction, bytes: &[u8]); pub fn snapshot(&self) -> Vec<TapEntry>; }`, `impl Default for Resp2Tap`

- [ ] **Step 1: Write the failing tests**

Create `src/network/dashboard/mod.rs`:
```rust
//! In-process HTTP dashboard for FerrumKV's live internals.
//!
//! Spawns alongside the RESP2 listener in `run_listener`'s `block_on`,
//! sharing the same tokio runtime and `KvEngine`. See
//! `docs/superpowers/specs/2026-07-06-dashboard-v0.5.0-design.md`.

pub mod tap;

pub use tap::{Direction, Resp2Tap, TapEntry};
```

Create `src/network/dashboard/tap.rs` with tests first:
```rust
//! Optional mirror of RESP2 wire bytes for the dashboard's live stream panel.
//!
//! The tap is gated by an `AtomicBool`: when no dashboard SSE client is
//! connected, `push` is a single relaxed atomic load + a predicted-not-taken
//! branch — effectively zero overhead on the RESP2 hot path. When active,
//! bytes are pushed into a fixed-capacity ring buffer; the SSE endpoint polls
//! `snapshot()` at 2Hz.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

/// Which direction a wire byte sequence flowed: client → server (request)
/// or server → client (response).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Request,
    Response,
}

/// One captured wire byte sequence, tagged with its direction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TapEntry {
    pub dir: Direction,
    pub bytes: Vec<u8>,
}

/// Fixed-capacity ring buffer mirroring RESP2 wire bytes. Thread-safe.
/// Shared via `Arc<Resp2Tap>` between `handle_client` (writer, hot path)
/// and `dashboard::sse` (reader, 2Hz cold path).
pub struct Resp2Tap {
    active: AtomicBool,
    buf: Mutex<VecDeque<TapEntry>>,
    capacity: usize,
}

impl Resp2Tap {
    /// Creates an inactive tap with the given fixed capacity. Pre-allocated.
    pub fn new(capacity: usize) -> Self {
        let mut buf = VecDeque::with_capacity(capacity);
        buf.reserve(capacity);
        Self {
            active: AtomicBool::new(false),
            buf: Mutex::new(buf),
            capacity,
        }
    }

    /// Marks the tap as active — a dashboard SSE client connected. Subsequent
    /// `push` calls will record bytes. Called by `dashboard::sse::stream` on entry.
    pub fn activate(&self) {
        self.active.store(true, Ordering::Relaxed);
    }

    /// Marks the tap as inactive — the dashboard SSE client disconnected.
    /// Called by `dashboard::sse::stream` on exit (drop or error).
    pub fn deactivate(&self) {
        self.active.store(false, Ordering::Relaxed);
    }

    /// Whether the tap is currently recording. Used by tests and the dashboard.
    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::Relaxed)
    }

    /// Records a wire byte sequence with its direction. **Hot path**: called
    /// on every RESP2 read and write. When inactive, this is a single relaxed
    /// atomic load + a branch the CPU predicts as "not taken" — effectively
    /// free. When active, takes the mutex and pushes (drops oldest if at
    /// capacity). Never allocates past construction.
    pub fn push(&self, dir: Direction, bytes: &[u8]) {
        if !self.active.load(Ordering::Relaxed) {
            return;
        }
        let mut buf = self.buf.lock().expect("resp2 tap mutex poisoned");
        if buf.len() >= self.capacity {
            buf.pop_front();
        }
        buf.push_back(TapEntry {
            dir,
            bytes: bytes.to_vec(),
        });
    }

    /// Returns a clone of all current entries. The SSE endpoint calls this
    /// at 2Hz. Lock held only for the clone duration.
    pub fn snapshot(&self) -> Vec<TapEntry> {
        let buf = self.buf.lock().expect("resp2 tap mutex poisoned");
        buf.iter().cloned().collect()
    }
}

impl Default for Resp2Tap {
    fn default() -> Self {
        Self::new(200)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inactive_push_is_no_op() {
        let tap = Resp2Tap::new(200);
        // Default is inactive.
        tap.push(Direction::Request, b"*3\r\n$3\r\nSET\r\n");
        assert!(tap.snapshot().is_empty());
    }

    #[test]
    fn activate_then_push_records() {
        let tap = Resp2Tap::new(200);
        tap.activate();
        tap.push(Direction::Request, b"*3\r\n");
        tap.push(Direction::Response, b"+OK\r\n");
        let snap = tap.snapshot();
        assert_eq!(snap.len(), 2);
        assert_eq!(snap[0].dir, Direction::Request);
        assert_eq!(snap[0].bytes, b"*3\r\n");
        assert_eq!(snap[1].dir, Direction::Response);
        assert_eq!(snap[1].bytes, b"+OK\r\n");
    }

    #[test]
    fn deactivate_stops_recording() {
        let tap = Resp2Tap::new(200);
        tap.activate();
        tap.push(Direction::Request, b"a");
        tap.deactivate();
        tap.push(Direction::Request, b"b"); // should be no-op
        let snap = tap.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].bytes, b"a");
    }

    #[test]
    fn capacity_bound_drops_oldest() {
        let tap = Resp2Tap::new(2);
        tap.activate();
        tap.push(Direction::Request, b"a");
        tap.push(Direction::Request, b"b");
        tap.push(Direction::Request, b"c"); // drops "a"
        let snap = tap.snapshot();
        assert_eq!(snap.len(), 2);
        assert_eq!(snap[0].bytes, b"b");
        assert_eq!(snap[1].bytes, b"c");
    }

    #[test]
    fn binary_bytes_round_trip() {
        // Wire bytes with Nul, \r\n, non-UTF-8 must survive unchanged.
        let bytes = vec![0x00, 0x0d, 0x0a, 0xff, 0xfe];
        let tap = Resp2Tap::new(200);
        tap.activate();
        tap.push(Direction::Request, &bytes);
        assert_eq!(tap.snapshot()[0].bytes, bytes);
    }

    #[test]
    fn default_capacity_is_200() {
        let tap = Resp2Tap::default();
        tap.activate();
        for _ in 0..300 {
            tap.push(Direction::Request, b"x");
        }
        assert_eq!(tap.snapshot().len(), 200);
    }

    #[test]
    fn is_active_reflects_state() {
        let tap = Resp2Tap::default();
        assert!(!tap.is_active());
        tap.activate();
        assert!(tap.is_active());
        tap.deactivate();
        assert!(!tap.is_active());
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib network::dashboard::tap 2>&1 | head -20`
Expected: FAIL with "could not find module" — `src/network/mod.rs` doesn't declare `dashboard` yet.

- [ ] **Step 3: Wire the module into `src/network/mod.rs`**

Read `src/network/mod.rs` and add `pub mod dashboard;` alongside the existing declarations (`server`, `shutdown`).

```rust
// In src/network/mod.rs, after the existing `pub mod` lines:
pub mod dashboard;
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib network::dashboard::tap 2>&1 | tail -20`
Expected: PASS — all 7 tests green.

Full gate:
```bash
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo check && cargo test --lib
```
Expected: all green.

- [ ] **Step 5: Commit**

```bash
git add src/network/dashboard/mod.rs src/network/dashboard/tap.rs src/network/mod.rs
git commit -m "feat(dashboard): add gated Resp2Tap wire-byte mirror (FERRUM-012)"
```

---

## Task 3: `KvEngine` new fields + `tick_command`/counters/uptime/distribution

**Files:**
- Modify: `src/storage/engine/mod.rs` (struct `KvEngine` lines 82-114, `new` lines 124-138, new methods block after `ahe_observe` ~line 992, new `KeyspaceDistribution` struct + `keyspace_distribution_sampled` method)

**Interfaces:**
- Consumes: `EvictionLog` (from Task 1), `next_rand01` (existing private method line 935)
- Produces: `pub fn tick_command(&self)`, `pub fn commands(&self) -> u64`, `pub fn evictions(&self) -> u64`, `pub fn uptime_secs(&self) -> u64`, `pub fn eviction_log_snapshot(&self) -> Vec<EvictionEvent>`, `pub fn keyspace_distribution_sampled(&self, k: usize) -> KeyspaceDistribution`, `pub struct KeyspaceDistribution { pub ttl_buckets: [u32; 6], pub size_buckets: [u32; 4], pub lfu_heat: [u32; 5] }`

**Reference: existing `KvEngine` struct (line 82) and `new` (line 124) — read these before editing to match field order and init style.**

- [ ] **Step 1: Write the failing tests**

Add these tests to `src/storage/engine/tests.rs` (existing file — read it first to match its `use super::*` style and test naming conventions).

```rust
// In src/storage/engine/tests.rs

#[test]
fn tick_command_increments_counter() {
    let engine = KvEngine::new();
    assert_eq!(engine.commands(), 0);
    engine.tick_command();
    engine.tick_command();
    engine.tick_command();
    assert_eq!(engine.commands(), 3);
}

#[test]
fn uptime_secs_grows_monotonically() {
    let engine = KvEngine::new();
    // Just constructed: uptime should be ~0.
    let first = engine.uptime_secs();
    std::thread::sleep(std::time::Duration::from_secs(2));
    let second = engine.uptime_secs();
    assert!(second >= first + 1, "uptime should advance: first={first}, second={second}");
}

#[test]
fn evictions_starts_at_zero() {
    let engine = KvEngine::new();
    assert_eq!(engine.evictions(), 0);
}

#[test]
fn eviction_log_snapshot_empty_when_no_eviction_log() {
    // Default engine has maxmemory == 0, so eviction_log is None.
    let engine = KvEngine::new();
    assert!(engine.eviction_log_snapshot().is_empty());
}

#[test]
fn keyspace_distribution_buckets_hit_with_synthetic_data() {
    use ferrum_kv::storage::engine::KeyspaceDistribution;
    let engine = KvEngine::new();
    // Insert keys with varied TTLs, sizes, and access patterns.
    // TTL buckets: [≤1s, ≤10s, ≤1m, ≤5m, ≤1h, ∞/no-TTL]
    engine.set(b"short-1s".to_vec(), b"x".to_vec()).unwrap();
    engine.expire_at_ms(b"short-1s", now_epoch_ms() + 500).unwrap();
    engine.set(b"med-10s".to_vec(), b"x".to_vec()).unwrap();
    engine.expire_at_ms(b"med-10s", now_epoch_ms() + 5_000).unwrap();
    engine.set(b"no-ttl".to_vec(), b"x".to_vec()).unwrap();
    // Size buckets: [≤64B, ≤256B, ≤1KB, >1KB]
    engine.set(b"s64".to_vec(), vec![b'a'; 64]).unwrap();
    engine.set(b"s300".to_vec(), vec![b'b'; 300]).unwrap();
    engine.set(b"s2000".to_vec(), vec![b'c'; 2000]).unwrap();

    let dist = engine.keyspace_distribution_sampled(200);
    // With 5 keys and sample=200, all keys are visited (sample >= len).
    // TTL: 2 keys have TTL (short-1s, med-10s), 3 have no TTL (no-ttl, s64, s300, s2000 — wait that's 4 no-TTL).
    // Actually: short-1s (TTL), med-10s (TTL), no-ttl (no TTL), s64 (no TTL), s300 (no TTL), s2000 (no TTL) = 2 TTL, 4 no-TTL.
    let ttl_total: u32 = dist.ttl_buckets.iter().sum();
    let size_total: u32 = dist.size_buckets.iter().sum();
    assert_eq!(ttl_total, 6, "all 6 keys counted in TTL buckets");
    assert_eq!(size_total, 6, "all 6 keys counted in size buckets");
    // The no-TTL bucket (index 5) should have 4.
    assert_eq!(dist.ttl_buckets[5], 4, "no-TTL bucket count");
    // LFU heat: 5 buckets, sum should be 6 (all keys counted).
    let lfu_total: u32 = dist.lfu_heat.iter().sum();
    assert_eq!(lfu_total, 6, "all keys counted in LFU heat");
}

/// Helper: current epoch millis. Used to set TTLs relative to "now".
fn now_epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib storage::engine::tests::tick_command_increments_counter 2>&1 | head -20`
Expected: FAIL with "no method `tick_command` found" or "no field `commands`" — the fields and methods don't exist yet.

- [ ] **Step 3: Implement the fields, `new` init, and new methods**

**3a. Add fields to the `KvEngine` struct** (after line 113, the `rng` field):

```rust
    /// Cumulative command count (every `execute_command` call, including
    /// invalid commands). Consumed by the dashboard's ops/s card.
    pub(crate) commands: Arc<AtomicU64>,
    /// Cumulative eviction count. Incremented once per `enforce_memory_limit`
    /// sweep (not per victim) to avoid hot-path atomic contention.
    pub(crate) evictions: Arc<AtomicU64>,
    /// Monotonic clock start instant, set once in `new`. Used for `uptime_secs`.
    /// `Arc`-shared so all clones read the same Instant; `OnceLock` because
    /// `Instant` is not atomically-storable and we want a monotonic clock
    /// immune to wall-clock jumps (NTP adjusts).
    pub(crate) start: Arc<OnceLock<Instant>>,
    /// Bounded ring buffer of recent eviction events for the dashboard.
    /// `None` when `maxmemory == 0` (no evictions possible) — saves allocation.
    /// Set to `Some` by `set_eviction_config` when a memory cap is configured.
    pub(crate) eviction_log: Option<Arc<EvictionLog>>,
```

Add `use std::sync::OnceLock;` and `use std::time::Instant;` to the top of `engine/mod.rs` if not already present (check existing imports first — `Instant` is likely already imported via `eviction.rs` usage).

**3b. Update `new()`** (line 124) — add the four new fields to the `Self { ... }` literal:

```rust
    pub fn new() -> Self {
        let start = Arc::new(OnceLock::new());
        start.set(Instant::now()).expect("OnceLock set once in new");
        Self {
            store: Arc::default(),
            aof: None,
            used_memory: Arc::new(AtomicU64::new(0)),
            eviction: Arc::new(RwLock::new(EvictionConfig::default())),
            hits: Arc::new(AtomicU64::new(0)),
            misses: Arc::new(AtomicU64::new(0)),
            ahe: Arc::new(Mutex::new(AdaptiveHybridState::default())),
            rng: Arc::new(AtomicU32::new(rng_seed())),
            commands: Arc::new(AtomicU64::new(0)),
            evictions: Arc::new(AtomicU64::new(0)),
            start,
            eviction_log: None,
        }
    }
```

**3c. Add new methods** after `ahe_observe` (line ~992), before the closing `}` of the `impl KvEngine` block:

```rust
    /// Increments the command counter. Called from `execute_command` in
    /// `server.rs` (a free function, not a method — hence this thin wrapper).
    pub fn tick_command(&self) {
        self.commands.fetch_add(1, Ordering::Relaxed);
    }

    /// Cumulative command count since engine construction.
    pub fn commands(&self) -> u64 {
        self.commands.load(Ordering::Relaxed)
    }

    /// Cumulative eviction count since engine construction.
    pub fn evictions(&self) -> u64 {
        self.evictions.load(Ordering::Relaxed)
    }

    /// Seconds since the engine was constructed. Monotonic; immune to wall-clock jumps.
    pub fn uptime_secs(&self) -> u64 {
        self.start
            .get()
            .expect("start Instant is set once in new")
            .elapsed()
            .as_secs()
    }

    /// Snapshot of the recent eviction log for the dashboard. Empty if no
    /// eviction log is configured (i.e. `maxmemory == 0`).
    pub fn eviction_log_snapshot(&self) -> Vec<EvictionEvent> {
        self.eviction_log
            .as_ref()
            .map(|log| log.snapshot())
            .unwrap_or_default()
    }

    /// Samples `k` keys randomly from the store and buckets them by TTL
    /// range, key size, and LFU counter heat. For the dashboard's keyspace
    /// distribution panel. O(k) under the read lock.
    ///
    /// Bucket boundaries:
    /// - `ttl_buckets`: [≤1s, ≤10s, ≤1m, ≤5m, ≤1h, ∞/no-TTL] (6 buckets)
    /// - `size_buckets`: [≤64B, ≤256B, ≤1KB, >1KB] (4 buckets)
    /// - `lfu_heat`: [0-25, 25-50, 50-100, 100-200, >200] (5 LFU counter ranges)
    pub fn keyspace_distribution_sampled(&self, k: usize) -> KeyspaceDistribution {
        let store = self.store.read().expect("store lock poisoned");
        let total = store.len();
        if total == 0 || k == 0 {
            return KeyspaceDistribution::default();
        }
        let mut dist = KeyspaceDistribution::default();
        let now = Instant::now();
        // Walk `k` random indices. With `k >= total` we visit every key.
        for _ in 0..k {
            let idx = (self.next_rand01() * total as f32) as usize;
            let idx = idx.min(total - 1);
            // indexmap supports positional access via `.get_index(idx)`.
            let Some((key, entry)) = store.get_index(idx) else {
                continue;
            };
            // TTL bucket.
            let ttl_bucket = match entry.expire_at {
                None => 5, // no TTL
                Some(expire_at) => {
                    let remaining = expire_at.saturating_sub(now);
                    let secs = remaining.as_secs();
                    match secs {
                        0..=1 => 0,
                        2..=10 => 1,
                        11..=60 => 2,
                        61..=300 => 3,
                        301..=3600 => 4,
                        _ => 5, // >1h treated as "not expiring soon" — bucket with no-TTL for display
                    }
                }
            };
            dist.ttl_buckets[ttl_bucket] += 1;
            // Size bucket (key + value length).
            let total_size = key.len() + entry.value.len();
            let size_bucket = match total_size {
                0..=64 => 0,
                65..=256 => 1,
                257..=1024 => 2,
                _ => 3,
            };
            dist.size_buckets[size_bucket] += 1;
            // LFU heat bucket.
            let lfu = entry.lfu_counter;
            let lfu_bucket = match lfu {
                0..=25 => 0,
                26..=50 => 1,
                51..=100 => 2,
                101..=200 => 3,
                _ => 4,
            };
            dist.lfu_heat[lfu_bucket] += 1;
        }
        dist
    }
```

**3d. Add the `KeyspaceDistribution` struct** near the top of `engine/mod.rs` (after the `KvEngine` struct or after `ValueEntry`, wherever the existing types live — read the file to find the right spot):

```rust
/// Bucketed keyspace statistics for the dashboard's distribution panel.
/// All arrays are fixed-size to avoid heap allocation on the sample path.
///
/// Bucket boundaries:
/// - `ttl_buckets[6]`: [≤1s, ≤10s, ≤1m, ≤5m, ≤1h, ∞/no-TTL]
/// - `size_buckets[4]`: [≤64B, ≤256B, ≤1KB, >1KB]
/// - `lfu_heat[5]`: [0-25, 25-50, 50-100, 100-200, >200] LFU counter ranges
#[derive(Debug, Default)]
pub struct KeyspaceDistribution {
    pub ttl_buckets: [u32; 6],
    pub size_buckets: [u32; 4],
    pub lfu_heat: [u32; 5],
}
```

**3e. Update `set_eviction_config`** (line 153) to instantiate `eviction_log` when a memory cap is set. Read the existing method first, then add after the current body:

```rust
    pub fn set_eviction_config(&self, cfg: EvictionConfig) -> Result<(), FerrumError> {
        // ... existing body ...
        let mut eviction = self.eviction.write().expect("eviction lock poisoned");
        // Instantiate the eviction log iff a memory cap is set.
        // We mutate `self` via interior mutability — but `eviction_log` is not
        // under the `eviction` RwLock, it's a separate `Option<Arc<...>>`.
        // We need a write lock on a different guard, OR redesign.
        // DESIGN NOTE: `eviction_log` should be set once at construction based
        // on the initial config, OR wrapped in its own lock. For simplicity,
        // we instantiate it eagerly in `new()` always, and `eviction_log_snapshot`
        // returns empty when `maxmemory == 0` (the log exists but stays empty
        // because `enforce_memory_limit` exits early). This avoids the mutation problem.
        *eviction = cfg;
        Ok(())
    }
```

**IMPORTANT — design adjustment during implementation:** The original design said `eviction_log: Option<Arc<EvictionLog>>` with `None` when `maxmemory == 0`. But `set_eviction_config` takes `&self`, so it cannot mutate `eviction_log`. Two options:
1. **Always instantiate** `eviction_log` in `new()` (not `None`). It stays empty because `enforce_memory_limit` exits early when `maxmemory == 0`. Cost: one `VecDeque` of 200 pre-allocated slots per engine (~4KB). Acceptable.
2. Wrap `eviction_log` in its own `RwLock<Option<Arc<EvictionLog>>>`. Heavier.

**Choose option 1**: change the field to `eviction_log: Arc<EvictionLog>` (not `Option`), always instantiate in `new()`, and `eviction_log_snapshot` always returns `log.snapshot()`. Update the field declaration, `new()`, and `eviction_log_snapshot` accordingly. The design spec's "None when maxmemory == 0" optimization is YAGNI — 4KB per engine is negligible.

Updated field:
```rust
    pub(crate) eviction_log: Arc<EvictionLog>,
```

Updated `new()` fragment:
```rust
            eviction_log: Arc::new(EvictionLog::new(200)),
```

Updated `eviction_log_snapshot`:
```rust
    pub fn eviction_log_snapshot(&self) -> Vec<EvictionEvent> {
        self.eviction_log.snapshot()
    }
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib storage::engine::tests 2>&1 | tail -30`
Expected: PASS — all new tests green, plus all existing engine tests still green.

Full gate:
```bash
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo check && cargo test --lib
```
Expected: all green.

- [ ] **Step 5: Commit**

```bash
git add src/storage/engine/mod.rs src/storage/engine/tests.rs
git commit -m "feat(engine): add command/eviction counters, uptime, keyspace distribution (FERRUM-012)"
```

---

## Task 4: `enforce_memory_limit` eviction tick + event push

**Files:**
- Modify: `src/storage/engine/mod.rs` (`enforce_memory_limit` lines 786-844)

**Interfaces:**
- Consumes: `EvictionLog` (Task 1), `EvictionEvent` (Task 1), `eviction_log` field (Task 3)
- Produces: side effect — `evictions` counter incremented, `EvictionEvent` pushed to log

- [ ] **Step 1: Write the failing test**

Add to `tests/eviction_test.rs` (existing file — read it first to match its setup style for forced evictions). The test should force an eviction by setting `maxmemory` low and inserting keys that exceed it, then assert the eviction counter and log.

```rust
// In tests/eviction_test.rs — add this test. Read the file first to match
// its existing imports and helper patterns (it likely has a helper that
// builds an engine with a small maxmemory and a forced-eviction policy).

#[test]
fn eviction_counter_and_log_populated_on_eviction() {
    use ferrum_kv::storage::engine::KvEngine;
    use ferrum_kv::storage::eviction::{EvictionConfig, EvictionPolicy};

    let engine = KvEngine::new();
    // 1KB cap, LRU policy, so inserting a 2KB value forces one eviction.
    engine.set_eviction_config(EvictionConfig {
        max_memory: 1024,
        policy: EvictionPolicy::AllKeysLru,
        samples: 5,
    }).unwrap();

    // Insert a key that fits.
    engine.set(b"small".to_vec(), b"x".to_vec()).unwrap();
    assert_eq!(engine.evictions(), 0);
    assert!(engine.eviction_log_snapshot().is_empty());

    // Insert a key that exceeds the cap — forces eviction of "small".
    engine.set(b"big".to_vec(), vec![b'y'; 2048]).unwrap();

    // The eviction counter should have ticked.
    assert!(engine.evictions() > 0, "eviction counter should increment");

    // The eviction log should contain an event for the evicted key.
    let snap = engine.eviction_log_snapshot();
    assert!(!snap.is_empty(), "eviction log should have events");
    let evicted_keys: Vec<&[u8]> = snap.iter().map(|e| e.key.as_slice()).collect();
    assert!(
        evicted_keys.iter().any(|k| k == &b"small"[..]),
        "evicted 'small' key should appear in log: {:?}",
        evicted_keys
    );
    // The event should record the policy name.
    assert_eq!(snap[0].policy, "allkeys-lru");
}

#[test]
fn eviction_log_respects_binary_keys() {
    use ferrum_kv::storage::engine::KvEngine;
    use ferrum_kv::storage::eviction::{EvictionConfig, EvictionPolicy};

    let engine = KvEngine::new();
    engine.set_eviction_config(EvictionConfig {
        max_memory: 64,
        policy: EvictionPolicy::AllKeysLru,
        samples: 5,
    }).unwrap();

    // Key with Nul and non-UTF-8 bytes.
    let binary_key = vec![0x00, 0xff, 0xfe, 0x0d, 0x0a];
    engine.set(binary_key.clone(), b"v".to_vec()).unwrap();
    // Force eviction by inserting a large key.
    engine.set(b"big".to_vec(), vec![b'y'; 128]).unwrap();

    let snap = engine.eviction_log_snapshot();
    assert!(!snap.is_empty());
    assert!(
        snap.iter().any(|e| e.key == binary_key),
        "binary key should appear in eviction log unchanged"
    );
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --test eviction_test eviction_counter_and_log 2>&1 | head -20`
Expected: FAIL — `evictions()` returns 0 and/or `eviction_log_snapshot()` is empty because `enforce_memory_limit` doesn't tick or push yet.

- [ ] **Step 3: Implement the tick and push in `enforce_memory_limit`**

Read `enforce_memory_limit` (lines 786-844). The bounded loop currently calls `track_remove` and `aof.append_del`. Add the eviction event push **before** `track_remove` (so the key still exists in the store for TTL lookup), and the counter tick **after** the loop.

```rust
// In enforce_memory_limit, inside the bounded loop, AFTER the victim is
// selected and unwrapped (`let Some(victim) = victim else { ... };`),
// BEFORE `self.track_remove(store, &victim.key);`:

            // Record the eviction event for the dashboard. The victim's
            // `expire_at` gives remaining TTL; `now` is already in scope
            // (or compute it here). Policy name is `'static` from the enum.
            let ttl_remaining_ms = victim.expire_at.and_then(|expire_at| {
                let now = Instant::now();
                if expire_at > now {
                    Some((expire_at - now).as_millis() as u64)
                } else {
                    Some(0)
                }
            });
            let now_millis = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            self.eviction_log.push(EvictionEvent {
                key: victim.key.clone(),
                policy: cfg.policy.name(),
                ttl_remaining_ms,
                ts: now_millis,
            });
            self.track_remove(store, &victim.key);
            evicted += 1;
            // ... existing aof.append_del ...

// AFTER the bounded loop, BEFORE the AHE observe block:
        if evicted > 0 {
            self.evictions.fetch_add(evicted, Ordering::Relaxed);
        }
        // ... existing AHE observe block ...
```

**Note:** Check whether `now: Instant` is already computed inside the loop scope. If `pick_victim_ahe` computes `now` internally but doesn't expose it, compute a fresh `Instant::now()` for the TTL remaining calculation — the slight time skew is irrelevant for a dashboard display.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --test eviction_test 2>&1 | tail -20`
Expected: PASS — both new tests green, plus all existing eviction tests still green.

Full gate:
```bash
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo check && cargo test
```
Expected: all green.

- [ ] **Step 5: Commit**

```bash
git add src/storage/engine/mod.rs tests/eviction_test.rs
git commit -m "feat(engine): tick eviction counter and push events to log (FERRUM-012)"
```

---

## Task 5: CLI `--dashboard-addr` + config directives

**Files:**
- Modify: `src/cli.rs` (`RawFlags` line 74, `CliArgs` line 43, `scan_argv` line 160, `merge` line 239, new `compose_dashboard_addr_from_file`)
- Modify: `src/config/file.rs` (`FileConfig` line 28, directive parsing)
- Modify: `ferrum.conf.example` (add example directives)

**Interfaces:**
- Consumes: `FileConfig` (existing), `DEFAULT_ADDR` (existing pattern)
- Produces: `CliArgs::dashboard_addr: String` (new field), `DEFAULT_DASHBOARD_ADDR: &str = "127.0.0.1:6381"`

**Reference: read `cli.rs` lines 43-92, 160-235, 239-327, and `config/file.rs` lines 28-48 + the directive parser before editing.**

- [ ] **Step 1: Write the failing tests**

Add to `src/cli.rs` inline `mod tests` (existing — read it first to match helpers like `TempConf`, `parse_run`).

```rust
// In src/cli.rs mod tests

#[test]
fn dashboard_addr_defaults_to_localhost_6381() {
    let args = parse_run(&[]);
    assert_eq!(args.dashboard_addr, "127.0.0.1:6381");
}

#[test]
fn dashboard_addr_flag_overrides_default() {
    let args = parse_run(&["--dashboard-addr", "0.0.0.0:9999"]);
    assert_eq!(args.dashboard_addr, "0.0.0.0:9999");
}

#[test]
fn dashboard_bind_and_port_from_config_compose() {
    let conf = TempConf::new("dashboard-conf", "dashboard-bind 0.0.0.0\ndashboard-port 9999\n");
    let args = parse_run(&["--config", conf.path()]);
    assert_eq!(args.dashboard_addr, "0.0.0.0:9999");
}

#[test]
fn dashboard_addr_flag_overrides_config() {
    let conf = TempConf::new("dashboard-conf2", "dashboard-bind 0.0.0.0\ndashboard-port 9999\n");
    let args = parse_run(&["--config", conf.path(), "--dashboard-addr", "127.0.0.1:6381"]);
    assert_eq!(args.dashboard_addr, "127.0.0.1:6381");
}

#[test]
fn dashboard_port_alone_from_config_uses_default_bind() {
    let conf = TempConf::new("dashboard-port-only", "dashboard-port 7777\n");
    let args = parse_run(&["--config", conf.path()]);
    assert_eq!(args.dashboard_addr, "127.0.0.1:7777");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib cli::tests::dashboard_addr_defaults 2>&1 | head -20`
Expected: FAIL with "no field `dashboard_addr`" — the field doesn't exist yet.

- [ ] **Step 3: Implement the CLI + config flow**

**3a. Add `DEFAULT_DASHBOARD_ADDR`** near `DEFAULT_ADDR` (line 20):
```rust
pub const DEFAULT_DASHBOARD_ADDR: &str = "127.0.0.1:6381";
```

**3b. Add fields to `RawFlags`** (line 74, after `port`):
```rust
    dashboard_addr: Option<String>,
    dashboard_bind: Option<String>, // from config file only, not a CLI flag
    dashboard_port: Option<u16>,    // ditto
```

**3c. Add field to `CliArgs`** (line 43, after `addr`):
```rust
    pub dashboard_addr: String,
```

**3d. Add `--dashboard-addr` to `scan_argv`** (line 160, after the `"--addr"` arm):
```rust
            "--dashboard-addr" => {
                let value = take_value(&mut iter, "--dashboard-addr")?;
                raw.dashboard_addr = Some(value);
            }
```

**3e. Add `compose_dashboard_addr_from_file`** near `compose_addr_from_file` (line 318):
```rust
fn compose_dashboard_addr_from_file(file: &FileConfig) -> String {
    let default_host = "127.0.0.1";
    let default_port: u16 = 6381;
    let host = file.dashboard_bind.as_deref().unwrap_or(default_host);
    let host = host.split_whitespace().next().unwrap_or(default_host);
    let port = file.dashboard_port.unwrap_or(default_port);
    format!("{host}:{port}")
}
```

**3f. Add the merge logic in `merge`** (line 239, after the `addr` block ~line 247):
```rust
    // --- dashboard_addr ----------------------------------------------------
    let dashboard_addr = if let Some(a) = raw.dashboard_addr {
        a
    } else if let Some(file) = file {
        compose_dashboard_addr_from_file(file)
    } else {
        DEFAULT_DASHBOARD_ADDR.to_string()
    };
```

**3g. Add `dashboard_addr` to the `CliArgs { ... }` literal** at the end of `merge` (line ~299):
```rust
        addr,
        dashboard_addr,
        // ... existing fields ...
```

**3h. Add fields to `FileConfig` in `src/config/file.rs`** (line 28, after `port`):
```rust
    pub dashboard_bind: Option<String>,
    pub dashboard_port: Option<u16>,
```

**3i. Add directive parsing in `src/config/file.rs`** — read the directive parser function and add `dashboard-bind` and `dashboard-port` cases matching the existing `bind`/`port` pattern. The parser likely has a match on directive name that sets the struct field.

**3j. Add example directives to `ferrum.conf.example`** after the existing `bind`/`port` block (line 16):
```
# Host and port the dashboard binds to. The dashboard is a localhost-only
# dev tool by default; binding to 0.0.0.0 disables the implicit security
# boundary and will emit a warning in the logs.
# dashboard-bind 127.0.0.1
# dashboard-port 6381
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib cli::tests::dashboard 2>&1 | tail -20`
Expected: PASS — all 5 new tests green.

Full gate:
```bash
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo check && cargo test --lib
```
Expected: all green.

- [ ] **Step 5: Commit**

```bash
git add src/cli.rs src/config/file.rs ferrum.conf.example
git commit -m "feat(cli): add --dashboard-addr flag and config directives (FERRUM-012)"
```

---

## Task 6: HTTP routing + `index.html` skeleton

**Files:**
- Create: `src/network/dashboard/http.rs`
- Create: `src/network/dashboard/index.html`
- Modify: `src/network/dashboard/mod.rs` (add `pub mod http; pub mod sse;`)

**Interfaces:**
- Consumes: `KvEngine`, `Resp2Tap` (Task 2), `sse::stream` (Task 7 — forward declare)
- Produces: `pub const DASHBOARD_HTML: &str`, `pub async fn handle_http(stream: TcpStream, engine: KvEngine, tap: Arc<Resp2Tap>)`

**Note:** Task 6 and Task 7 have a circular dependency — `http.rs` routes to `sse::stream`, but `sse.rs` doesn't exist until Task 7. Break it: Task 6 creates `http.rs` with a placeholder `sse::stream` call that returns `Err` (stub), Task 7 fills in `sse.rs`. The Task 6 tests only cover `/` and `/metrics` (not `/events`), so the stub is fine.

- [ ] **Step 1: Write the failing tests**

These are integration tests — create `tests/dashboard_test.rs`. The test spawns a real dashboard listener on a free port and asserts the `/` route returns HTML and `/metrics` returns valid JSON.

```rust
// In tests/dashboard_test.rs
//! Integration tests for the dashboard HTTP endpoints.
//!
//! Spawns a real KvEngine + dashboard listener on a free port, mirroring
//! the project's real-TCP testing convention (see tests/resp2_wire_test.rs).

use std::sync::Arc;

use ferrum_kv::network::dashboard::{handle_http, Resp2Tap};
use ferrum_kv::storage::engine::KvEngine;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Helper: bind a dashboard listener on a free localhost port, spawn `handle_http`
/// for one connection, and return the port. The caller connects and reads.
async fn spawn_dashboard_one_conn() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let engine = KvEngine::new();
    let tap = Arc::new(Resp2Tap::default());
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        handle_http(stream, engine, tap).await;
    });
    port
}

/// Helper: send a raw HTTP GET and return the full response bytes.
async fn http_get(port: u16, path: &str) -> Vec<u8> {
    let mut stream = tokio::net::TcpStream::connect(format!("127.0.0.1:{port}"))
        .await
        .unwrap();
    stream
        .write_all(format!("GET {path} HTTP/1.1\r\nHost: localhost\r\n\r\n").as_bytes())
        .await
        .unwrap();
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.unwrap();
    buf
}

#[tokio::test]
async fn root_route_returns_html() {
    let port = spawn_dashboard_one_conn().await;
    let resp = http_get(port, "/").await;
    let resp_str = String::from_utf8_lossy(&resp);
    assert!(resp_str.starts_with("HTTP/1.1 200"), "expected 200, got: {}", resp_str.lines().next().unwrap_or(""));
    assert!(resp_str.contains("text/html"), "expected text/html Content-Type");
    // The HTML should contain the title "FerrumKV Dashboard" so the browser tab is useful.
    assert!(resp_str.contains("FerrumKV Dashboard"), "expected title in HTML");
}

#[tokio::test]
async fn metrics_route_returns_json() {
    let port = spawn_dashboard_one_conn().await;
    let resp = http_get(port, "/metrics").await;
    let resp_str = String::from_utf8_lossy(&resp);
    assert!(resp_str.starts_with("HTTP/1.1 200"), "expected 200");
    assert!(resp_str.contains("application/json"), "expected JSON Content-Type");
    // Extract the body (after the blank line) and check it parses as JSON.
    let body = resp_str.split("\r\n\r\n").nth(1).unwrap_or("");
    // Basic JSON validity check: starts with { and ends with } (whitespace trimmed).
    let body_trim = body.trim();
    assert!(body_trim.starts_with('{'), "JSON body should start with '{{': {}", &body_trim[..20.min(body_trim.len())]);
    assert!(body_trim.ends_with('}'), "JSON body should end with '}}'");
}

#[tokio::test]
async fn unknown_route_returns_404() {
    let port = spawn_dashboard_one_conn().await;
    let resp = http_get(port, "/nonexistent").await;
    let resp_str = String::from_utf8_lossy(&resp);
    assert!(resp_str.starts_with("HTTP/1.1 404"), "expected 404, got: {}", resp_str.lines().next().unwrap_or(""));
}

#[tokio::test]
async fn non_get_returns_405() {
    let port = spawn_dashboard_one_conn().await;
    let mut stream = tokio::net::TcpStream::connect(format!("127.0.0.1:{port}"))
        .await
        .unwrap();
    stream
        .write_all(b"POST / HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .await
        .unwrap();
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.unwrap();
    let resp_str = String::from_utf8_lossy(&buf);
    assert!(resp_str.starts_with("HTTP/1.1 405"), "expected 405 for POST");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --test dashboard_test 2>&1 | head -20`
Expected: FAIL with "unresolved import `ferrum_kv::network::dashboard::handle_http`" — `http.rs` doesn't exist yet.

- [ ] **Step 3: Implement `http.rs` + `index.html` + module wiring**

**3a. Create `src/network/dashboard/index.html`** — a minimal but valid skeleton. The full UX (cards, tooltips, SSE consumption) is added in Task 10; this is enough to pass the tests:

```html
<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>FerrumKV Dashboard</title>
  <style>
    :root { color-scheme: dark; }
    body { font-family: system-ui, sans-serif; background: #0d1117; color: #c9d1d9; margin: 0; padding: 24px; }
    h1 { font-family: ui-monospace, monospace; color: #58a6ff; }
    .card { background: #161b22; border: 1px solid #30363d; border-radius: 8px; padding: 16px; margin: 8px 0; }
    .placeholder { color: #6e7681; font-style: italic; }
  </style>
</head>
<body>
  <h1>FerrumKV Dashboard</h1>
  <p class="placeholder">Loading… (full UI arrives in Task 10)</p>
  <div id="root"></div>
  <script>
    // Full SSE consumption + card rendering arrives in Task 10.
    // This skeleton only proves the HTML route serves.
    console.log("FerrumKV dashboard skeleton loaded");
  </script>
</body>
</html>
```

**3b. Create `src/network/dashboard/http.rs`**:

```rust
//! HTTP request routing for the dashboard.
//!
//! Parses the minimal HTTP request line (`GET <path> HTTP/1.1`) and routes
//! to the right responder. No POST/PUT/DELETE — the dashboard is read-only
//! per spec §10. Malformed requests get 400; non-GET gets 405.

use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt, BufStream};
use tokio::net::TcpStream;

use ferrum_kv::storage::engine::KvEngine;

use super::Resp2Tap;
use super::sse;

/// The single-file frontend, compiled into the binary.
pub const DASHBOARD_HTML: &str = include_str!("index.html");

/// Handle one HTTP connection: parse the request, route it, respond.
pub async fn handle_http(stream: TcpStream, engine: KvEngine, tap: Arc<Resp2Tap>) {
    let mut buf = BufStream::new(stream);
    // Read the request line: "GET <path> HTTP/1.1\r\n..."
    let mut line = String::new();
    match buf.read_line(&mut line).await {
        Ok(0) => return, // EOF before any data — connection closed
        Ok(_) => {}
        Err(_) => return, // IO error — drop silently (no response possible)
    }
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.len() != 3 {
        let _ = write_response(&mut buf, 400, "text/plain", "malformed HTTP request line\n").await;
        return;
    }
    let (method, path) = (parts[0], parts[1]);
    if method != "GET" {
        let _ = write_response(&mut buf, 405, "text/plain", "method not allowed\n").await;
        return;
    }
    // Drain the rest of the request headers (up to the blank line) — we don't
    // need them for GET, but we must consume them so the client is satisfied.
    loop {
        let mut header = String::new();
        match buf.read_line(&mut header).await {
            Ok(0) | Ok(_) if header.trim().is_empty() => break,
            Ok(_) => {}
            Err(_) => return,
        }
    }
    match path {
        "/" => {
            let _ = write_response(&mut buf, 200, "text/html; charset=utf-8", DASHBOARD_HTML).await;
        }
        "/metrics" => {
            let payload = sse::build_payload(&engine, &tap);
            let _ = write_response(&mut buf, 200, "application/json", &payload).await;
        }
        "/events" => {
            // SSE stream — drop the buf wrapper and pass the raw stream.
            let stream = buf.into_inner();
            sse::stream(stream, engine, tap).await;
        }
        _ => {
            let _ = write_response(&mut buf, 404, "text/plain", "not found\n").await;
        }
    }
}

/// Write a minimal HTTP response and flush.
async fn write_response(
    buf: &mut BufStream<TcpStream>,
    status: u16,
    content_type: &str,
    body: &str,
) -> std::io::Result<()> {
    let status_text = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "Internal Server Error",
    };
    let header = format!(
        "HTTP/1.1 {status} {status_text}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    buf.write_all(header.as_bytes()).await?;
    buf.write_all(body.as_bytes()).await?;
    buf.flush().await?;
    Ok(())
}
```

**3c. Update `src/network/dashboard/mod.rs`** — add the new submodules and re-exports:

```rust
//! In-process HTTP dashboard for FerrumKV's live internals.
//!
//! Spawns alongside the RESP2 listener in `run_listener`'s `block_on`,
//! sharing the same tokio runtime and `KvEngine`. See
//! `docs/superpowers/specs/2026-07-06-dashboard-v0.5.0-design.md`.

pub mod http;
pub mod sse;
pub mod tap;

pub use tap::{Direction, Resp2Tap, TapEntry};
pub use http::handle_http;
```

**3d. Create a stub `src/network/dashboard/sse.rs`** so `http.rs` compiles. Task 7 fills it in:

```rust
//! SSE stream for the dashboard. Stub — filled in by Task 7.

use std::sync::Arc;

use tokio::net::TcpStream;

use ferrum_kv::storage::engine::KvEngine;

use super::Resp2Tap;

/// Placeholder — Task 7 replaces this with the real 2Hz SSE loop.
pub async fn stream(_stream: TcpStream, _engine: KvEngine, _tap: Arc<Resp2Tap>) {
    // No-op stub. Task 7 implements the real SSE loop.
}

/// Placeholder — Task 7 replaces this with the real JSON payload builder.
/// For now, return a minimal valid JSON so `/metrics` tests pass.
pub fn build_payload(_engine: &KvEngine, _tap: &Resp2Tap) -> String {
    "{}".to_string()
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --test dashboard_test 2>&1 | tail -30`
Expected: PASS — all 4 tests green (the `/metrics` test only checks the JSON is `{...}`, and the stub returns `{}`).

Full gate:
```bash
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo check && cargo test
```
Expected: all green.

- [ ] **Step 5: Commit**

```bash
git add src/network/dashboard/http.rs src/network/dashboard/index.html src/network/dashboard/mod.rs src/network/dashboard/sse.rs tests/dashboard_test.rs
git commit -m "feat(dashboard): add HTTP routing + HTML skeleton (FERRUM-012)"
```

---

## Task 7: SSE stream + JSON payload builder

**Files:**
- Modify: `src/network/dashboard/sse.rs` (stub from Task 6)

**Interfaces:**
- Consumes: `KvEngine` (counters, `ahe_snapshot`, `keyspace_stats`, `eviction_log_snapshot`, `keyspace_distribution_sampled`, `used_memory`, `eviction_config`, `uptime_secs`, `commands`), `Resp2Tap` (Task 2)
- Produces: `pub async fn stream(stream: TcpStream, engine: KvEngine, tap: Arc<Resp2Tap>)`, `pub fn build_payload(engine: &KvEngine, tap: &Resp2Tap) -> String`

**Reference: read `src/storage/engine/mod.rs` lines 966-992 (`ahe_snapshot`, `ahe_alpha`) for the exact return type of `ahe_snapshot`.**

- [ ] **Step 1: Write the failing tests**

Add to `tests/dashboard_test.rs` (created in Task 6):

```rust
// In tests/dashboard_test.rs

#[tokio::test]
async fn sse_stream_emits_metrics_frame() {
    // Spawn a dashboard listener and connect an SSE client.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let engine = KvEngine::new();
    // Seed some data so the payload is non-trivial.
    engine.set(b"sse-test".to_vec(), b"value".to_vec()).unwrap();
    let tap = Arc::new(Resp2Tap::default());
    let tap_for_loop = tap.clone();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        handle_http(stream, engine, tap_for_loop).await;
    });

    // Connect and send GET /events.
    let mut stream = tokio::net::TcpStream::connect(format!("127.0.0.1:{port}"))
        .await
        .unwrap();
    stream
        .write_all(b"GET /events HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .await
        .unwrap();

    // Read until we see the first SSE data frame (or timeout).
    let mut buf = vec![0u8; 4096];
    let n = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        stream.read(&mut buf),
    )
    .await
    .expect("timed out waiting for SSE frame")
    .unwrap();

    let resp = String::from_utf8_lossy(&buf[..n]);
    // Should contain the SSE event header and a JSON payload.
    assert!(resp.contains("text/event-stream"), "expected SSE Content-Type: {}", resp.lines().next().unwrap_or(""));
    assert!(resp.contains("event: metrics"), "expected 'event: metrics' frame");
    assert!(resp.contains("data: "), "expected 'data: ' line");
    // The data should be valid JSON (extract after "data: ").
    let data_line = resp.lines().find(|l| l.starts_with("data: ")).unwrap();
    let json = &data_line["data: ".len()..];
    assert!(json.starts_with('{'), "expected JSON object");
    // Should contain the keys we seeded.
    assert!(json.contains("\"uptime_secs\""), "expected uptime_secs field");
    assert!(json.contains("\"memory\""), "expected memory field");
}

#[test]
fn build_payload_is_valid_json_with_all_fields() {
    // Unit test on build_payload directly (no SSE connection needed).
    let engine = KvEngine::new();
    let tap = Resp2Tap::default();
    let payload = ferrum_kv::network::dashboard::sse::build_payload(&engine, &tap);
    // Should be valid JSON. We use a minimal check here — a full serde_json
    // parse is ideal but requires the dev-dependency. For now, structural checks.
    assert!(payload.starts_with('{'));
    assert!(payload.contains("\"version\""));
    assert!(payload.contains("\"uptime_secs\""));
    assert!(payload.contains("\"memory\""));
    assert!(payload.contains("\"hit_rate\""));
    assert!(payload.contains("\"evictions\""));
    assert!(payload.contains("\"ops_per_sec\""));
    assert!(payload.contains("\"ahe\""));
    assert!(payload.contains("\"policy\""));
    assert!(payload.contains("\"eviction_log\""));
    assert!(payload.contains("\"keyspace\""));
    assert!(payload.contains("\"resp2_stream\""));
    assert!(payload.ends_with('}'));
}

#[test]
fn build_payload_base64_encodes_binary_keys_in_eviction_log() {
    // When an eviction log event has a binary key, the JSON should base64-encode
    // it, not emit raw bytes that would break JSON parsing.
    let engine = KvEngine::new();
    // Force an eviction with a binary key (set maxmemory low, insert binary key, insert big key).
    use ferrum_kv::storage::eviction::{EvictionConfig, EvictionPolicy};
    engine.set_eviction_config(EvictionConfig {
        max_memory: 64,
        policy: EvictionPolicy::AllKeysLru,
        samples: 5,
    }).unwrap();
    let binary_key = vec![0x00, 0xff, 0x0d, 0x0a, 0xfe];
    engine.set(binary_key.clone(), b"v".to_vec()).unwrap();
    engine.set(b"big".to_vec(), vec![b'y'; 128]).unwrap();

    let tap = Resp2Tap::default();
    let payload = ferrum_kv::network::dashboard::sse::build_payload(&engine, &tap);
    // The payload should still be valid JSON — the binary key is base64'd.
    assert!(payload.contains("\"key_b64\""), "expected key_b64 field in eviction_log entries");
    // Should NOT contain raw \x00 or other control chars that break JSON.
    // (A crude check: no raw Nul bytes in the payload string.)
    assert!(!payload.contains('\0'), "payload should not contain raw Nul");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --test dashboard_test sse_stream_emits 2>&1 | head -20`
Expected: FAIL — the stub `stream` is a no-op, so the SSE test times out; the stub `build_payload` returns `{}`, missing all fields.

- [ ] **Step 3: Implement the SSE stream + payload builder**

Replace `src/network/dashboard/sse.rs` (stub from Task 6) with the full implementation:

```rust
//! SSE stream for the dashboard — pushes live metrics to the browser at 2Hz.
//!
//! On enter, activates the `Resp2Tap` (so RESP2 wire bytes get recorded).
//! On exit (stream closed or error), deactivates it. Each 500ms tick
//! assembles a combined JSON payload and writes it as an SSE frame:
//! `event: metrics\ndata: {json}\n\n`.

use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncWriteExt, BufStream};
use tokio::net::TcpStream;
use tokio::time::interval;

use ferrum_kv::storage::engine::KvEngine;

use super::Resp2Tap;

/// The SSE tick interval — 500ms = 2Hz per spec §7.
const TICK_INTERVAL: Duration = Duration::from_millis(500);

/// Push live metrics to the browser via SSE. Runs until the client
/// disconnects or an IO error occurs. Activates the tap on entry,
/// deactivates on exit.
pub async fn stream(stream: TcpStream, engine: KvEngine, tap: Arc<Resp2Tap>) {
    tap.activate();
    let mut buf = BufStream::new(stream);

    // Write the SSE headers.
    let header = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: keep-alive\r\n\r\n";
    if buf.write_all(header.as_bytes()).await.is_err() {
        tap.deactivate();
        return;
    }
    if buf.flush().await.is_err() {
        tap.deactivate();
        return;
    }

    let mut ticker = interval(TICK_INTERVAL);
    // The first tick fires immediately; skip it so we don't send a frame
    // before the client is ready.
    ticker.tick().await;

    loop {
        ticker.tick().await;
        let payload = build_payload(&engine, &tap);
        let frame = format!("event: metrics\ndata: {payload}\n\n");
        if buf.write_all(frame.as_bytes()).await.is_err() {
            break; // client gone
        }
        if buf.flush().await.is_err() {
            break;
        }
    }

    tap.deactivate();
}

/// Assemble the combined JSON payload for one SSE tick (or a `/metrics` one-shot).
/// Hand-rolled JSON serialization — no `serde_json` dependency per design spec.
/// All binary fields (keys, wire bytes) are base64-encoded to keep the JSON
/// valid UTF-8 regardless of `Nul`/non-UTF-8 content.
pub fn build_payload(engine: &KvEngine, tap: &Resp2Tap) -> String {
    let mut out = String::with_capacity(2048);
    out.push('{');

    // version
    out.push_str("\"version\":\"");
    out.push_str(env!("CARGO_PKG_VERSION"));
    out.push_str("\",");

    // uptime_secs
    out.push_str("\"uptime_secs\":");
    out.push_str(&engine.uptime_secs().to_string());
    out.push(',');

    // memory
    let used = engine.used_memory();
    let max = engine.eviction_config().map(|c| c.max_memory).unwrap_or(0);
    out.push_str("\"memory\":{\"used\":");
    out.push_str(&used.to_string());
    out.push_str(",\"max\":");
    out.push_str(&max.to_string());
    out.push_str("},");

    // hit_rate
    let (hits, misses) = engine.keyspace_stats();
    out.push_str("\"hit_rate\":{\"hits\":");
    out.push_str(&hits.to_string());
    out.push_str(",\"misses\":");
    out.push_str(&misses.to_string());
    out.push_str("},");

    // evictions
    out.push_str("\"evictions\":");
    out.push_str(&engine.evictions().to_string());
    out.push(',');

    // ops_per_sec (lifetime average = total_commands / uptime_secs)
    let cmds = engine.commands();
    let uptime = engine.uptime_secs().max(1); // avoid div by zero
    out.push_str("\"ops_per_sec\":");
    // One decimal place is enough; format as f64.
    out.push_str(&format!("{:.1}", cmds as f64 / uptime as f64));
    out.push(',');

    // ahe (alpha + last_hit_ratio)
    let ahe = engine.ahe_snapshot();
    out.push_str("\"ahe\":{\"alpha\":");
    out.push_str(&format!("{:.3}", ahe.alpha));
    out.push_str(",\"last_hit_ratio\":");
    out.push_str(&format!("{:.3}", ahe.last_hit_ratio));
    out.push_str("},");

    // policy
    let policy = engine
        .eviction_config()
        .map(|c| c.policy.name())
        .unwrap_or("noeviction");
    out.push_str("\"policy\":\"");
    out.push_str(policy);
    out.push_str("\",");

    // eviction_log (array of events, keys base64'd)
    out.push_str("\"eviction_log\":[");
    let log = engine.eviction_log_snapshot();
    for (i, evt) in log.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str("{\"key_b64\":\"");
        out.push_str(&base64_encode(&evt.key));
        out.push_str("\",\"policy\":\"");
        out.push_str(evt.policy);
        out.push_str("\",\"ttl_remaining_ms\":");
        match evt.ttl_remaining_ms {
            Some(ms) => out.push_str(&ms.to_string()),
            None => out.push_str("null"),
        }
        out.push_str(",\"ts\":");
        out.push_str(&evt.ts.to_string());
        out.push('}');
    }
    out.push_str("],");

    // keyspace distribution
    let dist = engine.keyspace_distribution_sampled(200);
    out.push_str("\"keyspace\":{\"ttl_buckets\":[");
    for (i, b) in dist.ttl_buckets.iter().enumerate() {
        if i > 0 { out.push(','); }
        out.push_str(&b.to_string());
    }
    out.push_str("],\"size_buckets\":[");
    for (i, b) in dist.size_buckets.iter().enumerate() {
        if i > 0 { out.push(','); }
        out.push_str(&b.to_string());
    }
    out.push_str("],\"lfu_heat\":[");
    for (i, b) in dist.lfu_heat.iter().enumerate() {
        if i > 0 { out.push(','); }
        out.push_str(&b.to_string());
    }
    out.push_str("]},");

    // resp2_stream (tap snapshot, bytes base64'd)
    out.push_str("\"resp2_stream\":[");
    let tap_snap = tap.snapshot();
    for (i, entry) in tap_snap.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str("{\"dir\":\"");
        out.push_str(match entry.dir {
            super::Direction::Request => "req",
            super::Direction::Response => "res",
        });
        out.push_str("\",\"bytes_b64\":\"");
        out.push_str(&base64_encode(&entry.bytes));
        out.push_str("\"}");
    }
    out.push_str("]");

    out.push('}');
    out
}

/// Base64-encode bytes using the standard alphabet. No padding for simplicity
/// (the frontend can handle missing padding, and this avoids `=` in JSON strings
/// that could confuse naive parsers). Std-only, no new crate.
fn base64_encode(input: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((input.len() * 4 + 2) / 3);
    let mut chunks = input.chunks_exact(3);
    for chunk in &mut chunks {
        let b = ((chunk[0] as u32) << 16) | ((chunk[1] as u32) << 8) | chunk[2] as u32;
        out.push(ALPHABET[(b >> 18 & 63) as usize] as char);
        out.push(ALPHABET[(b >> 12 & 63) as usize] as char);
        out.push(ALPHABET[(b >> 6 & 63) as usize] as char);
        out.push(ALPHABET[(b & 63) as usize] as char);
    }
    let rem = chunks.remainder();
    match rem.len() {
        1 => {
            let b = (rem[0] as u32) << 16;
            out.push(ALPHABET[(b >> 18 & 63) as usize] as char);
            out.push(ALPHABET[(b >> 12 & 63) as usize] as char);
        }
        2 => {
            let b = ((rem[0] as u32) << 16) | ((rem[1] as u32) << 8);
            out.push(ALPHABET[(b >> 18 & 63) as usize] as char);
            out.push(ALPHABET[(b >> 12 & 63) as usize] as char);
            out.push(ALPHABET[(b >> 6 & 63) as usize] as char);
        }
        _ => {}
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_encodes_empty() {
        assert_eq!(base64_encode(b""), "");
    }

    #[test]
    fn base64_encodes_three_bytes() {
        assert_eq!(base64_encode(b"abc"), "YWJj");
    }

    #[test]
    fn base64_encodes_one_byte() {
        assert_eq!(base64_encode(b"a"), "YQ");
    }

    #[test]
    fn base64_encodes_two_bytes() {
        assert_eq!(base64_encode(b"ab"), "YWI");
    }

    #[test]
    fn base64_encodes_binary_with_nul() {
        // \x00\x01\x02 -> "AAEC"
        assert_eq!(base64_encode(&[0x00, 0x01, 0x02]), "AAEC");
    }
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --test dashboard_test 2>&1 | tail -30`
Expected: PASS — SSE test gets a frame within 2s, `build_payload` has all fields, binary key is base64'd.

Also run the unit tests:
```bash
cargo test --lib network::dashboard::sse 2>&1 | tail -10
```
Expected: PASS — 5 base64 tests green.

Full gate:
```bash
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo check && cargo test
```
Expected: all green.

- [ ] **Step 5: Commit**

```bash
git add src/network/dashboard/sse.rs tests/dashboard_test.rs
git commit -m "feat(dashboard): implement SSE stream + JSON payload builder (FERRUM-012)"
```

---

## Task 8: `serve` accept loop + `main.rs` wiring

**Files:**
- Create: `src/network/dashboard/serve.rs`
- Modify: `src/network/dashboard/mod.rs` (add `pub mod serve; pub use serve::serve;`)
- Modify: `src/network/server.rs` (`run_listener` signature + spawn dashboard, `accept_loop` + `handle_client` tap, `execute_command` tick)
- Modify: `src/main.rs` (bind dashboard listener, emit `0.0.0.0` warning, pass into `run_listener`)

**Interfaces:**
- Consumes: `handle_http` (Task 6), `Resp2Tap` (Task 2), `Shutdown` (existing), `KvEngine` (Task 3), `CliArgs::dashboard_addr` (Task 5)
- Produces: `pub async fn serve(listener: TcpListener, engine: KvEngine, tap: Arc<Resp2Tap>, shutdown: Shutdown) -> Result<(), FerrumError>`

**Reference: read `src/network/server.rs` lines 499-604 (`start`, `run_listener`, `accept_loop`) and `src/network/shutdown.rs` for the `Shutdown::notified()` API before editing.**

- [ ] **Step 1: Write the failing test**

Add to `tests/dashboard_test.rs`:

```rust
// In tests/dashboard_test.rs

#[tokio::test]
async fn serve_accepts_connections_until_shutdown() {
    use ferrum_kv::network::dashboard::serve;
    use ferrum_kv::network::shutdown::Shutdown;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let engine = KvEngine::new();
    let tap = Arc::new(Resp2Tap::default());
    let shutdown = Shutdown::new();

    let serve_task = tokio::spawn(async move {
        serve(listener, engine, tap, shutdown.clone()).await
    });

    // Connect one client — should succeed.
    let stream = tokio::net::TcpStream::connect(format!("127.0.0.1:{port}"))
        .await
        .expect("should connect while serve is running");
    drop(stream); // disconnect

    // Trigger shutdown — serve should exit.
    shutdown.trigger();
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        serve_task,
    )
    .await
    .expect("serve should exit within 2s of shutdown")
    .expect("serve task should not panic");
    assert!(result.is_ok(), "serve should return Ok on shutdown");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --test dashboard_test serve_accepts 2>&1 | head -20`
Expected: FAIL with "unresolved import `ferrum_kv::network::dashboard::serve`" — `serve.rs` doesn't exist yet.

- [ ] **Step 3: Implement `serve.rs` + wire into `server.rs` + `main.rs`**

**3a. Create `src/network/dashboard/serve.rs`**:

```rust
//! Dashboard HTTP accept loop.
//!
//! Mirrors `server::accept_loop`: `tokio::select!` between `shutdown.notified()`
//! and `listener.accept()`. Each connection spawns `handle_http`. The `Resp2Tap`
//! is cloned into each connection so the SSE endpoint can activate/deactivate it.

use std::sync::Arc;

use tokio::net::TcpListener;

use ferrum_kv::network::shutdown::Shutdown;
use ferrum_kv::storage::engine::KvEngine;

use super::Resp2Tap;
use super::handle_http;

/// Accept dashboard HTTP connections until `shutdown` is triggered.
/// Each connection is spawned to `handle_http`. Errors are logged but
/// never abort the loop — the dashboard is non-critical.
pub async fn serve(
    listener: TcpListener,
    engine: KvEngine,
    tap: Arc<Resp2Tap>,
    shutdown: Shutdown,
) -> Result<(), FerrumError> {
    loop {
        tokio::select! {
            biased;
            _ = shutdown.notified() => break,
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _)) => {
                        let engine = engine.clone();
                        let tap = tap.clone();
                        tokio::spawn(async move {
                            handle_http(stream, engine, tap).await;
                        });
                    }
                    Err(e) => {
                        log::error!("dashboard accept error: {e}");
                    }
                }
            }
        }
    }
    log::info!("dashboard serve loop exiting");
    Ok(())
}
```

**3b. Update `src/network/dashboard/mod.rs`** — add serve:

```rust
pub mod http;
pub mod serve;
pub mod sse;
pub mod tap;

pub use tap::{Direction, Resp2Tap, TapEntry};
pub use http::handle_http;
pub use serve::serve;
```

**3c. Modify `src/network/server.rs` — `run_listener`** to accept an optional dashboard listener and spawn `serve`. Read the existing `run_listener` (lines 518-546) first.

Change the signature:
```rust
pub fn run_listener(
    listener: StdTcpListener,
    dashboard_listener: Option<StdTcpListener>,  // NEW
    engine: KvEngine,
    tap: Arc<Resp2Tap>,                          // NEW — shared between RESP2 and dashboard
    shutdown: Shutdown,
    config: ServerConfig,
) -> Result<(), FerrumError> {
    listener.set_nonblocking(true)?;

    let mut builder = Builder::new_multi_thread();
    builder.enable_all().thread_name("ferrum-worker");
    if config.worker_threads > 0 {
        builder.worker_threads(config.worker_threads);
    }
    let runtime = builder
        .build()
        .map_err(|e| FerrumError::Internal(format!("failed to build tokio runtime: {e}")))?;

    let result = runtime.block_on(async move {
        let listener = TcpListener::from_std(listener)
            .map_err(|e| FerrumError::Internal(format!("TcpListener::from_std failed: {e}")))?;

        // Spawn the dashboard listener if provided.
        if let Some(dl) = dashboard_listener {
            dl.set_nonblocking(true)?;
            let dl = TcpListener::from_std(dl)
                .map_err(|e| FerrumError::Internal(format!("dashboard TcpListener::from_std failed: {e}")))?;
            let engine = engine.clone();
            let tap = tap.clone();
            let shutdown = shutdown.clone();
            tokio::spawn(async move {
                if let Err(e) = dashboard::serve(dl, engine, tap, shutdown).await {
                    log::error!("dashboard serve error: {e}");
                }
            });
        }

        accept_loop(listener, engine, tap, shutdown, config).await
    });

    runtime.shutdown_timeout(Duration::from_millis(500));
    result
}
```

**3d. Modify `accept_loop`** to thread the `tap` into each `handle_client`:
```rust
// Change signature to accept tap:
async fn accept_loop(
    listener: TcpListener,
    engine: KvEngine,
    tap: Arc<Resp2Tap>,          // NEW
    shutdown: Shutdown,
    config: ServerConfig,
) -> Result<(), FerrumError> {
    // ... inside the spawn block:
                    let engine = engine.clone();
                    let tap = tap.clone();          // NEW
                    let shutdown = shutdown.clone();
                    let config = config.clone();
                    tokio::spawn(async move {
                        if let Err(e) = handle_client(stream, engine, tap, shutdown, config).await {
                            error!("client handler error: {e}");
                        }
                        drop(guard);
                    });
```

**3e. Modify `handle_client`** to accept `tap: Arc<Resp2Tap>` and call `tap.push` after read/write. Read the existing `handle_client` (lines 137-227) first to find the exact read/write sites.

```rust
// Change signature:
async fn handle_client(
    stream: TcpStream,
    engine: KvEngine,
    tap: Arc<Resp2Tap>,          // NEW
    shutdown: Shutdown,
    config: ServerConfig,
) -> Result<(), FerrumError> {
    // ... after each successful read that produces a frame:
                    tap.push(Direction::Request, &read_buf[..consumed]);
    // ... after each successful write:
                    tap.push(Direction::Response, &out);
    // ...
}
```

**The exact placement depends on the existing handle_client structure** — the implementer must read the function and find where `read_buf` is filled (after a successful `read_with_optional_timeout`) and where `out` is written (after `execute_command` and `stream.write_all`). Add the `tap.push` call immediately after each successful IO, gated by the tap's internal `active` flag (which is the no-op when inactive).

**3f. Modify `execute_command`** to tick the command counter. Read the existing function (line 272) — it's a free function `fn execute_command(cmd, engine: &KvEngine, out)`.

```rust
pub fn execute_command(cmd: Command, engine: &KvEngine, out: &mut Vec<u8>) {
    engine.tick_command();   // NEW — first line, ticks the counter for every command
    match cmd {
        // ... existing body unchanged ...
```

**3g. Modify `src/main.rs`** to bind the dashboard listener, emit the `0.0.0.0` warning, and pass it into `run_listener`. Read `main.rs` lines 24-122 first.

After the RESP2 listener bind (line ~71) and before `run_listener`, add:

```rust
    // Dashboard listener — optional, bind failure is non-fatal.
    let dashboard_addr = &args.dashboard_addr;
    let dashboard_listener: Option<StdTcpListener> = match StdTcpListener::bind(dashboard_addr) {
        Ok(l) => {
            // Spec §7: warn if bound to a public address.
            let local = l.local_addr().unwrap();
            if local.ip().is_unspecified() {
                warn!("Dashboard bound to {} — no authentication is enforced", local);
            }
            info!("Dashboard listening on {local}");
            Some(l)
        }
        Err(e) => {
            // Non-fatal: dashboard is a dev convenience, not critical.
            warn!("failed to bind dashboard on {dashboard_addr}: {e} (dashboard disabled)");
            None
        }
    };

    // The tap is shared between the RESP2 server and the dashboard.
    let tap = Arc::new(Resp2Tap::default());
```

Then update the `run_listener` call:
```rust
    let server_result = ferrum_kv::network::server::run_listener(
        listener,
        dashboard_listener,          // NEW
        engine,
        tap,                          // NEW
        shutdown,
        server_config,
    );
```

Add the imports at the top of `main.rs`:
```rust
use ferrum_kv::network::dashboard::Resp2Tap;
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --test dashboard_test 2>&1 | tail -30`
Expected: PASS — serve test green.

Also run the full suite to catch any regression from the `handle_client`/`execute_command`/`run_listener` signature changes:
```bash
cargo test 2>&1 | tail -30
```
Expected: all green — the tap is inactive by default so existing tests see no behavior change.

Full gate:
```bash
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo check && cargo test
```
Expected: all green.

- [ ] **Step 5: Commit**

```bash
git add src/network/dashboard/serve.rs src/network/dashboard/mod.rs src/network/server.rs src/main.rs tests/dashboard_test.rs
git commit -m "feat(dashboard): wire serve into run_listener + RESP2 tap (FERRUM-012)"
```

---

## Task 9: Frontend — full UI consuming SSE

**Files:**
- Modify: `src/network/dashboard/index.html` (replace skeleton from Task 6 with full UI)

**Interfaces:**
- Consumes: SSE payload schema from Task 7 (`build_payload` JSON: `version`, `uptime_secs`, `memory`, `hit_rate`, `evictions`, `ops_per_sec`, `ahe`, `policy`, `eviction_log`, `keyspace`, `resp2_stream`)
- Produces: visible dashboard UI (cards, AHE gauge, eviction log, RESP2 stream, keyspace distribution, tooltips)

**No tests** — frontend is vanilla JS in an HTML file, not covered by Rust integration tests. Verification is manual (open `http://localhost:6381` in a browser). The implementer should `cargo run` and visually confirm the UI renders, but no automated test is added. This matches the project convention: wire-protocol behavior is tested, UI rendering is not.

- [ ] **Step 1: Read the payload schema**

Read `src/network/dashboard/sse.rs` `build_payload` to confirm the exact JSON field names and shapes the frontend must consume. The schema is fixed by Task 7; the frontend must match it verbatim.

- [ ] **Step 2: Replace `index.html` with the full UI**

The full `index.html` implements:

1. **Top bar**: version, policy, memory cap, ops/s, uptime, connection dot
2. **Four metric cards** (memory, hit rate, evictions, ops/s) with primary value + context + tooltip `[?]` icons
3. **AHE adaptive alpha gauge** (only visible when `policy` contains "ahe") — a slider showing `ahe.alpha`, with tooltip explaining α
4. **Eviction log** — scrolling list of recent evictions (key decoded from base64, policy, TTL remaining)
5. **RESP2 stream** — scrolling list of recent wire bytes (decoded from base64, with →/← direction arrows)
6. **Keyspace distribution** — TTL bucket bars, size bucket bars, LFU heat bars
7. **SSE consumption** via `EventSource('/events')`, updating the DOM on each `metrics` event
8. **Tooltip system** — `[?]` icons that reveal explanatory text on hover (CSS `:hover` reveal, no JS needed for basic tooltips)
9. **Navigation bar** — `[📊 Dashboard]` active, `[🧪 Sandbox →]` and `[🎬 Animations →]` grayed out with "coming soon"

The CSS follows the design system from spec §2.1: dark theme (GitHub-dark palette: `#0d1117` bg, `#161b22` card bg, `#58a6ff` blue accent, `#a371f7` AHE purple, `#3fb950` green), system monospace for data, system sans-serif for labels, 8px grid spacing.

The JS uses `EventSource` to connect to `/events`, parses each `data:` payload as JSON, and updates the DOM. Base64 decoding is done client-side via `atob`. No frameworks, no build step.

```html
<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>FerrumKV Dashboard</title>
  <style>
    :root { color-scheme: dark; }
    * { box-sizing: border-box; }
    body {
      font-family: system-ui, -apple-system, sans-serif;
      background: #0d1117; color: #c9d1d9; margin: 0; padding: 24px;
      max-width: 1200px; margin: 0 auto;
    }
    h1 { font-family: ui-monospace, 'Cascadia Code', monospace; color: #58a6ff; font-size: 1.5rem; margin: 0 0 16px 0; }
    h2 { font-size: 1rem; color: #8b949e; margin: 24px 0 8px 0; text-transform: uppercase; letter-spacing: 0.5px; }
    .topbar { display: flex; gap: 16px; align-items: center; flex-wrap: wrap; margin-bottom: 24px; }
    .topbar span { font-family: ui-monospace, monospace; font-size: 0.875rem; }
    .dot { width: 8px; height: 8px; border-radius: 50%; background: #3fb950; display: inline-block; }
    .dot.off { background: #f85149; }
    .cards { display: grid; grid-template-columns: repeat(4, 1fr); gap: 16px; margin-bottom: 24px; }
    @media (max-width: 1024px) { .cards { grid-template-columns: repeat(2, 1fr); } }
    .card { background: #161b22; border: 1px solid #30363d; border-radius: 8px; padding: 16px; }
    .card .label { font-size: 0.75rem; color: #8b949e; text-transform: uppercase; letter-spacing: 0.5px; }
    .card .value { font-family: ui-monospace, monospace; font-size: 1.5rem; color: #f0f6fc; margin: 8px 0; }
    .card .context { font-size: 0.75rem; color: #8b949e; }
    .card .bar { height: 8px; background: #21262d; border-radius: 4px; overflow: hidden; margin-top: 8px; }
    .card .bar .fill { height: 100%; background: #58a6ff; transition: width 0.5s; }
    .tooltip { position: relative; cursor: help; }
    .tooltip .text { display: none; position: absolute; bottom: 100%; left: 0; background: #30363d; color: #f0f6fc; padding: 8px; border-radius: 4px; font-size: 0.75rem; white-space: pre-wrap; width: 240px; z-index: 10; }
    .tooltip:hover .text { display: block; }
    .ahe-gauge { background: #161b22; border: 1px solid #30363d; border-radius: 8px; padding: 16px; margin-bottom: 24px; }
    .ahe-gauge .slider { height: 8px; background: #21262d; border-radius: 4px; position: relative; margin: 16px 0; }
    .ahe-gauge .slider .knob { position: absolute; width: 16px; height: 16px; background: #a371f7; border-radius: 50%; top: -4px; transition: left 0.5s; }
    .feeds { display: grid; grid-template-columns: 1fr 1fr; gap: 16px; margin-bottom: 24px; }
    @media (max-width: 1024px) { .feeds { grid-template-columns: 1fr; } }
    .feed { background: #161b22; border: 1px solid #30363d; border-radius: 8px; padding: 16px; max-height: 300px; overflow-y: auto; }
    .feed .entry { font-family: ui-monospace, monospace; font-size: 0.75rem; padding: 4px 0; border-bottom: 1px solid #21262d; }
    .feed .entry:last-child { border-bottom: none; }
    .feed .arrow { color: #58a6ff; }
    .keyspace { background: #161b22; border: 1px solid #30363d; border-radius: 8px; padding: 16px; margin-bottom: 24px; }
    .keyspace .buckets { display: flex; align-items: flex-end; gap: 8px; height: 60px; }
    .keyspace .bucket { flex: 1; background: #58a6ff; border-radius: 2px 2px 0 0; min-height: 2px; transition: height 0.5s; }
    .keyspace .bucket-label { font-size: 0.625rem; color: #8b949e; text-align: center; margin-top: 4px; }
    .nav { display: flex; gap: 16px; padding: 16px 0; border-top: 1px solid #30363d; margin-top: 24px; }
    .nav a { color: #58a6ff; text-decoration: none; font-size: 0.875rem; }
    .nav .disabled { color: #6e7681; cursor: not-allowed; }
    .hidden { display: none; }
  </style>
</head>
<body>
  <h1>🦀 FerrumKV Dashboard</h1>

  <div class="topbar">
    <span id="version">v—</span>
    <span id="policy">policy: —</span>
    <span id="memory-cap">cap: —</span>
    <span id="ops">— ops/s</span>
    <span id="uptime">uptime: —</span>
    <span><span class="dot" id="conn-dot"></span> <span id="conn-status">connecting</span></span>
  </div>

  <div class="cards">
    <div class="card">
      <div class="label">Memory <span class="tooltip">[?]<span class="text">Bytes used by the live dataset, including per-entry overhead. The bar shows usage against the configured maxmemory cap.</span></span></div>
      <div class="value" id="mem-value">—</div>
      <div class="context" id="mem-context">/ —</div>
      <div class="bar"><div class="fill" id="mem-bar" style="width: 0%"></div></div>
    </div>
    <div class="card">
      <div class="label">Hit Rate <span class="tooltip">[?]<span class="text">hits / (hits + misses) over the lifetime of the server. A "hit" means a key was found and was not expired. A "miss" means the key didn't exist or had already expired. Higher is better. 100% means every read found its key.</span></span></div>
      <div class="value" id="hit-value">—</div>
      <div class="context" id="hit-context">hits: — misses: —</div>
    </div>
    <div class="card">
      <div class="label">Evictions <span class="tooltip">[?]<span class="text">Total keys removed by the eviction policy since server start. An "eviction" happens when maxmemory is reached and the policy picks a victim to make room. This is distinct from keys expiring via TTL.</span></span></div>
      <div class="value" id="evict-value">—</div>
      <div class="context">total</div>
    </div>
    <div class="card">
      <div class="label">Ops/sec <span class="tooltip">[?]<span class="text">Lifetime average: total commands processed / uptime in seconds. Includes invalid commands (an op is an op). Rolling-window sparkline arrives in v0.5.1.</span></span></div>
      <div class="value" id="ops-value">—</div>
      <div class="context">lifetime avg</div>
    </div>
  </div>

  <div class="ahe-gauge hidden" id="ahe-gauge">
    <div class="label">🧠 AHE Adaptive Alpha <span class="tooltip">[?]<span class="text">α controls the blend between recency and frequency in eviction decisions. α = 1.0 means pure recency (like LRU). α = 0.0 means pure frequency (like LFU). AHE self-tunes α based on the observed hit ratio via the feedback loop in AdaptiveHybridState::observe. A rising hit ratio → α moves toward frequency. A falling hit ratio → α moves toward recency.</span></span></div>
    <div class="slider"><div class="knob" id="ahe-knob" style="left: 62%"></div></div>
    <div style="display: flex; justify-content: space-between; font-size: 0.75rem; color: #8b949e;">
      <span>← Frequency matters more</span>
      <span id="ahe-value">α = —</span>
      <span>Recency matters more →</span>
    </div>
  </div>

  <div class="feeds">
    <div class="feed">
      <div class="label">📋 Eviction Log <span class="tooltip">[?]<span class="text">Each entry shows a key that was evicted, the policy that made the decision, and how much TTL the key had remaining. EPS breakdown arrives in v0.5.1.</span></span></div>
      <div id="eviction-log"></div>
    </div>
    <div class="feed">
      <div class="label">⚡ RESP2 Stream <span class="tooltip">[?]<span class="text">Raw bytes flowing in (→ client request) and out (← server response) of the RESP2 wire protocol. Every Redis client speaks these bytes. → means the client sent this; ← means the server replied this.</span></span></div>
      <div id="resp2-stream"></div>
    </div>
  </div>

  <div class="keyspace">
    <div class="label">📊 Keyspace Distribution (sampled) <span class="tooltip">[?]<span class="text">Distribution of ~200 randomly sampled keys by TTL range, key size, and LFU counter heat. This explains why your eviction policy behaves a certain way: e.g. if most keys have no TTL, volatile-* policies are useless for your workload.</span></span></div>
    <h2>TTL Distribution</h2>
    <div class="buckets" id="ttl-buckets"></div>
    <h2>Key Size Distribution</h2>
    <div class="buckets" id="size-buckets"></div>
    <h2>LFU Heat</h2>
    <div class="buckets" id="lfu-heat"></div>
  </div>

  <div class="nav">
    <a href="/">📊 Dashboard</a>
    <a class="disabled" title="coming in v0.6.0">🧪 Sandbox →</a>
    <a class="disabled" title="coming in v0.5.1">🎬 Animations →</a>
  </div>

  <script>
    const es = new EventSource('/events');
    es.addEventListener('open', () => {
      document.getElementById('conn-dot').classList.remove('off');
      document.getElementById('conn-status').textContent = 'connected';
    });
    es.addEventListener('error', () => {
      document.getElementById('conn-dot').classList.add('off');
      document.getElementById('conn-status').textContent = 'disconnected';
    });
    es.addEventListener('metrics', (e) => {
      const d = JSON.parse(e.data);
      // Top bar
      document.getElementById('version').textContent = 'v' + d.version;
      document.getElementById('policy').textContent = 'policy: ' + d.policy;
      document.getElementById('memory-cap').textContent = 'cap: ' + formatBytes(d.memory.max);
      document.getElementById('ops').textContent = d.ops_per_sec.toFixed(1) + ' ops/s';
      document.getElementById('uptime').textContent = 'uptime: ' + formatUptime(d.uptime_secs);
      // Memory card
      document.getElementById('mem-value').textContent = formatBytes(d.memory.used);
      document.getElementById('mem-context').textContent = '/ ' + formatBytes(d.memory.max);
      const pct = d.memory.max > 0 ? (d.memory.used / d.memory.max * 100) : 0;
      document.getElementById('mem-bar').style.width = Math.min(pct, 100) + '%';
      // Hit rate card
      const total = d.hit_rate.hits + d.hit_rate.misses;
      const hr = total > 0 ? (d.hit_rate.hits / total * 100).toFixed(1) : '—';
      document.getElementById('hit-value').textContent = hr + (total > 0 ? '%' : '');
      document.getElementById('hit-context').textContent = 'hits: ' + d.hit_rate.hits + ' misses: ' + d.hit_rate.misses;
      // Evictions card
      document.getElementById('evict-value').textContent = d.evictions.toLocaleString();
      // Ops card
      document.getElementById('ops-value').textContent = d.ops_per_sec.toFixed(1);
      // AHE gauge
      if (d.policy.includes('ahe')) {
        document.getElementById('ahe-gauge').classList.remove('hidden');
        document.getElementById('ahe-knob').style.left = (d.ahe.alpha * 100) + '%';
        document.getElementById('ahe-value').textContent = 'α = ' + d.ahe.alpha.toFixed(2);
      } else {
        document.getElementById('ahe-gauge').classList.add('hidden');
      }
      // Eviction log
      const elog = d.eviction_log.map(e => {
        const key = atob(e.key_b64);
        const ttl = e.ttl_remaining_ms === null ? 'no TTL' : Math.round(e.ttl_remaining_ms / 1000) + 's';
        return '<div class="entry">key: "' + escapeHtml(key) + '" · policy: ' + e.policy + ' · TTL: ' + ttl + '</div>';
      }).join('');
      document.getElementById('eviction-log').innerHTML = elog || '<div class="entry" style="color:#6e7681">No evictions yet</div>';
      // RESP2 stream
      const stream = d.resp2_stream.map(e => {
        const bytes = atob(e.bytes_b64);
        const arrow = e.dir === 'req' ? '<span class="arrow">→</span>' : '<span class="arrow">←</span>';
        return '<div class="entry">' + arrow + ' ' + escapeHtml(bytes).replace(/\\r\\n/g, '\\r\\n') + '</div>';
      }).join('');
      document.getElementById('resp2-stream').innerHTML = stream || '<div class="entry" style="color:#6e7681">No traffic yet</div>';
      // Keyspace distributions
      renderBuckets('ttl-buckets', d.keyspace.ttl_buckets, ['≤1s','≤10s','≤1m','≤5m','≤1h','∞']);
      renderBuckets('size-buckets', d.keyspace.size_buckets, ['≤64B','≤256B','≤1KB','>1KB']);
      renderBuckets('lfu-heat', d.keyspace.lfu_heat, ['0-25','25-50','50-100','100-200','>200']);
    });

    function renderBuckets(id, buckets, labels) {
      const max = Math.max(...buckets, 1);
      const el = document.getElementById(id);
      el.innerHTML = buckets.map((b, i) => {
        const h = (b / max * 100);
        return '<div style="flex:1;text-align:center">' +
               '<div style="background:#58a6ff;height:' + h + '%;min-height:2px;border-radius:2px 2px 0 0"></div>' +
               '<div class="bucket-label">' + labels[i] + '</div>' +
               '</div>';
      }).join('');
    }
    function formatBytes(bytes) {
      if (bytes === 0) return '0 B';
      const k = 1024, sizes = ['B','KB','MB','GB'];
      const i = Math.floor(Math.log(bytes) / Math.log(k));
      return parseFloat((bytes / Math.pow(k, i)).toFixed(1)) + ' ' + sizes[i];
    }
    function formatUptime(secs) {
      const h = Math.floor(secs / 3600), m = Math.floor((secs % 3600) / 60), s = secs % 60;
      return h > 0 ? h + 'h ' + m + 'm' : m > 0 ? m + 'm ' + s + 's' : s + 's';
    }
    function escapeHtml(s) {
      return s.replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;').replace(/"/g,'&quot;');
    }
  </script>
</body>
</html>
```

- [ ] **Step 3: Verify manually**

Run the server and open the dashboard in a browser:
```bash
cargo run --release
```
Then open `http://localhost:6381` in a browser. Confirm:
- The four cards render with values (even if "0" / "—" before any SSE frame arrives)
- After ~500ms, the first SSE frame arrives and values populate
- The AHE gauge is hidden if policy is not AHE; visible if it is
- Tooltips appear on hover over `[?]` icons
- The nav bar shows "coming soon" for Sandbox and Animations

Then run `redis-benchmark -t set,get -n 100` in another terminal and confirm the dashboard updates live (ops/s rises, memory fills, hit rate computes, RESP2 stream shows bytes).

- [ ] **Step 4: Run the full gate**

```bash
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo check && cargo test
```
Expected: all green — the HTML file is `include_str!`'d, no Rust code changed, but the gate confirms no breakage.

- [ ] **Step 5: Commit**

```bash
git add src/network/dashboard/index.html
git commit -m "feat(dashboard): full frontend UI consuming SSE (FERRUM-012)"
```

---

## Task 10: Bench gate — tap inactive zero-overhead

**Files:**
- Modify: `benches/resp2_bench.rs` (add `dashboard_tap_inactive_zero_overhead` group)

**Interfaces:**
- Consumes: `Resp2Tap` (Task 2), existing `parse_frame`/`encode_*` bench patterns
- Produces: Criterion bench group asserting tap inactive adds <1% overhead

**Reference: read `benches/resp2_bench.rs` (101 lines) first to match its `criterion_group`/`criterion_main` structure and `bench_function` patterns.**

- [ ] **Step 1: Write the bench**

Add a new bench function and group to `benches/resp2_bench.rs`. The bench compares `parse_frame` throughput with and without a `Resp2Tap` push in the loop — but since `parse_frame` is the hot path and the tap would be in `handle_client` (not in parsing), we instead bench the **tap push itself** as a no-op vs a no-op baseline, confirming the inactive branch is effectively free.

```rust
// In benches/resp2_bench.rs — add at the end, before criterion_group!:

use ferrum_kv::network::dashboard::{Direction, Resp2Tap};
use std::sync::Arc;

fn bench_tap_inactive_push(c: &mut Criterion) {
    let tap = Arc::new(Resp2Tap::default());
    // Inactive (default). push should be a no-op: one atomic load + branch.
    c.bench_function("dashboard/tap/inactive_push", |b| {
        b.iter(|| {
            // Simulate the per-command tap push on the hot path.
            // Two pushes per command (request + response), matching handle_client.
            tap.push(Direction::Request, b"*3\r\n$3\r\nSET\r\n$4\r\nkey\r\n$5\r\nvalue\r\n");
            tap.push(Direction::Response, b"+OK\r\n");
        });
    });
}

fn bench_tap_active_push(c: &mut Criterion) {
    let tap = Arc::new(Resp2Tap::default());
    tap.activate();
    // Active — push takes the mutex and allocates. This is the overhead
    // the dashboard incurs when a client is connected. It's bounded by
    // the ring buffer (200 entries) and only runs when SSE is live.
    c.bench_function("dashboard/tap/active_push", |b| {
        b.iter(|| {
            tap.push(Direction::Request, b"*3\r\n$3\r\nSET\r\n$4\r\nkey\r\n$5\r\nvalue\r\n");
            tap.push(Direction::Response, b"+OK\r\n");
        });
    });
}

// Update criterion_group! to include the new benches:
// criterion_group!(benches, bench_parse_set, bench_parse_get, bench_encode_bulk, bench_encode_integer, bench_tap_inactive_push, bench_tap_active_push);
```

**Update the `criterion_group!` macro call** (near the bottom of the file) to include `bench_tap_inactive_push` and `bench_tap_active_push` in the group list. Read the existing call first to match its syntax.

- [ ] **Step 2: Run bench to verify it compiles and runs**

```bash
cargo bench --bench resp2_bench -- --quick dashboard/tap 2>&1 | tail -20
```
Expected: two bench results printed — `dashboard/tap/inactive_push` and `dashboard/tap/active_push`. The inactive push should be significantly faster (sub-nanosecond per iteration given the no-op branch) than the active push.

- [ ] **Step 3: Verify the overhead assertion**

The spec §9 success metric is "dashboard tap inactive → throughput unchanged within ±1%". This bench measures the tap push itself, not full RESP2 throughput. To assert the ±1% claim on real throughput, compare `resp2/parse/set` throughput with and without the tap push in the loop:

Add a comparative bench:
```rust
fn bench_parse_set_with_inactive_tap(c: &mut Criterion) {
    let tap = Arc::new(Resp2Tap::default());
    let mut group = c.benchmark_group("resp2/parse/set_with_tap");
    for &value_len in &[8usize, 64, 512, 4096] {
        let value = vec![b'x'; value_len];
        let frame = build_set_frame(b"some-key", &value);
        group.throughput(Throughput::Bytes(frame.len() as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(value_len),
            &frame,
            |b, frame| {
                b.iter(|| {
                    // Simulate handle_client's hot path: parse + tap push.
                    let parsed = parse_frame(black_box(frame)).unwrap();
                    assert!(matches!(parsed, FrameParse::Complete { .. }));
                    tap.push(Direction::Request, black_box(frame));
                });
            },
        );
    }
    group.finish();
}
```

Run both `resp2/parse/set` (existing, no tap) and `resp2/parse/set_with_tap` (new, with inactive tap) and compare:
```bash
cargo bench --bench resp2_bench -- --quick "resp2/parse/set" 2>&1 | tail -20
cargo bench --bench resp2_bench -- --quick "resp2/parse/set_with_tap" 2>&1 | tail -20
```
Expected: the `set_with_tap` throughput should be within ±1% of the `set` throughput at each `value_len`. If not, the tap push's atomic load + branch is not being predicted as "not taken" as expected, and the implementer must investigate (likely: move the `if` into an `#[inline(always)]` fn to help the compiler sink it).

**Note:** Criterion's `--quick` mode gives rough numbers; for the final assertion, run without `--quick` on a quiet machine. The implementer should eyeball the comparison and confirm the delta is <1%. This is not an automated test gate — it's a manual bench inspection per the project's bench conventions (`benches/` run in CI via `cargo bench --no-run`, full bench locally).

- [ ] **Step 4: Run the full gate**

```bash
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo check && cargo test && cargo bench --no-run
```
Expected: all green — `cargo bench --no-run` confirms the bench compiles without running it (CI mode).

- [ ] **Step 5: Commit**

```bash
git add benches/resp2_bench.rs
git commit -m "bench(dashboard): add tap inactive/active overhead benchmarks (FERRUM-012)"
```

---

## Self-Review

**1. Spec coverage:** Checked each section of the design spec (`docs/superpowers/specs/2026-07-06-dashboard-v0.5.0-design.md`):

| Spec section | Covered by |
|---|---|
| §2 Decision 1 (listener mount) | Task 8 (`run_listener` spawns `serve`) |
| §2 Decision 2 (CLI config flow) | Task 5 |
| §2 Decision 3 (keyspace sampling) | Task 3 (`keyspace_distribution_sampled`) |
| §2 Decision 4 (RESP2 tap shape) | Task 2 (`Resp2Tap`) + Task 8 (`handle_client` taps) |
| §2 Decision 5 (eviction log ownership) | Task 1 (`EvictionLog`) + Task 3 (field on `KvEngine`) + Task 4 (push in `enforce_memory_limit`) |
| §2 Decision 6 (frontend inline) | Task 6 (skeleton) + Task 9 (full UI) |
| §2 Decision 7 (HTTP routes) | Task 6 (`/`, `/metrics`) + Task 7 (`/events` SSE) |
| §2 Decision 8 (SSE frame format) | Task 7 |
| §2 Decision 9 (spawn timing) | Task 8 |
| §2 Decision 10 (`0.0.0.0` warning) | Task 8 (`main.rs`) |
| §2 Decision 11 (uptime source) | Task 3 (`start: Arc<OnceLock<Instant>>`) |
| §2 Decision 12 (command counter) | Task 3 (`tick_command`) + Task 8 (`execute_command` calls it) |
| §2 Decision 13 (eviction counter) | Task 4 |
| §2 Decision 14 (bench gate) | Task 10 |
| §2 Decision 15 (module layout) | Tasks 1, 2, 6, 7, 8 |
| §2 Decision 16 (KeyspaceDistribution type) | Task 3 |
| §3 Architecture (process topology) | Task 8 |
| §4 Component contracts | Tasks 1-8 (each maps to a contract) |
| §5 Error handling | Task 6 (400/404/405) + Task 8 (bind failure = warn) |
| §6 Testing strategy | Tasks 1-8 (each has TDD tests) + Task 10 (bench) |
| §7 Performance verification | Task 10 |
| §8 Security | Task 8 (`0.0.0.0` warning) + Task 6 (read-only routes) |

No gaps found — every spec section maps to at least one task.

**2. Placeholder scan:** Searched for "TBD", "TODO", "FIXME", "implement later", "similar to Task N", "add appropriate". Found: none. Every step has complete code. The only "read the file first" instructions are for tasks that modify existing code — the implementer must read the current state to find exact insertion points, which is correct (the plan can't hardcode line numbers that may shift).

**3. Type consistency:** Checked all cross-task type references:
- `EvictionEvent` (Task 1) → consumed in Task 3 (`eviction_log_snapshot` returns `Vec<EvictionEvent>`) and Task 4 (`enforce_memory_limit` pushes `EvictionEvent`). ✓
- `Resp2Tap` (Task 2) → consumed in Task 6 (`handle_http` param), Task 7 (`build_payload` param), Task 8 (`serve`/`accept_loop`/`handle_client` param). ✓
- `Direction` (Task 2) → consumed in Task 8 (`tap.push(Direction::Request, ...)`) and Task 10 (bench). ✓
- `Tick_command` (Task 3) → consumed in Task 8 (`engine.tick_command()` in `execute_command`). ✓
- `KeyspaceDistribution` (Task 3) → consumed in Task 7 (`build_payload` reads `dist.ttl_buckets` etc.). ✓
- `build_payload` (Task 7) → consumed in Task 6 (`/metrics` route calls `sse::build_payload`). ✓
- `CliArgs::dashboard_addr` (Task 5) → consumed in Task 8 (`main.rs` binds `&args.dashboard_addr`). ✓
- `serve` (Task 8) → consumed in Task 8 (`run_listener` spawns `dashboard::serve`). ✓

No type mismatches found.

**4. Ambiguity check:** One ambiguity: Task 3 Step 3e (`set_eviction_config`) initially said `Option<Arc<EvictionLog>>` but then self-corrected to always-instantiate `Arc<EvictionLog>`. The correction is written inline and is decisive — the implementer follows option 1 (always instantiate). No remaining ambiguity.

---

## Execution Handoff

Plan complete and saved to `docs/superpowers/plans/2026-07-06-dashboard-v0.5.0.md`. Two execution options:

**1. Subagent-Driven (recommended)** — I dispatch a fresh subagent per task, review between tasks, fast iteration.

**2. Inline Execution** — Execute tasks in this session using executing-plans, batch execution with checkpoints.

**Which approach?**
