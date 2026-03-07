# Decision: Cross-Implementation Performance Baseline Established

**Author:** Legolas (Performance Guru)
**Date:** 2026-03-07
**Status:** Active

## Context

Ran apples-to-apples YCSB benchmarks across Rust, C#, and C++ FASTER implementations.

## Key Numbers (16T, in-memory, 2.5M keys, uniform)

| Workload | Rust | C# | C++ |
|----------|------|-----|-----|
| A (50/50 R/W) | **39.21M** | 31.02M | 38.91M |
| B (95/5 R/W) | 41.37M | **49.56M** | 45.78M |
| C (100% Read) | 41.33M | **51.65M** | 48.07M |
| F (50/50 R/RMW) | 38.15M | 42.46M | **42.52M** |

## Decision

1. **Hash index prefetching** is the #1 optimization priority — closes the 40-77% single-thread gap with C#/C++.
2. Rust's write scaling is already best-in-class (102% efficiency at 16T on Workload A).
3. C# and C++ numbers provide concrete targets for future optimization work.

## Impact

- **Aragorn**: Hash index prefetching should be the next implementation task.
- **All**: Performance claims in docs/API must reference these measured numbers, not estimates.
- **Gandalf**: Architecture allows for prefetching — no structural changes needed.
