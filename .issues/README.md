# FerrumKV Issue Tracker

Local issue records for FerrumKV, tracked outside the GitHub issue tracker
for portability and version control alongside the code.

## Directory Layout

```
.issues/
├── README.md          # this file — naming & format conventions
├── 001-<slug>.md      # individual issue records
└── 002-<slug>.md
```

## File Naming Convention

```
<NNN>-<kebab-case-slug>.md
```

| Part | Rule |
|---|---|
| `NNN` | 3-digit zero-padded sequence number, assigned in discovery order (`001`, `002`, …). Leave gaps intentionally when reserving for PR-linked issues. |
| `<slug>` | Lowercase kebab-case, verb-or-noun phrase, ≤ 60 chars, ASCII only. Describes the symptom, not the fix (e.g. `exists-rejects-multi-key`, not `fix-exists-arity`). |
| Extension | `.md` (GitHub-Flavored Markdown). |

**Examples**

- `001-exists-rejects-multi-key.md`
- `002-info-keyspace-ttl-counts-hardcoded-zero.md`
- `003-eviction-sampling-not-true-random.md`

The filename sorts lexicographically by sequence, mirroring GitHub issue
numbers, so `ls .issues/` reads in creation order.

## File Format

Each issue file is GitHub-Flavored Markdown with a YAML front-matter block
at the top (parsed by GitHub's automatic renderer and most static-site
tooling) followed by structured sections.

### Front Matter

```yaml
---
id: FERRUM-NNN               # stable identifier, matches the NNN in filename
title: "<short summary>"      # ≤ 80 chars, no quotes needed unless it contains :
severity: critical | high | medium | low | info
status: open | in-progress | fixed | wontfix | duplicate
component: protocol | network | storage | persistence | config | error | ci | docs
found_date: YYYY-MM-DD        # ISO 8601 date of discovery
reporter: <name or handle>
---
```

### Body Sections

| Section | Required | Purpose |
|---|---|---|
| `## Summary` | yes | One-sentence problem statement. |
| `## Steps to Reproduce` | yes | Copy-pasteable shell commands. Prefer `redis-cli` over hand-crafted RESP frames. Include server startup flags. |
| `## Expected vs Actual` | yes | Side-by-side table; use Redis behaviour as the reference baseline where applicable. |
| `## Root Cause` | yes | `file:line` references and the offending code snippet. |
| `## Impact` | yes | Who/what is affected; blast radius. |
| `## Suggested Fix` | yes | Implementation approach, files to touch, risk assessment. |
| `## Verification` | yes | Commands or test names that confirm the fix. |
| `## Metadata` | yes | Repeats the YAML block as a fenced code block for quick scanning. |

## Severity Definitions

| Severity | Meaning |
|---|---|
| `critical` | Data loss, corruption, security vulnerability, or crash. Block release. |
| `high` | Core functionality broken with no workaround. |
| `medium` | Functional incompatibility or wrong output with a workaround. |
| `low` | Observability inaccuracy, cosmetic, or edge-case-only. |
| `info` | Non-bug: design note, improvement suggestion, or documentation gap. |

## Status Lifecycle

```
open → in-progress → fixed
                ↘ wontfix
                ↘ duplicate
```

When marking `fixed`, reference the commit/PR hash in the `## Metadata`
block and add a `fixed_date` field.

## Relationship to GitHub Issues

These files are the source of truth for findings discovered during code
review. When a finding is promoted to a GitHub issue, add a
`github_issue: #NN` field to the front matter and keep the local file as
the detailed record (GitHub issues hold the discussion, this file holds
the structured repro and root-cause analysis).

## Current Issues

| ID | Severity | Status | Title |
|---|---|---|---|
| FERRUM-001 | medium | open | EXISTS rejects multi-key requests |
| FERRUM-002 | low | open | INFO keyspace TTL counts hardcoded to zero |
| FERRUM-003 | low | fixed | Issues README example describes non-existent bug |
| FERRUM-004 | low | fixed | Repro scripts leave orphan server processes |
