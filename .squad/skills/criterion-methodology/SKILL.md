# Criterion Benchmark Methodology Skill

## When to Use
Writing or debugging Criterion benchmarks for FASTER. Avoiding common pitfalls with nextest integration.

## Pattern

### Custom `main()` Pattern (REQUIRED)
**Problem**: `criterion_main!()` runs full benchmarks when nextest calls the binary without `--bench` flag → 12+ minute hangs in CI.

**Solution**: Use custom `main()` in ALL benchmark files:
```rust
use criterion::{criterion_group, Criterion};

fn my_benchmarks(c: &mut Criterion) {
    c.bench_function("my_bench", |b| b.iter(|| { /* ... */ }));
}

criterion_group!(benches, my_benchmarks);

fn main() {
    if std::env::args().any(|arg| arg == "--bench") {
        // Full criterion run
        benches();
        Criterion::default().configure_from_args().final_summary();
    } else {
        // Minimal smoke test for nextest
        println!("Running smoke test (use --bench for full benchmarks)");
        // Optional: quick sanity check here
    }
}
```

### Criterion Configuration for Large Benchmarks
```rust
let mut criterion = Criterion::default()
    .measurement_time(std::time::Duration::from_secs(10))
    .warm_up_time(std::time::Duration::from_secs(3))
    .sample_size(20);  // Reduce from default 100 for long benchmarks
```

### Dataset Sizing Guidelines
- **Hash layout microbenchmarks**: 1K-10K operations
- **YCSB workloads**: 10M keys (in-memory), 100M+ keys (disk-bound)
- **Batch operations**: 100K ops (fast), 1M ops (comprehensive)

Criterion runs each benchmark ~100 times by default (configurable with `sample_size`). Large datasets × 100 iterations = test timeout.

### Running Benchmarks
```bash
cd rust

# Full criterion run (saves baseline "current")
cargo bench --bench ycsb -- --bench

# Compare against saved baseline
cargo bench --bench ycsb -- --bench --baseline main

# Smoke test only (nextest compatibility)
cargo bench --bench ycsb

# List available benchmarks
cargo bench --bench ycsb -- --bench --list
```

### Baseline Management
Criterion stores baselines PER-BENCHMARK:
```
target/criterion/
├── ycsb_workload_A_16T/
│   ├── base/           # Default baseline
│   ├── main/           # Named baseline "main"
│   └── estimates.json  # Current run
└── hash_bucket_lookup/
    ├── base/
    └── main/
```

Use scripts (see `perf-regression-detection` skill) to manage across all benchmarks.

### Comparing Two Named Baselines (no re-run)
```bash
# Compare baseline A vs baseline B without running benchmarks
cargo bench --bench ycsb -- --load-baseline A --baseline B
```

## Anti-Patterns
- **Using `criterion_main!()` directly**: Causes nextest hangs. Always use custom `main()`.
- **Large datasets without tuning `sample_size`**: Default 100 samples × large dataset = timeout.
- **Forgetting `--bench` flag**: Without it, you get smoke test, not full benchmarks.
- **Dead prototype code in benchmarks**: Remove unused benchmark functions — they still run in full mode (12+ hour debug builds).

## Key Files
- `rust/benches/ycsb.rs` — YCSB workload benchmarks
- `rust/benches/hash_layout_bench.rs` — Hash table microbenchmarks
- `rust/benches/core_benchmarks.rs` — Allocator and core operations

## Confidence
**High** — Pattern enforced across all 3 benchmark files (2026-03-09).

## Learned From
- Fix hash_layout_bench 12+ Minute Nextest Hang (2026-03-09): Custom `main()` discovery
- Fix hash_layout_bench Dead OA Prototype (2026-03-09): Dataset sizing and cleanup
- Criterion baseline structure learning: Multi-agent /tmp collision debugging
