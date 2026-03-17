# Unsafe Code Survey — faster-core

**Author:** Aragorn (Rust Expert)
**Date:** 2026-07-25
**Status:** Survey Complete
**Branch:** `rust`
**Requested by:** qbradley

## Executive Summary

The faster-core crate contains **119 `unsafe {}` blocks**, **31 `unsafe fn` declarations**, and **16 `unsafe impl` trait impls** across **17 files** (out of ~40 source files). The remaining ~23 files are gated with `#[deny(unsafe_code)]`. The crate-level `#![deny(unsafe_op_in_unsafe_fn)]` and `#![forbid(clippy::undocumented_unsafe_blocks)]` lints enforce discipline.

The PinnedPage work we just shipped (commit `96267b56`) eliminated the highest-risk class: concurrent page access via raw pointers in the read-only region. This survey identifies **6 additional refactoring opportunities**, ranked by risk.

## Totals

| Metric | Count |
|--------|-------|
| `unsafe {}` blocks | 119 |
| `unsafe fn` declarations | 31 |
| `unsafe impl Send/Sync` | 16 (8 types) |
| Files containing unsafe | 17 |
| Files gated `#[deny(unsafe_code)]` | ~23 |
| `// SAFETY:` comments | 136 |

## Categorized Inventory

### Category A: Raw Pointer Dereference (~31 sites)

| File | Line(s) | Pattern | Notes |
|------|---------|---------|-------|
| `allocator.rs` | 531, 554, 565 | `get()`/`get_mut()`/`get_raw()` — pointer arithmetic on page directory | No bounds check in release |
| `allocator.rs` | 639, 738 | `current_dir()` / `expand_directory()` — `AtomicPtr` deref | Acquire/Release ordering |
| `allocator.rs` | 387, 394 | `push_free_list_raw` — raw free_list pointer deref + embedded next-ptr write | Lock-free Treiber stack |
| `hybrid_log/page.rs` | 590, 616, 637, 670, 678, 713, 741 | `frame_ref()` — `*mut PageFrame → &PageFrame` | Relies on frame lifetime invariant |
| `hybrid_log/page.rs` | 354, 365 | `as_slice()` / `as_slice_mut()` — `from_raw_parts` on frame data | Caller must ensure exclusivity |
| `hybrid_log/page.rs` | 482 | `as_mut_ptr_at()` — raw mutable pointer into frame at offset | No bounds check in release |
| `hybrid_log/record_ops.rs` | 110, 148, 281, 319, 382 | RecordAccessor internal field access — deref stored `*const u8` | Validity depends on caller holding PinnedPage or mutable-region guarantee |
| `hybrid_log/flush.rs` | 164, 457 | Callback dereferences `*const PageTable`, constructs slice from frame ptr | Depends on external Drop ordering |
| `hybrid_log/log_allocator.rs` | 208, 506 | `as_mut_ptr()` / `from_raw_parts_mut` on frame | Mutable-region write path |
| `hash/table.rs` | 215, 248, 266 | `get_unchecked()` — elides bounds check on hot-path bucket lookup | Index masked by power-of-2 |
| `hash/index.rs` | 741 | `from_raw_parts` — bucket slice for serialization | Lifetime tied to `&self` |
| `store/operations.rs` | 471, 514, 815, 863, 1212 | `MutableRecordAccessor::new(ptr, size)` — hot-path record mutation | Relies on head-address invariant |
| `store/pending_io.rs` | 177, 491 | Read-completion callback context reconstruction | Same pattern as flush callback |
| `checkpoint/index_writer.rs` | 153, 692 | `HashBucket → [u8; 64]` type punning for serialization | `#[repr(C, align(64))]` guarantees layout |
| `recovery/index_recovery.rs` | 284 | `copy_nonoverlapping` — bucket deserialization | Same repr guarantee |

### Category B: Allocation / Deallocation (~13 sites)

| File | Line(s) | Pattern |
|------|---------|---------|
| `allocator.rs` | 219, 232, 240 | `alloc_zeroed()` / `dealloc()` for page directory pages |
| `allocator.rs` | 795, 842, 848, 863 | `ptr::read` free-list head, `Box::from_raw` for Drop cleanup |
| `hybrid_log/page.rs` | 313, 789 | `alloc_zeroed` for PageFrame, `Box::from_raw` in Drop |
| `device.rs` | 164, 174, 189, 199 | `Box::from_raw` — I/O callback context reconstruction |
| `buffer_pool.rs` | 68, 125 | `alloc()` / `dealloc()` for aligned buffers |

### Category C: I/O Callback / FFI Pattern (~15 sites)

