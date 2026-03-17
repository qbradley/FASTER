# Project Context

- **Owner:** qbradley
- **Project:** Rust implementation of Microsoft FASTER — a high-performance durable hash map
- **Stack:** Rust (primary), C++ (reference), C# (reference), C FFI
- **Goal:** Production-grade, no async runtime required, seamless Tokio integration, idiomatic Rust API + C FFI interface. Quality bar: mission-critical cloud services at planetary scale.
- **Created:** 2026-03-05

## Core Architecture

### Key Types & Modules

**Store:**
- `FasterKv<F: Functions>` — Core store, generic over Functions trait
- `FasterSession<F>` — Owns epoch protection per session
- `UnsafeContext<'a, F>` — Wraps `&'a mut FasterSession<F>` for batch ops with amortized epoch overhead
- **Key modules:** `store/kv.rs` (factory), `store/session.rs` (session + batch API), `store/operations.rs` (dispatch)

**Address Model:**
- `LogicalAddress` — 25-bit offset (LSB), 23-bit page, 16-bit reserved (MSB)
- `INVALID = 1` (NOT 0 — zero means empty bucket)
- `AtomicLogicalAddress` requires explicit `Ordering`
- Page size = 2^25 = 32 MiB (~1.4M records at 24 bytes/record)
- Regions: `Truncated < head < OnDisk < ReadOnly < Mutable < InMutable`

**Error Model:**
- `OperationStatus` — Control flow (Ok, Pending, NotFound, InPlaceUpdated, Copied)
- `FasterError` — True errors (IoError, ChecksumMismatch, CorruptedData)
- No `thiserror` — manual impls (~40 lines)

**Compaction:**
- `compaction/scanner.rs` — Stateless scan, conservative LIVE classification
- `compaction/mod.rs` — CompactionPlan, LiveRecord
- Module is `#[deny(unsafe_code)]`

**Other Modules:**
- `hash/` — Hash index
- `allocator/` — Page allocator
- `epoch/` — Epoch framework
- `device/` — Storage backends (InMemoryDevice, SyncFileDevice, UringDevice)
- `sync.rs` — Loom shim module (`crate::sync` → `std::sync` or `loom::sync`)

### Build & Test Commands

**Standard workflow:**
```bash
cargo fmt
cargo clippy
cargo nextest run              # Tier-1 tests (<1s each)
cargo nextest run --run-ignored  # Tier-2 tests (slow/exhaustive)
```

**Benchmark:**
```bash
cd rust && cargo bench --bench ycsb -p faster-core -- --nocapture
```

**Never use `--all-targets` with nextest** — Criterion binaries trigger full runs.

### Code Conventions

**Visibility:**
- `pub` — User-facing API only
- `pub(crate)` — Cross-module access (e.g., FasterKv fields accessed by session.rs)
- Private — Everything else

**Sync primitives:**
- Production code: `use crate::sync::{Arc, AtomicU64, Ordering}` (loom shim)
- Test code: Can use `std::sync` directly

**Builder pattern:**
- Turbofish required: `FasterKv::<SimpleFunctions<u64, u64>>::builder()` (`FasterKvBuilder` is non-generic)

**Unsafe discipline:**
- Every `unsafe` block needs `// SAFETY:` comment explaining invariants
- `#[deny(unsafe_code)]` on modules that shouldn't need unsafe (e.g., compaction)

**Test performance:**
- Tier-1: <1s in isolation, runs on every commit
- Tier-2: `#[ignore]`, slow tests (page-filling, I/O, stress tests)
- Proptest: `cases: 2` (tier-1), `cases: 64-256` (tier-2)

### Key Learnings

**Address Model:**
- `RecordInfo::is_null()` unreliable for detecting unwritten memory — version 0 is valid
- Truncated region handling: upsert/RMW must allocate fresh records, not return Aborted

**Performance:**
- Per-op epoch protect/unprotect costs >80% on hot paths → UnsafeContext amortizes
- `compute_safe_epoch()` scans 256 entries × Acquire = ~97-128ns

**Test Sizing:**
- Page fills cost ~1.4M records × ~1-2s minimum
- Proptest cases cost ~240ms each (epoch + allocator setup)
- Nextest parallel contention inflates times: 0.1s → 1.7s under load

