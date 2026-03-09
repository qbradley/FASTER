# Benchmark Baselines — FASTER Rust

This directory stores versioned benchmark baselines for performance tracking
across releases. Baselines are checked into git for reproducibility.

> ⚠️ **Baselines are recorded on a dedicated VM only.** Never record baselines
> on local dev machines — results would be meaningless for comparison.

---

## Directory Structure

```
rust/baselines/
├── README.md               ← this file
├── .gitkeep                ← placeholder for empty directory
└── v0.1.0/                 ← one directory per release
    ├── metadata.json       ← machine info, git SHA, Rust version, timestamp
    ├── summary.json        ← per-benchmark timing data (machine-readable)
    ├── report.md           ← human-readable markdown report
    └── criterion/          ← raw criterion output for detailed comparison
        ├── faster-core.log ← full criterion console output
        ├── faster-bench.log
        └── <bench_name>/   ← criterion estimates per benchmark
            └── estimates.json
```

### metadata.json

```json
{
  "version": "v0.1.0",
  "timestamp": "2026-03-10T12:00:00Z",
  "git": { "sha": "abc123...", "branch": "main", "dirty": false },
  "rust": { "rustc": "rustc 1.85.0 ..." },
  "machine": { "cpu": "...", "cores": 16, "memory_gb": 64.0, "os": "..." }
}
```

### summary.json

```json
{
  "version": "v0.1.0",
  "total_benchmarks": 49,
  "benchmarks": [
    { "name": "faster_hash_u64", "suite": "faster-core", "time_mid": 1.256, "unit": "ns" }
  ]
}
```

---

## Recording a New Baseline

Record a baseline when preparing a release:

```bash
# Record baseline for a release
rust/scripts/bench-record-baseline.sh v0.2.0

# Quick mode (fewer samples — useful for drafts, not final baselines)
rust/scripts/bench-record-baseline.sh v0.2.0 --quick

# Filter to specific benchmarks
rust/scripts/bench-record-baseline.sh v0.2.0 --bench ycsb
```

After recording, commit the baseline:

```bash
git add rust/baselines/v0.2.0/
git commit -m "perf: record benchmark baseline for v0.2.0"
```

---

## Comparing Against a Baseline

Compare current code against a stored baseline:

```bash
# Full comparison (exit code 0 = pass, 1 = regressions found)
rust/scripts/bench-release-compare.sh v0.1.0

# Custom regression threshold (default: 5%)
rust/scripts/bench-release-compare.sh v0.1.0 --threshold 10

# JSON output for CI integration
rust/scripts/bench-release-compare.sh v0.1.0 --json

# Quick comparison (fewer samples)
rust/scripts/bench-release-compare.sh v0.1.0 --quick
```

The comparison script:
1. Restores stored criterion data into `target/criterion/`
2. Runs all benchmark suites with `--baseline <version>`
3. Parses criterion's statistical comparison output
4. Flags regressions exceeding the threshold
5. Returns exit code 1 if any regressions found

---

## How Baselines Relate to Releases

| Event | Action | Script |
|-------|--------|--------|
| Pre-release | Record baseline on VM | `bench-record-baseline.sh v0.X.0` |
| Release gate | Compare candidate vs previous baseline | `bench-release-compare.sh v0.X-1.0` |
| Post-release | Commit baseline to git | `git add baselines/v0.X.0/` |
| PR review | Compare against last release | `bench-release-compare.sh v0.X.0` |

### Workflow: Record → Compare → Gate

```
1. Developer prepares release candidate
2. VM runs: bench-record-baseline.sh v0.2.0
3. VM runs: bench-release-compare.sh v0.1.0
4. If exit code 0 → release proceeds
5. If exit code 1 → investigate regressions before release
6. Baseline committed to git for future comparisons
```

---

## CI Smoke Test

A quick smoke test verifies benchmarks compile and run without measuring
performance:

```bash
rust/scripts/bench-ci-smoke.sh
```

This runs in <30 seconds and is safe for every CI build. It does NOT record
or compare baselines.

---

## Storage Policy

- Baselines are **checked into git** for reproducibility and traceability
- Each release gets one baseline directory
- Old baselines can be pruned if the repo grows too large, but keep at least
  the last 3 releases for comparison
- The `criterion/` subdirectory contains raw data — these files are larger
  but enable re-comparison without re-running benchmarks

---

## Scripts Reference

| Script | Purpose |
|--------|---------|
| `bench-record-baseline.sh` | Record a full baseline for a version |
| `bench-release-compare.sh` | Compare current code against a stored baseline |
| `bench-ci-smoke.sh` | Quick CI smoke test (compile + minimal run) |
| `bench-baseline.sh` | Manage criterion baselines (save/list/compare/delete) |
| `bench-compare.sh` | Ad-hoc regression detection against criterion baselines |

---

*Maintained by Legolas (Performance Guru). Last updated: 2026-03-10.*
