# Design: F-03 SLOWLOG

- **Date**: 2026-07-21
- **Feature**: F-03 (Phase 0, v0.5.0 hardening) — "Latency observability."
- **Status**: approved (brainstorming → design → plan)
- **Scope**: new command + CONFIG integration. Redis-compatible SLOWLOG
  (`GET [count]` / `LEN` / `RESET`) with runtime-tunable
  `slowlog-log-slower-than` (µs) and `slowlog-max-len`, reusing the
  F-01 CONFIG GET/SET machinery. Non-persistent (not written to AOF),
  matching Redis. Out of scope: client name (no `CLIENT SETNAME` yet),
  p99 export to INFO (deferred to F-06).

## Goal

Operators have no visibility into which commands are slow. F-03 adds a
bounded, global ring of the slowest commands the server has served, with
enough context (args, latency, client address, timestamp) to diagnose
stragglers — the table-stakes observability feature every real KV server
ships.

The cost must be near-zero when disabled: a single `Instant::now()` plus
one atomic load per command, with no allocation on the hot path unless a
command actually crosses the threshold.

## §1 Architecture

State lives on `KvEngine` (shared via `Arc`, so every connection clone
sees the same log). Two lock-free atomics hold the tunables; the log
itself is a mutex-guarded ring:

```rust
// KvEngine fields
pub(crate) slowlog_slower_than_us: Arc<AtomicU64>, // default 10_000 (10ms)
pub(crate) slowlog_max_len:          Arc<AtomicU64>, // default 128
pub(crate) slowlog:                  Arc<Mutex<VecDeque<SlowLogEntry>>>,
pub(crate) slowlog_seq:             Arc<AtomicU64>, // monotonic id source
```

```rust
pub(crate) struct SlowLogEntry {
    id:         u64,
    ts_secs:    u64,                 // Unix epoch seconds at execution
    duration_us: u64,                // command execution time, microseconds
    args:       Vec<Vec<u8>>,       // reconstruct the RESP arg vector
    client:     SocketAddr,          // peer address; empty/0.0.0.0 when unknown
}
```

The measurement hook is the single command-dispatch choke point,
`execute_command` in `src/network/server.rs`:

```rust
pub fn execute_command(
    cmd:    Command,
    engine: &KvEngine,
    client: Option<SocketAddr>,
    out:    &mut Vec<u8>,
) {
    // Snapshot args BEFORE the consuming match (NLL allows borrow-then-move).
    let slow_args = if engine.slowlog_active() {
        Some(cmd.args())
    } else {
        None
    };
    let start = Instant::now();
    match cmd { /* existing arms unchanged */ }
    let elapsed_us = start.elapsed().as_micros() as u64;
    if let Some(args) = slow_args {
        engine.maybe_push_slowlog(args, client, elapsed_us);
    }
}
```

`maybe_push_slowlog` reads `slowlog_slower_than_us`:
- `< 0`  → log everything (Redis semantics);
- `== 0` → logging disabled;
- `> 0`  → log only when `elapsed_us > threshold`
  (strictly greater, so a command *exactly* at the threshold is NOT
  logged — matches Redis).

On a hit it bumps `slowlog_seq`, pushes a `SlowLogEntry`, and trims the
front down to `slowlog_max_len`.

`handle_client` (server.rs:144) already computes `peer = stream.peer_addr()`;
it now forwards `Some(peer)` into `execute_command` (was called without it
at server.rs:203).

## §2 Components & Data Flow

| Component | Change |
|---|---|
| `src/protocol/parser.rs` | new `Command::SlowLog { sub: Vec<u8>, args: Vec<Vec<u8>> }`; parse `SLOWLOG GET [count] \| LEN \| RESET` with arity checks; add `Command::args(&self) -> Vec<Vec<u8>>` reconstructing the RESP arg vector for every variant |
| `src/storage/engine/mod.rs` | four new `pub(crate)` fields on `KvEngine` + `Default::new()` init; `slowlog_active()`, `maybe_push_slowlog(...)`, `slowlog_get(count: Option<usize>) -> Vec<SlowLogEntry>`, `slowlog_len() -> usize`, `slowlog_reset()` |
| `src/network/server.rs` | `execute_command` gains `client: Option<SocketAddr>`; call `cmd.args()` + `maybe_push_slowlog`; new `slowlog_get/len/reset` encoder arms; wire `Some(peer)` at the `handle_client` call site |
| `src/network/server.rs` (`config_get`/`config_set`) | expose `slowlog-log-slower-than` and `slowlog-max-len`; `CONFIG SET` parses + stores into the atomics with the same validation style as `maxmemory-samples` |

