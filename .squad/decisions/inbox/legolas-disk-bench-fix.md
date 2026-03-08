# Decision: Disk I/O Benchmark Methodology Fix

**Author:** Legolas (Performance Guru)
**Date:** 2026-03-07
**Branch:** `legolas/disk-io-bench-fix`
**Addresses:** Iteration 6 Action Items A2, A11

## Context

The W3-02 disk I/O benchmark showed 0% pending rate across all workloads with buffer_pages=128 on a 32GB VM. This means every operation completed synchronously from the in-memory portion of the hybrid log — we were measuring in-memory ceiling performance and mislabeling it as "disk I/O."

## Decisions

### D1: Default buffer_pages reduced from 128-256 to 16-32

Rationale: With 10M keys x 1KiB values, the dataset is ~10GB. A 128-page buffer (4MB) still allows the hybrid log to keep hot data in-memory. Reducing to 16 pages (512KB) creates a 20,000:1 dataset-to-buffer ratio, making disk reads far more likely once the OS page cache is cold.

### D2: I/O Classification Thresholds

- `pending_rate < 0.1%` → IN-MEMORY BASELINE
- `0.1% <= pending_rate < 5%` → MIXED
- `pending_rate >= 5%` → DISK-BOUND

These thresholds are empirically motivated. Anything below 0.1% means virtually no disk ops occurred. The 5% threshold reflects meaningful disk participation.

### D3: Statistical Rigor — 5 iterations, t-distribution CI

Default 5 iterations with mean, sample stddev, and 95% confidence interval using t-distribution critical values. HIGH VARIANCE flag at stddev > 10% of mean. This aligns with standard benchmarking practice (e.g., SPECjbb, MLPerf).

### D4: --force-disk Auto-Tuning

Starts at buffer_pages=16, halves until pending_rate > 0.1%. Uses short 5s probe runs to minimize auto-tune wall time. Reports final effective buffer_pages so results are reproducible.

## Impact

- **All future disk I/O benchmarks** should use the new defaults or --force-disk mode
- **Previous W3-02 results** should be relabeled as "IN-MEMORY BASELINE" in any comparison tables
- **CI benchmark jobs** should include --iterations 3 at minimum for regression detection

## Files Changed

- `rust/crates/samples/disk-io-bench/src/main.rs` — CLI args, multi-iteration, force-disk, validation gate, output formats
- `rust/crates/samples/disk-io-bench/src/stats.rs` — IterationSummary with mean/stddev/CI
- `rust/crates/samples/disk-io-bench/src/workload.rs` — Reduced default buffer_pages, documented memory budget
