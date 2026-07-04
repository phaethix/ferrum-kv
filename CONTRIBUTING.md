# Contributing to FerrumKV

First off — thank you for taking the time to contribute! 🦀

FerrumKV is a from-scratch RESP2-compatible KV storage server built for
systems programming practice. Whether you are fixing a typo, reporting a
bug, or implementing a new command, your contribution is welcome.

This document is the single source of truth for how to participate.
Please read it before opening your first issue or pull request.

> **TL;DR** — fork → branch → `cargo fmt && cargo clippy && cargo test`
> → open a PR against `master` with a Conventional Commit title. The rest
> of this file explains each step.

## Code of Conduct

By participating you agree to uphold the standards in
[`CODE_OF_CONDUCT.md`](./CODE_OF_CONDUCT.md). Please report unacceptable
behaviour to the maintainers listed at the bottom of that file.

## Repository Layout

```
src/
  cli.rs            # argument parsing + engine wiring
  main.rs           # entry point, signal handlers
  config/           # Redis-style config file parser
  error/            # unified FerrumError (9 variants)
  network/          # TCP accept loop, shutdown, command dispatch
  persistence/      # AOF writer + replay
  protocol/         # RESP2 parser + encoder
  storage/          # KvEngine, eviction, expire
tests/              # 9 integration suites (real TCP listener)
benches/            # Criterion microbenchmarks
.issues/            # local issue tracker (see .issues/README.md)
.github/            # CI workflow + issue/PR templates
docs/               # development plan + whitepaper
```

A fuller architectural reference lives in `.atomcode.md` and the
top-level README.

## Before You Start

1. **Check existing issues.** Look in the [GitHub issue tracker][gh-issues]
   and the local [`.issues/`](./.issues/) directory — someone may already
   be working on it, or the behaviour may be a documented intentional
   deviation from Redis (tracked as `FERRUM-NNN`).
2. **Open an issue first for non-trivial work.** New commands, behaviour
   changes, or refactors larger than ~50 lines should be discussed in an
   issue before you write code, so we can agree on scope and avoid wasted
   effort.
3. **Pick a branch name** following the Conventional Commits type:
   `feat/<slug>`, `fix/<slug>`, `refactor/<slug>`, `test/<slug>`,
   `docs/<slug>`, `bench/<slug>`, `ci/<slug>`, `chore/<slug>`.

## Development Workflow

### 1. Fork & clone

```bash
gh repo fork phaethix/ferrum-kv --clone
cd ferrum-kv
```

### 2. Create a feature branch from `master`

```bash
git checkout master
git pull
git checkout -b feat/<your-slug>
```

### 3. Develop

FerrumKV follows **test-driven development** for new features and bug
fixes. Write a failing test that captures the desired behaviour, then
make it pass.

```bash
cargo test                         # unit + integration
cargo test --test resp2_wire_test  # one suite
cargo test binary_safe             # filtered tests
```

Wire-protocol behaviour should be verified end-to-end in
`tests/resp2_wire_test.rs` against a real TCP listener rather than with
mocks — prefer adding cases there.

### 4. Run the quality gates — ALL must pass before you push

```bash
cargo fmt --all                            # auto-format
cargo fmt --all -- --check                # verify no diff
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features
cargo bench --no-run --all-features       # benches must compile
```

CI runs these exact commands on every push and pull request. A PR that
fails any gate will not be merged. The project enforces `-D warnings`,
so even a stray `unused_import` blocks the build.

### 5. Commit with Conventional Commits

Each commit message must start with one of:

| Type | Use for |
|---|---|
| `feat:` | A new command, capability, or user-facing behaviour |
| `fix:` | A bug fix |
| `refactor:` | Code restructuring with no behaviour change |
| `test:` | Adding or improving tests |
| `docs:` | Documentation only |
| `bench:` | Benchmark additions or changes |
| `ci:` | CI/build pipeline changes |
| `chore:` | Releases, version bumps, dependency updates |

Optional scope: `feat(network): ...`. The title line stays ≤ 72 chars,
imperative mood (`add`, not `added`/`adds`). Reference issues in the body
as `FERRUM-NNN`:

```
feat(expire): add EXPIAT support

Implements EXPIAT as a unix-seconds variant of PEXPIREAT, mirroring
Redis semantics. Closes FERRUM-006.

Co-Authored-By: Jane Doe <jane@example.com>
```

