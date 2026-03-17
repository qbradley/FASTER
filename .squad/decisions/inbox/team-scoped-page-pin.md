# Decision: Scoped Page Pin (Option C) for Safe Concurrent Page Access

**Authors:** Sam (Systems & Storage Expert) + Aragorn (Rust Expert)
**Date:** 2026-07-24
**Status:** Implemented
**Branch:** `rust`
**Commit:** `96267b56`
**Requested by:** qbradley

## Summary

Implemented Option C (Scoped Page Pin) from Aragorn's safe-page-access design analysis. A `PinnedPage` RAII guard prevents eviction while any pin is held, eliminating the use-after-free race class in the page frame access path. The packed `[pin_count:28 | state:4]` AtomicU32 ensures state checks and pin count modifications happen in a single CAS operation.

## Problem

The compaction scanner SIGSEGV (Sam's prior fix) was caused by a TOCTOU race: `get_physical_address()` checks head_address, then dereferences the frame pointer, but eviction can free the frame between those steps. Sam's head-address bounds check mitigated the most common crash (scanning already-evicted pages) but did not close the theoretical race window (~10-50ns).

Additionally, multiple code comments incorrectly claimed that epoch protection prevents page eviction. Epochs coordinate deferred callback drains — they do NOT hold back eviction. Frame memory was freed immediately via `Box::from_raw()` during eviction with no epoch deferral.

## Design

### Packed AtomicPageState

```
[pin_count: 28 bits (high)] | [state: 4 bits (low)]  — single AtomicU32
```

- `try_pin()`: CAS loop — reject Free/Evicted, increment pin_count
- `unpin()`: `fetch_sub(1 << 4)` — atomic decrement
- `try_evict(expected_state)`: strict CAS — only succeeds if pin_count == 0 AND state matches
- `try_transition(expected, desired)`: CAS loop — preserves pin_count across state changes

### PinnedPage RAII Guard

```rust
pub struct PinnedPage<'a> { frame: &'a PageFrame }
```

- Created by `PageTable::pin_page()` or `HybridLogAllocator::pin_page()`
- Provides safe `as_slice()`, `get_slice()` access to page frame data
- Drops → `unpin()` → re-enables eviction

### Frame Lifetime Guarantee

**Key invariant change:** Eviction no longer frees frame memory or nulls the slot pointer. Frames remain in the table in `Evicted` state and are recycled in-place by `get_or_allocate_frame()`. This guarantees that all non-null frame pointers are always valid for the lifetime of the `PageTable`, which makes `pin_page`'s `frame_ref()` call sound.

### Eviction Pin Check

`try_evict_frame()` uses `AtomicPageState::try_evict(Flushed)` — a single CAS that atomically verifies `pin_count == 0 AND state == Flushed`, then transitions to `Evicted`. If the page is pinned, eviction fails and the evictor skips to the next candidate.

## Files Changed

| File | Change |
|------|--------|
| `page.rs` | `AtomicPageState` → packed AtomicU32, `PinnedPage` struct, `pin_page()`, pin-aware `try_evict_frame()`, 8 new tests |
| `log_allocator.rs` | `pin_page()` with head-address check |
| `scan.rs` | `try_read_record()` uses `pin_page` — zero production `unsafe` |
| `record_ops.rs` | `from_log_pinned()`, pinned `read_record_info/read_key/read_value/read_header_and_match_key*` |
| `eviction.rs` | `try_evict_page()` treats Evicted as already-done |
| `operations.rs` | Fixed 6 incorrect epoch/eviction comments |
| `mod.rs` | Export `PinnedPage` |

## Verification

| Metric | Before | After |
|--------|--------|-------|
| Tests | 1726 pass | 1734 pass (+8 pin tests) |
| Clippy | clean | clean |
| scan.rs unsafe (production) | 1 | **0** |
| stress_disk 60s @16T | ~11M ops/s | ~11M ops/s (no regression) |
| SIGSEGV crash | Mitigated (TOCTOU) | **Eliminated** (pin prevents eviction) |

## Rationale

| Decision | Rationale |
|----------|-----------|
| Packed AtomicU32 (not separate AtomicU8 + AtomicU32) | Eliminates TOCTOU between state check and pin count modification — single CAS is atomic |
| Don't free frames on eviction | Guarantees non-null frame pointers are always valid, making pin_page sound without hazard pointers |
| Pin per page, not per record | One atomic inc/dec per page access, not per record — <3% overhead since pages hold ~1.4M records |
| Keep raw `get_physical_address` for mutable writes | Mutable region (above head_address) cannot be evicted — pinning would be pure overhead |
| `from_log_pinned` returns `(PinnedPage, RecordAccessor)` | Caller holds pin lifetime explicitly; no lifetime parameter change to RecordAccessor needed |

## Known Limitations

1. **RecordAccessor still holds raw pointer.** The `RecordAccessor` returned by `from_log_pinned` holds a raw pointer whose validity depends on the caller keeping the `PinnedPage` alive. The compiler does not enforce this. A future `PinnedRecordAccessor<'a>` with a borrow from `PinnedPage` would make this fully compiler-checked.

2. **Operations.rs mutable-region writes are not pinned.** These are safe because the mutable region is above `head_address` and cannot be evicted, but they still use `unsafe` for raw pointer construction. Pinning would add ~16ns overhead per hot-path operation with no safety benefit.

3. **Memory footprint.** Frames are never freed during runtime (only on `PageTable::drop`). With 256 buffer slots × 32MB pages = 8GB peak, this matches the existing peak (frames were freed and immediately reallocated as the circular buffer wraps). The change eliminates free/realloc cycles, which is slightly more memory-efficient.
