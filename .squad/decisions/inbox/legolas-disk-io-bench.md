# Decision: Disk I/O Benchmark Suite

**Agent:** Legolas (Performance Guru)
**Date:** 2026-03-07
**Status:** Implemented — ready for VM execution
**Impact:** Performance measurement infrastructure

## Decision

Created `disk-io-bench` — a dedicated benchmark binary for comparing FASTER's
three I/O device implementations (SyncFileDevice, UringDevice, TokioFileDevice)
under disk-bound workloads with both sync and Tokio client modes.

## Rationale

1. Our existing cross-impl-bench uses NullDevice (in-memory only) — it cannot
   measure disk I/O performance at all.
2. We need head-to-head device comparison data to validate architectural
   decisions (io_uring investment, Tokio adapter overhead, thread pool sizing).
3. The benchmark VM (20.59.58.117) has NVMe local storage ideal for this.

## What Was Built

- **Crate:** `rust/crates/samples/disk-io-bench/` (binary, not library)
- **Design doc:** `.squad/agents/legolas/disk-io-benchmark-design.md`
- **Runner script:** `rust/scripts/disk-io-bench-matrix.sh`

### Benchmark Matrix
- 3 devices × 2 client types × 4 workloads × 4 thread counts = 96 configurations
- Workloads: overcommit (cold reads), write-heavy (flush pressure), mixed (Zipfian), scan (sequential)
- Output: table, JSON, or CSV with ops/sec, latency percentiles, pending rate

### CLI Interface
```
disk-io-bench --device uring --client tokio --workload mixed --threads 8
disk-io-bench --run-all --output json --output-file results.json
```

## Pre-existing Build Issues Fixed

While implementing, found and fixed pre-existing compilation errors:
- `CompactionPlan` struct initializer missing 3 new fields in `scanner.rs`
- Duplicate lines in `drain.rs` (double `let mut drained`, double `fetch_sub`)
- Added `#[allow(dead_code)]` for `DrainList::has_pending()` (not yet called)

## Next Steps

- [ ] Run full matrix on bench VM (20.59.58.117) with NVMe storage
- [ ] Compare io_uring vs sync device overhead
- [ ] Use results to prioritize device optimization work
- [ ] Consider adding O_DIRECT comparison on VM
