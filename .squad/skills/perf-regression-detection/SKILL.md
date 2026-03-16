# Performance Regression Detection Skill

## When to Use
Running benchmarks in CI, before releases, or comparing branches to detect >5% throughput regressions.

## Pattern

### Quick Comparison Workflow
```bash
cd rust

# 1. Save baseline from main branch
git checkout main
scripts/bench-baseline.sh save main

# 2. Switch to feature branch
git checkout feature/my-optimization

# 3. Compare (runs benchmarks + flags regressions)
scripts/bench-compare.sh --baseline main --threshold 5
# Exit code 1 if any regression exceeds threshold
```

### Release Baseline Workflow
```bash
cd rust

# Before tagging release
scripts/bench-record-baseline.sh v0.1.0
# Stores in rust/baselines/ with git metadata

# After release, during next dev cycle
scripts/bench-release-compare.sh v0.1.0 --threshold 5
# Generates markdown report + exit code
```

### Interpreting Criterion Comparison Output
Criterion prints:
```
time:   [143.21 ms 145.67 ms 148.33 ms]
change: [-31.2% -28.4% -25.6%]  ← [low mid high] confidence interval
        Performance has improved.
```

The 3-line block = (time estimate, change range, verdict).
- **Regression**: positive change % + "regressed"
- **Improvement**: negative change % + "improved"
- **Stable**: "within noise threshold"

### Threshold Calibration
- **5% (default)**: Balances noise tolerance with meaningful regressions
- **3%**: Stricter, for release gates
- **10%**: Looser, for noisy VMs or early development

### Baseline Metadata
Stored in `target/criterion/.baselines/{name}.meta.json`:
```json
{
  "name": "main",
  "git_commit": "2d667519",
  "timestamp": "2026-03-09T18:46:00Z",
  "machine": "Standard D8ds v4 (8 vCPU, 32 GB)"
}
```

### CI Integration
```bash
# Tier 3 release gate (deep validation)
scripts/release-gate --tier 3
# Internally calls bench-release-compare.sh

# Fast CI smoke test (compile + run, no timing)
scripts/bench-ci-smoke.sh
```

## Anti-Patterns
- **Comparing baselines across machines**: CPU/cache differences dominate. Record per-machine baselines.
- **Ignoring CPU contention**: Check `uptime` before benchmarking. Load avg >80% causes false regressions.
- **Criterion baseline name confusion**: Baselines are stored PER-BENCHMARK in `target/criterion/{bench_name}/{baseline_name}/`. Scripts handle traversal.
- **Running in debug mode**: Always use `--release` for performance testing.

## Key Files
- `rust/scripts/bench-compare.sh` — Comparison engine
- `rust/scripts/bench-baseline.sh` — Baseline management
- `rust/scripts/bench-record-baseline.sh` — Release baselines with git metadata
- `rust/scripts/bench-release-compare.sh` — Release comparison with markdown report
- `rust/docs/benchmarking.md` — Full documentation

## Confidence
**High** — Production-tested across 3 benchmark waves and integrated into release-gate (Tier 3).

## Learned From
- Performance Regression Detection System Wave 1+2 (2026-03-09): Initial infrastructure
- Wave 3 Benchmark Baseline Infrastructure (2026-03-09T1846Z): Release workflow integration
- Post-Backlog-Sprint Benchmark (2026-03-11): CPU contention false-positive diagnosis
