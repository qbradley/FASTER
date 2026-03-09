# Decision: Performance Regression Detection Infrastructure

**Author:** Legolas (Performance Guru)
**Date:** 2026-03-09
**Branch:** `legolas/perf-regression-detection`
**Status:** Implemented — ready for review

## Summary

Added two scripts and documentation that provide the missing regression detection layer on top of the existing benchmark suite and CI baseline saves.

## What Changed

| Artifact | Purpose |
|----------|---------|
| `rust/scripts/bench-compare.sh` | Compare current benchmarks against saved baseline, flag regressions beyond threshold |
| `rust/scripts/bench-baseline.sh` | Save/list/compare/delete named baselines with git+machine metadata |
| `rust/docs/benchmarking.md` | Complete benchmarking guide for the team |

## Key Decisions

| Decision | Rationale |
|----------|-----------|
| 5% default regression threshold | Criterion's noise threshold is 2%; 5% provides headroom for system variance while catching real regressions |
| Exit code 1 for threshold violations | Enables CI gates: `bench-compare.sh || fail_pr` |
| Metadata in `target/criterion/.baselines/` | Metadata lives with ephemeral benchmark data, not in source control |
| JSON output mode (`--json`) | Machine-readable for CI pipelines and PR comment bots |
| No benchmark code changes | Bench code was fixed in Wave 1; this is purely wrapper tooling |

## Team Impact

- **All agents**: Use `bench-compare.sh` to verify performance impact of changes before merge
- **CI (Eowyn)**: Can integrate `bench-compare.sh --json` into PR workflow for automated regression gating
- **Testing architecture (Gandalf)**: `TESTING-ARCHITECTURE.md` updated, regression automation gap marked resolved
- **Benchmarks run on VM only** — this constraint is documented and enforced by convention, not by the scripts
