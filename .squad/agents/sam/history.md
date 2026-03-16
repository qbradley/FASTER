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
