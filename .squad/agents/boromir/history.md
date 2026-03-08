# Project Context

- **Owner:** qbradley
- **Project:** Rust implementation of Microsoft FASTER — a high-performance durable hash map
- **Stack:** Rust (primary), C++ (reference), C# (reference), C FFI
- **Goal:** Production-grade, no async runtime required, seamless Tokio integration, idiomatic Rust API + C FFI interface. Quality bar: mission-critical cloud services at planetary scale.
- **Created:** 2026-03-05

## Learnings

<!-- Append new learnings below. Each entry is something lasting about the project. -->

## 2026-03-05T19:25: Item 1e — CI Skeleton Created

**What:** Created `.github/workflows/rust-ci.yml` — a multi-job GitHub Actions CI pipeline for the Rust workspace.

**Jobs:**
1. **fmt** — Formatting check using nightly rustfmt (required for `imports_granularity` and `group_imports` options in `rustfmt.toml`).
2. **ci** — Clippy + check + test (debug & release) on a 3-OS matrix (ubuntu, windows, macos). Gated behind `fmt`. Uses `Swatinem/rust-cache@v2` for caching.
3. **miri** — Weekly scheduled + on-demand Miri runs with `-Zmiri-symbolic-alignment-check -Zmiri-retag-fields`. `continue-on-error: true` — informational for now.
4. **bench** — `cargo bench --workspace --save-baseline main` on pushes to main only.
5. **deny** — `cargo deny check` using `EmbarkStudios/cargo-deny-action@v2` against the existing `rust/deny.toml`.

**Local validation:** All 5 core commands (`check`, `clippy`, `fmt --check`, `test`, `test --release`) pass clean in the current workspace.

**Also:** Added CI status badge to `rust/README.md`.

**Learnings:**
- The `rustfmt.toml` uses `imports_granularity` and `group_imports`, which are nightly-only features. Formatting CI must use nightly toolchain.
- This environment uses `msrustup` (Microsoft internal), not public `rustup`. CI uses standard `dtolnay/rust-toolchain` actions since it targets GitHub-hosted runners.
- `cargo-deny` is not installed locally but the `deny.toml` config exists and is well-structured. Using the official GH action avoids toolchain issues.
- Path filtering (`rust/**`) keeps Rust CI from running on C#/C++ changes — important since the monorepo has three language ecosystems.

---

## 2026-03-05T19:20: Wave 1 Sprint Complete — 1e Done

**Context:** All 4 Wave 1 tasks executed in parallel by full team.

**Your Deliverable (1e):**
- Task 1e: CI skeleton — 5-job GitHub Actions workflow, all validated locally

**Decision Merged:**
- Decision #14: Rust CI Architecture — GitHub Actions (not Azure Pipelines), 5 jobs, path filtering, nightly rustfmt only

**Orchestration Log:** `.squad/orchestration-log/2026-03-05T1920-boromir.md`

**What's Next:**
- All future Rust PRs will now be gated on fmt + clippy + test (debug/release) × 3 OSes ✅
- Miri runs weekly (Sundays 06:00 UTC) + on manual dispatch for UB detection
- Benchmark baseline established for main branch

**Team Status:**
- Aragorn (1b/1c): Address + status/error complete, 127 tests passing
- Sam (1d): Hash traits complete, 64 tests passing
- All 127 tests passing in workspace. Zero regressions.



**What:** Gandalf completed 288 KB comprehensive Rust FASTER architecture specification (6101 lines, 14 sections). 3-part parallel document (Gandalf-A/B/C) due to massive context requirements.

**Artifact Location:** `.squad/agents/gandalf/rust-faster-architecture.md`

**Sections:** Vision, Crates, Data Structures, Concurrency, Storage, Operations, Checkpoint, API, FFI, Async Integration, Testing, Phases, 12 Key Decisions, Risk Register (25 risks, 5 critical)

**12 Core Architectural Decisions:**
1. No async/await in core (user directive) — callback/completion model
2. Custom epoch (not crossbeam-epoch) — FASTER-specific semantics
3. Inline variable-length records (C++ model)
4. Rust atomic patterns for hash index — lock-free
5. 32 MB default page size — 512-byte sector alignment
6. Completion-based Device trait — runtime-agnostic
7. Opaque FFI handles (~15 function API surface)
8. Own checkpoint format — forward-compatible binary
9. Result<Status, Error> taxonomy with bitflag Status
10. Thread-affine sessions (!Send compile-time) — mono-threaded safety
11. Unified Key/Value traits for fixed+variable-length
12. Epoch-coordinated grow protocol (doubles, no shrink)

