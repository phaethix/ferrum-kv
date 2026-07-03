---
id: FERRUM-004
title: "Issue reproduction scripts launch server without cleanup, leaving orphan processes"
severity: low
status: fixed
component: docs
found_date: 2026-07-04
reporter: AtomCode
---

## Summary

The "Steps to Reproduce" sections in `.issues/001-*.md` and `.issues/002-*.md`
start FerrumKV with `./target/release/ferrum-kv --addr ... &` but never kill
the background process. Readers who copy-paste the commands will accumulate
orphaned server processes holding the test port, causing `bind: address
already in use` on subsequent runs.

## Steps to Reproduce

```bash
# Copy-paste the issue #1 reproduction steps verbatim
./target/release/ferrum-kv --addr 127.0.0.1:16380 &
redis-cli -p 16380 SET k1 v
# ... (no kill command) ...

# Run again — second reproduction fails
./target/release/ferrum-kv --addr 127.0.0.1:16380 &
# => error: failed to bind 127.0.0.1:16380: Address already in use
```

## Expected vs Actual

| | Behavior |
|---|---|
| **Expected** | Reproduction script is self-contained: starts server, runs commands, cleans up |
| **Actual** | Server process left running after script completes; port held indefinitely |

## Root Cause

`.issues/001-exists-rejects-multi-key.md:20-26` and
`.issues/002-info-keyspace-ttl-counts-hardcoded-zero.md:20-27` — the bash
blocks start the server with `&` but contain no `kill`, `trap`, or
`pkill` cleanup.

## Impact

- Repeated reproductions fail with "address already in use".
- Orphan processes accumulate, consuming memory and file descriptors.
- Violates the `.issues/README.md` convention that reproduction steps should
  be "copy-pasteable shell commands" — they are copy-pasteable but not
  self-cleaning.

## Suggested Fix

Append a cleanup line to each reproduction block:

```bash
# cleanup
kill %1 2>/dev/null
```

Or use a trap at the top of the block:

```bash
trap 'kill $SRV_PID 2>/dev/null' EXIT
SRV_PID=$!
```

Also update `.issues/README.md` format convention to require cleanup steps
in reproduction scripts.

**Risk**: None. Documentation-only change.

## Verification

```bash
# Run the updated reproduction script twice in succession — both should succeed
# without "address already in use" errors.
```

## Metadata

```yaml
id: FERRUM-004
severity: low          # documentation hygiene, no code impact
status: fixed          # fixed in same review session
component: docs
found_date: 2026-07-04
fix: added cleanup lines to both issue reproduction scripts
```