**Multi-writer deadlock (2026-07-21 review):**
- Root cause: `maintenance()` submits async I/O (Sealed→Flushing), then runs eviction before callbacks land
- All pages stuck Flushing → head can't advance → buffer full → deadlock
- `SyncFileDevice` never enforces `max_outstanding: 1024` (unbounded mpsc)
- `allocate_at_tail` retries only once (not enough for async I/O)
- C++ `disk->TryComplete()` has no Rust equivalent — Device trait missing completion polling

**Shared workspace hazards:**
- Other agents (Legolas, Boromir) switch branches — always verify `git branch --show-current` before commit
- Git stash across branches dangerous — stash pop can fail on Cargo.lock conflicts

## Session Log (Compressed)

### AI-4 — Epoch-Amortized UnsafeContext Batch API (2026-07-17)
**Impact:** Upsert 2.83×, Read 9.76×, RMW 9.83× speedup via batch epoch amortization.  
**Files:** `store/session.rs`, `store/kv.rs`, `benches/ycsb.rs`  
**Architecture:** UnsafeContext holds `&'a mut FasterSession<F>`, batch methods take `&FasterKv<F>`. `Refresh()` updates epoch without leaving protection.

---

### Wave 1 K1 — Compaction Scanner (2026-03-06)
**Impact:** Stateless scanner with conservative LIVE classification, `#[deny(unsafe_code)]`.  
**Files:** `compaction/mod.rs`, `compaction/scanner.rs`, `store/kv.rs`  
**Bugs fixed:** Sentinel alignment (must start at `first_data_address()`), RecordInfo(0) is valid.

---

### Precheckin Fix — Formatting + Clippy Sweep (2026-03-09)
**Impact:** 33 files fmt drift, 6 unused imports, 27 undocumented unsafe blocks, nextest hang fix.  
**Fix:** Drop `--all-targets` from nextest.  
**Commit:** `74ce041f`

---

### Wave 4 — Mutation Testing on Critical Modules (2026-03-09)
**Results:** Allocator 79% kill rate, Hash 93% kill rate. 17 targeted tests added.  
**Findings:** Most survivors are equivalent mutations or Debug formatting, not test gaps.

---

### Wave 5 — Lossy LRU Cache Wrap-Around Mode (Session 6)
**Impact:** Wrap-around semantics for hybrid log — upsert/RMW on Truncated addresses allocate fresh records.  
**Tests:** 9 integration tests (eviction, re-upsert, RMW, concurrent).  
**Bugs fixed:** internal_upsert/internal_rmw returned Aborted on Truncated (infinite retry loops).  
**Results:** 1608 existing + 9 new tests pass.

---

### Code Quality Cleanup — Precheckin Gate (2026-03-10)
**Impact:** 17 tests → tier-2 (`#[ignore]`), 9 tests sped up via iteration reduction.  
**Target:** All tier-1 tests <1s in isolation.  
**Results:** 1628 tier-1 tests + 17 tier-2 tests (1645 total).

---

### Loom Shim Integration (2026-03-11)
**Impact:** Wired all 9 production files to `crate::sync` shim for loom testing.  
**Team:** Aragorn (shim), Sam (log_prefix param), Éowyn (DST tiers), Galadriel (miri expansion).  
**Commits:** 229f0890 (shim), fc0d3da9 (Sam), a4525cb5 (Éowyn), 9a7190d0 (Galadriel).  
**Results:** All 1719 tests pass.

---

### Multi-Writer Deadlock Review (2026-07-21)
**Verdict:** APPROVE WITH COMMENTS  
**Findings:** Fix A/B/C correct. Deadlock root cause: timing gap between I/O submission and callback landing. `SyncFileDevice` yields in `poll_completions` (good). `allocate_at_tail` retry loop bounded at 32 (good). `deadlock_tests.rs` strong regression coverage but `#[ignore]`-ed.  
**Action:** Validated fix by running ignored tests (all passed).

---

### Loom CI Compilation Fixes (2026-07-22)
**Impact:** Fixed all 7 loom compilation errors across 7 files.  
**Fixes:**
- `sync.rs`: Added `sleep` shim for loom's thread module (yields instead of sleeping — loom doesn't model time)
- `epoch/table.rs`: Split `register()` into cfg-gated method (non-loom) + associated fn (loom) with shared `register_impl`. Loom's Arc doesn't implement `Receiver`, so `self: &Arc<Self>` is unavailable.
- `allocator.rs`, `epoch/drain.rs`: Replaced `AtomicPtr::get_mut()` with `load(Relaxed)` — loom's AtomicPtr lacks `get_mut`; both sites have `&mut self` guaranteeing exclusive access.
- `device.rs`, `store/pending_io.rs`: Wrapped static atomics in `LazyLock` under `#[cfg(loom)]` — loom atomics aren't const-constructible.
- Updated 3 production call sites to UFCS `EpochTable::register(...)` which works under both cfgs.

