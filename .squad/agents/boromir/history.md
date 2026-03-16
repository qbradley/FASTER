# Boromir - QA Engineer History

## Core Context

**FASTER Rust Test Infrastructure:**
- Test suite: 30+ integration test files in `rust/crates/faster-core/tests/`
- Test frameworks: cargo test (standard), nextest (CI), loom (concurrency), miri (UB detection)
- CI pipeline: `.github/workflows/rust-ci.yml` — runs on push/PR, weekly DST extended campaign
- Mutation testing: cargo-mutants v27.0, configured in `rust/mutants.toml`
- Fuzz testing: 8 targets in `rust/fuzz/`, libfuzzer + arbitrary

**Test Organization:**
- Unit tests: `#[cfg(test)]` modules in `src/*/mod.rs`
- Integration tests: `tests/{subject}_{type}.rs` pattern
- Mutation tests: `tests/*_mutation_tests.rs` — targets specific mutation gaps
- Property tests: `tests/property_tests.rs` — proptest-based
- Specialized: `tests/loom_tests.rs`, `tests/miri_tests.rs`, `tests/deadlock_tests.rs`
- Common utilities: `tests/common/` — devices, helpers, assertions (not a test file)

**CI Jobs (rust-ci.yml):**
- `fmt` — formatting check (nightly)
- `ci` — clippy + tests (debug + release, matrix: ubuntu/windows/macos)
- `loom` — concurrency tests (--cfg loom)
- `dst` — DST smoke tests (~20s, 133 tests, excludes campaign_expanded)
- `dst-extended` — full campaign (scheduled/manual, ~4min, 3009 scenarios)
- `miri` — UB detection (nightly)
- `coverage` — llvm-cov (lcov.info artifact)
- `bench` — benchmarks (main branch only)
- `semver-checks` — API compatibility (informational pre-1.0)
- `deny` — license + advisory audit

**Test Device Doubles (faster_core::test_utils::devices):**
- `InMemoryDevice` — synchronous, returns `CompletedSync`
- `SlowDevice` — async with delay, returns `Submitted` (matches real devices)
- `FaultInjectingDevice` — callback-level errors (`IoStatus::Error`)
- `QueueFullDevice` — submission-level errors (`IoRequestResult::QueueFull`)
- `QueueFullThenSucceedDevice` — QueueFull for N writes, then succeeds

**Key Testing Insights:**
- **Page size:** 32MB (1 << 25) — exact boundary tests need full-page allocations
- **State machine:** Open → Sealed → Flushing → Flushed → Evicted → Truncated
- **Error injection layers:** Submission (QueueFull) vs Callback (IoStatus::Error)
- **OperationOutcome API:** NOT Result — use `.is_success()`, `.is_aborted()`, `.status()`
- **Atomic fetch_add:** Returns pre-increment value — compare `count >= n`, not post-increment load
- **Test economics:** Code is free, runtime costs money — optimize for fast tests that catch real bugs

## Major Work

### Mutation Testing Campaign (2024)
Killed 17 mutation gaps across 5 modules (eviction, flush, log_allocator, page, regions). Discovered equivalent mutations (| vs ^ on non-overlapping bits, > 0 vs >= 0 on unsigned), performance-only mutations (prefetch noop), and true gaps (boundaries, counters, arithmetic). Created mutation-killing test patterns: pinned reference vectors (hash), exact boundary testing, counter validation, marker bytes (offsets), logic independence (|| vs &&).

**Skills extracted:** mutation-test-strategy, integration-test-patterns

### Deadlock Test Harness (2026-03-11, sam/deadlock-fix)
Built test infrastructure with QueueFullDevice, QueueFullThenSucceedDevice, SlowDevice. Discovered QueueFull vs callback error distinction, OperationOutcome API patterns, atomic counter off-by-one. 7 passing tests, 6 ignored pending fix completion.

**Skills extracted:** test-device-authoring

## Skills Created (Reskill Audit 2026)

**New skills from history mining:**
1. `test-device-authoring` — Test double patterns for FASTER I/O (5.3 KB)
2. `integration-test-patterns` — State machine, boundary, multi-page testing (6.8 KB)
3. `mutation-test-strategy` — Mutation classification, kill patterns (7.0 KB)
4. `test-naming-conventions` — File and function naming standards (5.0 KB)

**Existing skills (verified):**
- `mutation-testing` — cargo-mutants tool usage (3.1 KB) — COMPLEMENTARY, kept
- `fuzz-target-authoring` — Fuzz target patterns (existing)

**Total knowledge formalized:** 24.1 KB of reusable testing patterns

## Learnings

### Windows CI fsync_dir Fix (2025)
- **Root cause:** `fsync_dir()` in `metadata_store.rs` calls `fs::File::open()` on a directory, which requires `FILE_FLAG_BACKUP_SEMANTICS` on Windows — not set by Rust's stdlib. Produces `PermissionDenied` (OS error 5).
- **Blast radius:** All 64 failing tests trace to one 3-line function. Every test calling `write_checkpoint_metadata()`, `delete_checkpoint()`, or `atomic_write()` was affected — spanning `checkpoint_recovery_tests.rs`, `recovery_edge_cases.rs`, `recovery/mod.rs`, `recovery/log_recovery.rs`, `metadata_store.rs` unit tests, and `orchestrator.rs` unit tests.
- **Fix:** `cfg(unix)` guard on directory fsync; no-op on non-Unix. NTFS journals metadata ops so rename-based atomic writes are already durable. Same approach as RocksDB/SQLite.
- **Key insight:** A single platform-incompatible helper function caused 64 test failures — always check stdlib filesystem operations for Windows compatibility when they touch directories or file handles.

### Lossy Mode Deadlock Regression Tests (2025)
- **Tests added:** `lossy_16_thread_sustained_progress` (30s, 16 threads) and `lossy_eviction_memory_bounded` (20s, 8 threads) in `deadlock_tests.rs`.
- **Pattern:** Tiny buffer (8 pages, max 4 in-memory) + lossy mode triggers eviction pipeline contention much faster than the original 128-page repro. Avoids 200s wait.
- **Watchdog design:** Per-thread stall detection (5s threshold via `Mutex<Instant>`) + global throughput monitor (AtomicU64 total_ops, 5s zero-progress panic). Dual watchdog catches both single-thread deadlock and collective starvation.
- **Memory bounding:** Raw address difference `(tail - head) >> 25` approximates in-memory page count without needing access to internal allocator snapshot. Ceiling at 2× buffer_size allows in-flight flush overhead.
- **Current status (pre-fix):** Both tests PASS — the deadlock may require larger buffers or longer runs to manifest. Tests are positioned as regression gates for when the fix lands.
- **Key insight:** Mixed workload (upsert + read + RMW + delete) on overlapping key ranges is critical — pure upserts don't trigger the hash-index invalidation + eviction race that causes the real deadlock.
