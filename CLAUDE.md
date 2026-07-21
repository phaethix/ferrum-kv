# FerrumKV — AI Agent Instructions

> **Single source of truth for AI collaboration rules on this project.**
> This file is the canonical, AI-agnostic contribution standard. The same
> rules are mirrored in `.atomcode.md` (AtomCode-specific wrapper); keep
> both in sync when updating. Any AI assistant working in this repo —
> AtomCode, Claude Code, Cursor, GitHub Copilot, Aider, or any other —
> MUST read and follow this file before making changes.
>
> Personal per-user overrides that should NOT be shared with teammates go
> in `.atomcode.user.md` (AtomCode, gitignored) or the equivalent
> per-tool local file; they load after this file (higher priority).

## Project Identity

- **Language**: Rust, edition 2024
- **Type**: From-scratch RESP2-compatible KV storage server (Redis wire protocol)
- **Runtime**: Tokio async, `signal-hook`, `indexmap`, zero heavy deps
- **Versions**: `Cargo.toml` = 0.5.2, README badge = v0.5.2

## Architecture Quick Reference

| Layer | Module | Entry points |
|---|---|---|
| Entry | `src/main.rs`, `src/cli.rs` | `main`, `build_engine`, `install_signal_handlers` |
| Network | `src/network/server.rs`, `shutdown.rs` | `run_listener`, `accept_loop`, `handle_client`, `execute_command` |
| Protocol | `src/protocol/parser.rs`, `encoder.rs` | `parse_frame`, `build_command`, `encode_*` |
| Storage | `src/storage/engine/` (mod.rs, entry.rs, util.rs, types.rs, tests.rs), `eviction.rs`, `expire.rs`, `sieve.rs`, `adaptive_climb.rs` | `KvEngine`, `pick_victim` (AHE in `eviction.rs`), `sweep_expired`, `AdaptiveClimb` |
| Persistence | `src/persistence/writer.rs`, `replay.rs` | `AofWriter`, `replay` |
| Config | `src/config/file.rs` | Redis-style directive parser |
| Error | `src/error/kind.rs` | `FerrumError` (unified, 9 variants) |

## Hard Rules (Non-Negotiable)

### Code quality gates — run before declaring any task done
```bash
cargo fmt --all -- --check                          # formatting (matches CI)
cargo clippy --all-targets --all-features -- -D warnings   # zero warnings allowed
cargo check --all-features                          # compiles
cargo test --all-targets --all-features            # all unit + integration green
cargo bench --no-run --all-features                # benches must compile
```
CI runs these (fmt, clippy, test, bench) on every PR. Do not push code
that fails any of them.

### Binary safety is sacred
Keys and values are `Vec<u8>`, never assume UTF-8. Do not introduce
`String`/`str` conversions in the data path. `NUL`, `\r\n`, and non-UTF-8
sequences must round-trip through network + AOF unchanged.

### Error handling
- All fallible operations return `Result<_, FerrumError>`.
- Never `.unwrap()` on runtime data; `.unwrap()` is only acceptable where a
  prior arity/guard check guarantees safety.
- Panics in the data path are bugs. Lock poisoning propagates via
  `From<PoisonError>` — use `?`, don't catch.
- `FerrumError::Display` text **is** the client-facing `-ERR` message — write
  it for operators, not for debugging.

### RESP2 / Redis compatibility
- FerrumKV speaks RESP2 only. Inline commands are rejected (first byte must
  be `*`).
- Redis is the reference semantics. When adding/changing a command, check
  Redis behavior first; deviations must be documented in README and have a
  test asserting the chosen behavior.
- Known incompatibilities are tracked in `.issues/`.

### Persistence correctness
- Any path that removes a key (`del_many`, `sweep_expired`,
  `enforce_memory_limit`, overwrite via `set`) must call `track_remove` AND
  `aof.append_del` (if AOF enabled). This is the cross-cutting invariant.
- AOF writes are RESP2 frames, same bytes as wire protocol. Replay uses a
  separate `read_record` (not `parse_frame`) because it needs Seek + precise
  `consumed` accounting — do not unify them.

### Concurrency
- `KvEngine` interior state: `Arc<RwLock<HashMap>>` + `AtomicU64` counters
  + `Arc<Mutex<AdaptiveHybridState>>`.
- The expire sweeper and AOF background flusher run on **dedicated OS
  threads** (`thread::spawn`), not tokio tasks — do not move them into the
  tokio pool.
- New shared state on `KvEngine` should default to lock-free atomics where
  possible; fall back to `RwLock` only when atomic ops can't express the
  invariant.

## Commit Conventions

