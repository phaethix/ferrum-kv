---
id: FERRUM-010
title: "Extract EvictionPolicy trait for pluggable eviction algorithms"
severity: medium
status: open
component: storage
found_date: 2026-07-04
reporter: PM Research
---

## Summary

Currently, eviction policy selection is done via `match` statements in `pick_victim` and scattered across the engine. Adding a new policy (like SIEVE in FERRUM-009) requires touching multiple locations in `eviction.rs` and the engine.

An `EvictionPolicy` trait would encapsulate each policy's state and candidate selection logic behind a common interface, making new policies as simple as implementing the trait and registering in a policy registry.

This is architectural prerequisite for FerrumKV's positioning as an "eviction algorithm platform."

## Design

### Trait Definition

```rust
/// A pluggable cache eviction policy.
///
/// Each implementation owns its policy-specific state (LRU lists, LFU counters,
/// SIEVE queue, AHE adaptive parameters, etc.) and provides hooks that the
/// engine calls at well-defined points in the key lifecycle.
pub trait EvictionPolicy: Send + Sync + std::fmt::Debug {
    /// Called every time a key is accessed (GET hit, SET overwrite, INCR, etc.).
    /// The policy should update whatever metadata it maintains for recency/frequency.
    fn record_access(&self, entry: &mut ValueEntry);

    /// Called when a new key is inserted. The policy may initialize metadata.
    fn record_insert(&self, entry: &mut ValueEntry);

    /// Called when a key is explicitly deleted (DEL, FLUSHDB, TTL expiry).
    /// The policy should remove any internal tracking state for this key.
    fn record_remove(&self, key: &[u8]);

    /// Select a victim key for eviction from the given set of candidates.
    /// Returns the index of the key to evict in `candidates`, or `None`
    /// if no suitable victim exists (noeviction / no volatile keys).
    fn pick_victim(
        &self,
        candidates: &[(Vec<u8>, &ValueEntry)],
        now: Instant,
        scope: EvictionScope,
    ) -> Option<usize>;

    /// Return the policy's name for INFO output and CONFIG GET.
    fn name(&self) -> &'static str;

    /// Return a snapshot of policy-specific metrics for INFO memory/stats.
    fn metrics(&self) -> HashMap<String, String> { HashMap::new() }
}
```

### Policy Registry

```rust
/// Registry of all available eviction policies, keyed by wire name.
pub fn policy_registry() -> HashMap<&'static str, Box<dyn Fn() -> Box<dyn EvictionPolicy>>>;

// Usage in engine initialization:
let policy: Box<dyn EvictionPolicy> = policy_registry()
    .get(config.policy.name())?
    ();
engine.set_eviction_policy(policy);
```

### Migration Path

1. Define the trait in `src/storage/eviction/trait.rs`
2. Implement a wrapper for each existing policy (LruPolicy, LfuPolicy, AhePolicy, etc.)
3. Replace `match`-based dispatch in `pick_victim` with `self.policy.pick_victim(...)`
4. Existing policies continue to work — the trait wraps them, doesn't replace them

## Impact

- **New policy addition**: Implement `EvictionPolicy` trait + one line in registry → done. No engine internals touched.
- **Research platform**: A researcher can write a new policy in their own crate, implement the trait, and benchmark against production policies.
- **Testing**: Each policy can be unit-tested in isolation with a mock candidate set.
- **Runtime switching**: `CONFIG SET maxmemory-policy allkeys-sieve` instantiates a new policy and swaps it in.

## Non-goals

- No hot-swapping of policies mid-eviction (swap happens between writes)
- No policy state serialization/deserialization (AOF does not persist policy internals)
- No user-defined policies via scripting (that's a v0.9+ idea)

## Suggested Fix

Files to touch:
- `src/storage/eviction/mod.rs` — define the trait + registry here
- `src/storage/eviction/lru.rs` — extract LRU implementation behind trait
- `src/storage/eviction/lfu.rs` — extract LFU implementation behind trait
- `src/storage/eviction/ahe.rs` — extract AHE implementation behind trait
- `src/storage/eviction/sieve.rs` — SIEVE behind trait (FERRUM-009)
- `src/storage/eviction/random.rs` — Random behind trait
- `src/storage/engine/mod.rs` — replace `match` dispatch with `self.policy.pick_victim()`
- `src/storage/engine/tests.rs` — update tests for trait-based dispatch

## Verification

```bash
cargo test --all-targets --all-features
# All 293 existing tests pass unchanged. Eviction behavior is identical;
# only the dispatch mechanism changed.

# New tests:
cargo test eviction_trait_roundtrip    # policy swap via CONFIG SET
cargo test eviction_trait_metrics      # each policy reports metrics
cargo test eviction_trait_isolation    # policy A state doesn't leak into policy B
```

## Metadata

```yaml
id: FERRUM-010
title: "Extract EvictionPolicy trait for pluggable eviction algorithms"
severity: medium
status: open
component: storage
found_date: 2026-07-04
reporter: PM Research
```
