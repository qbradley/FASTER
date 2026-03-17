# Decision: Allocator Bounds Checking (P1-B)

**Author:** Sam (Systems & Storage Expert)
**Date:** 2026-07-25
**Status:** Implemented
**Branch:** `rust`
**Commit:** `19efbfbb`
**Requested by:** qbradley (via Aragorn's P1-B survey item)

## Summary

Added unconditional runtime bounds checking to all allocator page-frame access paths. Previously, `MallocFixedPageSize::resolve()` used `debug_assert!` only — release builds had no protection against corrupted `LogicalAddress` values. A bitflip in the hash index could have caused silent memory corruption or SIGSEGV.

## What Changed

### allocator.rs — MallocFixedPageSize

| Method | Before | After |
|--------|--------|-------|
| `resolve()` | 3× `debug_assert!` (item_idx, page_idx, null page) | 3× unconditional `assert!` — panics in all builds |
| `try_resolve()` | N/A | New: returns `Option<(*mut T, usize)>` with full bounds checking |
| `try_get()` | N/A | New: returns `Option<&T>` — checked variant of `get()` |
| `try_get_mut()` | N/A | New: returns `Option<&mut T>` — checked variant of `get_mut()` |
| `try_get_ptr()` | N/A | New: returns `Option<*mut T>` — checked variant of `get_ptr()` |

**11 new tests** verify both panic (should_panic) and Option (returns None) behavior for OOB item indices, OOB page indices, and unallocated pages.

### log_allocator.rs — HybridLogAllocator

| Method | Before | After |
|--------|--------|-------|
| `get_physical_address()` | No explicit offset bounds check | Checks `offset >= page_size` → returns `None` |
| `mutable_record_at()` | `debug_assert!` + silent clamp via `.min(remaining)` | Unconditional reject: returns `None` if `record_size > remaining` or `< RECORD_HEADER_SIZE`. Debug_asserts retained as supplement. |

### page.rs — PinnedPage

| Method | Before | After |
|--------|--------|-------|
| `as_mut_ptr_at()` | `debug_assert!(offset < size)`, returns `*mut u8` | Returns `Option<*mut u8>` with unconditional bounds check |
| `get_slice()` | `offset + len` could overflow | Uses `checked_add` to prevent overflow |

## Design Rationale

1. **Unconditional assert in `resolve()`** (not Option): The 31+ existing call sites of `get()` in hash traversal code would require massive refactoring to handle Option. Panic-on-OOB is the right behavior here — a corrupted address indicates a serious invariant violation. The `try_*` variants are available for callers that want graceful handling.

2. **Reject instead of clamp in `mutable_record_at()`**: Previously clamped oversized records silently, masking bugs. Now returns None — callers already handle the Option return. The clamp was defense-in-depth but hid the real error.

3. **Option return for `as_mut_ptr_at()`**: No existing callers (method was defined but unused), so signature change is safe. Future callers get proper bounds checking.

## Bounds Checks Added: 7 Total

1. `resolve()`: item_idx < ITEMS_PER_PAGE (unconditional assert)
2. `resolve()`: page_idx < dir.capacity() (unconditional assert)
3. `resolve()`: !page_ptr.is_null() (unconditional assert)
4. `get_physical_address()`: offset < page_size (returns None)
5. `mutable_record_at()`: record_size <= remaining (returns None)
6. `mutable_record_at()`: record_size >= RECORD_HEADER_SIZE (returns None)
7. `as_mut_ptr_at()`: offset < frame.size() (returns None)
Plus: `get_slice()` overflow protection via `checked_add`.

## Test Results

- **1745 tests pass** (1734 original + 11 new bounds-checking tests)
- `cargo clippy -p faster-core -- -D warnings` clean
- No regressions

## Open Items

- The `try_get()` / `try_get_mut()` / `try_get_ptr()` variants are available but not yet used by existing call sites. Migration is optional — the unconditional asserts in `get()`/`get_mut()` catch OOB in all builds.
- P1-A (MutableRecordAccessor lifetime safety) remains the next priority per Aragorn's survey.
