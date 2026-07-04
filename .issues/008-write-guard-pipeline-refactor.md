---
id: FERRUM-008
title: "Extract WriteGuard pipeline to eliminate command boilerplate and enforce invariants"
severity: medium
status: open
component: storage
found_date: 2026-07-04
reporter: PM Review
---

## Summary

Every mutating command in `KvEngine` (~12 methods) manually repeats the same sequence: validate input, acquire write lock, enforce memory limit, log to AOF, track insertion/removal, return result. This pattern is error-prone and has already produced one known invariant (every deletion must call `track_remove` + AOF DEL) that requires explicit test coverage to guard.

A `WriteGuard` pattern centralizes the enforcement, reduces per-command boilerplate by ~60%, and eliminates the class of bugs where a new command forgets a step.

## Design

### WriteGuard

```rust
/// Held while a write operation is in progress. Created by `KvEngine::begin_write()`
/// and consumed by `WriteGuard::commit()` or dropped on error.
pub(crate) struct WriteGuard<'a> {
    store: RwLockWriteGuard<'a, HashMap<Vec<u8>, Value>>,
    aof: Option<&'a AofWriter>,
    mem: &'a AtomicU64,
    key_size: usize,
    value_room: usize, // how many bytes this write is allowed to consume
}
```

### KvEngine API surface

```rust
impl KvEngine {
    /// Begin a write operation. Validates key/value sizes, acquires the write
    /// lock, and enforces the memory limit. Returns a guard that must be
    /// committed or dropped.
    pub fn begin_write(&self, key: &[u8], payload_size: usize) -> Result<WriteGuard, FerrumError>;

    /// Begin a write for a batch operation (MSET, DEL multi-key, etc.)
    pub fn begin_batch_write(&self, total_payload: usize) -> Result<BatchWriteGuard, FerrumError>;
}
```

### WriteGuard methods

```rust
impl WriteGuard<'_> {
    /// Insert a value, tracking memory and returning the previous entry.
    pub fn insert(&mut self, key: Vec<u8>, value: Value) -> Option<Value>;

    /// Remove a key, tracking memory. Logs DEL to AOF if configured.
    pub fn remove(&mut self, key: &[u8]) -> Option<Value>;

    /// Log a SET command frame to the AOF.
    pub fn log_set(&self, key: &[u8], value: &[u8]);

    /// Log a DEL command frame to the AOF.
    pub fn log_del(&self, keys: &[Vec<u8>]);

    /// Log an EXPIRE/PEXPIRE/PEXPIREAT frame to the AOF.
    pub fn log_expire(&self, key: &[u8], seconds_or_millis: i64, cmd: &str);

    /// Drop the guard without applying changes (memory tracking is reverted).
    pub fn abort(self);
}
```

### Before → After example

```rust
// BEFORE (current — every SET-like command repeats this):
pub fn set(&self, key: Vec<u8>, value: Vec<u8>) -> Result<Option<Vec<u8>>, FerrumError> {
    validate_key(&key)?;
    validate_value(&value)?;
    let mut store = self.store.write()?;
    self.enforce_for_write(&mut store, &key, value.len())?;
    if let Some(aof) = &self.aof {
        log_aof_result("SET", aof.append_set(&key, &value));
    }
    let previous = self.track_insert(&mut store, key, ValueEntry::new(value));
    Ok(previous.and_then(live_payload))
}

// AFTER (v0.5 — guard enforces invariants):
pub fn set(&self, key: Vec<u8>, value: Vec<u8>) -> Result<Option<Vec<u8>>, FerrumError> {
    let mut guard = self.begin_write(&key, value.len())?;
    guard.log_set(&key, &value);
    Ok(guard.insert(key, Value::string(value)).and_then(Value::into_string))
}
```

## Impact

- **Eliminates** the invariant violation bug class (forgotten `track_remove` or AOF log)
- **Reduces** per-command boilerplate from ~12 lines to ~4 lines
- **Simplifies** the multi-type migration (new types use the same guard, no need to re-learn the dance)
- **No user-facing change**: pure internal refactor, all existing tests pass unchanged

## Non-goals (for this refactor)

- ❌ Not changing the public API of `KvEngine`
- ❌ Not touching read-path commands (GET, MGET, STRLEN, TTL, etc.)
- ❌ Not adding new features — purely structural

## Suggested Fix

Files to touch:
- `src/storage/engine/mod.rs` — add `WriteGuard`, `BatchWriteGuard`, `begin_write()`, `begin_batch_write()`
- `src/storage/engine/util.rs` — remove `log_aof_result`, `track_insert`, `track_remove` (moved into guard)
- All mutating methods in `mod.rs` — rewrite to use guard
- `src/storage/engine/tests.rs` — add guard-specific unit tests (abort reverts, double-insert tracks correctly)

## Verification

```bash
cargo test --all-targets --all-features
# All 293 existing tests must pass unchanged
# New guard-specific tests:
# - WriteGuard::abort() reverts memory tracking
# - WriteGuard drop on error does not corrupt state
# - BatchWriteGuard with MSET partial failure reverts all inserts
```

## Metadata

```yaml
id: FERRUM-008
title: "Extract WriteGuard pipeline to eliminate command boilerplate and enforce invariants"
severity: medium
status: open
component: storage
found_date: 2026-07-04
reporter: PM Review
```
