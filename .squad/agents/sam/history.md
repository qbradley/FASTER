# Project Context

- **Owner:** qbradley
- **Project:** Rust implementation of Microsoft FASTER — a high-performance durable hash map
- **Stack:** Rust (primary), C++ (reference), C# (reference), C FFI
- **Goal:** Production-grade, no async runtime required, seamless Tokio integration, idiomatic Rust API + C FFI interface. Quality bar: mission-critical cloud services at planetary scale.
- **Created:** 2026-03-05

## Core Context

- **RecordInfo bit layout:** `[63:Fin][62:Tom][61:Inv][60:Seal][59..48:Version(12)][47..0:PrevAddr(48)]`. Version max 4095 (12 bits).
- **Sealing protocol:** `seal()` = Release, `is_sealed()` = Acquire, `try_seal()` = AcqRel/Acquire CAS. Write paths (upsert, rmw) fall back to copy-to-tail on sealed records. Delete permitted on sealed.
- **Revivification:** `try_revivify(expected)` atomically clears sealed bit 60 via CAS (AcqRel/Acquire). Conditions: mutable region + sealed + CAS wins + value fits. Status variant: `OperationStatus::Revivified`.
- **EPVS (Epoch Protection Version Scheme):** `state` module — Phase, SystemState, AtomicSystemState. CheckpointStateMachine uses AtomicSystemState. Two-phase intermediate CAS prevents ABA. Version bumps only on Prepare→InProgress.
- **Key files:** `record/record_info.rs` (RecordInfo, sealing, revivification), `status.rs` (OperationStatus variants), `store/operations.rs` (write paths, integration tests), `state/` module (EPVS).
- **Hash table:** `find_entry()` skips tentative entries — must call `update_entry()` with `entry.without_tentative()`. `entry_count()` returns 0 (intentional). `count()` reflects bump allocations only.
- **Testing config pattern:** `FasterKvConfig { hash_index_size_log2: 1..10, buffer_size_pages: 4, mutable_fraction: 0.5, max_in_memory_pages: 3, grow_config.enabled: false }`.
- **Multi-agent safety:** Stage+commit immediately. Use `git worktree` for isolated directory. Sub-agents can destructively revert files — always verify.

## Learnings
<!-- Append new learnings -->
- Fuzz targets in `rust/fuzz/` are a standalone workspace — they reference faster-core by path but have their own Cargo.toml. When the store API changes (e.g., OperationOutcome wrapping OperationStatus), fuzz targets need manual updates since they're not in the workspace test matrix.
- Proptest tests with `#[ignore]` inside `proptest!` blocks work fine — the attribute goes between `#[test]` and the function signature.
- Debug-mode test times are dominated by memory allocation (32MB pages, hash table construction) not algorithmic work. Reducing iteration counts by 5× can bring concurrent tests under 1s without losing contention coverage.
- The `OFFSET_BITS = 25` constant (32MB pages) means any test that allocates even one full page is inherently slow in debug. These should always be tier-2.
- Multiple concurrent agents sharing working tree creates constant conflicts — stage immediately.
- **Multi-writer deadlock root cause:** Not QueueFull per se — SyncFileDevice uses unbounded mpsc channel, `max_outstanding` is dead code. Real deadlock: pages stuck in Flushing state (I/O pending), evictor can't advance head (needs Flushed), allocator hits SF-10 (buffer full), maintenance does nothing useful (all pages already Flushing). Only I/O worker callback threads can break the cycle.
- `flush_sealed_pages` `continue` on QueueFull is wrong — scans remaining pages which all also return QueueFull, making zero progress. Should `break` and signal caller.
- `allocate_at_tail` calls maintenance exactly once and retries once — insufficient for pipeline stall. Writer threads need multi-retry with yield to let I/O callbacks fire.
- `maintenance()` return at kv.rs:1708 is `let _ = ...` — discards flush result, so can't detect zero-progress cycles.
- Flush completion signaling is entirely callback-driven (raw fn pointers on I/O threads). No condvar/waker — adding one requires `Arc` shared between FlushCallbackContext and maintenance caller.
- `FaultInjectingDevice` (io_error_injection.rs) supports Error and torn writes but NOT QueueFull injection — needs extension for deadlock testing.
- `advance_head` is strictly contiguous — can't skip unflushed pages. One slow page blocks entire eviction pipeline. This is by-design (matches C++).
- **Deadlock fix pattern:** Break-on-QueueFull + poll_completions + bounded retry loop with yield is the standard C++ FASTER pattern (RETURN_NOT_OK + TryComplete + DoThrottling). All three together close the pipeline stall.
- In lossy mode, maintenance() can evict pages between find_record_for_key (chain walk) and the subsequent get_record call. Never .expect() on get_record results in the read/rmw/delete hot paths — always handle None gracefully.
- The epoch protection comment in find_record_for_key was misleading: epoch guards don't prevent eviction when a separate maintenance thread drives lossy eviction concurrently. Snapshot-based classification is advisory, not a guarantee.
- Three operation paths were affected by the same race: internal_read (NotFound), internal_rmw FuzzyRegion/ReadOnly (Aborted), internal_delete FuzzyRegion/ReadOnly (NotFound). The Mutable paths already had proper None handling.
- EvictionPolicy::default() has max_in_memory_pages=256; never triggers eviction for small buffers. Must set explicitly for disk-heavy workloads.
- FASTER's flush pipeline is async (Sealed→Flushing→Flushed). Buffer needs 4x target_pages headroom so the tail doesn't lap the head while waiting for I/O callbacks.
- A dedicated maintenance thread (5ms interval) is essential for sustained writes beyond the buffer — writer retry loops alone can't pump the flush/evict pipeline fast enough.
- SimpleFunctions requires V: Copy; Vec<u8> needs a custom Functions impl (clone-based upsert/read/rmw like BlockFunctions in read-cache-sim).
- Sub-agent destructively reverted critical files — always verify sub-agent changes.
- Bash heredocs are reliable for recreating deleted files.
- Two-phase intermediate CAS protocol prevents ABA in EPVS state transitions.
- Keep version bumps on Prepare→InProgress only.
- Use `git worktree` for isolated directory — prevents file reverts from other agents' checkouts.
- `find_entry()` skips tentative entries — must call `update_entry()` with `entry.without_tentative()`.
- All 25 memory pressure tests run in <3s with small config.

