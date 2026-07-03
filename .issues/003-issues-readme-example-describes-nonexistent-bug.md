---
id: FERRUM-003
title: "Issues README example describes a non-existent AOF replay bug"
severity: low
status: fixed
component: docs
found_date: 2026-07-04
reporter: AtomCode
github_issue: "#N/A"
---

## Summary

The `.issues/README.md` naming-convention section used `003-aof-replay-skips-corrupt-tail-silently.md`
as an example filename, but the described behavior ("silently skips corrupt tail")
does not match the actual implementation — AOF replay **physically truncates**
the corrupt tail and records `truncated_tail=true`, which is correct behavior,
not a bug. The example could mislead readers into believing this is the next
issue to fix.

## Steps to Reproduce

```bash
# Read the README example
sed -n '31p' .issues/README.md
# => - `003-aof-replay-skips-corrupt-tail-silently.md`

# Compare against actual replay behavior (replay.rs:55-144)
grep -n "set_len\|truncated_tail" src/persistence/replay.rs
# => 75: stats.truncated_tail = true;
#    136: file.set_len(last_good)
```

The replay code truncates the corrupt tail and flags it — it does not
"silently skip" anything.

## Expected vs Actual

| | Behavior |
|---|---|
| **README example implies** | AOF replay silently skips corrupt tail (a bug) |
| **Actual code** | AOF replay truncates corrupt tail to `last_good` and sets `truncated_tail=true` (correct) |

## Root Cause

`.issues/README.md:31` — example filename was chosen for format illustration
without verifying it described a real issue:

```markdown
- `003-aof-replay-skips-corrupt-tail-silently.md`
```

## Impact

- Readers of the issue tracker conventions may mistakenly believe AOF replay
  has an unresolved silent-skip bug.
- Could waste implementer time investigating a non-existent defect.
- Undermines trust in the issue tracker's accuracy.

## Suggested Fix

Replace the example with one describing a real design observation, or remove
the `003` example entirely. The replacement should be a genuine finding from
code review.

**Risk**: None. Documentation-only change.

## Verification

```bash
# After fix, the README example should describe a real finding or be absent
grep -n "003-" .issues/README.md
# expect: a real finding slug, or no match
```

## Metadata

```yaml
id: FERRUM-003
severity: low            # documentation accuracy, no code impact
status: fixed            # fixed in same review session
component: docs
found_date: 2026-07-04
fix: replaced with real eviction-sampling observation
```