| File | Line(s) | Pattern |
|------|---------|---------|
| `device.rs` | 36, 247, 264 | `IoCompletionCallback` type alias — `unsafe fn(*mut u8, IoStatus, u32)` |
| `device.rs` | 346, 355, 365, 378, 470, 483, 498, 504, 519, 525 | `InMemoryDevice` / `UringDevice` — `read_async`/`write_async` raw buffer + callback |
| `sync_file_device.rs` | 241, 248, 295, 412, 455 | Slice construction from raw buffer, `read_async`/`write_async` |
| `hybrid_log/flush.rs` | 157, 465, 599, 646, 810, 845, 912 | `flush_completion_callback` + I/O submission calls |
| `store/pending_io.rs` | 173, 830 | `read_completion_callback` + I/O submission |

### Category D: Type Punning / Transmute (~4 sites)

| File | Line(s) | Pattern |
|------|---------|---------|
| `checkpoint/index_writer.rs` | 153, 692 | `&HashBucket as *const [u8; 64]` — repr(C) bucket serialization |
| `recovery/index_recovery.rs` | 284 | `copy_nonoverlapping` into `HashBucket` from raw bytes |
| `hybrid_log/record_ops.rs` | 110, 281 | `ptr as *const AtomicRecordInfo` — record header reinterpretation |

### Category E: Prefetch Intrinsics (~4 sites)

| File | Line(s) | Pattern |
|------|---------|---------|
| `hash/prefetch.rs` | 32, 40, 73, 81 | `_mm_prefetch` (x86) / `prfm` asm (aarch64) — non-faulting hints |

### Category F: `unsafe impl Send/Sync` (16 impls, 8 types)

| Type | File | Line(s) | Justification |
|------|------|---------|---------------|
| `MallocFixedPageSize<T>` | `allocator.rs` | 330, 336 | Exclusive page ownership, CAS-protected free list |
| `FreeListPush` | `allocator.rs` | 361 | Deferred callback, single-use |
| `PageFrame` | `page.rs` | 278, 281 | Exclusive heap allocation, no interior mutability beyond atomics |
| `PageTable` | `page.rs` | 531, 533 | Slots are `AtomicPtr`, state is `AtomicU32` — all atomic |
| `DrainList` | `epoch/drain.rs` | 65, 71 | Lock-free Treiber stack, CAS-serialized |
| `FlushCallbackContext` | `flush.rs` | 145 | Sent to I/O thread, consumed exactly once |
| `IoRequest` | `sync_file_device.rs` | 211 | Sent via channel to worker thread |
| `FasterKv<F>` | `store/kv.rs` | 237, 239 | All fields are Send+Sync (Arc, atomics), session access is `&mut` |
| `AlignedBuffer` | `buffer_pool.rs` | 43, 46 | Exclusive heap allocation |

## Risk-Prioritized Refactoring Opportunities

### P0: Concurrent Access to Raw Pointers ✅ DONE

**PinnedPage (commit `96267b56`)** — Eliminated the use-after-free race in the page frame read path. The read-only region is now fully protected by pin-count CAS. No remaining P0 items.

### P1: Raw Pointer Arithmetic with Potential OOB

#### P1-A: `MutableRecordAccessor` on Hot Path — No Lifetime Tie to Page

**Files:** `store/operations.rs:471,514,815,863,1212` + `hybrid_log/record_ops.rs:250`
**Risk:** `MutableRecordAccessor` stores a raw `*mut u8` with no compiler-enforced lifetime connection to the underlying page. The SAFETY argument is "mutable region cannot be evicted while sessions are active" — this is correct but relies on an **invariant not encoded in the type system**. If a future refactor allows head-address to advance during active sessions, these raw pointers become dangling.
**Failure mode:** SIGSEGV or silent data corruption.
**What could race:** Head-address advancement during session operations (currently impossible, but not structurally prevented).

**Proposed wrapper:** `MutableRecordGuard<'a>` — borrows from a `&'a MutableRegionToken` that is tied to the session's epoch guard. The token proves the mutable region is stable. ~150 LOC wrapper + ~200 LOC call-site migration across `operations.rs`.
**Estimated LOC:** ~350
**Difficulty:** M

#### P1-B: `allocator.rs get()/get_mut()` — Unbounded Pointer Arithmetic

**Files:** `allocator.rs:531,550,565`
**Risk:** `resolve()` computes `page_ptr.add(item_idx)` where `item_idx` comes from address bit manipulation. In release builds there are no bounds checks. A corrupted `LogicalAddress` (e.g., from a bitflip in the hash index) could index out of bounds.
**Failure mode:** SIGSEGV or reading adjacent page's memory (data corruption).
**What could race:** Directory expansion could swap the page pointer array while `resolve()` is reading from the old one. The Acquire/Release ordering on the `AtomicPtr` prevents this for the directory pointer, but stale page pointers within the old directory could be freed in Drop.

