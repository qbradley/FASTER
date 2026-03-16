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

