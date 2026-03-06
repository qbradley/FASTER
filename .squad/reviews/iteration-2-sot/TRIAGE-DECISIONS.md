# TRIAGE-DECISIONS.md — Iteration 2 SoT Review

**Triage lead:** Grand Admiral Thrawn (architect)
**Date:** Iteration 2 post-review
**Scope:** SF-1 through SF-15, C-1 through C-11
**Baseline:** MF-1, MF-2, MF-3 already resolved and committed

---

## Decision Summary

| ID | Title | Decision | Rationale |
|----|-------|----------|-----------|
| **SF-1** | Version chain cycle detection | **Fix now** | Trivial guard (counter + constant). Prevents infinite hang on corrupted data — DoS vector. |
| **SF-2** | `as_mut_slice(&self)` aliasing UB | **Mitigate** | Refactoring all write methods to `&mut self` or raw ptrs is high-effort. Added TODO citing SF-2. Current code works because write calls are sequential; Miri validation deferred. |
| **SF-3** | `upsert_copy_to_tail` discards old value | **Mitigate** | `SimpleFunctions` (blind overwrite) is unaffected. Merge-on-upsert is a future use case. Added TODO citing SF-3. Fix requires also adding `find_record_for_key` to the Fuzzy/ReadOnly upsert path. |
| **SF-4** | `SeqCst` overuse in allocator | **Note for later** | Performance optimization, not correctness. Requires benchmarking on both x86-64 and ARM. Linked to D-1 dissent. Added TODO on `snapshot()`. |
| **SF-5** | Redundant atomic loads in chain traversal | **Note for later** | Performance optimization. Requires passing `AddressInfo` through call chain. Added TODO on `find_record_for_key`. |
| **SF-6** | Triple-copy value path | **Note for later** | Performance optimization requiring API changes (`upsert_in_place_raw`). Added TODO at the hot path. |
| **SF-7** | Page number overflow at `MAX_PAGE` | **Fix now** | Trivial guard (one `if` check). Silent data corruption in a storage engine is unacceptable, even if 256 TB is unreachable today. |
| **SF-8** | Frame recycling race | **Fix now** | Replaced `store(Open)` with `try_transition` CAS. Same pattern already used elsewhere (`try_evict_frame`). Prevents concurrent zeroing of active data. |
| **SF-9** | Circular buffer frame aliasing | **Mitigate** | Added TODO citing SF-9 on `get_frame`. Full fix (page-number field in `PageFrame`) deferred — epoch protection prevents this in normal operation. |
| **SF-10** | `FlushCallbackContext` raw pointer lifetime | **Note for later** | Documented invariant is correct today. `Arc<PageTable>` or `FasterKv::Drop` belongs in the async I/O integration milestone. Added TODO on the raw pointer field. |
| **SF-11** | `free_immediate()` bypasses epoch | **Mitigate** | Restricted from `pub` to `pub(crate)`. External callers can no longer trigger use-after-free. Internal usage is audited and correct (Drop/cleanup paths only). |
| **SF-12** | Inconsistent module paths | **Fix now** | Mechanical find-replace. Changed `crate::hash_bucket::` → `crate::hash::bucket::`, `crate::hash_index::` → `crate::hash::index::`, `crate::hash::hash::KeyHash` → `crate::hash::KeyHash` across all `store/` code. |
| **SF-13** | `pending_io` exported but unintegrated | **Mitigate** | Added pre-alpha warning to module doc. Types are needed for incremental development; hiding them breaks the module structure. |
| **SF-14** | No timeout on pending I/O | **Note for later** | Entire pending I/O subsystem is incomplete (O-2). Timeout/cancellation design belongs with full async integration. Added TODO on `is_completed`. |
| **SF-15** | TOCTOU snapshot→access | **Mitigate** | Self-rebutted (epoch protection prevents eviction). Added `debug_assert!(is_in_memory)` after `get_physical_address` in both upsert and RMW mutable paths. |
| **C-1** | Alignment checks debug-only | **Mitigate** | Added TODO on `read_async`. With O_DIRECT the OS returns EINVAL anyway; without O_DIRECT, silent corruption is possible but callers enforce alignment via `RecordLayout`. |
| **C-2** | Snapshot non-atomicity | **Note for later** | Linked to D-1 dissent and SF-4. Rolled into the `snapshot()` TODO. Requires architecture-specific analysis (x86 vs ARM). |
| **C-3** | `find_record_for_key` stops at memory boundary | **Ignore** | Acknowledged "Fixed-size MVP" limitation (stated in module doc). On-disk records return `Pending` by design. |
| **C-4** | `drain_up_to` allocates two Vecs | **Note for later** | Performance optimization (~6% at 10M ops/sec). Added TODO citing C-4. `SmallVec` or in-place relinking is straightforward but not MVP-critical. |
| **C-5** | Flush callback ignores short writes | **Mitigate** | Added TODO in the callback. Short writes are rare on modern hardware but the failure mode (partial flush marked complete) is serious for durability. |
| **C-6** | `advance_to_next_page` returns stale address | **Ignore** | Self-rebutted: "no caller uses the returned address value." The value is used only as a success indicator (Some vs None). |
| **C-7** | `buffer_pool` at crate root | **Ignore** | Module placement is fine for MVP. The pool may serve additional consumers (e.g., read cache) in future iterations. |
| **C-8** | Inconsistent device placement | **Ignore** | Known temporary scaffolding. `SyncFileDevice` will move to `faster-device` crate per the existing plan. |
| **C-9** | `try_allocate(0)` succeeds | **Mitigate** | Added `debug_assert!(size > 0)` in `try_allocate`. All current callers pass sizes from `RecordLayout::compute` which are always > 0. |
| **C-10** | `begin_unsafe` re-entry debug-only | **Ignore** | Self-rebutted. `&mut self` → `SessionGuard<'a>` borrow model makes double-call impossible in safe Rust. The `debug_assert` is belt-and-suspenders. |
| **C-11** | Arithmetic overflow near `u32::MAX` | **Ignore** | Practically unreachable. `align_to_sector` inputs bounded to ~32 MB; `evict_pages` page numbers bounded to MAX_PAGE (8,388,607), far below `u32::MAX`. |

