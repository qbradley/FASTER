# Decision: Foundation Test Architecture

**Author:** Rex (QA Engineer)
**Date:** 2026-03-06
**Status:** DOCUMENTED

## Context

Item 1i requires integration tests, stress tests, property tests, and benchmarks that validate Phase 1 modules work together correctly.

## Decisions

1. **Integration test file structure:** Three files instead of the plan's two (`foundation_smoke.rs` + `foundation_props.rs`). Split into `integration_basics.rs` (cross-module correctness), `concurrent_stress.rs` (multi-threaded), and `property_tests.rs` (proptest). This gives clearer failure isolation and allows running categories independently.

2. **`#[ignore]` tags on heavy stress tests:** Tests with 16 threads × 100K+ cycles are tagged `#[ignore]` for CI nightly runs only. Default `cargo test` runs 4-thread and 8-thread variants (~10K cycles each) which complete in <1s.

3. **Property test case counts:** 256-1024 cases per property (exceeds the spec's 256 minimum). Higher counts for simpler properties (address/hash), lower for allocation-heavy tests.

4. **Benchmark organization:** New benchmarks added to existing `core_benchmarks.rs` (keeps one benchmark binary per crate). `faster-bench/benches/main.rs` gets end-to-end pipeline benchmarks since it depends on `faster-core`.

5. **No Miri for stress tests:** The concurrent stress tests use OS threads and standard atomics, which Miri supports poorly. These tests are validated via ThreadSanitizer in CI instead. Property tests and integration basics are Miri-compatible.

## Impact

- All agents: The test suite is the quality gate for Phase 2. Any Phase 2 PR must pass `cargo test --workspace`.
- Ahsoka: Benchmark baselines established. Future performance changes should be compared against these.
- Mando: Integration tests document the intended API usage patterns. Refactoring must not break these contracts.