**Proposed wrapper:** `CheckedSlot<'a, T>` — a reference type returned by `get()` that bounds-checks the address against the page's capacity and ties the lifetime to an epoch guard. For `get_mut()`, return a `&mut T` with the same bounds guarantee. ~80 LOC wrapper + ~40 LOC call-site changes.
**Estimated LOC:** ~120
**Difficulty:** S

### P1-C: `push_free_list_raw` — Treiber Stack with Tag-Based ABA

**Files:** `allocator.rs:385-404`
**Risk:** The tag field prevents ABA by incrementing on each CAS. But the tag is a fixed number of bits — after enough wraparound, a stale tag could match. In practice this requires ~2^N concurrent operations between a read and CAS retry, which is astronomically unlikely but not structurally impossible.
**Failure mode:** Free-list corruption → double allocation → data corruption.
**What could race:** Concurrent `push` + `pop` operations on the same free-list head.

**Proposed wrapper:** Not recommended — the Treiber stack is a well-understood pattern and the tag-based ABA is standard. The epoch framework already ensures items are not pushed until all readers have drained. **Accept as-is** with a note that formal verification (e.g., via loom model checking) would increase confidence.
**Estimated LOC:** N/A (loom test: ~100 LOC)
**Difficulty:** S (loom test only)

### P2: I/O Callback Context Lifecycle

#### P2-A: `TypedIoContext::from_raw` — Double-Free in Release Builds

**Files:** `device.rs:185-199`
**Risk:** The generation counter that detects double-consume of a callback context is **debug-only**. In release builds, if a device implementation invokes a callback twice (bug in device), the second invocation calls `Box::from_raw` on freed memory.
**Failure mode:** Double-free → heap corruption → crash or silent corruption.
**What could race:** Device bug invoking callback twice, or `reclaim()` racing with an unexpected callback invocation.

**Proposed wrapper:** `OwnedIoContext<T>` — wraps `NonNull<T>` and sets the pointer to a sentinel on consume. The consume method returns `Option<Box<T>>` and subsequent calls return `None`. Adds ~1 ns overhead (a null check). ~60 LOC.
**Estimated LOC:** ~60
**Difficulty:** S

#### P2-B: Raw `*const PageTable` in Flush Callbacks

**Files:** `hybrid_log/flush.rs:157-164`, `store/pending_io.rs:173-177`
**Risk:** Flush and read-completion callbacks store a `*const PageTable` / `*const HybridLogAllocator` as raw pointers. Their validity depends entirely on the external Drop order of `FasterKv`. If anyone changes `FasterKv`'s field order or adds an early-return in `Drop`, these become dangling.
**Failure mode:** SIGSEGV in I/O completion callback (hard to diagnose — happens on worker thread).
**What could race:** `FasterKv::drop()` completing before in-flight I/O callbacks.

**Proposed wrapper:** Use `Arc<PageTable>` in callback contexts instead of raw pointers. The flush path already uses heap-allocated contexts, so the Arc overhead is negligible relative to I/O latency. Alternatively, a `WeakRef<PageTable>` that returns `None` if the table is dropped. ~100 LOC.
**Estimated LOC:** ~100
**Difficulty:** M (touches flush + pending_io paths, needs careful testing)

### P2-C: `as_mut_ptr_at` — Unbounded Mutable Pointer into Frame

**Files:** `hybrid_log/page.rs:479-482`
**Risk:** Returns `*mut u8` with no lifetime, no bounds check in release. The caller can hold this pointer arbitrarily long, past eviction.
**Failure mode:** Out-of-bounds write or use-after-eviction.
**What could race:** Eviction advancing past the page while a stale pointer is held.

**Proposed wrapper:** Already partially addressed by PinnedPage. For mutable writes, a `PinnedMutSlice<'a>` that returns `&'a mut [u8]` bounded by the pin lifetime and offset-checked would close the gap. ~50 LOC.
**Estimated LOC:** ~50
**Difficulty:** S

### P3: Type Punning / Serialization

#### P3-A: HashBucket ↔ `[u8; 64]` Transmute

**Files:** `checkpoint/index_writer.rs:153,692`, `recovery/index_recovery.rs:284`
**Risk:** Reinterprets `HashBucket` as raw bytes for serialization. Sound because `HashBucket` is `#[repr(C, align(64))]`, but fragile if the repr or alignment changes.
**Failure mode:** Corrupt checkpoint → data loss on recovery.

