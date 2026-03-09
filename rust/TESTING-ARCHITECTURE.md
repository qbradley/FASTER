# FASTER Rust — Testing Architecture

> Definitive guide to the project's tiered validation strategy.
> For quick-reference test commands, see [TESTING.md](TESTING.md).
> For fuzzing specifics, see [FUZZING.md](FUZZING.md).

---

## 1. Testing Philosophy

**Test economics model.** Writing tests is nearly free (AI-assisted, fast iteration). Running tests costs CI minutes. Investigating false positives costs engineer hours — the most expensive resource. Every design decision follows from this:

| Principle | Why |
|-----------|-----|
| **Zero false positives** | A flaky test is worse than no test — it trains engineers to ignore failures. |
| **Fast by default** | The gate developers hit most often (Tier 1) must finish in under 60 seconds. |
| **Deterministic first** | Prefer loom over stress tests, DST over random timing. Non-determinism is a last resort. |
| **Tiered cost** | Expensive validation (Miri, fuzz, DST campaigns) runs less frequently, not never. |
| **Coverage as compass** | Coverage reports find blind spots. High % is not a goal — tested invariants are. |

---

## 2. Tiered Validation Architecture

```
Commit → [Tier 1: Fast Gate] → PR → [Tier 2: Correctness Gate] → Merge
                                         ↓
                              Nightly → [Tier 3: Deep Validation]
                                         ↓
                              Release → [Tier 4: Release Gate]
```

### Tier 1 — Fast Gate

| | |
|-|-|
| **When** | Every commit, before push |
| **Time budget** | < 60 seconds |
| **Invoke** | `rust/scripts/precheckin` |
| **What runs** | `cargo fmt --check` → `cargo clippy --workspace --all-targets -D warnings` → `cargo test --doc --workspace` → `cargo nextest run --workspace --all-targets` |
| **Skip tests** | `rust/scripts/precheckin --skip-tests` (fmt + clippy only, ~5s) |
| **Fail = block** | Do not push if Tier 1 fails. |

### Tier 2 — Correctness Gate

| | |
|-|-|
| **When** | Every PR, via CI |
| **Time budget** | < 5 minutes |
| **What runs** | Everything in Tier 1, plus: Miri tests, Loom tests, DST tests |
| **Invoke** | CI pipeline, or manually: |

```bash
# Tier 1 (prerequisite)
rust/scripts/precheckin

# Miri — undefined behavior detection
cargo +nightly miri test -p faster-core --test miri_tests

# Loom — deterministic concurrency
cargo test -p faster-core --test loom_tests --features loom

# DST — deterministic simulation
cargo test -p faster-dst
```

### Tier 3 — Deep Validation

| | |
|-|-|
| **When** | Nightly, or pre-merge for high-risk changes |
| **Time budget** | < 30 minutes |
| **What runs** | Tier 2 + fuzz smoke + DST campaigns + stress tests with high thread counts |
| **Invoke** | |

```bash
# Fuzz smoke (10s per target, ~1 min total)
./rust/scripts/fuzz-ci --smoke

# Full fuzz (60s per target, ~6 min total)
./rust/scripts/fuzz-ci

# Stress tests (unlocked via #[ignore] removal or explicit run)
cargo test -p faster-core --test concurrent_stress -- --ignored
cargo test -p faster-core --test hash_index_concurrent -- --ignored

# Coverage report
cargo llvm-cov --workspace --html --open
```

### Tier 4 — Release Gate

| | |
|-|-|
| **When** | Pre-release only |
| **Time budget** | < 2 hours |
| **What runs** | Tier 3 + extended fuzz (5 min/target) + full DST campaigns + benchmarks |
| **Invoke** | |

```bash
# Extended fuzz
FUZZ_DURATION=300 ./rust/scripts/fuzz-ci

# Benchmarks (run on dedicated VM, never local)
cargo bench -p faster-core
cargo bench -p faster-bench

# Full DST campaigns with expanded seed exploration
cargo test -p faster-dst -- --ignored
```

### Tier Summary

| Tier | Name | Budget | Trigger | Key Tools |
|------|------|--------|---------|-----------|
| 1 | Fast Gate | < 60s | Every commit | fmt, clippy, nextest |
| 2 | Correctness | < 5 min | Every PR | + Miri, Loom, DST |
| 3 | Deep | < 30 min | Nightly | + fuzz, stress, coverage |
| 4 | Release | < 2 hr | Pre-release | + extended fuzz, benchmarks |

