# Project Context

- **Owner:** qbradley
- **Project:** Rust implementation of Microsoft FASTER — a high-performance durable hash map
- **Stack:** Rust (primary), C++ (reference), C# (reference), C FFI
- **Goal:** Production-grade, no async runtime required, seamless Tokio integration, idiomatic Rust API + C FFI interface. Quality bar: mission-critical cloud services at planetary scale.
- **Created:** 2026-03-05

## Core Context

### Data Structures & Memory Layout
- **RecordInfo bit layout:** `[63:Fin][62:Tom][61:Inv][60:Seal][59..48:Version(12)][47..0:PrevAddr(48)]`. Single u64 atomic, all coordination state packed. Version max 4095 (12 bits).
- **Sealing protocol:** `seal()` = Release, `is_sealed()` = Acquire, `try_seal()` = AcqRel/Acquire CAS. Write paths fall back to copy-to-tail on sealed. Delete permitted on sealed.
- **Revivification:** `try_revivify(expected)` atomically clears sealed bit 60 via AcqRel/Acquire CAS. Conditions: mutable region + sealed + CAS wins + value fits. Returns `OperationStatus::Revivified`.
- **Hash table:** `find_entry()` skips tentative. Must call `update_entry()` with `entry.without_tentative()`. `entry_count()` returns 0 (intentional).
- **Cache-line awareness:** RecordInfo uses single u64 → single cache line. Epoch table scans 256 entries (16KB, ~128ns) — amortize with batch contexts.

### Concurrency & Crash Consistency
- **EPVS (Epoch Protection Version Scheme):** Two-phase intermediate CAS (Complete→Prepare→InProgress). Version bumps only on Prepare→InProgress, preventing ABA. Module: `state/`.
- **Memory ordering:** Release for writers (seal), Acquire for readers (is_sealed), AcqRel/Acquire for CAS. Explicit justification required for every atomic.
- **Crash analysis:** Every write path gets "power fails here" test. Checksums, version numbers, atomic metadata commits. Append-only where possible.
- **Epoch protection ≠ eviction prevention:** Guards prevent page struct reuse (ABA), NOT content eviction in lossy mode. `get_record()` can return None after successful `find_record_for_key()`.