**Learnings:**
- Loom's `Arc` doesn't implement `core::ops::Receiver` → `self: &Arc<Self>` breaks under loom. Use cfg-gated associated function pattern.
- Loom's `AtomicPtr` has no `get_mut()`. Use `load(Relaxed)` under exclusive access as the portable alternative.
- Loom atomics aren't const — any `static` initialization must use `LazyLock` or equivalent under loom.
- Loom's thread module has no `sleep` — shim it with `yield_now()` since loom doesn't model wall-clock time.

### Stress Test Binary — stress_5min.rs (2026-07-22)
**Impact:** Created sustained stress test example at `rust/crates/faster-core/examples/stress_5min.rs`.
**Features:** CLI-configurable threads/duration/lossy/key-range/report-interval, YCSB-like mixed workload (50% upsert, 35% read, 10% RMW, 5% delete), per-thread correctness oracle with sanity bounds, throughput monitoring with cliff detection, xorshift64 thread-local RNG (zero external deps).
**Verified:** 4-thread 10s smoke test: ~8.9M ops/s non-lossy, ~9.2M ops/s lossy, 0 violations both modes.
**API notes:** `SimpleFunctions<u64,u64>` Output type is `Option<u64>` (not raw `u64`). `FasterKv::new()` takes `FasterKvConfig` struct directly — builder pattern also available via `FasterKv::<SF>::builder()`.

## Learnings

- `SimpleFunctions<K, V>::Output` is `Option<V>`, not `V` — read/RMW output must be `&mut Option<u64>`.
- For examples, use `std::sync::Arc` (not `crate::sync`) since examples aren't under the loom shim.
- `complete_pending()` should be called periodically in long-running workloads to drain async I/O.
- `InMemoryDevice::new()` completes I/O synchronously — Pending status is rare but still possible during page transitions.
- Batching atomic counter updates (flush every 256 ops) eliminates contention on global counters at 4+ threads.
- Throughput cliff detection (ThroughputMonitor) catches "slow deadlocks" that pass correctness checks but have collapsed throughput. Pattern: AtomicU64 counter + periodic window snapshots + post-warmup cliff assertion at 10% of peak.
- For timed stress tests, window duration must be chosen so that test_duration / window_duration > warmup_windows + 1 to get at least one checked window. E.g., 10s test needs ≤2s windows with 2 warmup windows.

### Safe Page Access Design Deep Dive (2026-07-24)
**Impact:** Comprehensive analysis of unsafe surface area in page frame access path, triggered by Sam's compaction SIGSEGV fix.  
**Key finding:** Epoch protection does NOT prevent page eviction — it only coordinates drain callbacks. Code comments claiming otherwise are incorrect. Frame memory is freed immediately via `Box::from_raw()` during eviction with no epoch deferral.  
**Sam's fix analysis:** Head-address bounds check mitigates the ABA bug but has a TOCTOU window (~10-50ns) between check and frame access where eviction can race.  
**Inventory:** ~36 unsafe blocks across 5 files (page.rs, log_allocator.rs, record_ops.rs, scan.rs); 9 on the hot read/write path; 5 in the critical address→pointer→data chain.  
**Options evaluated:** (A) Epoch-guarded handle — unsound without epoch semantic changes; (B) Arc<PageFrame> — 30%+ throughput loss from atomic contention; (C) Scoped page pin — sound, <3% overhead, per-slot pin count; (D) Safe wrapper with double-check — defense-in-depth, <2% overhead, not formally sound.  
**Recommendation:** Option D now (immediate hardening, ~200 lines), Option C next sprint (formal soundness, ~800 lines across 8 files). Combined state+pin_count packed atomic (3-bit state + 29-bit pin count) eliminates TOCTOU in pin implementation.  
**Output:** `.squad/decisions/inbox/aragorn-safe-page-access-design.md`

