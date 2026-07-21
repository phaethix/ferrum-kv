# F-04 AOF REWRITE — Design

- **Feature**: F-04 (Phase 0, v0.5.0 hardening) — "AOF compaction. Critical for any non-toy AOF user."
- **Status**: Approved design, ready for TDD implementation.
- **Branch**: `feat/aof-rewrite`
- **References**: `product-strategy.md` Phase 0; `development-plan.md` "方案 A：记最终态".

## Goal

Compact the append-only file so a long-running server does not grow the AOF
without bound. Every overwrite of a key currently appends another `SET`, so
the on-disk log is a full history rather than the current state. A rewrite
produces a fresh, minimal AOF containing exactly one final-state command per
live key (plus its TTL), then swaps it in atomically.

## Non-goals (YAGNI for v0.5)

- Auto-rewrite by growth percentage (`auto-aof-rewrite-percentage` /
  `auto-aof-rewrite-min-size`). Operator triggers it explicitly via
  `BGREWRITEAOF`.
- RDB snapshotting (tracked separately as v0.5 RDB work in the whitepaper).
- Fork-based copy-on-write. FerrumKV runs its background work on dedicated OS
  threads (sweeper, AOF fsync flusher); rewrite follows the same model and
  uses an in-memory delta buffer instead of a fork.

## Execution model

`BGREWRITEAOF` → `KvEngine::rewrite_aof()` returns `+OK` immediately ("started")
and the rewrite runs on a dedicated OS thread (`ferrum-aof-rewrite`), so the
server never pauses for the rewrite — required to pass the "1M keys" exit
criterion.

`AofWriter` gains:

- `path: PathBuf` — needed to reopen the file handle after the swap.
- `rewriting: AtomicBool` — gates delta buffering.
- `delta: Mutex<Vec<u8>>` — bounded (64 MiB). Appends during a rewrite are
  copied here. If it would exceed the cap the rewrite is aborted (old AOF kept,
  a warning logged).
- `rewrite_lock: Arc<Mutex<()>>` — serialises `append_*` against the
  swap so no command can be in flight across the rename.

### Append path (`append_*`)

Holds `rewrite_lock` for the whole call: writes the bytes to the current file
(under `inner`), then, if `rewriting` is set, also pushes the same bytes into
`delta`. Because the lock is held across both the write and the buffer copy,
the two are atomic with respect to `finish_rewrite`.

### Rewrite thread

1. Begin: `aof.begin_rewrite()` — set `rewriting = true`, clear `delta`.
2. Snapshot the keyspace: take the store **read** lock, collect every *live*
   entry as `(key, value, Option<ttl_deadline>)`, release the lock. The clone
   is the only writer-stalling step, and it is brief (read lock, no writers
   blocked for long on a 1M-key clone).
3. Serialise the compact AOF to a temp file (no engine lock held during I/O):
   for each live entry emit `SET key value`, and if it has a TTL emit
   `PEXPIREAT key <abs_epoch_ms>`. This is the project's "方案 A：记最终态" —
   identical RESP2 bytes to the wire protocol, so replay is 100% compatible and
   the format does not change.
4. Complete (`finish_rewrite`): under `rewrite_lock` (so no append is in
   flight) — `fsync(temp)` → `rename(temp, path)` → lock `inner` and
   `mem::replace` its `BufWriter<File>` with a fresh append handle at `path`
   (the background flusher keeps working because it shares the same
   `Arc<Mutex<BufWriter<File>>>`), then write `delta` into the new file and
   `fsync`. Clear `delta`, set `rewriting = false`.
5. On any error: `abort_rewrite()` — clear `delta`, set `rewriting = false`,
   leave the old AOF in place, log a warning. The server keeps running.

Because `finish_rewrite` holds `rewrite_lock` for the entire swap, writes that
happen during the rewrite either land in the old file (its bytes are already
semantically superseded by the compact snapshot) **and** in `delta` (replayed
onto the new file), or — after the swap — land directly in the new file.
Nothing is lost.

## Command surface

- New `Command::RewriteAof` (no arguments; arity rejects extra args).
- `execute_command` arm: if `engine.aof` is `None`, reply
  `-ERR AOF not enabled`; otherwise call `engine.rewrite_aof()` and reply
  `+OK` (Redis' `BGREWRITEAOF` returns `+OK` immediately, rewriting in
  background).
- No new `CONFIG` parameters (command-triggered only).

## Testing

- **Unit (`AofWriter` + engine)**:
  - delta captures appends issued during a rewrite;
  - bounded-delta overflow aborts the rewrite and keeps the old AOF;
  - after rename+reopen, replaying the new file yields the pre-rewrite state
    plus the in-window deltas (round-trip equality);
  - compact content is correct: exactly one `SET` per live key, a `PEXPIREAT`
    for every TTL key, no expired keys, no redundant overwrites.
- **Integration (`tests/aof_rewrite_test.rs`, real TCP listener)**:
  - `SET` N keys → `BGREWRITEAOF` → read the on-disk AOF and assert it is
    compact (one `SET` per key, no duplicate overwrites);
  - restart the server against that AOF and assert the full keyspace is
    restored;
  - a write issued *during* the rewrite is preserved after restart.
- **Scale**: an `#[ignore]` test exercising `BGREWRITEAOF` with 1M keys to
  satisfy the roadmap exit criterion ("AOF rewrite tested with 1M keys"); CI
  runs a 50k-key correctness case so the suite stays fast.

## Compatibility / risk

- **None** to the on-disk format: the compact AOF is the same RESP2 command
  stream the existing `replay` already consumes. Old AOFs replay unchanged;
  new compact AOFs replay unchanged.
- No persistence cross-version concern. No AOF-write during replay (unchanged
  invariant).
- The only new runtime risk is the rename+swap window; it is fully covered by
  `rewrite_lock` and the delta buffer, and asserted by the tests above.
