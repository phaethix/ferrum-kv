<!--
Thanks for contributing! Read CONTRIBUTING.md first.
Keep the title a Conventional Commit: feat(scope): ... / fix(scope): ... /
refactor(scope): ... / test: / docs: / bench: / ci: / chore: ...
Reference issues as FERRUM-NNN in the body when relevant.
-->

## Summary

<!-- One or two sentences: what does this PR do? -->

## Motivation

<!-- Why? Link the issue (Closes #NN / FERRUM-NNN) or describe the problem. -->

## Changes

<!-- Bullet list of the substantive changes. Call out anything tricky. -->

-

## Compatibility Check

FerrumKV targets RESP2 / Redis semantics. If this change touches wire
behaviour or command semantics:

- [ ] This matches Redis behaviour exactly.
- [ ] This is an intentional deviation — documented in README **and**
      asserted by a test, and tracked as a `FERRUM-NNN` issue.
- [ ] N/A (no wire/semantic change).

## Quality Gates — all must pass before requesting review

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features
cargo bench --no-run --all-features
```

- [ ] `cargo fmt --check` clean
- [ ] `cargo clippy -- -D warnings` clean
- [ ] `cargo test` green
- [ ] `cargo bench --no-run` compiles
- [ ] No new `unsafe` (or each block has a `// SAFETY:` justification)
- [ ] Binary safety preserved — no `String`/`str` in the data path
- [ ] Key-removal paths call `track_remove` + `aof.append_del` (if AOF)

## Test Plan

<!-- How did you verify this? New/updated test names, manual repro. -->

-
