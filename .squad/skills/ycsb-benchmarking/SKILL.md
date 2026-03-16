# YCSB Benchmarking Skill — FASTER Rust

## When to Use
Measuring FASTER throughput, latency distributions, or comparing against C# baseline.

## Pattern

### Workload Characteristics
- **Workload A (50/50 read/update)**: Update-heavy, measures hash contention + RMW paths. Target: >47M ops/s (16T).
- **Workload C (100% read)**: Read-heavy, measures hash lookup + record prefetch efficiency. Target: >49M ops/s peak.
- **Workload F (50% read, 50% RMW)**: RMW-heavy, measures atomic update paths. Target: ~45M ops/s.

### Running YCSB Benchmarks
```bash
cd rust
# Full criterion run (takes ~15-20 min)
cargo bench --bench ycsb -- --bench

# Quick smoke test (for CI)
cargo bench --bench ycsb  # custom main() detects no --bench flag
```

### Key Configuration Points
- **Thread counts**: 1, 2, 4, 8, 16 (match CPU core count for peak throughput)
- **Dataset sizes**: 10M keys = in-memory; 100M+ keys = disk-bound
- **Prefetch pipeline**: L1=8 ops ahead (hash bucket), L2=4 ops ahead (record data)
- **Epoch refresh**: Every 256 ops (batch workloads)

### Baseline Comparison (C# parity)
C# baseline is ~55M ops/s on similar hardware. Rust implementation at -14% gap (47.47M ops/s) as of 2026-03-07.

### Interpreting Results
- **ops/s**: Primary metric. Track across thread counts for scalability curve.
- **P50/P99/P99.9 latencies**: Distribution tails reveal contention hotspots.
- **Prefetch impact**: Expect +6-7% throughput, +39% P99.9 improvement with two-level prefetch.
- **Regression threshold**: >5% throughput drop requires investigation.

### Criterion Output Files
```
target/criterion/ycsb_workload_*/
├── estimates.json  # Point estimates (median, mean, stddev)
├── sample.json     # Raw timing samples
└── report/         # HTML visualizations
```

## Anti-Patterns
- **Running under CPU contention**: Check `uptime` and `ps` before benchmarking. Load avg >80% can cause +10% noise.
- **Using nextest for benchmarks**: Criterion needs `--bench` flag explicitly — nextest doesn't pass it. Always use `cargo bench`.
- **Comparing across machines**: Hardware differences (cache size, CPU microarch) dominate signal. Use same VM or record baselines per machine.

## Confidence
**High** — Used across 3 waves of comprehensive benchmarking (Wave 3: 2026-03-07).

## Learned From
- Wave 3 Comprehensive Benchmarking (2026-03-07): 47.47M ops/s Workload A, +16% improvement
- Two-Level Prefetch Pipeline (Task A4, 2026-03-08): +6-7% throughput validation
- Post-Backlog-Sprint Benchmark (2026-03-11): CPU contention diagnosis pattern
