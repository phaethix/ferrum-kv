# Design: F-07 WriteGuard Pipeline Refactor

- **Date**: 2026-07-20
- **Feature**: F-07 (Phase 0, v0.5.0 hardening) — "Eliminate write-path boilerplate. Foundation for clean eviction integration."
- **Status**: approved (brainstorming → design)
- **Scope**: refactor only. No behavior change, no new public API, no new eviction trait (E-04 is explicitly re-scoped to "Beyond v0.8").

## Goal

Today every mutating and several read commands in `src/storage/engine/mod.rs`
repeat the same three-part boilerplate plus a duplicated lazy-expiry dance:

1. Acquire the store write lock (`let mut store = self.store.write()?`).
2. Log the AOF record by hand (`if let Some(aof) = &self.aof { log_aof_result("SET", aof.append_set(...)); }`), with the command name hardcoded per call site.
3. Call `enforce_for_write` before an insert, then `track_insert` / `track_remove`.

On top of that, the lazy-expiry sequence — `if entry.is_expired(now) { track_remove; log_expire_drop; record_miss/record_hit }` — is copied a dozen-plus times across `get`, `exists`, `exists_many`, `mget`, `ttl_ms`, `memory_usage`, `del`, `del_many`, `expire_at_ms`, `persist`.

F-07 centralizes both into a single `WriteGuard` abstraction so that (a) the command methods become 3-5 line thin wrappers, and (b) the points where `on_access` / `on_insert` / `on_remove` fire become obvious and single-sited — exactly the hooks a future E-04 `EvictionPolicy` trait would hang off.

## §1 Architecture

New `pub(crate) mod write_guard` in `src/storage/engine/write_guard.rs`:

```rust
pub(crate) struct WriteGuard<'a> {
    engine: &'a KvEngine,
    store:  RwLockWriteGuard<'a, HashMap<Vec<u8>, ValueEntry>>,
    now:    Instant,
    now_ms: i64,
}
```

`WriteGuard::begin(engine) -> Result<Self>` takes the write lock once and
captures `Instant::now()` and `current_epoch_ms()`. The lock is held for the
whole command — identical to the current per-method `store.write()?`, so AOF
ordering (write lock held while appending) is preserved.

`KvEngine`'s public commands (`set`, `get`, `del`, `incr_by`, `append`, `mset`,
`del_many`, `exists`, `exists_many`, `mget`, `ttl_ms`, `memory_usage`,
`expire_at_ms`, `persist`, `flushdb`) become thin wrappers:

```rust
pub fn set(&self, key: Vec<u8>, value: Vec<u8>) -> Result<Option<Vec<u8>>, FerrumError> {
    let mut g = WriteGuard::begin(self)?;
    g.insert(key, value)
}
```

The guard **reuses** the existing private `KvEngine` helpers
(`track_insert`, `track_remove`, `track_clear`, `enforce_for_write`,
`touch_access`, `log_expire_drop`, `apply_delta`, `sieve_notify_*`,
`ac_notify_*`, `ahe_*`) by calling them as `self.engine.track_insert(&mut self.store, ...)`.
There is **zero logic duplication** — only the *decision of when to call them*
moves out of each command and into the guard method.

AOF logging collapses into one private helper on the guard:

```rust
fn log(&self, cmd: &str, res: io::Result<()>) {
    if let Some(aof) = &self.engine.aof {
        log_aof_result(cmd, res);
    }
}
```

Multi-record cases (INCRBY / APPEND re-emit `PEXPIREAT`) are covered by
`insert_keep_ttl(key, val, ttl: Option<Duration>, reemit_pexpireat: Option<i64>)`.

## §2 Components & Data Flow

`WriteGuard` methods map 1:1 onto current command bodies:

| Guard method | Replaces |
|---|---|
| `fetch_live(key) -> Option<Vec<u8>>` | `get` / `mget` read body |
| `contains_live(key) -> bool` | `exists` / `exists_many` |
| `insert(key, val) -> Result<Option<Vec<u8>>>` | `set` |
| `insert_nx(key, val) -> Result<bool>` | `set_nx` |
| `insert_many(pairs)` | `mset` |
| `insert_keep_ttl(key, val, ttl, reemit_pexpireat) -> Result<...>` | `incr_by`, `append` |
| `remove(key) -> bool` / `remove_many(keys) -> usize` | `del` / `del_many` |
| `remove_expired_now(key) -> bool` | `expire_at_ms` past-deadline branch |
| `expire_at(key, abs_ms) -> Result<bool>` | `expire_at_ms` main path |
| `persist(key) -> bool` | `persist` |
| `clear()` | `flushdb` |
| `ttl_of(key) -> TtlStatus` | `ttl_ms` |
| `memory_of(key) -> Option<u64>` | `memory_usage` |

Data flow per command: `WriteGuard::begin` (lock + timestamps) → typed guard
method → reuses `engine.*` helpers → returns an **owned** `Vec<u8>` (never a
borrow into the guarded map, to keep the borrow checker happy and allow
callers to use the value freely). AOF is logged inside the guard method, under
the held lock.

## §3 Error Handling

- **Lock poisoning**: `begin`'s `store.write()?` propagates `PoisonError` via
  the existing `From` impl — unchanged.
- **Validation**: `KeyTooLong` / `ValueTooLarge` are raised *up front* inside
  `insert` / `insert_many` / `insert_keep_ttl` via `validate_key` / `validate_value`,
  exactly as today, so error semantics are identical.
- **Out of memory**: `enforce_for_write` → `OutOfMemory` propagates unchanged.
- **No new panic paths**: `unwrap` is used only where an existing arity/guard
  check already guarantees safety (unchanged).
- **AOF**: still fire-and-forget through `log_aof_result` (no `?`), preserving
  today's non-fatal AOF-write semantics.

## §4 Testing

- **No behavior change** is the contract. All 10 integration suites
  (`aof`, `async`, `timeout`, `concurrency`, `eviction`, `expire`, `kv_engine`,
  `max_clients`, `resp2_wire`, `shutdown`) plus the engine's inline unit tests
  must stay green — this is the primary safety net.
- **New focused unit tests** (in `write_guard.rs` / `mod.rs` tests):
  - lazy expiry fires on `fetch_live` of an expired key and logs `DEL`;
  - `insert` clears the prior TTL and logs `SET`;
  - `insert_keep_ttl` preserves the TTL and re-emits `PEXPIREAT`;
  - `remove` logs `DEL` only when the key was live;
  - `enforce_for_write` still triggers eviction under `max_memory`.
- **Gates**: `cargo fmt --all -- --check`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo test --all-targets --all-features`, `cargo bench --no-run --all-features`.
- **Regression**: re-run `examples/hit_ratio_bench.rs` across all policies to confirm zero hit-ratio regression (F-07 exit criterion: "No benchmark regression").

## Out of scope (explicit)

- E-04 `EvictionPolicy` trait is **not** implemented here. F-07 only makes the
  insertion/removal/access sites single-sited so that trait can be added later
  without touching every command.
- No new commands, no protocol changes, no persistence-format changes.
