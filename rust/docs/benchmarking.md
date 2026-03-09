# Benchmarking Guide — FASTER Rust

> ⚠️ **Run benchmarks on a dedicated VM only.** Never run benchmarks on local
> dev machines — thermal throttling, background processes, and scheduler noise
> produce unreliable results.

---

## Quick Reference

| What | Command | Time |
|------|---------|------|
| Save baseline | `rust/scripts/bench-baseline.sh save main` | ~5 min |
| Compare against baseline | `rust/scripts/bench-compare.sh --baseline main` | ~5 min |
| Compare with custom threshold | `rust/scripts/bench-compare.sh --threshold 10` | ~5 min |
| List saved baselines | `rust/scripts/bench-baseline.sh list` | instant |
| Compare two baselines | `rust/scripts/bench-baseline.sh compare v1 v2` | ~5 min |
| Quick comparison (fewer samples) | `rust/scripts/bench-compare.sh --quick` | ~2 min |
| Run a single benchmark suite | `cargo bench -p faster-core --bench ycsb` | ~2 min |
| JSON output for CI | `rust/scripts/bench-compare.sh --json` | ~5 min |

---

## 1. Benchmark Suites

Three Criterion benchmark suites cover different performance dimensions:

### core_benchmarks.rs — Micro-benchmarks (34 benchmarks)

Low-level component performance. Use these to detect regressions in
individual building blocks.

| Group | What it measures |
|-------|-----------------|
| Hash functions | `faster_hash_u64`, `faster_hash_bytes` (12B–4KB), `Hashable` trait |
| Hash bucket entries | `Entry::new`, tag/address extraction, atomic CAS |
| Hash bucket operations | `find_entry` (hit/miss), `try_insert`, fill-level sweep |
| Allocator | Bump allocation, free-list reuse, throughput |
| Overflow chains | Traversal depth 0–5, find/insert in chain |
| Epoch framework | Protect/unprotect, bump, bump-with-drain |
| Record serialization | Write/read for fixed (`u64`) and variable-length records |
| Hash index | Sequential/concurrent insert, find hit/miss, mixed R/W |
| Compaction | `record_size_from_bytes`, `eq_from_bytes` for fixed + variable |

### ycsb.rs — Workload Benchmarks (11 benchmarks)

End-to-end KV store throughput under realistic access patterns.

| Benchmark | Pattern |
|-----------|---------|
| `sequential_upsert` | 100K sequential writes |
| `sequential_read` | 100K sequential reads |
| `mixed_50_50` | 50% read / 50% write (YCSB Workload A) |
| `read_heavy_95_5` | 95% read / 5% write (YCSB Workload B) |
| `rmw` | 100K read-modify-write operations |
| `multithread_upsert` | Upsert with 1/2/4/8 threads |
| `read_latency` | Per-operation read latency |
| `batch_upsert` | Batched upsert with prefetch pipeline |
| `batch_read` | Batched read with prefetch pipeline |
| `batch_mixed_50_50` | Batched mixed workload |
| `batch_rmw` | Batched RMW |

### hash_layout_bench.rs — Hash Table Layout (4 benchmarks)

Isolated hash index lookup performance to detect layout regressions.

| Benchmark | What it measures |
|-----------|-----------------|
| `faster_find/{3K,12K,50K}` | Raw `find_entry()` at various table sizes |
| `faster_prefetch_find/{3K,12K,50K}` | Prefetch + find (L1 prefetch effect) |
| `faster_batch16` | Batched prefetch + find (realistic 16-op batch) |

---

## 2. Saving Baselines

A baseline is a named snapshot of benchmark results. CI automatically saves
a baseline named `main` on every push to the main branch (see
`.github/workflows/rust-ci.yml`).

### Save a baseline manually

```bash
# Save as 'main' (default)
rust/scripts/bench-baseline.sh save main

# Save with a descriptive name
rust/scripts/bench-baseline.sh save before-refactor

# Quick mode (fewer samples, less reliable but faster)
rust/scripts/bench-baseline.sh save quick-check --quick
```

The script:
1. Runs `cargo bench -p faster-core` with `--save-baseline <name>`
2. Writes metadata (git commit, timestamp, machine info) to
   `target/criterion/.baselines/<name>.meta.json`

### List and inspect baselines

```bash
# List all saved baselines
rust/scripts/bench-baseline.sh list

# Show detailed metadata for a baseline
rust/scripts/bench-baseline.sh info main

# Delete a baseline
rust/scripts/bench-baseline.sh delete old-baseline
```

---

## 3. Detecting Regressions

The `bench-compare.sh` script runs benchmarks and compares against a saved
baseline, flagging any regressions that exceed a configurable threshold.