- Conventional Commits prefixes: `feat:`, `fix:`, `test:`, `docs:`,
  `refactor:`, `bench:`, `chore:`, `ci:`.
- **Commit messages MUST be in English.** No Chinese in the subject line
  or body. Code comments may remain Chinese; commit metadata is English-only.
- **Subject line**: imperative mood (`Add ...`, not `Added ...` / `Adds ...`),
  lowercase after the prefix, no trailing period, ≤ 72 chars total
  including the prefix. Example: `fix(persistence): sync version to 0.4.0`.
- **Scope**: include a Conventional Commits scope when the change is
  localized to one module, e.g. `fix(persistence):`, `refactor(storage):`,
  `feat(network):`. Omit the scope for cross-cutting changes (`docs:`, `chore:`).
- **Body**: wrapped at 72 columns, blank line after the subject. Explain
  **what changed and why**, not how the code works. One logical change per
  commit. Reference issues as `FERRUM-NNN` when relevant.
- **Footer**: use `BREAKING CHANGE: <description>` for backwards-incompatible
  changes. Do not add `Co-Authored-By` trailers unless the user explicitly asks.
- **Verification trailer**: for non-trivial changes, end the body with a
  `Verified:` line listing the fast checks run. Not required for pure-docs
  or typo commits.

### Example commit

```
fix(persistence): sync version to 0.4.0 and guard AOF-wire frame parity

Address project analysis risks R2 and R3.

R2 — version sync: bump Cargo.toml 0.3.0 -> 0.4.0 to align with the
README badge that has advertised v0.4.0 since the tokio runtime work.

R3 — AOF/wire frame parity: add 6 cross-cutting tests in
persistence/resp proving that bytes produced by encode_command (the AOF
on-disk format) are accepted by protocol::parser::parse_frame and
round-trip to the same Command, including binary-safe (NUL, embedded
CRLF, high bytes) and multi-key cases. Guards the invariant that AOF and
the wire protocol share identical RESP2 frame semantics, even though
replay uses a separate read_record (Seek + precise consumed accounting).

Verified: cargo fmt --check, cargo clippy --all-targets -- -D warnings,
cargo test --all-targets all green (199 lib + 36 doc + integration).
```

## Pull Request Conventions

All changes to `master` MUST go through a pull request. No direct pushes
to `master`. A PR is the unit of review; one PR addresses one cohesive
concern — not a grab-bag of unrelated edits.

### PR title

- Conventional Commits format, same rules as commit subject lines
  (English, imperative, lowercase after prefix, ≤ 72 chars, with scope
  when localized).
- The PR title becomes the squash-merge commit subject by default, so it
  must stand alone without reading the body.
- Example: `fix(persistence): sync version to 0.4.0 and guard AOF-wire frame parity`

### PR body

Markdown, wrapped ~72 columns, MUST contain the sections below in order.
Skip a section only when it is genuinely N/A (say so explicitly).

1. **Summary** — one paragraph: what the PR does and which issue(s)/risk(s)
   it addresses (`Addresses FERRUM-NNN`, `Addresses project analysis risk R2`).
2. **What changed** — grouped bullet list, each bullet pointing at the file
   or module. Distinguish behavioral changes from pure refactors/tests/docs.
3. **Why** — rationale. For a bug fix, include the root cause. For a
   refactor, explain the maintainability or performance motivation. For a
   deviation from Redis behavior, cite the Redis reference and the chosen
   behavior.
4. **Compatibility / risk** — backwards-incompatible changes,
   persistence-format implications (AOF replay across versions), or
   anything needing operator awareness. State `None` when there is none.
5. **Verification** — the exact fast-check commands run and their outcome.
   Never claim success without evidence. If a check was skipped, say so.
6. **Issue references** — `Closes FERRUM-NNN` / `Refs FERRUM-NNN` lines for
   GitHub auto-link. Omit if none.

### Example PR body

```
Addresses project analysis risks R2 and R3.

## What changed
- `Cargo.toml`: bump `version` 0.3.0 -> 0.4.0 to match the README badge.
- `src/persistence/resp.rs`: add 6 `aof_frame_is_wire_compatible_*` tests
  asserting `encode_command` output parses back through `parse_frame` to
  the same `Command` (SET, DEL multi-key, FLUSHDB, MSET, PEXPIREAT, and a
  binary-safe NUL/CRLF/high-byte case).

## Why
- R2: the README badge advertised v0.4.0 while Cargo.toml was stuck at
  0.3.0, confusing release tooling and consumers.
- R3: AOF replay uses a separate `read_record` (Seek + precise `consumed`
  accounting) rather than `parse_frame`. The two code paths must share
  identical RESP2 frame semantics; these tests guard that invariant so a
  future drift fails CI rather than corrupting replay.

## Compatibility / risk
None. Version bump has no runtime effect; the new tests are
compile-time-only additions.

## Verification
- `cargo fmt --check` — clean
- `cargo clippy --all-targets -- -D warnings` — zero warnings
- `cargo test --all-targets` — 199 lib + 36 doc + all integration suites
  green, 0 failed

Refs FERRUM-005
```

