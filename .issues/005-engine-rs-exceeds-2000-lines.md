---
id: FERRUM-005
title: "engine.rs exceeds 2000 lines, mixing storage with counters/memtrack/AOF bridge"
severity: low
status: open
component: storage
found_date: 2026-07-04
reporter: AtomCode
---

## Summary

`src/storage/engine.rs` is 2,087 lines and concentrates the core `KvEngine`,
memory tracking (`used_memory` / `enforce_memory_limit` / `track_insert` /
`track_remove`), AOF bridging (`log_aof_result` / `aof.append_*` calls), the
RNG helpers (`rng_seed` / `xorshift32` / `next_rand01`), keyspace statistics,
AHE observation, and ~90 inline unit tests in a single file.

## Steps to Reproduce

```bash
wc -l src/storage/engine.rs   # 2087
list_symbols src/storage/engine.rs   # 140 symbols, multiple concerns
```

## Expected vs Actual

| Aspect | Expected | Actual |
|---|---|---|
| File size | < ~800 lines per concern | 2,087 lines |
| Cohesion | One responsibility per module | Storage + counters + memtrack + AOF bridge + RNG + stats + tests |

## Root Cause

The engine grew organically through Phase 1–8 of the development plan without
a per-concern split. The inline test density (≈90 tests) further inflates the
file. No functional defect — purely a maintainability concern flagged in the
2026-07-04 project analysis report (risk R1).

## Impact

- Slower navigation / code review on the project's most-touched file.
- Higher cognitive load when modifying any single concern (a memtrack tweak
  forces scrolling past storage logic and vice versa).
- No runtime or correctness impact.

## Suggested Fix

Split `engine.rs` into a `storage/engine/` sub-module directory, e.g.:

```
src/storage/engine/
├── mod.rs           # KvEngine struct, public API, set/get/del/exists/...
├── memtrack.rs      # used_memory, track_insert/remove/clear, enforce_memory_limit
├── aof_bridge.rs    # log_aof_result, AOF append call sites
├── rng.rs           # rng_seed, xorshift32, next_rand01
└── stats.rs         # keyspace_stats, record_hit/miss, ahe_snapshot/observe
```

Inline tests should move with their subject. Risk: medium — requires promoting
several `pub(crate)` helpers to `pub` within the crate and updating `use`
paths in `network/server.rs`, `storage/expire.rs`, `storage/eviction.rs`, and
the integration test suites. Must be done on a dedicated branch with the full
CI gate (fmt + clippy -D warnings + test) green at every step.

Out of scope for the `fix/project-risk-mitigations` branch, which landed R2
(version sync) and R3 (AOF ↔ parse_frame consistency tests) only.

## Verification

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets --all-features
wc -l src/storage/engine/*.rs   # each file < ~800 lines
```

## Metadata

```yaml
id: FERRUM-005
title: "engine.rs exceeds 2000 lines, mixing storage with counters/memtrack/AOF bridge"
severity: low
status: open
component: storage
found_date: 2026-07-04
reporter: AtomCode
```