## Session Log

### W2-02 — Sealed Bit in RecordInfo (P-03) (2026-03-07)
**Bit layout:** `[63:Fin][62:Tom][61:Inv][60:Seal][59..48:Version(12)][47..0:PrevAddr(48)]`

Version narrowed 13→12 bits (max 4095). `seal()` uses Release, `is_sealed()` uses Acquire, `try_seal()` uses AcqRel/Acquire CAS. Write paths audited; sealed records fall back to copy-to-tail. Delete permitted on sealed.

**Branch:** `sam/sealed-bit-p03`, **Commit:** `f2d516c1`

---

### EPVS Implementation (Iteration 6 A1)
Created `state` module (Phase, SystemState, AtomicSystemState) with 48 unit tests. Rewrote CheckpointStateMachine to use AtomicSystemState. Widened checkpoint metadata version u32→u64 with format_version field. Increased epoch PHASE_COUNT 8→16.

**Results:** 1215 lib tests passing (30 new). 42 integration tests passing.
**Branch:** `sam/epvs-implementation`, **Commit:** `88010752`

---

### A5 — Revivification (P-04) (2026-03-08)
CAS-based unsealing: `try_revivify(expected)` atomically clears sealed bit. AcqRel/Acquire ordering matches `try_seal` pattern. New `OperationStatus::Revivified` variant. `with_sealed_cleared()` clears bit 60.

**Conditions:** (1) mutable region, (2) sealed, (3) CAS wins, (4) value fits.
**Files:** `record/record_info.rs` (+8 unit tests +1 proptest), `status.rs`, `store/operations.rs` (+5 integration tests).
**Results:** 1228 tests passing. Clippy clean.
**Branch:** `sam/revivification`, **Commit:** `2bb358a5`

---

### Memory Pressure and OOM Testing (2026-03-08)
25 tests in `tests/memory_pressure_tests.rs` (1184 LOC): allocator stress (4), buffer pool/hybrid log (5), hybrid log checkpoint (2), hash table saturation (6), combined pressure (2), property-based (3), device variants (2).

**Key pattern:** Small `FasterKvConfig` with `hash_index_size_log2: 1..10`, `buffer_size_pages: 4`, `mutable_fraction: 0.5`, `max_in_memory_pages: 3`, `grow_config.enabled: false`.
**Branch:** `sam/memory-pressure-tests`, **Commit:** `737ab04c`

---

### page-cache and page-store Sample Crates (2026-03-09)
Two disk-I/O-heavy samples: `page-cache` (lossy cache with linear/Zipf key distributions) and `page-store` (durable store with delete-based retention via `--max-live-pages` and `--retention-ratio`). Both use PageFunctions (custom Functions for u64 → Vec<u8>), dedicated maintenance thread for flush pipeline, retry-on-Aborted for allocation pressure, and SyncFileDevice with `"log."` prefix.

**Key insight:** Buffer needs 4× target pages for async flush headroom. EvictionPolicy::default() never triggers for small buffers.
**Files:** `crates/samples/page-cache/{Cargo.toml,src/main.rs}`, `crates/samples/page-store/{Cargo.toml,src/main.rs}`, workspace `Cargo.toml`.
**Results:** Both compile, clippy clean, and sustain >180K writes/sec on default config.
**Branch:** `sam/page-cache-store`, **Commit:** `b6c8fd88`