---

## 3. Current Test Inventory

**Totals:** 1,952 `#[test]` functions across 79 test files and inline modules.

### 3.1 Integration Tests — `crates/faster-core/tests/`

17 test files, 404 tests, 16,224 LOC.

| File | Tests | LOC | Category |
|------|------:|----:|----------|
| `miri_tests.rs` | 71 | 1,935 | UB detection (Miri) |
| `checkpoint_recovery_tests.rs` | 42 | 1,704 | Checkpoint + recovery |
| `compaction_integration.rs` | 32 | 1,402 | Compaction pipeline |
| `hybrid_log_tests.rs` | 31 | 885 | Hybrid log operations |
| `epvs_integration.rs` | 28 | 1,795 | Epoch/version state |
| `variable_length.rs` | 27 | 1,030 | Variable-length records |
| `memory_pressure_tests.rs` | 25 | 1,184 | Memory pressure / eviction |
| `property_tests.rs` | 23 | 677 | Property-based (proptest) |
| `loom_tests.rs` | 21 | 1,799 | Deterministic concurrency (Loom) |
| `e2e_tests.rs` | 19 | 814 | End-to-end workflows |
| `hash_index_correctness.rs` | 18 | 640 | Hash index correctness |
| `integration_basics.rs` | 15 | 460 | Cross-module integration |
| `concurrent_stress.rs` | 13 | 510 | Concurrent stress |
| `batch_tests.rs` | 8 | 271 | Batch operations |
| `hash_index_concurrent.rs` | 8 | 623 | Concurrent hash index |
| `quickstart_verify.rs` | 4 | 194 | Quickstart example validation |
| `write_pending_completion.rs` | 3 | 301 | Async write completion |

### 3.2 Inline Unit Tests — `crates/faster-core/src/`

61 `#[cfg(test)]` modules, 1,252 `#[test]` functions embedded in source files.

Top modules by test count:

| Source file | Tests |
|-------------|------:|
| `hash/bucket.rs` | 92 |
| `record/traits.rs` | 60 |
| `record/record_info.rs` | 53 |
| `store/kv.rs` | 41 |
| `recovery/log_recovery.rs` | 38 |
| `hash/index.rs` | 37 |
| `epoch/tests.rs` | 35 |
| `address.rs` | 35 |
| `record/layout.rs` | 34 |
| `hash/table.rs` | 34 |

### 3.3 Deterministic Simulation Tests — `crates/faster-dst/`

Framework: custom DST with controlled scheduler, fault injection, and crash simulation.
7 test files, 61 tests, 2,569 LOC.

| File | Tests | LOC | Focus |
|------|------:|----:|-------|
| `crash_injection.rs` | 12 | 520 | Injected crashes during operations |
| `checksum_tests.rs` | 10 | 579 | Data integrity after crash/recovery |
| `crud_simulation.rs` | 10 | 326 | CRUD under simulated scheduling |
| `campaign_tests.rs` | 9 | 255 | Multi-seed campaign execution |
| `crash_recovery.rs` | 8 | 357 | Write → crash → recover → verify |
| `scheduler_tests.rs` | 8 | 325 | Scheduler determinism |
| `seed_exploration.rs` | 4 | 207 | Seed space exploration |

DST framework sources (14 modules):

```
src/campaign.rs    src/crash.rs     src/harness.rs    src/runtime.rs
src/channel.rs     src/device.rs    src/invariant.rs  src/scenario.rs
src/clock.rs       src/fault.rs     src/report.rs     src/scheduler.rs
src/sim_store.rs   src/task.rs      src/trace.rs      src/workload.rs
```

Scenarios: `checkpoint_crash`, `compaction_crash`, `crud_stress`, `recovery_stress`, `torn_write`.

### 3.4 Property Tests (proptest)

42 `proptest!` macro invocations across 16 files.

| Location | Count | Coverage |
|----------|------:|----------|
| `tests/property_tests.rs` | 19 | Address, hash, bucket, record round-trips |
| `tests/epvs_integration.rs` | 8 | Epoch state transitions |
| `src/hash/bucket.rs` | 2 | Bucket tag/entry invariants |
| 14 other `src/` modules | 1 each | Module-local algebraic properties |

### 3.5 Fuzz Targets — `fuzz/fuzz_targets/`

5 targets, 518 LOC total. Framework: `libfuzzer-sys` + `arbitrary`.

