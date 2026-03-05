# FASTER Rust — Testing Policy & Release Criteria

> Canonical reference for test categories, tooling, time budgets, and quality gates
> in the `faster-core` crate and its workspace siblings.

---

## Test Time Budgets

| Category | Target | Hard Limit | Exemptions |
|---|---|---|---|
| Unit tests (`#[cfg(test)]`) | < 500 ms | **< 1 s** | — |
| Property tests (proptest) | < 500 ms / case | **< 1 s / case** | — |
| Concurrent stress tests | < 2 s | 5 s with justification | — |
| Loom tests | — | — | Deterministic exploration is inherently slow |
| Miri tests | — | — | Interpretation overhead ≈ 100× |

If a test exceeds its limit, add a comment at the test site documenting **why** the
excess is justified (e.g., large state-space in loom, warm-up cost in benchmarks).

The full `cargo test --all` suite should complete in **< 30 seconds** (excluding
loom and miri, which run separately).

---

## Test Categories

### Unit Tests — `src/` `#[cfg(test)]` modules

Pure logic verification. No I/O, no threads, no allocator-heavy setup.
Each module's `mod tests` block covers its own invariants in isolation.

### Integration Tests — `tests/integration_basics.rs`

Cross-module interactions that verify the *seams* between foundation modules
(address, hash, hash\_bucket, record, epoch, allocator). These complement
unit tests by confirming modules compose correctly.

### Property Tests — `tests/property_tests.rs` (and inline `mod proptests`)

Invariant verification via random input generation with
[proptest](https://crates.io/crates/proptest). Covers algebraic properties
across address, hash, hash\_bucket, and record modules.

### Correctness Tests — `tests/hash_index_correctness.rs`

Single-threaded, deterministic end-to-end validation of the hash index.
Each test targets a specific behavioral property of `HashIndex`.

### Concurrent Stress Tests — `tests/concurrent_stress.rs`, `tests/hash_index_concurrent.rs`

Multi-threaded correctness under real `std::thread` concurrency with
`Barrier`-synchronized starts. Thread counts are parameterized; high counts
are `#[ignore]` for CI nightly runs.

### Loom Tests — `tests/loom_tests.rs`

Deterministic concurrency testing via [loom](https://crates.io/crates/loom).
Each test re-implements the minimal core algorithm using loom primitives to
explore all possible interleavings. Gated behind `--features loom`.

### Miri Tests — `tests/miri_tests.rs`

Undefined-behavior detection for all `unsafe` code via
[Miri](https://github.com/rust-lang/miri). Exercises raw pointer arithmetic,
allocation, free-list operations, and cache-line-aligned bucket access.
Gated behind `#[cfg(miri)]`; single-threaded only.

### Benchmarks — `benches/core_benchmarks.rs`

Performance regression tracking via [Criterion](https://crates.io/crates/criterion).
Run separately from the test suite; `profile.bench` inherits `release` with debug info.

---

## When to Use Each Tool

| Scenario | Tool | Guidance |
|---|---|---|
| Algebraic properties (round-trip, commutativity, monotonicity) | **proptest** | 64–256 cases for cheap tests; 16–32 for tests that allocate large structures. Use `ProptestConfig::with_cases()`. |
| Atomic CAS loops, lock-free structures, ordering-sensitive patterns | **loom** | Reimplement the core algorithm with loom primitives. Keep state space small: 2–3 threads, 2–4 operations. |
| `unsafe` blocks — raw pointer math, transmute, `slice::from_raw_parts`, alloc/dealloc | **miri** | Single-threaded tests only. Use small iteration counts (miri is ≈ 100× slower). |
| Throughput, contention patterns, real-world thread counts | **stress tests** | Supplement loom — covers interleavings too large for deterministic exploration. |
| API surface, module composition, error paths | **integration tests** | Standard `#[test]` in `tests/`. No special tooling needed. |

---

## Coverage Policy

- Code coverage is generated via [`cargo-llvm-cov`](https://github.com/taiki-e/cargo-llvm-cov).
- Coverage is a tool to identify **what is not tested** — high coverage does not imply good coverage.
- Use reports to find untested code paths, then write targeted tests.
- **No minimum coverage percentage target.** Focus on critical-path coverage.

### Mandatory coverage rules

| Code pattern | Required test |
|---|---|
| Any `unsafe` block | Corresponding miri test |
| Any atomic CAS / ordering-sensitive pattern | Corresponding loom test |

---

## Running Tests

```bash
# Full test suite (< 30s target)
cargo test --all

# Loom tests — deterministic concurrency (~30s)
cargo test --features loom -p faster-core --test loom_tests

# Miri tests — UB detection (~5s)
cargo +nightly miri test -p faster-core --test miri_tests

# Code coverage — HTML report
cargo llvm-cov --all --html --open

# Benchmarks
cargo bench -p faster-core
cargo bench -p faster-bench
```

---

## Adding New Tests — Checklist

Before merging code that adds or modifies functionality, verify:

- [ ] Does the new code contain `unsafe`? → **Add a miri test.**
- [ ] Does the new code use atomics or CAS? → **Add a loom test.**
- [ ] Does the new code have algebraic invariants? → **Add a proptest.**
- [ ] Is every new test under the time budget? If not, **optimize or add a justification comment.**
- [ ] Does the test verify **correctness**, or does it merely exercise code paths?
- [ ] For concurrent tests: is the thread count parameterized with high counts `#[ignore]`-gated?

---

## Workspace Crate Map

| Crate | Role | Test expectations |
|---|---|---|
| `faster-core` | Core KV engine — zero async deps | Full suite: unit, integration, property, loom, miri, stress, bench |
| `faster-tokio` | Async adapter (Tokio runtime) | Integration tests against `faster-core` APIs |
| `faster-device` | I/O device abstraction | Device-specific integration tests |
| `faster-ffi` | C FFI bindings | FFI round-trip tests, cbindgen validation |
| `faster-bench` | Standalone benchmarks | Criterion benchmarks only |
