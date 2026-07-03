---
id: FERRUM-002
title: "INFO keyspace reports expires=0 and avg_ttl=0 regardless of TTL keys"
severity: low
status: fixed
component: network
found_date: 2026-07-02
fixed_date: 2026-07-04
fix_branch: fix/info-keyspace-ttl-stats
reporter: AtomCode
---

## Summary

`INFO keyspace` always reports `expires=0` and `avg_ttl=0` even when keys
with TTLs exist, because the values are hardcoded placeholders rather than
computed from the dataset. Redis reports the actual count of keys with
TTLs and the average remaining TTL.

## Steps to Reproduce

```bash
./target/release/ferrum-kv --addr 127.0.0.1:16380 &

redis-cli -p 16380 SET k1 v
redis-cli -p 16380 SET k3 v
redis-cli -p 16380 PEXPIRE k1 60000   # k1 now has a 60s TTL
redis-cli -p 16380 INFO keyspace

# cleanup
kill %1 2>/dev/null
```

## Expected vs Actual

| | Response |
|---|---|
| **Redis** | `db0:keys=2,expires=1,avg_ttl=59999` |
| **FerrumKV** | `db0:keys=2,expires=0,avg_ttl=0` |

State at query time: `k1` has a 60s TTL, `k3` has no TTL, `k2` is absent.
`expires` should be `1` and `avg_ttl` should reflect k1's remaining TTL.

## Root Cause

`src/network/server.rs:423-429` — `render_info` hardcodes the placeholder
values:

```rust
if wants("keyspace") {
    let keys = engine.dbsize().unwrap_or(0);
    out.push_str("# Keyspace\r\n");
    if keys > 0 {
        out.push_str(&format!("db0:keys={keys},expires=0,avg_ttl=0\r\n"));
    }
    out.push_str("\r\n");
}
```

The engine does not maintain a count of keys with TTLs nor an aggregate TTL
statistic, so there is no source of truth to fill these fields.

## Impact

- `redis-cli --stat`, monitoring proxies, and Redis Insight rely on
  `expires`/`avg_ttl` to assess TTL-key health; they will always show 0.
- Operators monitoring expiration behavior will mistakenly conclude no keys
  have TTLs, potentially missing stale-data incidents.

## Suggested Fix

Two implementation options:

**Option A (O(1) query, more state to maintain):**
Add `expire_count: AtomicU64` to `KvEngine`, incremented in `expire_at_ms`
when a TTL is first set on a persistent key, decremented when a TTL'd key
is removed (`del_many`, `sweep_expired`, `enforce_memory_limit`,
`persist` clears TTL, overwrite via `set`). Maintain a running TTL sum
similarly for `avg_ttl`. `render_info` reads the atomics.

**Option B (O(n) query, no new state):**
Add `KvEngine::expire_stats() -> (usize, u64)` that takes the read lock,
iterates entries with `expire_at.is_some()`, counts them, and sums
`expire_at - now`. `render_info` calls it. Acceptable since `INFO` is an
administrative command called infrequently.

**Recommendation**: Option B for correctness with minimal surface area;
upgrade to Option A if `INFO keyspace` becomes a hot path.

**Risk**: Low. `INFO` is read-only and administrative; no data-path impact.
Add a test asserting `expires` and `avg_ttl` are non-zero after `PEXPIRE`.

## Verification

```bash
redis-cli -p 16380 SET k1 v
redis-cli -p 16380 PEXPIRE k1 60000
redis-cli -p 16380 INFO keyspace
# expect: db0:keys=1,expires=1,avg_ttl=<close to 60000>

redis-cli -p 16380 PERSIST k1
redis-cli -p 16380 INFO keyspace
# expect: db0:keys=1,expires=0,avg_ttl=0
```

## Metadata

```yaml
id: FERRUM-002
severity: low           # observability inaccuracy, no functional/data impact
status: fixed           # fixed via PR #18 (fix/info-keyspace-ttl-stats @ 4e9cfcf)
component: network      # server.rs render_info; may touch engine.rs
found_date: 2026-07-02
fixed_date: 2026-07-04
fix_branch: fix/info-keyspace-ttl-stats
```