| Target | Attack surface |
|--------|---------------|
| `fuzz_record_parsing.rs` | RecordInfo deserialization from arbitrary bytes |
| `fuzz_record_layout.rs` | Layout computation with random key/value sizes |
| `fuzz_compaction_record_size.rs` | Compaction record size calculation edge cases |
| `fuzz_hash.rs` | Hash function collision and distribution |
| `fuzz_store_ops.rs` | CRUD operation sequences against the store |

### 3.6 Benchmarks — `crates/faster-core/benches/`

3 files, 1,747 LOC. Framework: Criterion 0.5.

| File | LOC | Focus |
|------|----:|-------|
| `core_benchmarks.rs` | 1,106 | Core KV operations (read, upsert, RMW) |
| `ycsb.rs` | 487 | YCSB workload simulation |
| `hash_layout_bench.rs` | 154 | Hash table layout performance |

Baseline snapshots stored in `benches/baseline-iter*.json`.

### 3.7 Inventory Summary

| Category | Files | Tests | LOC |
|----------|------:|------:|----:|
| Inline unit tests | 61 modules | 1,252 | ~18,000 |
| Integration tests | 17 files | 404 | 16,224 |
| DST tests | 7 files | 61 | 2,569 |
| Property tests | 16 files | 42 macros | (included above) |
| Loom tests | 1 file | 21 | 1,799 |
| Miri tests | 1 file | 71 | 1,935 |
| Fuzz targets | 5 files | — | 518 |
| Benchmarks | 3 files | — | 1,747 |
| **Total** | **~80** | **~1,950** | **~40,000** |

---

## 4. How to Run Tests

### Quick Reference

| What | Command | Time |
|------|---------|------|
| **Standard quality gate** | `rust/scripts/precheckin` | ~30s |
| Quality gate (lint only) | `rust/scripts/precheckin --skip-tests` | ~5s |
| Quality gate + commit | `rust/scripts/checkin -m "feat: description"` | ~30s |
| Unit + integration | `cargo nextest run --workspace --all-targets` | ~20s |
| Doctests | `cargo test --doc --workspace` | ~5s |
| Miri (UB detection) | `cargo +nightly miri test -p faster-core --test miri_tests` | ~30s |
| Loom (concurrency) | `cargo test -p faster-core --test loom_tests --features loom` | ~30s |
| DST (simulation) | `cargo test -p faster-dst` | ~15s |
| Fuzz (smoke) | `./rust/scripts/fuzz-ci --smoke` | ~1 min |
| Fuzz (CI) | `./rust/scripts/fuzz-ci` | ~6 min |
| Fuzz (single target) | `./rust/scripts/fuzz-ci --target hash` | ~60s |
| Fuzz (minimize corpus) | `./rust/scripts/fuzz-minimize` | varies |
| Stress tests (ignored) | `cargo test -p faster-core --test concurrent_stress -- --ignored` | ~60s |
| Coverage report | `cargo llvm-cov --workspace --html --open` | ~60s |
| Benchmarks | `cargo bench -p faster-core` | ~5 min |
| Save benchmark baseline | `rust/scripts/bench-baseline.sh save main` | ~5 min |
| Compare against baseline | `rust/scripts/bench-compare.sh --baseline main` | ~5 min |
| List baselines | `rust/scripts/bench-baseline.sh list` | instant |

### Scripts

All scripts live in `rust/scripts/` and are executable from the repo root.

| Script | Purpose |
|--------|---------|
| `precheckin` | Tier 1 quality gate: fmt → clippy → doctests → tests |
| `checkin` | Runs `precheckin`, then stages and commits with Co-authored-by trailer |
| `fuzz` | Basic fuzz runner. Usage: `./scripts/fuzz [seconds-per-target]` |
| `fuzz-ci` | CI fuzz runner with `--smoke`, `--target`, env var overrides |
| `fuzz-minimize` | Corpus minimization via `cargo fuzz cmin` |
| `disk-io-bench-matrix.sh` | Disk I/O benchmark matrix |
| `bench-baseline.sh` | Save/list/compare benchmark baselines with metadata |
| `bench-compare.sh` | Compare benchmarks against baseline, flag regressions |

### Prerequisites

```bash
# Required for all tiers
cargo install cargo-nextest

# Required for Tier 2+
rustup toolchain install nightly   # for Miri

# Required for Tier 3+
cargo install cargo-fuzz           # for fuzz testing
cargo install cargo-llvm-cov       # for coverage

# Required for Tier 4
# Criterion benchmarks — no extra install needed
```

