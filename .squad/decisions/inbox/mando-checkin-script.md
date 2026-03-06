# Decision: Mandatory `checkin` Script for All Commits

**Agent:** Mando (Rust Expert)
**Date:** 2026-03-07
**Status:** Proposed
**Impact:** All agents — workflow change for every Rust commit

## Decision

All agents MUST use `rust/scripts/checkin` instead of raw `git commit` for any
commit touching Rust code. The script enforces the full quality gate
(fmt → clippy → doctests → tests) before allowing the commit through.

## Rationale

1. **No more "oops, forgot clippy"** — the script runs every gate automatically
2. **Consistent ordering** — gates run in a fixed sequence (fast checks first)
3. **Co-authored-by trailer** — auto-appended, no manual copy-paste needed
4. **Fail-fast** — first failure aborts; clear error messages with fix hints
5. **Works from anywhere** — auto-detects repo root, `cd`s to `rust/`

## Usage Examples

```bash
# Full quality gate + commit
rust/scripts/checkin -m "feat: add epoch framework"

# Fast iteration (fmt + clippy only, no tests)
rust/scripts/checkin --skip-tests -m "wip: refactor allocator"

# Multiple -m flags work too
rust/scripts/checkin -m "fix: address overflow" -m "Fixes edge case in page_offset calculation"
```

## Rules

- **ALWAYS** use `checkin` instead of `git commit` for Rust changes
- The script runs `git add -A` automatically — no need to stage manually
- Use `--skip-tests` only during rapid iteration; final commits must pass all gates
- If the script fails, fix the reported issue and re-run — do not bypass

## What It Enforces

| Order | Gate | Purpose |
|-------|------|---------|
| 1 | `cargo fmt --check` | Code formatting |
| 2 | `cargo clippy -D warnings` | Lint-free code |
| 3 | `cargo test --doc` | Doc examples compile and pass |
| 4 | `cargo nextest run` | All unit + integration tests pass |

## Location

`rust/scripts/checkin` (executable, works from any directory in the repo)
