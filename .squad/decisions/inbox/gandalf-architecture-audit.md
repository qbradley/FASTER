# Architecture & Maintainability Audit: faster-core

**Author:** Gandalf (Lead / System Architect)  
**Requested by:** qbradley  
**Date:** 2026-03-17  
**Scope:** `rust/crates/faster-core/src/` — 53,656 lines across 60+ files  
**Files audited:** kv.rs, session.rs, operations.rs, log_allocator.rs, flush.rs, bucket.rs, device.rs, eviction.rs, error.rs, compaction/* (7 files), functions.rs, lib.rs, status.rs

---

## Overall Architecture Grade: **B+**

**Rationale:** The architecture is fundamentally sound — clean module boundaries, correct concurrency primitives, excellent documentation in key files, and a layering strategy that correctly confines unsafe code. The single generic parameter `F: Functions` is the right abstraction for pluggable semantics. The epoch-based session model faithfully implements the FASTER paper.

However, several patterns fall short of the bar for mission-critical cloud services: silent failure paths that can lose data, duplicated logic that will diverge, and a few abstraction leaks that couple modules unnecessarily. These are all fixable, but they represent real risk in a planetary-scale deployment today.

---

## File-by-File Assessment

### `store/kv.rs` — 3,185 lines | Grade: B

**Can a new engineer understand this in a day?** The public API, yes. The maintenance/compaction internals, a week.

The struct is a legitimate facade — 14 fields, each a well-scoped subsystem. 47 public functions are well-organized into commented sections (Session, Batch, CRUD, Checkpoint, Maintenance, Grow, Accessors). The single generic `F: Functions` is clean.

**Critical concerns:**
- **Silent CAS failures in `complete_write_pending` (L1372, 1402, 1425, 1449):** `let _ = self.hash_index.update(...)` discards CAS failures. A failed CAS means the RCU record was allocated at the tail but never linked into the hash index — it becomes an orphan. The old on-disk record remains authoritative. This is **silent data loss on the write-completion path**.
- **I/O dispatch failures silently swallowed (L1101):** In release builds, failed `issue_read()` is completely invisible. The pending operation is lost with no counter, no log, no signal.
- **Triplicated page-flush loop (L272, L747, L1528):** The "iterate pages, seal Open, flush Sealed" pattern appears in `Drop::drop`, `checkpoint`, and `flush_and_evict`. Three copies that must stay in sync.
- **`entry_count()` returns 0 (L2025):** A public method that lies. Any capacity-planning or correctness check using this gets silently wrong behavior.
- **`maintenance()` double-computes buffer metrics (L1707 vs L1731):** Same logic computed in `u32` then re-computed in `u64` with different variable names. A subtle overflow bug waiting to happen.
- **`complete_write_pending` (L1316–1453):** 137 lines duplicating the write path from `operations.rs`. Any future change to write semantics must be made in **two places**.

**Strengths:** Excellent Drop impl with ordered shutdown (L247–318). Well-documented public API with examples. `session_scope` RAII pattern is clean. `Box<dyn Device>` prevents monomorphization on the hot I/O path (vtable dispatch per flush/read), but this is a pragmatic trade-off for ergonomics.

### `store/session.rs` — 1,642 lines | Grade: B+

**Can a new engineer understand this in a day?** Yes, with the module docs.

Clean ownership model. `FasterSession` is `!Send` via `PhantomData<*const ()>` — correct compile-time thread-affinity enforcement. The `SessionGuard` RAII pattern and `UnsafeContext` epoch-amortization are well-designed.

**Concerns:**
- **`UnsafeContext` CRUD methods (L699–856) duplicate `FasterKv` CRUD (kv.rs L889–1087):** 158 lines of near-identical code. The `UnsafeContext` path skips metrics and sim_yield, creating an observability gap for the "fast path" it was designed for.
- **`UnsafeContext::refresh()` (L665–674) breaks layering:** Directly indexes into `epoch_table.table[idx].local_current_epoch` with raw atomic stores. This should be an `EpochTable::refresh(index)` method.
- **Naming:** `begin_unsafe`/`end_unsafe`/`UnsafeContext` match C++ FASTER but confuse Rust developers who associate `unsafe` with memory safety. `enter_epoch`/`EpochAmortizedContext` would be clearer.
- **`drain_completed()` name is misleading (L361):** It drains *un-completed* pending ops. Should be `abandon_pending()`.
- **Overly-public API:** `end_unsafe()`, `next_serial()`, `epoch_thread()` are `pub` but should be `pub(crate)`.

**Strengths:** Zero `unsafe` in the entire file. Excellent module-level docs with architecture section. Zero-copy drains via `std::mem::take()`.

### `store/operations.rs` — 1,780 lines | Grade: B+

**Can a new engineer understand this in a day?** The protocol doc at the top (L1–39) makes this possible.

Well-structured CRUD implementations with clear region-based dispatch. The `InternalContext` bundle (L69–75) avoids threading many parameters. The `find_record_for_key` chain walker (L150–201) has proper cycle detection (MAX_CHAIN_DEPTH) and self-loop guard.

**Concerns:**
- **`allocate_at_tail` retries up to 32 times (L208, L245–257):** Each retry calls `maintenance()` + `yield_now()`. At ~32 iterations this is a few milliseconds of cooperative waiting, but there's no backoff — it's flat polling. Under sustained load, 32 threads each spinning 32 times creates a thundering-herd effect.
- **`layout_for_fixed` vs variable-length (L115–117):** The `total_size()` is wrong for variable-length values. The doc explains this (L109–113) but it's a footgun — any caller using `layout_for_fixed(...).total_size()` for allocation sizing will corrupt data. The `safe_read_record_size` workaround (L129–134) is clever but fragile.

**Strengths:** Excellent two-level prefetch pipeline (hash bucket → record). Protocol doc at file top is a model of clarity.

### `hybrid_log/log_allocator.rs` — 895 lines | Grade: A-

**Can a new engineer understand this in a day?** Yes. The ASCII diagram is exceptional.

Lock-free bump allocator with 5 monotonic address boundaries. CAS loops are clean. Zero heap allocations on the hot path.

**Concerns:**
- **`try_allocate` returns `Option` (L116):** `None` is overloaded for 4+ failure modes (sealed, page-full, buffer-full, MAX_PAGE overflow). Callers cannot distinguish them for informed retry decisions.
- **`shift_read_only_to_tail` single-threaded assumption (L308–310):** The TODO admits `safe_read_only_address` should advance via epoch callback, not immediately. This is a latent correctness bug for multi-threaded workloads.
- **Minor layering leak:** Tracks `flushed_until_address` (L60) despite claiming it "does not handle flushing" (L18).

**Strengths:** Module doc with invariant diagram. Every public function documented with failure modes. Only 3 unsafe sites, all with SAFETY comments.

### `hybrid_log/flush.rs` — 1,041 lines | Grade: B

**Can a new engineer understand this in a day?** The async callback lifecycle takes effort.

`PageFlusher` is stateless (sector_size + page_size) — correctly `Send + Sync`. The async/sync split is clean. `FlushError` is a rich enum distinguishing all failure modes.

**Concerns:**
- **`flush_completion_callback` silent failure (L187):** On I/O error, the page is left in `Flushing` state with no logging, no counter, no retry signal in release builds. This is a production debugging nightmare.
- **Short-write detection silent in release (L168–176):** Completely invisible — page stays in `Flushing` forever.
- **Dead code: `FlushRequest` (L40–48):** Defined, `#[derive(Debug)]`'d, never constructed or consumed.
- **`flush_page` vs `flush_page_sync` duplication (L222–248 vs L316–340):** ~30 lines of duplicated state-check + CRC trailer logic.

**Strengths:** Good rollback on error (L289–304). CRC trailer for integrity. Proper `Arc` lifetime management for async callbacks.

### `hash/bucket.rs` — 2,141 lines | Grade: A

**Can a new engineer understand this in a day?** The bit layout ASCII art and correspondence tables make this a pleasure to read.

This is some of the best-documented Rust code in the crate. Zero unsafe. Cache-line alignment verified at compile time. Property tests with proptest. Memory ordering rationale documented per-method.

**Minor concerns:**
- **`insert_in_chain` (L859):** Returns `Result<(), ()>` but `Err` is unreachable — dead code.
- **`for_each_bucket` (L771):** Unbounded chain walk with no cycle detection. Corruption could cause infinite loop.
- **`find_entry` returns first tag match (L591):** Correct for FASTER semantics but undocumented design choice.

### `device.rs` — 904 lines | Grade: B-

**Can a new engineer understand this in a day?** The `TypedIoContext` lifecycle is hard to follow.

**Critical concerns:**
- **`Device` trait lacks `flush`/`fsync` (L297–370):** No durability guarantee. If a backend buffers writes, there's no way to force them to stable storage. This is critical for crash consistency.
- **`TypedIoContext::from_raw` re-boxes data (L268):** Unnecessary heap allocation on every I/O completion. `ManuallyDrop::take` moves data out, then `Box::new(data)` re-allocates it.
- **No sector-alignment validation:** Neither `NullDevice` nor `InMemoryDevice` validates the alignment contract, even in debug builds.
- **`InMemoryDevice` not feature-gated:** Lives in `faster-core` (not a test crate), so production code can accidentally depend on it. Should be behind `#[cfg(test)]` or `test-utils` feature.
- **Parameter ordering inconsistency:** `read_async(offset, dest, len, ...)` vs `write_async(source, offset, len, ...)`.

**Strengths:** Sentinel-based double-free detection in `TypedIoContext`. `InMemoryDevice` explicitly documented as non-production.

### `hybrid_log/eviction.rs` — 615 lines | Grade: B+

**Can a new engineer understand this in a day?** Yes. Small, focused module.

**Concerns:**
- **`advance_head` evicts inline (L159–215):** Combined with `evict_pages` which also calls `try_evict_page`, pages can be evicted from two code paths. Not a bug, but the dual-path logic isn't documented.
- **Batch size controls scan width, not eviction count (L98–112):** The doc says "stops after batch_size pages" which implies eviction count, but the code scans batch_size pages regardless of evictions.
- **`EvictionPolicy` has no validation:** `eviction_batch_size: 0` makes `evict_pages` a no-op. Should `derive(Copy)` (8 bytes).

**Strengths:** Zero unsafe. Stateless beyond configuration. Clean separation from allocator.

### `compaction/*` — 7 files, ~3,227 lines | Grade: B+

**Can a new engineer understand this in a week?** Yes. The pipeline decomposition is textbook.

Excellent module structure: scanner → copier → address_update → begin_address → orchestrator, with policy as a clean strategy pattern. `CompactionPlan` → `CopyResult` → `SwingStats` → `TruncationResult` data flow is strictly linear.

**Critical concerns:**
- **`swing()` all-CAS-failed is silently ignored (orchestrator.rs L194):** If every hash pointer swing fails, the orchestrator proceeds to truncate the old region. Copied records are accessible, but un-swung records point to truncated addresses — **data loss**.
- **`collect_stats` makes auto-compaction policies inert (orchestrator.rs L281–299):** Returns `live_data_bytes == total_log_bytes`, so `SpaceAmplificationPolicy` always sees amplification = 1.0 and never triggers. Users who configure it get no compaction.
- **`drain_epoch` spin-wait (orchestrator.rs L252–263):** 10M-spin hard-coded timeout with no backoff. Each spin calls `bump_current_epoch_no_callback()`, burning CPU.
- **No crash-recovery testing:** `crash_point!` instrumentation exists but no test verifies recovery from crash between phases.

**Strengths:** Module docs are excellent. Safety invariant (never classify live as dead) is prominently documented. Clean layering — compaction uses only public APIs of hash/hybrid_log.

### `error.rs` — 577 lines | Grade: A

**Can a new engineer understand this in an hour?** Yes. Model error module.

Rich `FasterError` enum with 8 variants covering all failure domains. `Display`, `Error`, `From` conversions for composition. The error-handling audit table (L33–77) documenting every `unwrap`/`expect` in the crate is exemplary. `#[non_exhaustive]` would be nice for forward compatibility but the type is likely internal enough that it's not critical.

---

## Top 5 Maintainability Risks (Ranked)

### 1. **Silent Failure Paths — Data Loss Risk** 🔴

| Location | What happens | Impact |
|----------|-------------|--------|
| kv.rs L1372, 1402, 1425, 1449 | CAS failure in write completion silently ignored | Orphaned records, lost writes |
| kv.rs L1101 | I/O dispatch failure swallowed in release | Lost pending operations |
| flush.rs L187 | Async I/O error leaves page in Flushing forever | Silent page loss |
| orchestrator.rs L194 | All-CAS-failed swing proceeds to truncate | Hash entries point to truncated data |

**Why this matters:** A cloud service operating at planetary scale will hit every edge case. Silent data loss means customers discover corruption days later, after backups have been overwritten. Every failure path must either succeed, retry, or surface an error.

**Fix:** Replace every `let _ = hash_index.update(...)` in the write-completion path with a retry loop or error return. Add metrics counters for all suppressed errors. Make `flush_completion_callback` log errors unconditionally.

### 2. **Duplicated Logic That Will Diverge** 🔴

| Code path A | Code path B | Lines duplicated |
|-------------|-------------|-----------------|
| kv.rs `complete_write_pending` | operations.rs `internal_upsert/rmw/delete` | ~137 lines |
| kv.rs `Drop::drop` page flush | kv.rs `checkpoint` page flush | ~25 lines |
| kv.rs `checkpoint` page flush | kv.rs `flush_and_evict` page flush | ~25 lines |
| kv.rs `compact()` address calc | kv.rs `maybe_compact()` address calc | ~10 lines |
| session.rs `UnsafeContext` CRUD | kv.rs `FasterKv` CRUD | ~158 lines |
| flush.rs `flush_page` state check | flush.rs `flush_page_sync` state check | ~30 lines |

**Why this matters:** Every duplication is a divergence bug waiting to happen. When someone fixes a subtle issue in `internal_upsert`, they won't know to also fix `complete_write_pending`. When flush semantics change, three page-iteration loops must be updated in lockstep.

**Fix:** Extract `flush_all_pages_sync()`, `compaction_range()`, and an RCU-copy-to-tail helper. For the session CRUD duplication, factor the shared skeleton into a `fn perform_operation<F>(...)` that takes a closure for the differences.

### 3. **`try_allocate` Failure-Mode Opacity** 🟡

The hot-path allocator returns `Option<LogicalAddress>` where `None` conflates sealed, page-full, buffer-full, and MAX_PAGE overflow. Callers (operations.rs `allocate_at_tail`) cannot make informed retry decisions.

**Why this matters:** Under sustained load, the 32-iteration flat-polling retry loop in `allocate_at_tail` (operations.rs L245–257) creates a thundering herd. If callers could distinguish "page boundary" (advance) from "buffer full" (maintenance) from "overflow" (fatal), they could take targeted action instead of calling maintenance() 32 times.

**Fix:** Return `Result<LogicalAddress, AllocError>` with variants `Sealed`, `PageBoundary`, `BufferFull`, `Overflow`.

### 4. **Epoch / Layering Leaks** 🟡

- `UnsafeContext::refresh()` directly indexes into `epoch_table.table[idx]` and manipulates atomics. This is epoch-table internals leaking into the session layer.
- `shift_read_only_to_tail()` admits it should use epoch callbacks but doesn't (log_allocator.rs L308–310). This is a latent correctness bug for multi-threaded sessions.
- `maintenance()` in kv.rs orchestrates device truncation, hash invalidation, and begin-address advancement — three concerns that should be encapsulated (e.g., `LossyEvictionManager`).

**Why this matters:** When the epoch system is extended for checkpoint/grow, these leaks become coupling points that make refactoring dangerous.

**Fix:** Add `EpochTable::refresh_slot(index)`. Extract lossy-mode maintenance into a dedicated type. Track the `shift_read_only` epoch-callback as a blocking issue.

### 5. **Auto-Compaction Policy Doesn't Work** 🟡

`collect_stats()` (orchestrator.rs L281–299) returns pessimistic stats where `live_data_bytes == total_log_bytes`. This means:
- `SpaceAmplificationPolicy` always sees amplification = 1.0 → never triggers
- `TombstonePercentPolicy` always sees 0% tombstones → never triggers
- Only `LogSizeBudgetPolicy` and `ManualPolicy` actually work

Users who configure auto-compaction with space-amplification or tombstone-percentage policies will get no compaction, silently.

**Fix:** Either implement real stats collection (scan a sample of pages) or document the limitation prominently and remove the non-functional policies from the public API.

---

## Recommended Refactoring Priorities

### P0 — Do Now (Data Safety)
1. **Fix silent CAS failures in `complete_write_pending`** — add retry loop or error propagation
2. **Add unconditional error logging in `flush_completion_callback`** — at minimum a metrics counter
3. **Guard compaction orchestrator against all-CAS-failed swing** — check `stats.swung > 0` before truncate

### P1 — Next Sprint (Maintainability)
4. **Extract `flush_all_pages_sync`** from the triplicated loop in kv.rs
5. **Extract RCU-copy-to-tail helper** shared by operations.rs and kv.rs write completion
6. **Add `EpochTable::refresh_slot(index)`** and remove raw epoch-table access from session.rs

### P2 — Planned Refactor (Quality)
7. **Return `Result<LogicalAddress, AllocError>` from `try_allocate`** with distinct failure modes
8. **Merge or remove `complete_pending`/`complete_pending_sync`** — `try_complete_pending` subsumes both
9. **Remove or `unimplemented!()` the `entry_count()` stub** — a public API that returns 0 is worse than not having the method
10. **Fix `collect_stats` or remove non-functional policies** from the public API

### P3 — Technical Debt
11. Consider making `Device` generic on `FasterKv` to enable monomorphization on hot I/O paths
12. Add `flush`/`fsync` to the `Device` trait for crash consistency
13. Gate `InMemoryDevice` behind a `test-utils` feature flag
14. Rename `begin_unsafe`/`UnsafeContext` to epoch-oriented names for Rust idiom
15. Add crash-recovery tests for compaction (crash between copy and swing phases)

---

## Architecture Strengths Worth Preserving

1. **Module documentation is exceptional.** ASCII diagrams in allocator, bit layouts in bucket, protocol docs in operations, pipeline overview in compaction. This is above industry average.
2. **Unsafe confinement is correct.** `#[deny(unsafe_code)]` on safe modules (12 of 18 top-level modules). Unsafe is confined to device, allocator, hash table, and epoch — exactly where it belongs.
3. **The `Functions` trait abstraction is clean.** One generic parameter, associated types for Key/Value/Input/Output/Context. `SimpleFunctions` and `CounterFunctions` are good default implementations.
4. **Error type design is strong.** `FasterError` vs `OperationStatus` correctly separates exceptional failures from control-flow signals. The error-handling audit table in error.rs is exemplary.
5. **Compaction pipeline decomposition is textbook.** Scanner → Copier → AddressUpdater → BeginAddressAdvancer with pure data flowing between phases.
6. **Test coverage is comprehensive.** Property tests on bucket operations, concurrent tests on hash index, multi-threaded store tests. The 2,100+ test count is well-distributed.

---

## Appendix: Lines of Code by Module

| Module | Lines | Unsafe sites | `#[deny(unsafe_code)]` |
|--------|------:|:------------:|:---------------------:|
| store/ | 8,786 | 1 | ❌ |
| hash/ | 7,299 | 8 | ❌ |
| hybrid_log/ | 5,019 | 7 | ❌ |
| compaction/ | 3,227 | 0 | ✅ |
| record/ | 2,816 | 3 | ✅ |
| recovery/ | 3,920 | 1 | ❌ |
| checkpoint/ | 3,484 | 2 | ❌ |
| epoch/ | 1,847 | 0 | ❌ |
| grow/ | 2,768 | 0 | ✅ |
| device.rs | 904 | ~20 | ❌ |
| Other | 3,586 | 2 | Mixed |
| **Total** | **53,656** | **~44** | **12/18** |