---

## 5. Test Categories

### Unit Tests

**Purpose:** Verify module-internal logic in isolation.
**Location:** `#[cfg(test)] mod tests` blocks inside `src/` files.
**Characteristics:** No I/O, no threads, no allocator-heavy setup. < 500ms each.
**Count:** 1,252 tests across 61 modules.

### Integration Tests

**Purpose:** Verify cross-module interactions and API contracts.
**Location:** `crates/faster-core/tests/*.rs`
**Characteristics:** May use the full store, multiple threads, temp files. < 1s each.
**Count:** 404 tests across 17 files.

### Property-Based Tests (proptest)

**Purpose:** Verify algebraic invariants via random input generation.
**Location:** `tests/property_tests.rs` and inline `proptest!` blocks in `src/`.
**Characteristics:** Round-trip encoding, monotonicity, commutativity. < 500ms per case.
**Count:** 42 proptest macros. Config: 64–256 cases for cheap tests, 16–32 for expensive.

### Deterministic Simulation Tests (DST)

**Purpose:** Test crash recovery, data integrity, and scheduling under controlled conditions.
**Location:** `crates/faster-dst/` (framework) and `crates/faster-dst/tests/` (test files).
**Characteristics:** Simulated clock, deterministic scheduler, injected faults, seed-based reproducibility.
**Count:** 61 tests. Scenarios: checkpoint crash, compaction crash, CRUD stress, recovery stress, torn write.

### Crash Consistency Tests

**Purpose:** Verify the write → crash → recover → verify cycle.
**Location:** `crates/faster-dst/tests/crash_injection.rs`, `crash_recovery.rs`, `checksum_tests.rs`.
**Characteristics:** Simulated process crashes at arbitrary points. Validates data survives.
**Count:** 30 tests.

### Miri Tests

**Purpose:** Detect undefined behavior in `unsafe` code.
**Location:** `crates/faster-core/tests/miri_tests.rs`
**Characteristics:** Single-threaded only (Miri limitation). ~100× slower than native. Exercises allocator, buffer pool, hash table, records, epoch, compaction.
**Count:** 71 tests. Gated with `#[cfg(miri)]`.
**Rule:** Every new `unsafe` block requires a corresponding Miri test.

### Loom Tests

**Purpose:** Deterministically explore all thread interleavings for lock-free code.
**Location:** `crates/faster-core/tests/loom_tests.rs`
**Characteristics:** Re-implements algorithms with loom primitives. Small state space: 2–3 threads, 2–4 operations. Gated with `#![cfg(feature = "loom")]`.
**Count:** 21 tests.
**Rule:** Every new atomic CAS or ordering-sensitive pattern requires a corresponding Loom test.

### Fuzz Tests

**Purpose:** Find crashes, panics, and hangs via random/mutated input.
**Location:** `fuzz/fuzz_targets/`
**Characteristics:** Requires nightly + `cargo-fuzz`. Independent workspace (excluded from main build). Corpus stored in `fuzz/corpus/`.
**Count:** 5 targets.

### Benchmarks

**Purpose:** Detect performance regressions.
**Location:** `crates/faster-core/benches/`
**Characteristics:** Criterion with HTML reports. Baselines stored in JSON. **Run on dedicated VM only** — never on local dev machines (noisy results).
**Regression detection:** `rust/scripts/bench-compare.sh` compares against saved baselines with configurable threshold (default 5%). See [benchmarking.md](docs/benchmarking.md).
**Count:** 3 benchmark suites.

---

## 6. Testing Gaps

Honest assessment of what's missing or incomplete:

| Gap | Severity | Notes |
|-----|----------|-------|
| **Mutation testing** | Medium | Not configured. Consider `cargo-mutants` to verify test quality. |
| **Async/Tokio test suite** | Medium | `faster-tokio` crate exists but has minimal dedicated test coverage. |
| **Cross-platform CI** | Low | Tests run on Linux only. No Windows/macOS CI matrix. |
| **Performance regression automation** | ✅ Resolved | `bench-compare.sh` compares against saved baselines with configurable threshold. See [benchmarking.md](docs/benchmarking.md). |
| **Coverage gating** | Low | `cargo-llvm-cov` available but not enforced in CI. No minimum threshold (by design). |
| **DST campaign scale** | Low | Current campaigns use small seed spaces. Extended campaigns need Tier 4 time. |
| **FFI round-trip tests** | Medium | `faster-ffi` crate has limited test coverage for C API surface. |
| **Device abstraction tests** | Low | `faster-device` and `faster-uring` have minimal dedicated tests. |