**Proposed wrapper:** `impl AsBytes for HashBucket` — a safe trait with a `fn as_bytes(&self) -> &[u8]` method that uses `from_raw_parts` internally but is verified correct once. Could use `bytemuck::Pod` if the dependency is acceptable. ~30 LOC.
**Estimated LOC:** ~30
**Difficulty:** S

#### P3-B: RecordInfo Reinterpretation

**Files:** `hybrid_log/record_ops.rs:110,281`
**Risk:** Casts raw byte pointer to `*const AtomicRecordInfo`. Sound because RecordInfo is repr(transparent) over AtomicU64, but if that changes, reads become UB.
**Failure mode:** Torn reads on record headers → corrupted record traversal.

**Proposed wrapper:** `RecordAccessor::record_info()` could return a copy via `AtomicU64::load` from a known offset rather than casting the pointer. ~20 LOC change.
**Estimated LOC:** ~20
**Difficulty:** S

### P4: `unsafe impl Send/Sync` — One-Time Verification

All 8 types with manual Send/Sync impls are **correctly justified** based on the current field types:
- `MallocFixedPageSize<T>`: Internal `AtomicPtr` + `AtomicU64`, all operations via CAS. ✅
- `PageFrame`: Owns `NonNull<u8>`, no interior mutability. ✅
- `PageTable`: Slots are `AtomicPtr<PageFrame>`, states are `AtomicU32`. ✅
- `DrainList`: Lock-free Treiber stack, all access via atomics. ✅
- `FlushCallbackContext`: Single-use, consumed by callback. ✅
- `IoRequest`: Sent via channel, consumed once. ✅
- `FasterKv<F>`: All fields are Send+Sync, sessions require `&mut`. ✅
- `AlignedBuffer`: Exclusive allocation. ✅

**Recommendation:** No refactoring needed. Add a `// VERIFY:` comment block on each impl citing the current field list, so future field additions trigger manual review.
**Estimated LOC:** ~40 (comments only)
**Difficulty:** S

### P4-B: Prefetch Intrinsics

**Files:** `hash/prefetch.rs:32,40,73,81`
**Risk:** None. Prefetch hints are non-faulting by CPU specification. The unsafe is solely because `core::arch` intrinsics require it.
**Recommendation:** Accept as-is. Already well-documented with SAFETY comments.

## Priority Matrix

| ID | Category | Risk | Difficulty | LOC | Recommendation |
|----|----------|------|------------|-----|----------------|
| P0 | Page frame race | ~~Critical~~ | ~~L~~ | ~~800~~ | ✅ DONE (PinnedPage) |
| P1-A | MutableRecordAccessor lifetime | High | M | ~350 | **Tackle next** — highest remaining risk |
| P1-B | Allocator get/get_mut bounds | High | S | ~120 | Second priority — easy win |
| P1-C | Treiber stack ABA | Medium | S | ~100 | Loom test only — verify, don't refactor |
| P2-A | IoContext double-free | Medium | S | ~60 | Quick hardening — 1 hour |
| P2-B | Raw PageTable in callbacks | Medium | M | ~100 | Good hygiene — eliminates Drop-order dependency |
| P2-C | `as_mut_ptr_at` unbounded ptr | Medium | S | ~50 | Natural extension of PinnedPage |
| P3-A | HashBucket serialization | Low | S | ~30 | Bytemuck or manual AsBytes trait |
| P3-B | RecordInfo reinterpretation | Low | S | ~20 | Offset-based load instead of pointer cast |
| P4 | Send/Sync verification | Low | S | ~40 | Comments only |

**Total estimated LOC for all remaining refactors: ~870**

## Recommendation: What to Tackle Next

**P1-A: `MutableRecordAccessor` lifetime safety** is the clear next target.

**Why:**
1. It's the same class of bug as the page-frame SIGSEGV we just fixed — raw pointer with no compiler-enforced lifetime.
2. It's on the **hot write path** (`operations.rs`), meaning every upsert/RMW/delete touches it.
3. The invariant it relies on ("mutable region is above head_address, cannot be evicted") is correct today but is an **implicit contract** — nothing in the type system prevents a future change from breaking it.
4. The PinnedPage pattern we just built provides the template: a scoped guard that ties pointer validity to a compiler-checked lifetime.

**Sketch:** A `MutableRegionToken` is created when a session enters an epoch (in `begin_unsafe` / `UnsafeContext::new`). It borrows from the session guard, proving the mutable region is stable. `MutableRecordAccessor` becomes `MutableRecordAccessor<'a>` borrowing from the token. When the epoch guard drops, the token is invalidated, and any stale accessor is a compile error.

**After P1-A**, tackle P2-A (IoContext double-free protection) and P1-B (allocator bounds checks) as quick wins — both are under 120 LOC and can be done in a single session.