---

## Code Changes Made

### Fix Now (4 items)

1. **SF-1** — `store/operations.rs`: Added `MAX_CHAIN_DEPTH = 4096` constant and depth counter in `find_record_for_key`. Returns `None` with a `debug_assert` if the limit is exceeded.

2. **SF-7** — `hybrid_log/log_allocator.rs`: Added `if current.page().0 >= MAX_PAGE { return None; }` guard in both `try_allocate` (page-exact-fill path) and `advance_to_next_page` (before creating next page). Imported `MAX_PAGE` from `address`.

3. **SF-8** — `hybrid_log/page.rs`: Replaced `store(PageState::Open)` with `try_transition(state, PageState::Open)` CAS in `get_or_allocate_frame`. The CAS winner performs the zero + reset; losers skip and return the recycled frame.

4. **SF-12** — `store/operations.rs`, `store/kv.rs`, `store/session.rs`, `store/pending_io.rs`: Replaced all backward-compat path aliases with canonical module paths (`crate::hash::bucket::*`, `crate::hash::index::*`, `crate::hash::KeyHash`).

### Mitigate (9 items)

5. **SF-2** — `hybrid_log/record_ops.rs`: Added TODO comment on `as_mut_slice` documenting the Stacked Borrows violation and fix path (raw pointer ops or `&mut self`).

6. **SF-3** — `store/operations.rs`: Added TODO comment on `upsert_copy_to_tail` documenting that `old_value` is not read on the RCU path.

7. **SF-9** — `hybrid_log/page.rs`: Added TODO comment on `get_frame` about missing page-number verification in the circular buffer.

8. **SF-11** — `allocator.rs`: Changed `free_immediate` from `pub` to `pub(crate)` with `#[allow(dead_code)]` (kept for Drop paths and tests).

9. **SF-13** — `store/pending_io.rs`: Added pre-alpha scaffolding warning to module-level doc.

10. **SF-15** — `store/operations.rs`: Added `debug_assert!(is_in_memory)` after `get_physical_address` in upsert and RMW mutable paths.

11. **C-1** — `sync_file_device.rs`: Added TODO about promoting alignment `debug_assert`s to runtime checks.

12. **C-5** — `hybrid_log/flush.rs`: Added TODO about validating `bytes_transferred >= valid_bytes` for short write detection.

13. **C-9** — `hybrid_log/log_allocator.rs`: Added `debug_assert!(size > 0)` at the top of `try_allocate`.

### Note for Later (6 items — TODO comments only)

14. **SF-4 + C-2** — `hybrid_log/log_allocator.rs`: Added TODO on `snapshot()` about SeqCst→Acquire downgrade and read ordering.

15. **SF-5** — `store/operations.rs`: Added TODO on `find_record_for_key` about passing `AddressInfo` snapshot.

16. **SF-6** — `store/operations.rs`: Added TODO at the in-place upsert hot path about triple-copy elimination.

17. **SF-10** — `hybrid_log/flush.rs`: Added TODO on `FlushCallbackContext::page_table` about `Arc<PageTable>` or `FasterKv::Drop`.

18. **SF-14** — `store/pending_io.rs`: Added TODO on `is_completed` about timeout/cancellation.

19. **C-4** — `epoch/drain.rs`: Added TODO on `drain_up_to` about `SmallVec` optimization.

### Ignored (5 items — no code changes)

20. **C-3** — MVP limitation by design (on-disk records return `Pending`).
21. **C-6** — No caller uses the return value; harmless.
22. **C-7** — Module placement is reasonable; may serve future consumers.
23. **C-8** — Known scaffolding; planned move to `faster-device` crate.
24. **C-10** — Type system prevents re-entry in safe Rust.
25. **C-11** — Input values practically bounded well below overflow range.

---

## Verification

- `cargo clippy -p faster-core -- -D warnings` — **clean** (0 warnings, 0 errors)
- `cargo nextest run -p faster-core` — **656 passed, 3 skipped, 0 failed**
- Changes are **unstaged** per instructions (no commit made)
