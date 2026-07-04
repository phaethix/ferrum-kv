---
id: FERRUM-005
title: "engine.rs exceeds 2000 lines, mixing storage with counters/memtrack/AOF bridge"
severity: low
status: fixed
component: storage
found_date: 2026-07-04
fixed_date: 2026-07-04
fix_branch: refactor/split-engine-module
reporter: AtomCode
---

## Summary

`src/storage/engine.rs` was 2,087 lines and concentrated the core `KvEngine`,
memory tracking (`used_memory` / `enforce_memory_limit` / `track_insert` /
`track_remove`), AOF bridging (`log_aof_result` / `aof.append_*` calls), the
RNG helpers (`rng_seed` / `xorshift32` / `next_rand01`), keyspace statistics,
AHE observation, and ~90 inline unit tests in a single file.

## Steps to Reproduce

```bash
wc -l src/storage/engine.rs   # was 2087
list_symbols src/storage/engine.rs   # 140 symbols, multiple concerns
```

## Expected vs Actual

| Aspect | Expected | Actual (post-fix) |
|---|---|---|
| File size | < ~1000 lines per concern | mod.rs 993 / tests.rs 918 / util.rs 133 / entry.rs 91 / types.rs 36 |
| Cohesion | One responsibility per module | Storage API in mod.rs; tests in tests.rs; helpers in util.rs; record in entry.rs; ancillary types in types.rs |

## Root Cause

The engine grew organically through Phase 1–8 of the development plan without
a per-concern split. The inline test density (≈90 tests) further inflated the
file. No functional defect — purely a maintainability concern flagged in the
2026-07-04 project analysis report (risk R1).

## Impact

- Slower navigation / code review on the project's most-touched file.
- Higher cognitive load when modifying any single concern (a memtrack tweak
  forces scrolling past storage logic and vice versa).
- No runtime or correctness impact.

## Suggested Fix

Applied on branch `refactor/split-engine-module`: split `engine.rs` into a
`storage/engine/` sub-module directory:

```
src/storage/engine/
├── mod.rs        # KvEngine struct + all impl blocks + Default + re-exports (993 lines)
├── entry.rs      # ValueEntry, LFU_DECAY_MINUTES, live_payload (91 lines)
├── util.rs       # validate_key/value, current_epoch_ms, deadline_to_epoch_ms,
│                 # log_aof_result, rng_seed, xorshift32, sample_candidates, entry_bytes (133 lines)
├── types.rs      # SweepStats, TtlStatus (36 lines)
└── tests.rs      # the engine's ~90 inline unit tests (918 lines)
```

Visibility adjustments (all kept crate-internal):
- `ValueEntry` and its fields: `pub(crate)` (consumed by `util.rs` and `tests.rs`).
- `KvEngine` fields: `pub(crate)` (white-box access from `tests.rs`).
- Free helpers in `util.rs`: `pub(super)` / `pub(crate)` as needed.
- `mod.rs` re-exports `SweepStats`, `TtlStatus`, and `current_epoch_ms` so
  external paths (`storage::engine::KvEngine`, `::current_epoch_ms`, etc.)
  stay identical — `network/server.rs`, `persistence/replay.rs`,
  `storage/expire.rs`, and all 9 integration test suites required zero
  changes.

Risk: low — pure structural refactor, no semantic change. Verified with the
full CI gate.

## Verification

```bash
cargo fmt --check                              # clean
cargo clippy --all-targets -- -D warnings      # zero warnings
cargo test --all-targets --all-features        # 193 lib + 36 doc + all integration suites green
wc -l src/storage/engine/*.rs                  # each file <= 993 lines
```

All 293 tests pass unchanged; no test logic was modified, only relocated.

## Metadata

```yaml
id: FERRUM-005
title: "engine.rs exceeds 2000 lines, mixing storage with counters/memtrack/AOF bridge"
severity: low
status: fixed
component: storage
found_date: 2026-07-04
fixed_date: 2026-07-04
fix_branch: refactor/split-engine-module
reporter: AtomCode
```