### Basic comparison

```bash
# Compare current code against the 'main' baseline (default)
rust/scripts/bench-compare.sh

# Compare against a specific baseline
rust/scripts/bench-compare.sh --baseline before-refactor

# Custom threshold (default is 5%)
rust/scripts/bench-compare.sh --threshold 10

# Only run hash-related benchmarks
rust/scripts/bench-compare.sh --bench hash

# Quick mode + JSON output (for CI)
rust/scripts/bench-compare.sh --quick --json
```

### Exit codes

| Code | Meaning |
|------|---------|
| 0 | All benchmarks within threshold |
| 1 | One or more regressions exceed threshold |
| 2 | Script error (missing baseline, build failure) |

### Understanding the output

```
═══ Regression Analysis ═══

Benchmark                                           Change    CI Low   CI High  Status
──────────────────────────────────────────────────  ────────  ────────  ────────  ──────────
faster_hash_u64                                      +0.23%    -1.20%    +1.66%  ⚪ no change
ycsb/sequential_upsert/100K_ops                      +6.74%    +4.50%    +8.98%  🔴 REGRESSED
ycsb/batch_read/100K_ops                             -3.21%    -5.10%    -1.32%  🟢 improved

── Summary ──
  Total compared:       34
  Threshold violations: 1 (>5% regression)
  Improvements:         5
  Within noise:         28

❌ FAIL: 1 benchmark(s) regressed beyond 5% threshold

Regressions exceeding threshold:
  • ycsb/sequential_upsert/100K_ops (+6.74%)
```

**Status indicators:**
- 🔴 **REGRESSED** — Exceeds the threshold. Investigate before merging.
- 🟡 **regressed** — Criterion detected a regression but within threshold.
- 🟢 **improved** — Performance improved.
- ⚪ **no change** — Within noise / no significant change.

**Columns:**
- **Change**: Point estimate of performance change (positive = slower).
- **CI Low / CI High**: 95% confidence interval bounds.
- **Status**: Classification based on threshold and Criterion's own analysis.

### Comparing two named baselines

```bash
# Compare two previously saved baselines directly
rust/scripts/bench-baseline.sh compare v1.0 v2.0
```

---

## 4. CI Integration

### Current CI behavior (`.github/workflows/rust-ci.yml`)

The `bench` job runs on every push to `main`:
1. Checks out the code
2. Runs `cargo bench --workspace --no-fail-fast -- --save-baseline main`
3. This saves the baseline as `main` in the criterion cache

### Adding regression detection to PRs

To add regression detection to the PR workflow, add a step that:
1. Restores the `main` baseline from cache
2. Runs `bench-compare.sh` against it
3. Posts results as a PR comment

Example CI step:
```yaml
- name: Compare benchmarks
  run: |
    rust/scripts/bench-compare.sh \
      --baseline main \
      --threshold 5 \
      --json > bench-results.json
```

---

## 5. Methodology

### Criterion configuration

