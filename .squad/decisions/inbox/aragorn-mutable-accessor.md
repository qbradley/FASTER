# Decision: Lifetime-Bound MutableRecordAccessor (P1-A)

**Author:** Aragorn (Rust Expert)
**Date:** 2026-07-25
**Status:** Implemented
**Branch:** `rust`
**Commit:** `8c829953`
**Requested by:** qbradley

## Summary

Added a lifetime parameter to `MutableRecordAccessor<'a>` and a safe factory method `HybridLogAllocator::mutable_record_at()` with bounds checking. This ties every mutable record accessor to the allocator's lifetime, preventing dangling pointers at compile time. Reduced unsafe call sites from 22 to 14 across the three affected files.

## Problem

`MutableRecordAccessor` stored a raw `*mut u8` pointer with no compiler-enforced lifetime connection to the underlying page. The safety argument ("mutable region cannot be evicted while sessions are active") was correct but relied on an **invariant not encoded in the type system**. This is the same bug class as the SIGSEGV fixed by PinnedPage — a raw pointer that could, under a future refactoring mistake, become dangling.

Additionally, no bounds checking was performed on record construction in release builds. A corrupted `LogicalAddress` could produce out-of-bounds writes past page boundaries.

## Design

### Lifetime Parameter

```rust
pub struct MutableRecordAccessor<'a> {
    ptr: *mut u8,
    record_size: u32,
    _lifetime: PhantomData<&'a ()>,
}
```

The `'a` is bound to the `&'a HybridLogAllocator` reference at construction time. The compiler ensures the accessor cannot outlive the allocator reference.

### Safe Factory Method

```rust
impl HybridLogAllocator {
    pub fn mutable_record_at(
        &self,
        addr: LogicalAddress,
        record_size: u32,
    ) -> Option<MutableRecordAccessor<'_>> {
        let ptr = self.get_physical_address(addr)?; // head-address check
        let remaining = page_size.saturating_sub(offset);
        let bounded_size = record_size.min(remaining); // OOB protection
        Some(unsafe { MutableRecordAccessor::new(ptr, bounded_size) })
    }
}
```

**Bounds checking:** `record_size` is clamped to the remaining page space. A `debug_assert!` catches mismatches in debug builds; the clamp prevents OOB writes in release.

### Why Not PinnedPage?

Mutable-region pages (above `head_address`) cannot be evicted. Pinning would add ~16ns per operation with zero safety benefit. The correct safety mechanism is lifetime binding to the allocator reference, which is zero-cost.

## Files Changed

| File | Change |
|------|--------|
| `record_ops.rs` | `MutableRecordAccessor<'a>` with `PhantomData`, `LogRecordWriter` return types updated, 1 test migrated |
| `log_allocator.rs` | `mutable_record_at()` factory method (+1 encapsulated unsafe) |
| `operations.rs` | 5 production + 1 test `MutableRecordAccessor::new` calls → `mutable_record_at()` |
| `copier.rs` | `allocate_with_retry` return type updated |
| `kv.rs` | `allocate_with_retry` return type updated |
| `hybrid_log_mutation_tests.rs` | 1 test migrated to `mutable_record_at()` |

## Verification

| Metric | Before | After |
|--------|--------|-------|
| Tests | 1734 pass | 1734 pass |
| Clippy | clean | clean |
| `unsafe {}` in operations.rs | 8 | **2** |
| `unsafe {}` in record_ops.rs | 11 | **8** |
| `unsafe {}` in log_allocator.rs | 3 | 4 (+1 encapsulated) |
| `MutableRecordAccessor::new` sites | 14 | **4** (miri tests only) |
| Net unsafe reduction | — | **−8** |
| Performance | baseline | no regression (zero-cost lifetime) |

## Rationale

| Decision | Rationale |
|----------|-----------|
| `PhantomData<&'a ()>` not `PhantomData<&'a mut u8>` | We don't need the compiler to enforce exclusivity at the type level — `&mut self` on write methods already does that. A shared-ref phantom is sufficient for lifetime binding. |
| Clamp to page boundary, not panic | Release builds must not panic on corrupted addresses. Clamping is defense-in-depth; the debug_assert catches bugs during development. |
| Keep `unsafe fn new()` public | Test code (miri_tests.rs) constructs accessors from raw heap buffers without an allocator. The escape hatch is necessary for testing the accessor itself. |
| Factory on HybridLogAllocator, not on MutableRecordAccessor | The allocator owns the head-address check and page table lookup. Centralizing construction there eliminates duplication. |

## Remaining Work

From the P1 tier of the unsafe survey:
- **P1-B:** Allocator `get()/get_mut()` unbounded pointer arithmetic (~120 LOC)
- **P2-A:** IoContext double-free protection (~60 LOC)
- **P2-C:** `as_mut_ptr_at` unbounded mutable pointer (~50 LOC)

These are independent of this change and can be tackled in separate sessions.
