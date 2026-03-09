# Decision: Criterion Benchmarks Must Use Custom main() for Nextest Compatibility

**Author:** Legolas (Performance Guru)
**Date:** 2026-03-09
**Status:** Implemented

## Context

All three criterion bench binaries (`hash_layout_bench`, `ycsb`, `core_benchmarks`) used `criterion_main!()` which runs full statistical benchmarks when invoked by `cargo nextest run --all-targets`. The `hash_layout_bench` alone took 12+ minutes, blocking every precheckin run.

## Decision

**All `harness = false` criterion benchmarks MUST use a custom `main()` instead of `criterion_main!()`.**

Pattern:
```rust
criterion_group!(benches, bench_foo, bench_bar);

fn main() {
    if std::env::args().any(|a| a == "--bench") {
        benches(); // Full criterion run (cargo bench)
    } else {
        // Optional smoke test for nextest/cargo test
    }
}
```

## Rationale

- `criterion_main!()` does not implement nextest's test discovery protocol
- When nextest invokes the binary without `--bench`, criterion defaults to full benchmark mode
- Full mode runs 100 samples × auto-tuned iterations × warmup for EVERY benchmark function
- This is a category error: nextest wants a quick pass/fail test, not a 12-minute statistical analysis

## Impact

- Precheckin time: 12+ minutes (hanging) → 38 seconds
- `cargo bench` continues to work identically
- Any new bench files must follow this pattern

## Who Needs to Know

- **All agents adding benchmarks:** Follow the custom `main()` pattern
- **Aragorn:** Precheckin is unblocked