RESP encoding for `SLOWLOG GET` (newest-first):

```
*N                          // N entries
  *6                        // one entry
    :<id>
    :<ts_secs>
    :<duration_us>
    *<M>                    // the command's args
      $<len>\r\n<arg>\r\n ...
    $<len>\r\n<client_addr>\r\n   // e.g. "127.0.0.1:6391"
    $0\r\n                    // client name — empty (no CLIENT SETNAME yet)
```

`SLOWLOG LEN` → `:count`. `SLOWLOG RESET` → `+OK`.

Data flow per command: parse → `execute_command(cmd, engine, Some(peer), out)`
→ snapshot args (only if active) → time the match → conditionally push →
reply already encoded by the existing arms.

## §3 Error Handling

- **Arity**: `SLOWLOG` with no subcommand, or an unknown subcommand, or
  `GET` with a non-integer count, returns `-ERR wrong number of arguments`
  / `-ERR unknown subcommand` via the existing `WrongArity` / parser
  error path. `CONFIG SET` with a non-integer value returns the same
  `-ERR Invalid argument …` shape used for `maxmemory-samples`.
- **Clock**: `ts_secs` comes from `SystemTime::now()`; it cannot fail in
  practice, so a `SystemTimeError` (clock before UNIX epoch) is handled by
  saturating to `0` — never a client-facing error.
- **Lock poisoning**: `slowlog` is behind `Mutex`; a poisoned lock
  propagates via `From<PoisonError>` like the rest of the engine. The two
  tunable atomics cannot poison.
- **No new panic paths**: `unwrap` only where an existing arity/guard
  check guarantees safety (unchanged).
- **AOF**: SLOWLOG is read-only metadata; it must NOT be appended to the
  AOF. No AOF hook is added, and `CONFIG SET slowlog-*` is likewise
  non-persistent (consistent with F-01's `maxmemory-*` handling).

## §4 Testing (TDD)

Unit tests (in `mod.rs` `#[cfg(test)]` or a `slowlog.rs` test module):

- `maybe_push_slowlog` respects the threshold: equal-to-threshold is NOT
  logged, strictly-greater is logged, `<0` logs everything, `0` logs
  nothing.
- id is strictly monotonic across pushes.
- ring trims to `max_len` (oldest dropped first) when exceeded.
- `slowlog_get` returns newest-first; a `count` smaller than the ring
  returns only that many; `None` returns all.
- `slowlog_reset` empties the ring (`len == 0`).
- `Command::args()` reconstructs the exact RESP arg vector for `SET`,
  `MSET` (all pairs), `INCRBY`, `GET`, `CONFIG`, etc.

Integration test (new `tests/slowlog_test.rs`, real TCP per repo
convention — do NOT mock the protocol):

- `CONFIG SET slowlog-log-slower-than 0` → run a command → `SLOWLOG LEN`
  is `0`.
- `CONFIG SET slowlog-log-slower-than -1` → run a command → `SLOWLOG LEN`
  is `1`; `SLOWLOG GET` returns one entry whose args match the command
  and whose client-addr field is the loopback peer.
- `SLOWLOG GET 1` with several entries returns only the newest.
- `SLOWLOG RESET` → `SLOWLOG LEN` is `0`.
- `CONFIG GET slowlog-log-slower-than` / `slowlog-max-len` round-trip the
  set values; `CONFIG GET *` includes both.

Gates: `cargo fmt --all -- --check`, `cargo clippy --all-targets
--all-features -- -D warnings`, `cargo test --all-targets --all-features`
(all 10 integration suites + inline units green), `cargo bench --no-run
--all-features`.

## §5 Implementation Order

1. `Command::SlowLog` + parser arm + `Command::args()` (with unit test
   for `args()`).
2. `KvEngine` fields + `Default::new()` init + `slowlog_active` /
   `maybe_push_slowlog` / `slowlog_get` / `slowlog_len` / `slowlog_reset`
   (with unit tests).
3. `execute_command` signature + hook + `handle_client` call-site wiring.
4. `SLOWLOG GET/LEN/RESET` encoder arms in `execute_command`.
5. `CONFIG GET/SET` wiring for the two new parameters.
6. Integration test `tests/slowlog_test.rs`.
7. Run all gates; fix; commit per concern; open PR.

## Out of scope (explicit)

- `CLIENT SETNAME` / client name in the entry (YAGNI for v0.5).
- Exporting slow-query counts or p99 to `INFO` (belongs to F-06).
- AOF persistence of the slow log (Redis does not persist it).
- Histogram / sampling modes beyond the simple threshold ring.