### ThreadSanitizer Strategy for Compaction Scanner Race (2026-07-24)
**Impact:** TSan test infrastructure for detecting the scanner-evictor use-after-free race.
**Files:** `tests/tsan_compaction.rs`, `tests/tsan_suppressions.txt`, `tests/README.md`, `scripts/run-tsan.sh`
**Architecture:** Three tests exercise different race vectors: primary scanner-vs-eviction, 4-writer production-like, and lossy-mode begin_address advance. Each uses a dedicated maintenance thread (evictor) running concurrently with a compaction thread (scanner) while writer threads create continuous buffer pressure. Tests are `#[ignore]` tier-2 and require `cargo +nightly` with `RUSTFLAGS="-Z sanitizer=thread"`.
**Key insight:** With 32 MiB pages and synchronous InMemoryDevice, the flush-to-evict window within `maintenance()` is nanoseconds under normal execution. TSan's 5-15× instrumentation overhead widens this to microseconds, making the race detectable. The suppression file excludes known-safe benign races in epoch counters and RecordInfo atomics.
**Commit:** `4675d33a`

## Learnings

- TSan requires nightly Rust (`cargo +nightly`) and `RUSTFLAGS="-Z sanitizer=thread"`.
- Page size is 2^25 = 32 MiB — filling multiple pages requires ~1.4M records each at 24 bytes/record. Tests need 10+ seconds to generate enough data for page transitions.
- With synchronous I/O (InMemoryDevice), `maintenance()` flushes + evicts in a single call. The race window between `shift_read_only_to_tail` and `evict_pages` is too narrow for normal execution but TSan widens it.
- TSan suppression files use pattern matching on function/symbol names: `race:epoch` suppresses all races in functions containing "epoch".
- `RUST_TEST_THREADS=1` recommended with TSan to avoid test-level parallelism interference.

### Scoped Page Pin — Option C Implementation (2026-07-24)
**Impact:** Implemented PinnedPage RAII guard (Option C from safe-page-access design analysis) with packed [pin_count:28 | state:4] AtomicU32 CAS. Joint work with Sam.
**Files:** `page.rs` (PackedPageState, PinnedPage, pin_page, pin-aware try_evict_frame), `log_allocator.rs` (pin_page), `scan.rs` (zero-unsafe scanner), `record_ops.rs` (from_log_pinned, pinned reader methods), `eviction.rs` (pin-respecting eviction), `operations.rs` (epoch comment corrections).
**Key design decisions:**
- Packed state+pin_count in single AtomicU32 eliminates TOCTOU between state check and pin increment.
- Eviction marks Flushed→Evicted but does NOT free frames or null slots — frames are recycled in-place by get_or_allocate_frame. This guarantees non-null frame pointers are always valid, making pin_page's unsafe frame_ref sound.
- try_evict uses strict CAS (expected = Flushed state, pin_count == 0) — pinned pages are atomically skipped.
- Scanner (crash site) migrated to PinnedPage: zero production unsafe blocks in scan.rs.
- LogRecordReader methods (read_record_info, read_key, read_value, read_header_and_match_key*) use from_log_pinned for eviction-safe reads in the read-only region.
- Mutable-region writes in operations.rs retain raw path — pinning unnecessary since pages above head_address cannot be evicted.
- Fixed all incorrect comments claiming epoch protection prevents page eviction.
**Results:** 1734 tests pass (+8 new). Clippy clean. stress_disk 60s @ 16 threads: ~11M ops/s, 0 crashes.
**Commit:** `96267b56`

### Lifetime-Bound MutableRecordAccessor (P1-A) (2026-07-25)
**Impact:** Added lifetime parameter to `MutableRecordAccessor<'a>` and safe factory `HybridLogAllocator::mutable_record_at()` with bounds checking. Eliminates raw pointer construction at 10 production+test call sites.
**Files:** `record_ops.rs` (struct + lifetime), `log_allocator.rs` (factory method), `operations.rs` (5 production + 1 test site migrated), `copier.rs` + `kv.rs` (return type updates), `hybrid_log_mutation_tests.rs` (1 test migrated).
**Key design decisions:**
- `PhantomData<&'a ()>` ties accessor to allocator lifetime — compiler prevents dangling pointers.
- `mutable_record_at()` clamps record_size to page remainder — prevents OOB writes even with corrupted addresses.
- Mutable region pages don't need PinnedPage (can't be evicted), so lifetime tie is to the allocator, not a pin guard.
- `unsafe fn new()` retained as escape hatch for raw-buffer tests (miri_tests.rs).
**Unsafe reduction:** operations.rs 8→2, record_ops.rs 11→8, log_allocator.rs 3→4. Net: 22→14 (−8). MutableRecordAccessor::new sites: 14→4.
**Results:** 1734 tests pass. Clippy clean. No performance regression (no new atomics or branches on hot path).
**Commit:** `8c829953`