**What This Means For You:**
- **All:** You are now operating under this architecture. Decisions binding for implementation phases.
- **Elrond:** Async adapter design bridges callback core to Future/async/await ecosystem (Decision #3 path).
- **Aragorn:** Core code stays fully sync, zero async runtime deps. No tokio in core.
- **Sam:** Device trait is callback-based (Box<dyn FnOnce>), not Future-based.
- **Éowyn:** Simulation framework must model callback completion ordering for testing.
- **Galadriel:** Unsafe audit scope: hash index atomics (AcqRel/Acquire), record pointer arithmetic, FFI boundary.
- **Frodo:** Phase roadmap and implementation sequence encoded in Part 3.
- **Saruman:** Your async/await recommendation overridden by user directive; async ergonomics achieved via adapter layer.

**Risk Profile:** 25 identified risks (5 critical ≥15, 8 high 10-14). Primary mitigations: deterministic simulation, Miri verification, security audit, cross-implementation comparison.

**Next Steps:** Risk mitigation task force (Éowyn lead), unsafe audit planning (Galadriel lead), Phase 1 sprint (Frodo lead), async adapter spike (Elrond lead).

## 2026-03-06: Item 1i — Foundation Integration Tests and Benchmarks

**What:** Created comprehensive integration test suite and benchmark additions for all Phase 1 foundation modules.

**Deliverables:**

1. **Integration tests** (`rust/crates/faster-core/tests/integration_basics.rs` — 15 tests):
   - LogicalAddress ↔ RecordInfo round-trip and mutation chains
   - Allocator → store record → read back (single and 100-record batch)
   - Hash key → extract tag → HashBucketEntry → find_entry round-trip
   - Tentative entry invisibility to find_entry
   - Epoch protect/unprotect + drain callback firing
   - try_insert fills bucket then fails on 8th entry
   - Variable-length record (Vec<u8>) round-trip
   - OperationStatus in Result context
   - Full pipeline: hash → bucket → alloc → record write → read
   - Epoch-protected hash insert pattern
   - Allocator free + reuse

2. **Concurrent stress tests** (`tests/concurrent_stress.rs` — 10 tests, 3 `#[ignore]`):
   - Epoch protect/unprotect storm (4, 8, 16 threads) with drain callback counting
   - safe_epoch monotonicity verification under contention
   - Concurrent bucket try_insert (4 and 7 threads, no lost entries)
   - Multi-bucket concurrent insert (8 threads × 7 entries each)
   - Concurrent alloc/free (4, 8, 16 threads)
   - Combined epoch + bucket + allocator pattern (the real hash index pattern)
   - Thread registration storm (32 threads × 100 register/unregister cycles)

3. **Property-based tests** (`tests/property_tests.rs` — 18 test groups, 256-1024 cases each):
   - LogicalAddress page/offset round-trip
   - RecordInfo field encode/decode round-trip
   - Hash tag always in 14-bit range
   - Hash → tag → bucket → find_entry always succeeds
   - HashBucketEntry encode/decode round-trip
   - Record write/read for u64, u32, and Vec<u8>
   - RecordLayout size consistency invariants
   - Hash determinism
   - Hash index range for power-of-two table sizes
   - Address validity semantics
   - Allocator unique addresses
   - RecordInfo mutation preserves unmodified fields

4. **Benchmark additions** (`benches/core_benchmarks.rs` + `faster-bench/benches/main.rs`):
   - Epoch protect/unprotect latency (single-threaded)
   - Epoch protect+refresh+unprotect latency
   - Epoch bump (no callback) and bump+drain
   - Record write/read u64+u64 and Vec<u8> 32B+128B
   - Hash bytes 1KB and 4KB throughput
   - Allocator throughput group (bump and alloc+free cycle)
   - Bucket find with parameterized fill levels (1, 3, 5, 7 slots)
   - Pipeline insert benchmark (epoch+hash+alloc+write)
   - Pipeline lookup benchmark (epoch+find+read)
   - Epoch contended benchmark (0, 2, 4 background threads)

5. **Test infrastructure** (`tests/common/mod.rs`):
   - Shared helpers: `addr()`, `fill_bucket()`, `hash_key()`

**Test counts:**
- Before: 332 unit + 58 doc = 390 tests
- After: 332 unit + 15 integration + 10 concurrent + 18 property + 58 doc = 433 tests (+ 3 ignored nightly)
- All pass on `cargo test --workspace`. Zero warnings. Zero clippy issues.

**Key benchmark results (single-threaded, release):**
- Epoch protect+unprotect: ~171 ns
- Epoch bump+drain: ~225 ns
- Record write u64+u64: ~1.3 ns
- Record read u64+u64: ~1.4 ns
- Hash bucket find (hit slot 0): ~970 ps
- Hash bucket find (miss, 7 slots): ~3.2 ns
- Allocator bump: ~7.8 ns
- Full pipeline insert: ~190 ns
- Epoch contended (4 bg threads): ~395 ns

**Learnings:**
- proptest's `prop_assert_eq!` macro doesn't support Rust 2024 inline format captures — must use positional/named args for format strings inside proptest blocks.
- The epoch system's try_drain is invoked on every unprotect, which makes epoch protect+unprotect more expensive than a simple atomic pair (~170ns vs ~5ns for raw CAS). This is by design — drain amortization.
- Criterion's throughput groups with `--quick` can panic on too-few samples. Parameterized `bench_with_input` is more robust for CI quick-bench validation.
- Allocator free-list reuse path (~18ns) is 2.3× slower than bump path (~7.8ns) due to CAS contention on the Treiber stack head. This is expected and matches C++ FASTER behavior.

---


## 2026-03-06: AI-7 — thread::sleep Audit in faster-core

**What:** Audited all `thread::sleep` usage across `faster-core` (src, tests, benches).

**Findings:**
- **Zero sleeps in test code.** The retrospective goal is already met — no test code uses `thread::sleep`.
- **Zero sleeps in benchmark code.**
- **Two sleeps in production code, both justified:**
  1. `src/checkpoint/orchestrator.rs:282` — 1ms poll loop waiting for flush completion with a deadline timeout. Proper polling pattern for I/O wait.
  2. `src/store/kv.rs:263` — 1ms poll loop in `Drop` impl waiting for in-flight I/O before device close. Necessary cleanup pattern.

**No changes required.** Both production sleeps are short (1ms), bounded by deadlines, and are standard I/O polling patterns where no better signaling mechanism is available from the underlying APIs.

**Validation:** 1161 tests pass, clippy clean.

**Learning:** The write_pending_completion tests (previously 18.5s, now ~2s each) use proper synchronization — no sleeps. The convention "no thread::sleep in tests" is already upheld.

## 2026-03-07: SF-3/5/8/9 — Compaction Scanner Runtime Warnings & Edge-Case Tests

**What:** Addressed 4 should-fix items from the final review synthesis of the variable-length compaction feature.

**Changes (commit 4c11175b):**

1. **SF-3 (Tombstone vector warning):** Added `log::warn!` when `tombstone_records` vector exceeds 100 MB (checked every 1024 pushes). Estimated via `count * size_of::<LiveRecord>()`.

2. **SF-5 (Hash collision hop metrics):** Changed `is_current_version()` return type from `bool` to `(bool, usize)` to return hop count. Track cumulative hops in `CompactionPlan.total_chain_hops`. Warn when total exceeds 1,000,000.

3. **SF-8 (Post-scan byte validation):** Two checks before `Ok(plan)`: (a) scanned bytes vs address-range span (>5% threshold), (b) `live_bytes + dead_bytes + tombstone_bytes == total_bytes_scanned`.

4. **SF-9 (5 new test categories):** `record_nearly_fills_page`, `moderate_corruption_plausible_wrong_size`, `version_chain_variable_length_different_sizes`, `empty_key_record_size`, `eq_from_bytes_trailing_garbage`.

**Learnings:**
- `log` crate added as non-optional dep (safety warnings should always emit). `tracing` bridges to `log` so no conflict.
- Alignment padding gotcha: key len 2→3 doesn't change padded record size due to 8-byte alignment. Key len 4→5 DOES cross: pad(24,8)=24 vs pad(25,8)=32.
- With `mutable_fraction=0.9` and small records, upsert stays in-place (`InPlaceUpdated`). Need `shift_read_only_to_tail()` first for `CopyUpdated`.
- `VarLenFunctions` (Vec<u8> key+value) defined inline for scanner tests since `SimpleFunctions` requires `V: Copy`.
- Concurrent squad agents can continuously overwrite shared files between edit and build steps.

**Validation:** All 20 scanner tests pass (18 existing + 2 fixed).