### Recommended Next Steps

1. **Mutation testing trial** — Run `cargo-mutants` on `faster-core` to measure test effectiveness.
2. **Tokio integration tests** — Add async operation tests for `faster-tokio` crate.
3. ~~**Benchmark CI** — Add Criterion `--save-baseline` / `--baseline` comparison to nightly CI.~~ ✅ Done: `bench-compare.sh` and `bench-baseline.sh` provide full regression detection. See [benchmarking.md](docs/benchmarking.md).
4. **FFI smoke tests** — Add C-side tests that call through `faster-ffi` and verify round-trips.

---

## 7. Adding New Tests

### Where Tests Go

| Test type | Location | Naming |
|-----------|----------|--------|
| Unit test for `src/foo.rs` | `#[cfg(test)] mod tests` in the same file | `test_<behavior>` |
| Integration test | `crates/faster-core/tests/<feature>_tests.rs` | `test_<scenario>` |
| Property test | `tests/property_tests.rs` or inline `proptest!` | `prop_<invariant>` |
| Miri test | `tests/miri_tests.rs` | `miri_<module>_<behavior>` |
| Loom test | `tests/loom_tests.rs` | `loom_<algorithm>_<scenario>` |
| DST test | `crates/faster-dst/tests/<scenario>.rs` | `test_<crash_scenario>` |
| Fuzz target | `fuzz/fuzz_targets/fuzz_<target>.rs` | `fuzz_<attack_surface>` |
| Benchmark | `crates/faster-core/benches/<suite>.rs` | Criterion group name |

### Decision Tree for New Code

```
New code has `unsafe`?
  └─ Yes → Add Miri test
  └─ No  → Continue

New code uses atomics / CAS?
  └─ Yes → Add Loom test
  └─ No  → Continue

New code has algebraic invariants?
  └─ Yes → Add proptest
  └─ No  → Continue

New code touches crash recovery path?
  └─ Yes → Add DST test
  └─ No  → Continue

New code parses untrusted input?
  └─ Yes → Add fuzz target
  └─ No  → Continue

Default → Unit test + integration test if cross-module
```

### Time Budget Compliance

| Category | Target | Hard Limit |
|----------|--------|------------|
| Unit test | < 500ms | 1s |
| Property test | < 500ms/case | 1s/case |
| Integration test | < 1s | 2s |
| Stress test | < 2s | 5s (with justification) |
| Loom test | no limit | deterministic exploration is inherently slow |
| Miri test | no limit | ~100× interpretation overhead |

If a test exceeds its budget, add a comment explaining why.

### Checklist Before Merging

- [ ] All new `unsafe` covered by Miri test
- [ ] All new atomics/CAS covered by Loom test
- [ ] All algebraic invariants covered by proptest
- [ ] Tests pass: `rust/scripts/precheckin`
- [ ] No `#[ignore]` without a tracking comment
- [ ] Time budgets respected (or justified)

---

## Workspace Architecture

```
rust/
├── crates/
│   ├── faster-core/        # Core engine — owns the bulk of tests
│   │   ├── src/            # 61 inline #[cfg(test)] modules
│   │   ├── tests/          # 17 integration test files
│   │   └── benches/        # 3 Criterion benchmark suites
│   ├── faster-dst/         # Deterministic simulation testing
│   │   ├── src/            # Framework: scheduler, crash, fault, scenarios
│   │   └── tests/          # 7 simulation test files
│   ├── faster-bench/       # Standalone benchmarks
│   ├── faster-device/      # I/O device abstraction
│   ├── faster-ffi/         # C FFI bindings
│   ├── faster-tokio/       # Async Tokio adapter
│   ├── faster-uring/       # io_uring integration
│   └── samples/            # Example applications
├── fuzz/                   # Fuzz targets (excluded workspace)
│   ├── fuzz_targets/       # 5 libfuzzer targets
│   └── corpus/             # Persisted fuzz corpus
└── scripts/                # Quality gate scripts
    ├── precheckin          # Tier 1 gate
    ├── checkin             # Gate + commit
    ├── fuzz               # Basic fuzz runner
    ├── fuzz-ci            # CI fuzz runner
    └── fuzz-minimize      # Corpus minimization
```

---

*Last updated: 2025. Maintained by the FASTER Rust team.*
