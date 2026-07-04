# Repository Settings

The GitHub repository settings below cannot be encoded in git, so they
are documented here for reproducibility and onboarding new maintainers.
Apply them under **Settings** on <https://github.com/phaethix/ferrum-kv>.

## Branch Protection — `master`

Settings → Branches → Branch protection rules → Add rule for `master`:

- ✅ Require a pull request before merging
  - Require approvals: **1** (raise to 2 for releases if the team grows)
  - Dismiss stale pull request approvals when new commits are pushed
  - Require review from Code Owners (`CODEOWNERS` enforces this)
- ✅ Require status checks to pass before merging
  - Require branches to be up to date before merging
  - Required checks:
    - `fmt + clippy + test` (the `check` job in `.github/workflows/ci.yml`)
- ✅ Require conversation resolution before merging
- ✅ Require linear history (rebase merges; no merge commits)
- ❌ Do **not** enable "Allow merge commits"
- ❌ Do **not** enable "Allow force pushes" on `master`
- ❌ Do **not** enable "Allow deletions"

## Merge Strategy

- **Squash and merge** for single-commit feature PRs, OR
- **Rebase and merge** for multi-commit PRs with a clean, atomic commit
  history (preferred for non-trivial features so the log stays reviewable).
- Commit titles must follow Conventional Commits (see `CONTRIBUTING.md`).
  GitHub's squash-merge default title is the PR title — keep the PR title
  Conventional-Commits–shaped so the squashed commit is well-formed.

## Permissions

| Team/role | Permission |
|---|---|
| Owner (@phaethix) | Admin |
| Contributors | Read (fork → PR model; no direct push to `master`) |

Direct push to `master` is blocked by branch protection. All changes
land via pull request.

## Issue & PR Templates

- Bug report: `.github/ISSUE_TEMPLATE/bug_report.yml`
- Feature request: `.github/ISSUE_TEMPLATE/feature_request.yml`
- PR template: `.github/PULL_REQUEST_TEMPLATE.md`
- Blank issues are disabled; the config
  (`.github/ISSUE_TEMPLATE/config.yml`) redirects security reports to
  `SECURITY.md` and questions to Discussions.

## Code Owners

`.github/CODEOWNERS` auto-requests review from `@phaethix` on every PR,
with the storage / persistence / protocol paths flagged as the
binary-safety-critical data path.

## Features to Enable

Settings → General:

- ✅ Issues
- ✅ Discussions (for Q&A and design threads — keeps issues actionable)
- ✅ Projects (optional)
- ❌ Wiki (documentation lives in `docs/` and README to avoid drift)
- ✅ Restrict editing to users with write access (for the issue tracker)

Settings → Security:

- ✅ Dependency graph
- ✅ Dependabot alerts
- ✅ Dependabot security updates
- ✅ Private vulnerability reporting (in addition to `SECURITY.md` email)
- ✅ Code scanning (CodeQL default config) — optional but recommended
