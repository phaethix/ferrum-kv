---
id: FERRUM-001
title: "EXISTS rejects multi-key requests (Redis incompatibility)"
severity: medium
status: fixed
component: protocol
found_date: 2026-07-02
fixed_date: 2026-07-04
fix_branch: fix/exists-multi-key
reporter: AtomCode
---

## Summary

`EXISTS` only accepts a single key and returns an arity error when given
multiple keys, whereas Redis (since 3.0.3) accepts `EXISTS key [key ...]`
and returns the total count of existing keys (counting duplicates).

## Steps to Reproduce

```bash
# Start FerrumKV on a test port
./target/release/ferrum-kv --addr 127.0.0.1:16380 &

redis-cli -p 16380 SET k1 v        # => OK
redis-cli -p 16380 SET k3 v        # => OK
redis-cli -p 16380 EXISTS k1 k2 k3 k1   # k1, k3 exist; k2 missing; k1 duplicated

# cleanup
kill %1 2>/dev/null
```

## Expected vs Actual

| | Response |
|---|---|
| **Redis** | `:3` (integer 3 — total count of existing keys, duplicates included) |
| **FerrumKV** | `-ERR wrong number of arguments for 'EXISTS' command` |

## Root Cause

`src/protocol/parser.rs:338-345` — `build_command` enforces single-key arity:

```rust
b"EXISTS" => {
    if args.len() != 1 {
        return Err(FerrumError::WrongArity { cmd: "EXISTS" });
    }
    Ok(Command::Exists { key: args.into_iter().next().unwrap() })
}
```

The `Command::Exists { key: Vec<u8> }` variant (`parser.rs:25`) carries a
single key, and `execute_command` (`server.rs:312-316`) only replies `:1`/`:0`.

## Impact

- Any Redis client or tooling that batches key existence checks via
  `EXISTS k1 k2 k3` receives an error instead of a count.
- The connection stays open (the frame is well-formed, so it is classified
  as `FrameParse::Invalid`, not a protocol error), but the operation fails.
- Monitoring scripts, migration tools, and Redis Insight may misbehave.

## Suggested Fix

1. Change the enum variant:
   ```rust
   Exists { keys: Vec<Vec<u8>> },   // was: Exists { key: Vec<u8> }
   ```
2. Relax the parser arity check to reject only the empty case:
   ```rust
   b"EXISTS" => {
       if args.is_empty() {
           return Err(FerrumError::WrongArity { cmd: "EXISTS" });
       }
       Ok(Command::Exists { keys: args })
   }
   ```
3. Add `KvEngine::exists_many(&[Vec<u8>]) -> Result<usize, FerrumError>`
   that counts live (non-expired) keys, reusing `is_expired` + lazy
   expiration semantics.
4. Update `execute_command` to encode the count as an integer.

**Risk**: Low. The change is additive for multi-key callers and preserves
single-key behavior (returns `:1`/`:0`). Update the unit test
`parses_exists_command` and the README command table.

## Verification

```bash
redis-cli -p 16380 SET k1 v
redis-cli -p 16380 SET k3 v
redis-cli -p 16380 EXISTS k1 k2 k3 k1   # expect :3
redis-cli -p 16380 EXISTS k2             # expect :0
redis-cli -p 16380 EXISTS               # expect -ERR wrong arity (unchanged)
```

## Metadata

```yaml
id: FERRUM-001
severity: medium          # functional incompatibility, no data loss
status: fixed             # fixed via PR #17 (fix/exists-multi-key @ c7a3f48)
component: protocol       # parser.rs + server.rs
found_date: 2026-07-02
fixed_date: 2026-07-04
fix_branch: fix/exists-multi-key
```