Keep commits focused and atomic — one logical change per commit. Use
`git rebase -i master` to tidy history before pushing.

### 6. Push and open a Pull Request

```bash
git push -u origin feat/<your-slug>
```

Open the PR against `master`. Fill in the pull request template
(`.github/PULL_REQUEST_TEMPLATE.md`) — it asks for a summary, the
motivation, the test plan, and a checklist of the quality gates.

## Hard Rules (Non-Negotiable)

These are project-specific invariants. Violating them will block a PR
regardless of whether CI is green.

### Binary safety is sacred

Keys and values are `Vec<u8>`. **Never** assume UTF-8 in the data path
or introduce `String`/`str` conversions there. `NUL`, `\r\n`, and
non-UTF-8 sequences must round-trip through the network layer and the
AOF unchanged. The `binary_safe_*` and `non_utf8_*` tests guard this —
keep them passing.

### RESP2 / Redis compatibility

FerrumKV speaks RESP2 only. Inline commands are rejected (the first byte
must be `*`). Redis is the reference semantics: when you add or change a
command, check Redis behaviour first. Any intentional deviation must be
documented in the README and have a test asserting the chosen behaviour,
and be tracked in `.issues/`.

### Persistence correctness

Any code path that removes a key (`del_many`, `sweep_expired`,
`enforce_memory_limit`, overwrite via `set`) must call `track_remove`
**and** `aof.append_del` (if AOF is enabled). This is the cross-cutting
invariant — see `enforce_memory_limit` and `sweep_expired` for the
canonical pattern.

### Error handling

- All fallible operations return `Result<_, FerrumError>`.
- Never `.unwrap()` on runtime data. `.unwrap()` is only acceptable
  where a prior arity/guard check guarantees safety (the existing pattern
  in `build_command`).
- Panics in the data path are bugs. Lock poisoning propagates via
  `From<PoisonError>` — use `?`, don't catch.
- `FerrumError::Display` text **is** the client-facing `-ERR` message —
  write it for operators, not for debugging.

### No `unsafe`

No `unsafe` unless justified in a `// SAFETY:` comment. The project
currently has none.

## Style

- 4-space indent; `cargo fmt` is authoritative — do not hand-format.
- Public items have `///` doc comments; private helpers may use `//`.
- Match arms favour exhaustiveness over a `_ =>` catch-all wherever the
  enum is owned by this crate, so new variants force compiler-driven
  updates.
- Match the surrounding file's comment density — do not over-comment
  obvious code, and preserve existing (including Chinese) comments.

## Reporting Bugs

Open a [GitHub issue][gh-issues] using the **Bug report** template. A
good bug report includes:

1. FerrumKV version (`ferrum-kv --version` or the git commit).
2. The exact command used to start the server (flags matter — AOF,
   `--io-threads`, `--appendfsync`).
3. A minimal reproduction with `redis-cli` commands. Prefer real client
   input over hand-crafted RESP frames.
4. Expected vs actual output, with Redis behaviour as the reference
   baseline where applicable.
5. Logs / backtrace if the server crashed.

Security-sensitive bugs must **not** be filed as public issues — see
[`SECURITY.md`](./SECURITY.md) for responsible disclosure.

## Suggesting Enhancements

Open a GitHub issue using the **Feature request** template. Describe the
use case and why the existing commands do not cover it. If you can,
sketch the RESP2 response shape and the closest Redis analogue. Be
prepared to discuss scope before implementation begins.

## Issue Tracker

Findings discovered during code review are recorded in `.issues/` as
numbered `FERRUM-NNN` records (see [`.issues/README.md`](./.issues/README.md)
for the format). When a finding is promoted to a GitHub issue, the local
file becomes the detailed record and the GitHub issue holds the
discussion. Reference `FERRUM-NNN` in commit and PR bodies.

## Licensing

By contributing you agree that your changes will be licensed under the
project's [MIT license](./LICENSE). No further CLA is required.

## Maintainers

| Handle | Role |
|---|---|
| @phaethix | Lead maintainer |

Need help? Open an issue and mention `@phaethix`.

[gh-issues]: https://github.com/phaethix/ferrum-kv/issues
