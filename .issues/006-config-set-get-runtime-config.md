---
id: FERRUM-006
title: "Add CONFIG SET and CONFIG GET for runtime configuration"
severity: medium
status: open
component: config
found_date: 2026-07-04
reporter: PM Review
---

## Summary

FerrumKV currently requires a restart to change any runtime parameter. Redis-style `CONFIG GET` and `CONFIG SET` allow operators to inspect and change configuration without downtime. This is the single highest-leverage operational feature missing from the current release.

## Design

### CONFIG GET `<pattern>`

Returns all config keys matching a glob-style pattern (`*` supported, Redis-compatible). Response format:

```
*2          (array of 2: key, value)
$10
maxmemory
$6
67108864
```

### CONFIG SET `<key>` `<value>`

Sets a single config directive at runtime. Returns `+OK` on success, `-ERR` for unsupported/immutable keys.

### Supported directives (v0.5 scope)

| Directive | GET | SET | Notes |
|-----------|-----|-----|-------|
| `maxmemory` | ✅ | ✅ | Takes effect on next write |
| `maxmemory-policy` | ✅ | ✅ | Takes effect immediately |
| `maxmemory-samples` | ✅ | ✅ | Takes effect immediately |
| `timeout` | ✅ | ✅ | New connections only |
| `maxclients` | ✅ | ✅ | New connections only |
| `loglevel` | ✅ | ✅ | Takes effect immediately |
| `appendfsync` | ✅ | ✅ | Takes effect on next AOF write |
| `slowlog-log-slower-than` | ✅ | ✅ | Takes effect immediately (once F-03 lands) |
| `slowlog-max-len` | ✅ | ✅ | Takes effect immediately (once F-03 lands) |
| `requirepass` | ✅ | ❌ | Immutable at runtime (auth changes are a security risk) |
| `io-threads` | ✅ | ❌ | Immutable at runtime (tokio runtime is fixed at startup) |
| `bind` | ✅ | ❌ | Immutable at runtime |
| `port` | ✅ | ❌ | Immutable at runtime |
| `appendonly` | ✅ | ❌ | Immutable at runtime (AOF init is startup-only) |
| `appendfilename` | ✅ | ❌ | Immutable at runtime |

### Implementation approach

1. **Parser**: Add `ConfigGet { pattern: Vec<u8> }` and `ConfigSet { key: Vec<u8>, value: Vec<u8> }` to `Command` enum
2. **Execution**: `execute_command` dispatches to a new `ConfigStore` that holds resolved config values
3. **ConfigStore** lives on `KvEngine` or alongside it, keyed by directive name, with typed getters/setters
4. **Pattern matching**: Simple glob (`*` only, matching Redis behavior) — `*` matches everything, `max*` matches `maxmemory`, `maxmemory-policy`, etc.
5. **Existing config structs** (`EvictionConfig`, `ServerConfig`, `AofConfig`) gain `apply_directive(&mut self, key: &str, value: &str)` methods

## Impact

- Ops teams can tune memory limits and eviction policy without restarting
- Removes the #1 operational friction point for production-like deployments
- Pattern matching works with existing config file format — `CONFIG GET *` dumps everything

## Suggested Fix

Files to touch:
- `src/protocol/parser.rs` — add `ConfigGet`, `ConfigSet` command variants + parsing
- `src/network/server.rs` — add execution dispatch
- `src/config/` — new `config_store.rs` with typed config storage + glob matching
- `src/storage/engine/mod.rs` — expose config mutation hooks
- `tests/` — new integration test: `resp2_config_test.rs`

## Verification

```bash
# Set and verify maxmemory
redis-cli -p 6380 CONFIG SET maxmemory 128mb
redis-cli -p 6380 CONFIG GET maxmemory
# Should return: maxmemory 134217728

# Pattern match
redis-cli -p 6380 CONFIG GET max*
# Should return all maxmemory directives

# Immutable key
redis-cli -p 6380 CONFIG SET port 9999
# Should return: -ERR Unsupported CONFIG parameter: port
```

## Metadata

```yaml
id: FERRUM-006
title: "Add CONFIG SET and CONFIG GET for runtime configuration"
severity: medium
status: open
component: config
found_date: 2026-07-04
reporter: PM Review
```
