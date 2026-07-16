---
id: FERRUM-009
title: "Implement SIEVE cache eviction algorithm (NSDI'24)"
severity: medium
status: in-progress
component: storage
found_date: 2026-07-04
reporter: PM Research
---

## Summary

SIEVE (NSDI'24, Zhang et al.) is a cache eviction algorithm that is **simpler than LRU** but beats 9 state-of-the-art algorithms on 45%+ of 1,559 production traces, with 2x LRU throughput. It uses one FIFO queue + one pointer ("hand"). Implementation is ~20 lines of code.

Adding SIEVE to FerrumKV's eviction policy roster (currently 10 policies) serves two purposes:
1. It provides a modern, academically-validated baseline for FerrumKV's eviction benchmark suite
2. It opens the door to a FerrumKV-original variant: **SIEVE-S** (SIEVE with TTL-awareness), where items near expiry get demoted faster

## Design

### SIEVE Algorithm (from NSDI'24 paper)

```
Data structures:
  - queue: FIFO queue of cached keys
  - hand: pointer into the queue
  - visited: per-key boolean flag

On cache hit:
  visited[key] = true

On cache miss (eviction needed):
  while visited[queue[hand]] is true:
      visited[queue[hand]] = false
      hand = (hand + 1) % queue.len()
  evict queue[hand]
  insert new key at queue[hand] (replace in place)
  hand = (hand + 1) % queue.len()
```

Key insight: SIEVE does **quick demotion** — one missed access is enough to evict. LRU gives every item a "second chance" on promotion. SIEVE gives none. This is counter-intuitively beneficial for most real-world workloads.

### SIEVE-S (FerrumKV Original Variant)

For keys with TTL:
```
remaining_ttl_ratio = remaining_ttl_ms / original_ttl_ms
if remaining_ttl_ratio < SIEVE_S_THRESHOLD:  // default: 0.1
    visited[key] = false  // force-demote items about to expire
```

This means items within 10% of their TTL are treated as "already visited=false" regardless of recent access, making them immediate eviction candidates. This is the TTL-aware twist that neither vanilla SIEVE nor AdaptiveClimb offers.

### Integration with Existing Eviction System

- New variants: `EvictionPolicy::AllKeysSieve`, `EvictionPolicy::VolatileSieve`, `EvictionPolicy::AllKeysSieveS`, `EvictionPolicy::VolatileSieveS`
- SIEVE state lives alongside the existing `AdaptiveHybridState` in the engine
- `pick_victim` dispatch adds SIEVE branches
- SIEVE's `visited` flag can reuse the existing LFU counter byte (1 bit for visited, 7 bits for counter — or a separate bool)

## Impact

- **Research**: SIEVE is the new academic baseline. Having it in FerrumKV signals that the project tracks the literature.
- **Differentiation**: SIEVE-S is a genuine FerrumKV original. The NSDI'24 paper does not discuss TTL integration.
- **Performance**: SIEVE is lock-free on hits (just set a boolean). Eviction is O(1) amortized. Expected throughput >= current LRU implementation.

## Suggested Fix

Files to touch:
- `src/storage/eviction.rs` — add SIEVE/SIEVE-S policy variants + candidate selection logic
- `src/storage/engine/mod.rs` — add SIEVE state to engine, wire into `pick_victim` dispatch
- `tests/` — integration test: SIEVE eviction under memory pressure
- `benches/` — SIEVE vs LRU vs LFU benchmark

## Verification

```bash
# Functional: SIEVE evicts under memory pressure
./target/release/ferrum-kv --maxmemory 1mb --maxmemory-policy allkeys-sieve
redis-benchmark -p 6380 -n 100000 -c 50 -t set
# Should complete without OOM errors

# Benchmark: SIEVE vs LRU vs LFU on Zipfian workload
cargo bench --bench eviction_bench
# SIEVE should match or beat LRU on miss ratio

# Unit: SIEVE hand wraps correctly at queue boundary
cargo test sieve_hand_wraps
cargo test sieve_quick_demotion
```

## References

- Zhang et al., "SIEVE is Simpler than LRU: an Efficient Turn-Key Eviction Algorithm for Web Caches," NSDI'24.
- https://www.usenix.org/conference/nsdi24/presentation/zhang-yazhuo
- https://github.com/Thesys-lab/NSDI24-SIEVE

## Implementation

Implemented in PR against `master`. Summary of the approach:

- New `src/storage/sieve.rs` (`SieveState`) holds an `IndexMap<Vec<u8>, bool>`
  FIFO queue + a hand pointer + per-key `visited` bit. `IndexMap` gives O(1)
  insertion-order iteration *and* O(1) removal by key, which the hand sweep
  needs to drop tombstones. This keeps SIEVE stateful and self-contained,
  mirroring how `AdaptiveHybridState` lives in the engine.
- `EvictionPolicy` gains four variants: `allkeys-sieve`, `volatile-sieve`,
  `allkeys-sieves`, `volatile-sieves` (the `-s` pair is the FerrumKV-original
  TTL-aware SIEVE-S; keys within `SIEVE_S_TTL_THRESHOLD` of expiry are
  force-demoted).
- The engine maintains the SIEVE queue on every insert / access / remove (so a
  runtime switch via `set_eviction_config` is immediately consistent) and
  special-cases SIEVE in `enforce_memory_limit` — SIEVE does not use the random
  sample path, unlike the approximate policies.
- Unit tests in `sieve.rs` (quick demotion, all-visited wrap, volatile scope,
  SIEVE-S force-demote) and end-to-end tests in `tests/eviction_test.rs`.

Design note: because the engine only stores a monotonic deadline, SIEVE-S's
"near expiry" is approximated against a fixed `SIEVE_S_HORIZON_MS` horizon
rather than each key's original TTL.

## Metadata

```yaml
id: FERRUM-009
title: "Implement SIEVE cache eviction algorithm (NSDI'24)"
severity: medium
status: open
component: storage
found_date: 2026-07-04
reporter: PM Research
```
