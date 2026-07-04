---
id: FERRUM-007
title: "Add AUTH command and requirepass for password authentication"
severity: high
status: open
component: network
found_date: 2026-07-04
reporter: PM Review
---

## Summary

FerrumKV has zero authentication. Any client that can reach the port can execute any command, including `FLUSHDB`. This makes the server unsafe to expose beyond loopback. Adding Redis-style single-password authentication (`requirepass` + `AUTH`) is the minimum viable security boundary.

## Design

### requirepass directive

A new config file directive and CLI flag:

```
# ferrum.conf
requirepass "my-secret-password"
```

```
--requirepass "my-secret-password"
```

When set, the server rejects all commands except `AUTH` and `PING` from unauthenticated clients. After a successful `AUTH <password>`, normal command processing resumes.

### AUTH command

```
AUTH <password>       → +OK / -ERR invalid password
AUTH <username> <password>  → -ERR ACL not supported (future compat)
```

### Behavior

- `requirepass` not set (default): `AUTH` returns `-ERR Client sent AUTH, but no password is set`
- `requirepass` set: all commands from unauthenticated clients return `-ERR NOAUTH Authentication required.` except `AUTH` and `PING`
- Failed AUTH: returns `-ERR invalid password`, client remains unauthenticated
- Successful AUTH: returns `+OK`, client authenticated for the life of the connection
- Connection close clears auth state (obviously)

### Implementation approach

1. **Parser**: Add `Auth { password: Vec<u8>, username: Option<Vec<u8>> }` to `Command` enum
2. **ServerConfig**: Add `requirepass: Option<String>` field
3. **Connection state**: Track `authenticated: bool` per connection in `handle_client`
4. **Pre-execution check**: Before dispatching any command, verify auth if `requirepass` is set. `AUTH` and `PING` bypass.
5. **INFO**: Add `# Security` section showing `requirepass: yes/no` (never expose the actual password)
6. **CLI**: Add `--requirepass` flag, merged into config via existing two-pass system

## Impact

- **Security**: Zero to basic. Still no TLS, still no ACL — but the door is no longer wide open.
- **Redis compatibility**: `AUTH` behavior matches Redis exactly for the single-password case.
- **Breaking change**: None when `requirepass` is not set (default). Existing behavior preserved.

## Non-goals (deferred to v0.8+)

- ❌ ACL with multiple users and command categories
- ❌ `AUTH` with username (returns `-ERR` for now)
- ❌ Password hashing (stored in plaintext, matching Redis' basic `requirepass`)

## Suggested Fix

Files to touch:
- `src/protocol/parser.rs` — add `Auth` variant + parsing
- `src/network/server.rs` — pre-command auth check, track per-connection auth state
- `src/config/file.rs` — parse `requirepass` directive
- `src/cli.rs` — add `--requirepass` flag
- `src/main.rs` — wire into `ServerConfig`
- `src/protocol/encoder.rs` — no changes needed (existing error encoding)
- `tests/` — integration test: auth required, auth success, auth failure, PING bypasses auth

## Verification

```bash
# Start with password
./target/release/ferrum-kv --requirepass secret123

# Redis CLI: commands rejected before AUTH
redis-cli -p 6380 PING     # +PONG (always allowed)
redis-cli -p 6380 GET foo  # -NOAUTH Authentication required.
redis-cli -p 6380 AUTH secret123  # +OK
redis-cli -p 6380 GET foo  # (normal behavior)

# Wrong password
redis-cli -p 6380 AUTH wrongpass  # -ERR invalid password

# No password set
./target/release/ferrum-kv  # default: no password
redis-cli -p 6380 AUTH foo  # -ERR Client sent AUTH, but no password is set
```

## Metadata

```yaml
id: FERRUM-007
title: "Add AUTH command and requirepass for password authentication"
severity: high
status: open
component: network
found_date: 2026-07-04
reporter: PM Review
```