---

### Multi-Writer Deadlock Preparatory Analysis (2026-03-10)
Full code inventory of the 3-point structural deadlock: (1) `flush_sealed_pages` `continue` on QueueFull makes zero progress, (2) `allocate_at_tail` single-retry insufficient for pipeline stalls, (3) SyncFileDevice `max_outstanding` is dead code (unbounded channel never returns QueueFull). Mapped all touch points across flush.rs, eviction.rs, log_allocator.rs, operations.rs, kv.rs, device.rs, sync_file_device.rs. Identified `FaultInjectingDevice` as extensible test harness (needs QueueFull mode). Documented risks: contiguous eviction constraint, callback thread safety, discarded flush results.

**Output:** `.squad/decisions/inbox/sam-deadlock-prep-analysis.md`

---

### Multi-Writer Deadlock Fix Implementation (2026-03-11)
Implemented all three fixes from Aragorn's design to close the multi-writer flush pipeline deadlock:

**Fix A** (`5b50d994`): `flush_sealed_pages` now returns `FlushBatchResult { flushed, queue_full }` and breaks on QueueFull instead of continuing. `maintenance()` captures the `queue_full` flag (was `let _ =`).

**Fix B** (`a35b7846`): Added `poll_completions()` to `Device` trait (default returns 0). `SyncFileDevice` implementation yields to give I/O workers CPU time. `allocate_at_tail` now has a bounded retry loop (MAX_ALLOC_RETRIES=32) calling maintenance+yield between attempts.

**Fix C** (`75cba9ea`): `maintenance()` now calls `poll_completions()` after every flush batch, and adds yield+poll when queue_full or buffer_pressure is detected.

**Files:** `flush.rs`, `mod.rs` (export), `device.rs`, `sync_file_device.rs`, `operations.rs`, `kv.rs`.
**Results:** 1728 tests pass. 6 previously-ignored deadlock tests now pass (including `multi_writer_forward_progress` with 3 writers + SlowDevice). Clippy clean.
**Branch:** `sam/deadlock-fix`, **Commits:** `5b50d994`, `a35b7846`, `75cba9ea`
- The Device trait already has a default `poll_completions()` method (returns 0) added during the deadlock fix. All Device implementations (NullDevice, InMemoryDevice, SyncFileDevice, SimulatedDevice) either use the default or have their own implementation. No additional work needed.
- Recovery code was tightly coupled to the hardcoded "log." prefix for log segment filenames. Added `log_prefix` parameter to `recover_fold_over()` and `recover_snapshot()` methods, and threaded it through all validation functions (`validate_log_file`, `validate_log_file_for_head`, `validate_page_checksums`).
- The simplest correct approach for the prefix fix is to pass the prefix as a parameter to recovery functions, rather than storing it in checkpoint metadata or trying to auto-discover it. This makes recovery explicit and predictable.

## 2026-03-11: Backlog Sprint — API Parameterization & Test Classification

**Timestamp:** 2026-03-11T19:33:26Z  
**Collaboration:** Quadrant sprint (Aragorn, Sam, Éowyn, Galadriel)

### What Happened

Fixed log prefix coupling in recovery API by adding parameterized prefix support. Verified `poll_completions` trait already had default implementation. Established tier-2 test classification criteria and optimized/marked 16 slow tests.

### Key Changes

1. **Log Prefix Coupling Fix**
   - Added `log_prefix: &str` parameter to recovery functions
   - Updated 4 hardcoded references in log_recovery.rs
   - Made recovery API explicit and device-config-agnostic

2. **poll_completions** (already complete)
   - Device trait has default impl (returns 0)
   - All Device implementations correct
   - No additional work required

3. **Tier-2 Test Classification**
   - Established criteria: 32MB pages, minimum-case proptests, multi-round I/O, concurrent stress
   - Optimized 4 tests by reducing iterations
   - Marked 12 tests tier-2
   - Result: 0 tier-1 tests >1s (was 17)

### Decisions Generated

- **Log Prefix Coupling Fix:** Parameter approach chosen over metadata storage
- **Tier-2 Test Classification Criteria:** Clear guidelines for marking slow tests

### Team Coordination

- **Aragorn:** Loom shim integration — SUCCESS
- **Éowyn:** DST smoke test split — SUCCESS
- **Galadriel:** Miri coverage expansion — SUCCESS
- **Coordinator:** Fixed 8 missed test callsites

**Commits:**
- fc0d3da9: Add log_prefix parameter to recovery API
- 33fb44cf: Update test callsites for log_prefix param
- 4fca1523 (Coordinator): Fix checkpoint/checksum test callsites

### Verification

All 1719 existing tests pass. Recovery API now supports custom prefixes. Test timing optimized for CI performance.