### Merge strategy

- Prefer **squash merge** for single-concern PRs so `master` history is
  one commit per PR. Use **rebase merge** when the PR's commit breakdown
  is meaningful to preserve. Avoid merge commits on top of merge commits.
- The branch must be up-to-date with `master` before merge (rebase if CI
  reports staleness).
- Delete the feature branch after merge.

## Branch Protection & AI Agent Conduct

These rules bind every contributor, human or AI.

### Branch protection (enforce on GitHub)

Configure the `master` branch with:

- **Require a pull request before merging** (no direct pushes).
- **Require approvals**: at least 1 for team repos; for solo/learning
  repos the owner may self-approve but the PR record is still mandatory.
- **Require status checks to pass** before merging: `fmt`, `clippy`,
  `test`, `bench --no-run`.
- **Require branches to be up to date** before merging.
- **Require linear history** (no merge commits on `master`).
- **Do not allow force pushes** to `master`.

### AI agent conduct (non-negotiable)

Any AI assistant working in this repo — AtomCode, Claude Code, Cursor,
GitHub Copilot, Aider, or any other — MUST:

1. **Never push directly to `master`/`main`.** All changes go on a feature
   branch (`fix/...`, `refactor/...`, `feat/...`, `test/...`, `docs/...`)
   and reach `master` only via a merged PR.
2. **Open a PR, do not self-merge.** After pushing a feature branch,
   either provide the `compare` URL + suggested Title/Body for the human
   to open the PR, or use `gh pr create` if the user explicitly asks. Wait
   for the user to approve the merge — never merge a PR you opened.
3. **Confirm before risky operations.** Force push (even on feature
   branches), `git reset --hard` on a shared branch, history rewrites,
   dependency removals, and deleting branches require explicit user
   approval in the current turn. State the risk and the rollback path
   before executing.
4. **Run the CI gates locally before declaring done**: `cargo fmt --all
   -- --check`, `cargo clippy --all-targets --all-features -- -D warnings`,
   `cargo check --all-features`, `cargo test --all-targets --all-features`,
   `cargo bench --no-run --all-features`. Report results faithfully; never
   claim a check passed without running it.
5. **Rebase, don't merge-commit, when syncing with master.** If a feature
   branch falls behind `master`, `git rebase master` and resolve conflicts
   locally rather than creating a merge commit.
6. **One concern per PR.** Do not bundle a refactor with a bug fix or an
   unrelated docs change. If a task spawns multiple concerns, split into
   multiple branches/PRs and say so.

If a previous turn violated rule 1 (direct push to `master`), the
corrective action is: force-with-lease `master` back to its pre-push
state, move the commits to a feature branch, and re-enter via PR — done
only with explicit user authorization, since it is a force push.

## Issue Tracker

Findings live in `.issues/NNN-<slug>.md` (see `.issues/README.md` for the
naming/format convention). When code review surfaces a new defect or doc
inaccuracy, record it as a new numbered issue before finishing. Mark
`status: fixed` only after the fix is verified by a fast check + a test.

## Skill / Workflow Defaults

| Task | Workflow |
|---|---|
| New command / feature | brainstorm → plan → TDD |
| Bug report / wrong output | reproduce before reading code |
| Before merge / finishing branch | code review → verification |
| Multi-file refactor | plan → execute task by task |

## Testing Conventions

- Unit tests are inline `#[cfg(test)] mod tests` in each module.
- Integration tests live in `tests/` (10 suites: aof, async, timeout,
  concurrency, eviction, expire, kv_engine, max_clients, resp2_wire,
  shutdown).
- Wire-protocol behavior is tested end-to-end in `tests/resp2_wire_test.rs`
  via a real TCP listener — prefer adding cases there over mocking.
- Benches use Criterion; run `cargo bench --no-run` in CI, full bench locally.

## Style

- 4-space indent, `cargo fmt` is authoritative.
- Public items have `///` doc comments; private helpers may use `//`.
- Match arms: exhaustiveness over `_ =>` catch-all wherever the enum is
  owned by this crate.
- No `unsafe` unless justified in a `// SAFETY:` comment; currently none.