### Flush Pipeline & Deadlock Prevention
- **Three-point deadlock:** Writers → Allocator (needs space) → Eviction (needs flushed pages) → Flusher (needs I/O callbacks) → I/O Workers (need CPU time) → unblocks Allocator.
- **Fix pattern:** (A) Break on QueueFull, (B) Poll completions + bounded retry with yield, (C) Maintenance integration. All three required. Based on C++ FASTER: RETURN_NOT_OK + TryComplete + DoThrottling.
- **Buffer headroom:** Needs 4× target pages for async flush (tail can't lap unflushed pages).
- **Eviction:** `max_in_memory_pages=256` default never triggers for small tests. Explicitly set for disk-heavy workloads.

### Key Files
- `record/record_info.rs` — RecordInfo, sealing, revivification
- `status.rs` — OperationStatus variants
- `store/operations.rs` — Write paths, lossy eviction race handling
- `state/` — EPVS, two-phase CAS protocol
- `hybrid_log/flush.rs` — Break-on-QueueFull pattern
- `device/device.rs` — poll_completions trait
- `recovery/log_recovery.rs` — Checksums, validation, parameterized prefix

### Testing & Config Patterns
- **Small config:** `hash_index_size_log2: 1-10, buffer_size_pages: 4, mutable_fraction: 0.5, max_in_memory_pages: 3, grow_config.enabled: false`. For memory pressure tests.
- **Tier-2 criteria:** 32MB pages, minimum-case proptests, multi-round I/O, concurrent stress. Keep tier-1 <1s.
- **OFFSET_BITS = 25:** 32MB pages → memory allocation dominates debug tests, not algorithmic work.
- **Multi-agent safety:** Stage+commit immediately. Use `git worktree` for isolation. Sub-agents can destructively revert — always verify.

## Key Learnings

<!-- Essential insights distilled from implementation work -->

- **Flush pipeline deadlock:** Multi-writer deadlock occurs when pages stuck in Flushing state → evictor can't advance → allocator hits buffer full → I/O workers need CPU time. Fix requires: (A) break on QueueFull, (B) poll_completions + bounded retry with yield, (C) maintenance integration. All three fixes mandatory.
- **Lossy eviction races:** Epoch guards prevent ABA (page reuse), NOT content eviction. `get_record()` can return None after successful `find_record_for_key()`. Always handle None gracefully in read/rmw/delete hot paths. Classification is advisory, not guaranteed.
- **Memory ordering discipline:** seal=Release, is_sealed=Acquire, CAS=AcqRel/Acquire. Every atomic gets explicit justification. RecordInfo packs all coordination into single u64 → single cache line touch.
- **EPVS ABA prevention:** Two-phase intermediate CAS (Complete→Prepare→InProgress). Version bumps ONLY on Prepare→InProgress, not on initial transition. Prevents stale CAS from succeeding after full cycle.
- **Cache-line costs:** Epoch table scan hits 256 entries (16KB, ~128ns/scan). compute_safe_epoch() called on every unprotect(). Amortization critical for performance.
- **Recovery parameterization:** Don't hardcode prefixes in recovery logic. Pass as parameters (log_prefix, data_prefix) to support flexible device configs.
- **Test performance:** 32MB pages (OFFSET_BITS=25) → allocation dominates debug time, not algorithms. Tier-2 tests for slow cases. Small config for memory pressure: hash_index_size_log2=1-10, buffer_size_pages=4.
- **Buffer headroom:** Async flush needs 4× target pages. Tail can't lap unflushed pages. EvictionPolicy::default() has max_in_memory_pages=256 — explicitly set lower for disk workloads.
- **Fuzz target isolation:** Fuzz targets in `rust/fuzz/` are standalone workspace. Not in main test matrix. Manual updates needed when core API changes.
- **Multi-agent hazards:** Sub-agents can destructively revert files during conflicts. Stage+commit immediately. Use `git worktree` for isolation.
- **CI thread starvation:** On constrained CI runners, threads spawned alongside many active worker threads may never get scheduled before a stop flag is set. Fix: use a `started` AtomicBool with Acquire/Release so the main thread spins until the target thread has run at least once. Never rely on scheduling fairness for assertions.
- **Statistical CAS assertions:** On macOS (and single-core CI), thread scheduling after a Barrier is deterministic enough that one CAS side wins every trial. Hard-assert only safety invariants (mutual exclusion); downgrade distribution checks to soft warnings. Add asymmetric yield-based jitter to improve contention diversity.

## Implementation History

<!-- Condensed record of major work items -->

### W2-02 — Sealed Bit (2026-03-07)
Bit layout: `[63:Fin][62:Tom][61:Inv][60:Seal][59..48:Ver][47..0:Prev]`. Version 13→12 bits. Release/Acquire ordering. Write paths fall back to copy-to-tail on sealed. Delete permitted on sealed.  
**Branch:** `sam/sealed-bit-p03`, **Commit:** `f2d516c1`

### EPVS Implementation (Iteration 6 A1)
Created `state` module (Phase, SystemState, AtomicSystemState) with 48 unit tests. Rewrote CheckpointStateMachine. Two-phase intermediate CAS for ABA prevention. Widened checkpoint metadata version u32→u64.  
**Branch:** `sam/epvs-implementation`, **Commit:** `88010752`

### A5 — Revivification (2026-03-08)
CAS-based unsealing via `try_revivify(expected)`. AcqRel/Acquire ordering. New `OperationStatus::Revivified` variant. 8 unit tests + 1 proptest + 5 integration tests. 1228 tests passing.  
**Branch:** `sam/revivification`, **Commit:** `2bb358a5`

### Memory Pressure Testing (2026-03-08)
25 tests (1184 LOC) in `tests/memory_pressure_tests.rs`: allocator stress, buffer pool, checkpoint, hash saturation, combined pressure, property-based, device variants. Small config pattern established.  
**Branch:** `sam/memory-pressure-tests`, **Commit:** `737ab04c`

### page-cache & page-store Samples (2026-03-09)
Two disk-I/O samples with PageFunctions, dedicated maintenance thread, retry-on-Aborted, SyncFileDevice. Demonstrated 4× buffer headroom requirement and eviction policy configuration.  
**Branch:** `sam/page-cache-store`, **Commit:** `b6c8fd88`

### Multi-Writer Deadlock Fix (2026-03-11)
Three-point fix: (A) FlushBatchResult with break-on-QueueFull, (B) poll_completions trait + bounded retry, (C) maintenance integration with poll+yield. 6 previously-ignored tests now pass. 1728 tests passing.  
**Branch:** `sam/deadlock-fix`, **Commits:** `5b50d994`, `a35b7846`, `75cba9ea`

### Backlog Sprint — API Parameterization (2026-03-11)
Added `log_prefix` parameter to recovery API (fc0d3da9, 33fb44cf). Tier-2 test classification: optimized 4 tests, marked 12 tier-2, 0 tier-1 >1s. Quadrant sprint with Aragorn, Éowyn, Galadriel.

### Lossy Mode Deadlock Fix (P0)
**Root cause:** `InMemoryDevice::truncate_until` zeroed `guard[..offset].fill(0)` under a write lock. As the log advanced, offset grew to GB-scale, making each truncation take seconds while blocking all concurrent `write_async` (flush) operations. After ~200s, the truncation time dominated all 16 writer threads' retry budgets → 0 ops/s permanent freeze. 13GB RSS was the growing Vec never shrinking.  
**Fix:** (A) InMemoryDevice::truncate_until → no-op (data below begin_address is never read, zeroing is cosmetic). (B) Maintenance pre-flush eviction block: use `evict_pages` + `try_advance_begin` instead of `evict_and_truncate` — no device truncation before flush. (C) Main eviction block: separated `evict_pages` + `try_advance_begin` + hash invalidation from device truncation, so the critical eviction path doesn't hold device locks during begin/hash updates. 1720 tests passing, clippy clean, loom check clean.

## Learnings

- **InMemoryDevice write-lock contention:** The RwLock<Vec<u8>> in InMemoryDevice serializes all write_async and truncate_until calls. Any O(N) operation under the write lock (like zeroing GB of data) creates a chokepoint that blocks ALL flush operations, starving the entire pipeline. truncate_until was O(total_logical_pages × page_size) and called multiple times per maintenance cycle.
- **Eviction churn in lossy mode:** In lossy mode, eviction → hash invalidation → keys appear "new" → copy-to-tail → tail advances → more eviction. This creates sustained tail advance that doesn't exist in non-lossy mode (where the key space saturates in mutable region and upserts are in-place). The effective data rate is ~50 MB/s for 1M keys with 5% delete rate.
- **Device truncation is optimization, not correctness:** `truncate_until` reclaims device space but is never needed for forward progress. Eviction (advancing head) + begin_address advancement (for correct Truncated classification) are the critical operations. Truncation can be deferred or made a no-op without affecting correctness.
- **Separate eviction from truncation in maintenance hot path:** Pre-flush eviction should never call truncate_until — it only needs to advance head so the allocator can reclaim buffer slots. Coupling eviction with device truncation causes unnecessary I/O contention during the most time-critical part of maintenance.
- **DST internals inventory (2026-03-12):** Completed comprehensive audit of all FasterKv subsystems for deterministic simulation requirements. Key findings: (1) InMemoryDevice/NullDevice invoke I/O callbacks synchronously inside write_async — simulator must model CompletedSync timing. (2) sync.rs already has 3-tier abstraction (loom > simulation > std) but thread::spawn, alloc::alloc_zeroed, and std::fs are NOT yet intercepted — needed for full DST. (3) Two minimum deadlock traces identified require modeling: page state machine (6 states, 10 transitions), device write lock contention (RwLock), bounded retry loop (32 attempts), and O(n) hash invalidation. (4) Overflow bucket allocator in MallocFixedPageSize grows unboundedly — 64 MB per page under skewed workloads. (5) compute_safe_epoch() O(n) scan runs on every unprotect() — amortized by fast-path drain_count==0 check.

### DST v2.0 Internals Inventory (2026-03-16)
Produced sam-dst-internals-inventory.md (38KB) — comprehensive audit of FasterKv subsystems for deterministic simulation requirements:

1. **Mapped 10 subsystems with 25 state transitions and 9 invariants:** InMemoryDevice/NullDevice completion callback timing, sync.rs 3-tier abstraction (loom > simulation > std), page state machine (6 states, 10 transitions), eviction pipeline, hash invalidation, overflow bucket allocator, compute_safe_epoch, write-lock contention model, bounded retry loops, and timeout handling.

2. **Both deadlocks traced through exact state transitions.** Bug 1 (multi-writer): infinite retry under buffer pressure → eviction starvation. Bug 2 (lossy mode): O(n) truncation under lock blocks flush pipeline → permanent 0 ops/s. Both root causes mapped to specific RwLock hold windows and scheduler decisions.

3. **Found sync.rs 3-tier abstraction ready for DST injection.** Thread spawning, memory allocation, and file I/O already have loom/simulation/std cfg hierarchy. Identified 5 interception points (alloc_zeroed, fs ops, thread::spawn, AtomicXXX operations, timing-sensitive code blocks).

4. **Defined minimum simulator state machine:** 11 page states, 7 device completion modes, 4 task scheduling levels, 3 eviction priority classes. Sufficient to reproduce both known deadlocks at 1M keys in <10 seconds with small buffers.

### DST v2.0 Phase 1 — SimClock Extensions + sim_hooks (2026-03-16)
Implemented SimInstant and yield/sleep hooks behind `#[cfg(feature = "simulation")]`:

1. **sim_time.rs (NEW):** `SimInstant` — drop-in replacement for `std::time::Instant`. Backed by `Arc<AtomicU64>` (nanoseconds), thread-local install/remove. Full API: `now()`, `elapsed()`, `Add<Duration>`, `Sub<Duration>`, `Sub<SimInstant>`, `PartialOrd`/`Ord`, `checked_add/sub`, `Debug`. 11 unit tests including cross-thread sharing.

2. **sim_hooks.rs (EXTENDED):** Added thread-local yield/sleep scheduling hooks: `on_yield()`, `on_sleep(d)`, `take_yield_request()`, `take_sleep_request()`, `request_yield()`, `request_sleep(d)`. 5 unit tests. Complements existing `sim_yield!`/`crash_point!` macros.

3. **sync.rs (MODIFIED):** Simulation tier now swaps `Instant → SimInstant` via type alias. Custom `thread` module wraps `yield_now()` and `sleep()` to record scheduling events in sim_hooks before delegating to OS. Loom tier untouched. Std tier untouched.

4. **Test compatibility:** 4 tests in `pending_io.rs` and 4 in `session.rs` that construct past instants via `Instant::now() - Duration` needed sim clock installation under simulation (SimInstant starts at 0, subtraction would underflow). Added cfg-gated `install_test_clock()`/`remove_test_clock()` helpers — no-ops under std.

**Verified:** std 1722 tests, simulation 1738 tests (16 new), loom 25 tests, clippy clean on both std and simulation.
**Branch:** `rust`, **Commit:** `717d9903`


---

### 2026-03-16: DST v2.0 Phase 1 — SimInstant + sim_hooks (P1)

**What:** Implemented SimClock foundation layer for DST v2.0 Phase 1 — drop-in `SimInstant` replacement for `std::time::Instant` and scheduling hooks.

**Files:**
- `rust/crates/faster-core/src/sync.rs` — aliased Instant → SimInstant under simulation feature
- `rust/crates/faster-core/src/sim_time.rs` — new SimInstant with thread-local clock backing
- `rust/crates/faster-core/src/sim_hooks.rs` — extended with yield/sleep scheduling hooks

**Tests:** 1722 pass (16 new). Loom 25 tests clean. Simulation 1738 tests pass.

**Key Pattern:** Tests that compute past instants via `Instant::now() - Duration` must install a test clock. Pattern documented in decisions.md.

**Cross-Agent Impact:**
- Provides foundational SimClock that Eowyn's SimDeviceV2 + DstRunner depend on (P2–P5)
- DST internals inventory (prior session) identified 10 subsystems requiring simulation — P1 enables hooking at sync layer

**Commit:** `717d9903`

**Branch:** `rust`

### Small-Pages Feature Flag (2026-03-16)
Implemented `#[cfg(feature = "small-pages")]` to set `OFFSET_BITS = 16` (64 KB pages) for DST testing:

1. **address.rs:** Cfg-gated `OFFSET_BITS` (25 → 16 under small-pages). Fixed `MAX_PAGE` computation to use u64 arithmetic (avoids `1u32 << 32` overflow when PAGE_BITS=32). Cfg-gated the constants test.

2. **allocator.rs:** Cfg-gated `ITEMS_PER_PAGE_BITS` (20 → 16 under small-pages) to satisfy the `ITEMS_PER_PAGE_BITS ≤ OFFSET_BITS` compile-time assertion. Added `#[allow(clippy::absurd_extreme_comparisons)]` on `bump_allocate` (MAX_PAGE=u32::MAX makes the guard tautological).

3. **log_allocator.rs:** Added `#[allow(clippy::absurd_extreme_comparisons)]` on two SF-7 page overflow guards.

4. **Cargo.toml:** Added `small-pages = []` feature to faster-core and `small-pages = ["faster-core/small-pages"]` to faster-dst.

5. **Test fixes (6 files):** Replaced hardcoded 25-bit offset masks with `MAX_OFFSET`, fixed `make_addr` in hash_index_concurrent to use `from_raw` for offset overflow, scaled buffer_size_pages in mutation/deadlock tests for small-page geometry.

**Verified:** 1722 tests pass (no feature), 1722 tests pass (small-pages), clippy clean both configs, loom 25 tests pass, DST crate builds with small-pages.
**Branch:** `temp/revert-validation-small-pages`, **Commit:** `3ea5ca81`

### Disk-Backed Forever Stress Test (2026-03-16)
Created `stress_disk.rs` example — production-realistic stress test using SyncFileDevice on real disk:

1. **SyncFileDevice + lossless mode:** Exercises flush/eviction pipeline on actual NVMe I/O, not InMemoryDevice. No lossy eviction — tests the compaction/GC path.

2. **Bounded working set:** Per-thread VecDeque tracks live keys. When at capacity (max_live_keys / num_threads), deletes oldest key before inserting. Live key count stays exactly at the cap (~50K in tests), preventing disk exhaustion.

3. **YCSB-like workload:** 60% upsert, 20% read, 20% delete across 16 threads (configurable). Each thread owns a disjoint key range partition — no cross-thread conflicts.

4. **Dedicated maintenance thread:** Pumps flush→evict pipeline every 5ms independent of worker threads.

5. **Graceful SIGINT:** Raw libc signal handler sets a static AtomicBool; all threads check it. Final stats printed before exit. process::exit(0) avoids pre-existing SyncFileDevice Drop segfault.

6. **Monitoring:** ops/sec, live key count, upsert/read/delete breakdown, RSS from /proc/self/statm, throughput cliff detection.

7. **Validated:** 20M+ ops/sec sustained, 30s smoke test clean, RSS bounded (~200-370 MB), clippy -D warnings clean.

**Branch:** `rust`, **Commit:** `103a887a`

### Wire Log Compaction into maintenance() (2026-03-17)
Closed the loop on bounded lossless disk operation. The compaction pipeline was fully built (scan → copy → pointer swing → begin-address advance → truncate_until) but never called from the maintenance loop:

1. **`LogSizeBudgetPolicy` (NEW):** Added to `compaction/policy.rs`. Triggers compaction when `total_log_bytes > budget_bytes`. Unlike `SpaceAmplificationPolicy`, doesn't need accurate live-data estimates — works with the O(1) `collect_stats` heuristic that treats all data as live. Follows C++ FASTER's `hlog_size_budget` model. 4 unit tests + 1 doc test.

2. **`maintenance()` now calls `maybe_compact()`:** Added step 5 at end of `kv.rs:maintenance()` — when `auto_compact` is enabled, calls `maybe_compact()` which checks the policy and runs the full 4-phase compaction pipeline if triggered. This is the critical missing link: without it, even `auto_compact: true` had no effect because nothing ever invoked the check.

3. **`stress_disk.rs` enables compaction:** Set `auto_compact: true`, import `LogSizeBudgetPolicy`, configure budget = `4 × buffer_size_pages × page_size` (2 GB with default 16 × 32 MB config). The maintenance thread now automatically compacts → advances begin_address → truncates old segments.

**Verified:** 1726 tests pass, clippy -D warnings clean, doc tests pass.
**Branch:** `rust`, **Commit:** `762bc3bd`

### Compaction SIGSEGV Fix (2026-03-17)
Fixed three compaction bugs found by Legolas's disk stress retest (SIGSEGV at T+30s under 16-thread load):

1. **SIGSEGV (critical):** Root cause was circular-buffer ABA hazard in `get_physical_address()`. After eviction, frame slots (`page % buffer_size`) get recycled for newer pages. The compaction scanner called `get_physical_address(old_addr)` for on-disk pages and received a valid pointer to the *wrong* page's frame. Concurrently, writer threads calling `maintenance()` inline (from `allocate_at_tail` retry) could evict and free frames the scanner was reading → use-after-free → SIGSEGV. Fix: (A) Added head_address bounds check to `get_physical_address()` — returns None for addresses below head. (B) Limited `compact()` scan range to `[head_address, safe_read_only)` so scanner only touches in-memory pages.

2. **95% throughput cliff (severe):** Compaction ran synchronously inside `maintenance()`, blocking the flush/evict pipeline for seconds. Writer threads stalled on buffer-full retries because no maintenance could run. Fix: (A) `maybe_compact()` uses `try_lock` instead of blocking lock. (B) Capped scan range to 4 pages per cycle so maintenance returns quickly between compaction chunks.

3. **170K-line log spam (minor):** Tombstone warning fired every 1024 entries. Changed to power-of-two intervals (~20 messages total).

**Verified:** 1726 tests pass, clippy -D warnings clean, stress_disk runs 5+ min (300s) under 16-thread load without crash. Throughput ~20-24M ops/s sustained, RSS bounded at ~7.8 GB.
**Branch:** `rust`, **Commit:** `d6f734fd`



### Scoped Page Pin — Option C Implementation (2026-07-24)
**Impact:** Implemented PinnedPage RAII guard with packed state+pin_count atomic CAS to eliminate use-after-free race in page frame access path. Joint work with Aragorn.
**Files:** `page.rs` (PackedPageState, PinnedPage, pin_page, try_evict with pin check), `log_allocator.rs` (pin_page), `scan.rs` (pin-protected reads), `record_ops.rs` (from_log_pinned, pinned reader methods), `eviction.rs` (pin-aware eviction), `operations.rs` (comment fixes), `mod.rs` (PinnedPage export).
**Key design:** Eviction no longer frees frame memory — frames stay in slots for recycling, guaranteeing all non-null frame pointers are always valid. try_evict_frame atomically checks pin_count == 0 via single CAS. Scanner (the SIGSEGV crash site) now has zero production unsafe blocks.
**Results:** 1734 tests pass (1726→1734, +8 pin tests). Clippy clean. stress_disk 60s @ 16 threads: ~11M ops/s sustained, no crash.
**Commit:** `96267b56`

### Allocator Bounds Checking — P1-B (2026-07-25)
**Impact:** Added unconditional runtime bounds checking to all allocator page-frame access methods. OOB access now panics (get/get_mut/get_ptr) or returns None (try_get/try_get_mut/try_get_ptr) instead of silently reading/writing past page boundaries.
**Files:** `allocator.rs` (resolve→unconditional assert, try_resolve+try_get/try_get_mut/try_get_ptr, 11 new tests), `log_allocator.rs` (get_physical_address offset check, mutable_record_at reject-not-clamp), `page.rs` (PinnedPage::as_mut_ptr_at→Option, get_slice overflow protection).
**Key design:** resolve() upgrades 3 debug_assert to unconditional assert (item_idx, page_idx, null page). try_resolve() provides Option path for callers wanting graceful handling. mutable_record_at no longer silently clamps oversized records — returns None. PinnedPage::as_mut_ptr_at returns Option<*mut u8>.
**Bounds checks added:** 6 unconditional runtime checks across 3 files (3 in resolve, 1 in get_physical_address, 1 in mutable_record_at size, 1 in as_mut_ptr_at). Plus overflow protection in get_slice.
**Results:** 1745 tests pass (1734→1745, +11 bounds tests). Clippy clean.
**Commit:** `19efbfbb`

### TypedIoContext Double-Free Guard — P2-A (2026-07-25)
**Impact:** Added release-mode double-free detection to `TypedIoContext::from_raw` and `reclaim`. Previously, the generation-counter guard was debug-only — in release builds a device bug invoking a callback twice would silently corrupt the heap via `Box::from_raw` on freed memory.
**Files:** `device.rs` — replaced debug-only `ContextEnvelope` with always-present `IoContextEnvelope<T>` carrying an `AtomicU64` sentinel + `ManuallyDrop<T>` data. Added 5 new tests.
**Key design:** Every I/O context is heap-allocated inside an `IoContextEnvelope { sentinel: AtomicU64, data: ManuallyDrop<T> }`. Both `from_raw()` and `reclaim()` atomically swap `sentinel` from `ALIVE (0x4641_5354_4552_494F)` → `CONSUMED (0xDEAD_DEAD_DEAD_DEAD)`. If the old value isn't `ALIVE`, panic before `Box::from_raw`. The atomic swap serialises concurrent double-callbacks (only one thread observes ALIVE). Debug builds retain generation counter for richer diagnostics.
**Overhead:** One `AtomicU64` swap (~1ns) on each I/O completion — negligible vs disk I/O latency. One extra `Box::new` per `from_raw` to re-box the extracted data.
**Results:** 1960 tests pass (faster-core + faster-tokio). Clippy clean. Pre-existing `faster-dst` compile error (unrelated `flush_page` signature change from another agent) does not affect this work.
**Commit:** `a92e44ff`