All benchmarks use [Criterion.rs](https://bheisler.github.io/criterion.rs/book/)
v0.5 with the following defaults:

| Parameter | Default | Notes |
|-----------|---------|-------|
| Sample size | 100 | `hash_layout_bench` uses 10 (large dataset setup) |
| Warm-up time | 3s | `ycsb.rs` uses 1s (longer individual iterations) |
| Measurement time | 5s | `hash_layout_bench` uses 3s |
| Noise threshold | 2% | Changes below this are classified as noise |
| Confidence level | 95% | For confidence interval bounds |

### Custom `main()` pattern

All three benchmark files use a custom `main()` instead of `criterion_main!()`:

```rust
fn main() {
    if std::env::args().any(|a| a == "--bench") {
        benches();  // Full criterion run
    } else {
        // Smoke test — fast validation for nextest
    }
}
```

This prevents nextest from running full benchmarks when it discovers the
benchmark binaries via `--all-targets`. The smoke test validates that setup
code doesn't panic, completing in <1 second.

### Baseline naming conventions

| Name | When saved | Purpose |
|------|-----------|---------|
| `main` | CI push to main | Golden baseline for regression detection |
| `current` | `bench-compare.sh` runs | Temporary, overwritten each comparison |
| `<descriptive>` | Manual `bench-baseline.sh save` | Named snapshots for A/B analysis |

### Reducing noise

For reliable benchmark results:

1. **Use a dedicated VM** — no shared resources, no desktop environment
2. **Pin CPU frequency** — disable turbo boost and frequency scaling
3. **Minimize background processes** — stop unnecessary services
4. **Run multiple times** — criterion's statistical analysis handles variance,
   but extreme outliers from system interference cannot be corrected
5. **Use `--quick` sparingly** — fewer samples means wider confidence intervals

```bash
# Optional: pin CPU frequency on Linux (requires root)
echo performance | sudo tee /sys/devices/system/cpu/cpu*/cpufreq/scaling_governor

# Optional: disable turbo boost on Intel
echo 1 | sudo tee /sys/devices/system/cpu/intel_pstate/no_turbo
```

---

## 6. Troubleshooting

### "No saved baseline found"

The comparison script needs a pre-existing baseline. Save one first:
```bash
rust/scripts/bench-baseline.sh save main
```

Or if you're running on a fresh CI runner, ensure the criterion cache is
restored from a previous `main` branch run.

### Benchmarks take too long

Use `--quick` for faster (but noisier) results:
```bash
rust/scripts/bench-compare.sh --quick
```

Or filter to specific benchmarks:
```bash
rust/scripts/bench-compare.sh --bench "ycsb"
```

### Large confidence intervals

Wide CI bounds mean high variance. Check for:
- Background CPU load (`top`, `htop`)
- Thermal throttling (`sensors`, check CPU frequency)
- Memory pressure (swapping invalidates all results)

### Criterion reports "No change" but performance feels different

Criterion's noise threshold is 2%. Changes below this are classified as noise
even if statistically significant. For sub-2% regressions, use longer
measurement times or increase sample sizes by editing the benchmark file.

---

## 7. Release Benchmarking

For release gating, baselines are recorded and stored as version-controlled
artifacts in `rust/baselines/`. This provides reproducible, cross-release
comparison that survives CI cache eviction.

### Workflow: Record → Compare → Gate

```
1. Prepare release candidate on dedicated VM
2. Record baseline:     rust/scripts/bench-record-baseline.sh v0.2.0
3. Compare vs previous: rust/scripts/bench-release-compare.sh v0.1.0
4. If exit code 0 → release proceeds
5. If exit code 1 → investigate regressions before releasing
6. Commit baseline:     git add rust/baselines/v0.2.0/ && git commit
```

### Recording a release baseline

```bash
# Record full baseline for a release version
rust/scripts/bench-record-baseline.sh v0.2.0

# Quick mode for draft baselines (fewer samples)
rust/scripts/bench-record-baseline.sh v0.2.0 --quick
```

This runs ALL benchmark suites (faster-core + faster-bench) and saves:
- `metadata.json` — machine info, git SHA, Rust version, timestamp
- `summary.json` — per-benchmark timing data (machine-readable)
- `report.md` — human-readable markdown report
- `criterion/` — raw criterion output for detailed re-comparison

### Comparing against a release baseline

```bash
# Compare current code against stored baseline
rust/scripts/bench-release-compare.sh v0.1.0

# Custom threshold (default: 5%)
rust/scripts/bench-release-compare.sh v0.1.0 --threshold 10

# JSON output for CI integration
rust/scripts/bench-release-compare.sh v0.1.0 --json
```

The comparison script restores criterion data from `rust/baselines/`, runs
benchmarks, and flags regressions. Exit code 1 means regressions were found.

### CI smoke test

A quick smoke test verifies benchmarks compile and execute without measuring
performance:

```bash
rust/scripts/bench-ci-smoke.sh
```

This completes in <30 seconds and catches compilation failures, data setup
panics, and configuration errors. Safe for every CI build.

### Integration with release-gate

The release-gate script should call `bench-release-compare.sh` as one of its
gates. The exit code (0 = pass, 1 = fail) integrates directly:

```bash
# In release-gate.sh or CI pipeline:
if ! rust/scripts/bench-release-compare.sh "$PREVIOUS_VERSION" --threshold 5; then
    echo "Performance regression detected — blocking release"
    exit 1
fi
```

### Baseline storage

Baselines are checked into git under `rust/baselines/`. See
[`rust/baselines/README.md`](../baselines/README.md) for the format
specification and storage policy.

---

## Quick Reference (Complete)

| What | Command | Time |
|------|---------|------|
| Record release baseline | `rust/scripts/bench-record-baseline.sh v0.X.0` | ~10 min |
| Compare vs release baseline | `rust/scripts/bench-release-compare.sh v0.X.0` | ~10 min |
| CI smoke test | `rust/scripts/bench-ci-smoke.sh` | <30 sec |
| Save criterion baseline | `rust/scripts/bench-baseline.sh save main` | ~5 min |
| Compare vs criterion baseline | `rust/scripts/bench-compare.sh --baseline main` | ~5 min |
| List saved baselines | `rust/scripts/bench-baseline.sh list` | instant |

---

*Maintained by Legolas (Performance Guru). Last updated: 2026-03-10.*
