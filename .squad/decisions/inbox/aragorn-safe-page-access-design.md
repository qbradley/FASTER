# Design Deep Dive: Minimizing Unsafe in the Page Frame Access Path

**Author:** Aragorn (Rust Expert)  
**Date:** 2026-07-24  
**Requested by:** qbradley  
**Branch:** `rust`  
**Triggered by:** Sam's compaction SIGSEGV fix + MIRI audit findings

## Executive Summary

The page frame access path contains **~35 unsafe blocks** across 5 files, with the critical
vulnerability surface concentrated in one function: `get_physical_address()`. Sam's fix added
a head-address bounds check that **mitigates** the ABA bug but does **not** eliminate the
TOCTOU window. After evaluating four design options, I recommend **Option C (Scoped Page Pin)**
as the correct long-term solution, with **Option D (Safe Wrapper with Validation)** as a
low-cost interim hardening step we can ship this week.

**Key finding:** Epoch protection does NOT prevent page eviction. This is the fundamental
design gap — the code comments claim epoch guards prevent eviction, but they don't. Epochs
coordinate deferred callback drains. Page frames are freed **immediately** via
`Box::from_raw()` during eviction, with no epoch deferral.

---

## 1. Inventory of Unsafe Call Sites in the Page Access Path

### Critical path: logical address → raw pointer → data access

```
User operation (read/upsert/rmw)
  └─→ operations.rs: get_physical_address(addr)          [1 unsafe]
        └─→ log_allocator.rs:191: head_address check     [0 unsafe — Sam's fix]
        └─→ log_allocator.rs:202: frame.as_mut_ptr().add(offset)  [1 unsafe]
              └─→ page.rs:424: self.frame_ref(ptr)        [1 unsafe — ptr→&PageFrame]
                    └─→ page.rs:396: &*ptr                [1 unsafe — raw deref]
  └─→ record_ops.rs:172: RecordAccessor::new(ptr, size)   [1 unsafe — ptr→accessor]
        └─→ record_ops.rs:105: &*(ptr as *const AtomicRecordInfo) [1 unsafe]
        └─→ record_ops.rs:143: from_raw_parts(ptr, size)  [1 unsafe — ptr→slice]
```

### Full inventory by file

| File | Unsafe blocks | Hot path? | Description |
|------|:---:|:---:|-------------|
| **page.rs** | 21 | 4 in hot path | Frame alloc/dealloc, ptr→ref, slice construction, Send/Sync impls |
| **log_allocator.rs** | 3 | 1 in hot path | `ptr.add(offset)` in `get_physical_address()` |
| **record_ops.rs** | 11 | 3 in hot path | RecordAccessor/MutableRecordAccessor construction, atomic casts, slices |
| **scan.rs** | 1 | 1 | `from_raw_parts` in `try_read_record()` |
| **compaction/scanner.rs** | 0 | — | `#[deny(unsafe_code)]` — delegates to RecordAccessor |

**Total:** ~36 unsafe blocks, **9 on the hot read/write path**, 5 in the critical
address→pointer→data chain.

### All production call sites of `get_physical_address()`

| File | Function | Epoch protected? |
|------|----------|:---:|
| `operations.rs` | `prefetch_record()` | ✅ |
| `operations.rs` | `internal_upsert()` — revivify sealed | ✅ |
| `operations.rs` | `internal_upsert()` — in-place update | ✅ |
| `operations.rs` | `rmw_copy_to_tail()` — lookup | ✅ |
| `operations.rs` | `rmw_copy_to_tail()` — modify | ✅ |
| `operations.rs` | `rmw_create_at_tail()` — lookup | ✅ |
| `record_ops.rs` | `RecordAccessor::from_log()` | ❌ caller's responsibility |
| `record_ops.rs` | `LogRecordReader::get_record()` | ❌ caller's responsibility |
| `record_ops.rs` | `LogRecordWriter::allocate_space()` | ✅ (mutable region) |
| `scan.rs` | `try_read_record()` | ❌ caller's responsibility |

**6 sites in operations.rs** are always epoch-protected (SessionGuard RAII).
**4 sites** depend on the caller to hold epoch protection — this is where bugs happen.

---

## 2. Sam's Fix Analysis

### What Sam changed

**log_allocator.rs:188-203** — Added head-address bounds check:
```rust
pub fn get_physical_address(&self, addr: LogicalAddress) -> Option<*mut u8> {
    // Sam's fix: reject addresses below head (evicted pages)
    let head = self.head_address.load(Ordering::Acquire);
    if addr.raw() < head.raw() {
        return None;  // Page evicted, frame slot may be recycled
    }
    let frame = self.page_table.get_frame(addr.page())?;
    let offset = addr.offset().0 as usize;
    Some(unsafe { frame.as_mut_ptr().add(offset) })
}
```

**kv.rs** — Clamped compaction scan range to `[max(first_data_address, head_address), safe_read_only)`:
- Scanner only touches in-memory pages
- Bounded scan to MAX_COMPACT_PAGES=4 per cycle

### Does Sam's fix fully close the race window?

**No.** There is a TOCTOU gap:

```
Thread A (scanner)                    Thread B (maintenance/eviction)
─────────────────────                ──────────────────────────────
1. load head_address → H
2. check: addr >= H → OK
                                     3. evict page → advance head past addr
                                     4. Box::from_raw(frame_ptr) → FREE
5. get_frame(page) → &PageFrame
6. frame.as_mut_ptr().add(offset)
7. DEREF → USE-AFTER-FREE 💥
```

Between steps 2 and 5, another thread can evict the page and free the frame. The Acquire
ordering on head_address load (step 1) ensures we see the *latest* head at that instant,
but it doesn't prevent head from advancing between the check and the frame access.

**In practice**, this window is extremely narrow (~10-50ns) and the scanner operates on
pages well behind head, so the risk is low. But it is **not sound** — the unsafe code has
an invariant ("pointer is valid") that can be violated by concurrent eviction.

**Sam's fix is a valuable defense-in-depth measure** that eliminates the most common crash
scenario (scanning already-evicted pages). It reduces the bug from "always crashes under
load" to "theoretical race window." But it doesn't achieve soundness.

---

## 3. Evaluation of Design Options

### Option A: Epoch-Guarded Page Handle (Borrow-Like Semantics)

```rust
pub struct PageHandle<'epoch> {
    ptr: *const u8,
    len: usize,
    _epoch: PhantomData<&'epoch EpochGuard>,
}
```

**Correctness: ❌ UNSOUND**

The fundamental problem: **epochs do not prevent page eviction.** This is the most
important finding of this analysis.

Evidence from the codebase:
- `eviction.rs:89-120`: `evict_pages()` checks page state and address boundaries.
  It does **not** check epoch status.
- `page.rs:525`: `Box::from_raw(ptr)` frees frame memory **immediately** during
  eviction — no epoch callback deferral.
- `epoch/table.rs:367-389`: `compute_safe_epoch()` determines when **drain callbacks**
  can fire. It has no connection to page eviction decisions.

The epoch system coordinates callback scheduling (e.g., hash index cleanup after eviction).
It does **not** hold back the eviction itself. Tying a lifetime to `EpochGuard` gives a
false sense of safety — the compiler enforces "handle doesn't outlive guard," but the guard
doesn't prevent the underlying memory from being freed.

**To make Option A sound**, we would need to change eviction to defer frame deallocation
until safe_epoch advances past the eviction epoch. This is possible but transforms the
epoch system from "callback coordinator" to "memory protector" — a significant semantic
change that could delay eviction and increase memory pressure.

| Criterion | Assessment |
|-----------|------------|
| Prevents ABA? | ❌ No — epoch doesn't block eviction |
| Performance | Zero-cost (if it worked) |
| Blast radius | ~15 call sites + epoch system changes |
| Maintenance burden | **High** — false safety guarantee unless epoch semantics are changed |

**Verdict:** Do not implement unless we also change eviction to epoch-defer frame deallocation.

### Option B: Arc<PageFrame> with Generation Counter

```rust
pub struct PageFrameRef {
    frame: Arc<PageFrame>,
    generation: u64,
}
```

**Correctness: ✅ Sound (with generation check)**

Arc refcounting ensures the frame memory is not freed while any reference exists.
Generation counter detects ABA (frame recycled for different page).

**Performance: ❌ UNACCEPTABLE**

| Operation | Cost | Frequency | Impact |
|-----------|------|-----------|--------|
| Arc clone (atomic inc) | ~15-20ns | Every `get_physical_address()` call | Hot path |
| Arc drop (atomic dec) | ~15-20ns | Every scope exit | Hot path |
| Generation check | ~1ns (branch) | Every access | Negligible |
| **Total overhead** | **~30-40ns per access** | **40M+ ops/s** | **~25-35% throughput loss** |

At 40M ops/s with multiple `get_physical_address()` calls per operation (prefetch + access +
chain walk), we're looking at 120-200M atomic increments per second. On a 16-thread workload,
this creates massive cache-line contention on the Arc refcount.

Additionally, eviction must wait for refcount → 0, which creates a **deadlock risk**: if the
scanner holds an Arc reference while eviction needs to advance head to free buffer space for
the scanner's output, we have a circular dependency.

| Criterion | Assessment |
|-----------|------------|
| Prevents ABA? | ✅ Yes — generation mismatch returns None |
| Performance | ❌ ~30% throughput loss (atomic refcount contention) |
| Blast radius | ~20 files (PageTable, LogAllocator, all callers) |
| Maintenance burden | Medium — Arc semantics are well-understood |

**Verdict:** Reject. Performance cost is unacceptable for a system targeting 40M+ ops/s.

### Option C: Scoped Page Pin (RwLock-like)

```rust
pub struct PinnedPage<'a> {
    frame: &'a PageFrame,
    _pin: PagePinGuard,  // Prevents eviction of this specific page
}

impl PageTable {
    pub fn pin_page(&self, page: Page) -> Option<PinnedPage<'_>> { ... }
}
```

**Correctness: ✅ SOUND**

Pin count on the page frame slot prevents eviction while any pin is held. Eviction checks
pin count before freeing — if pinned, skip this page and try the next one.

**Performance: ✅ ACCEPTABLE**

| Operation | Cost | Frequency | Impact |
|-----------|------|-----------|--------|
| Pin (atomic inc on per-slot counter) | ~5-8ns | Once per page access | Amortizable |
| Unpin (atomic dec) | ~5-8ns | Once per scope exit | Amortizable |
| Eviction pin check | ~1ns (atomic load) | Once per eviction candidate | Negligible |
| **Total overhead** | **~10-16ns per pinned access** | **Per page, not per record** | **<3% throughput** |

Key insight: pin/unpin is **per page**, not per record. A compaction scan of 4 pages does
4 pin/unpin pairs, not 4 million. The hot path in operations.rs accesses 1-3 pages per
operation, so the overhead is 10-48ns per operation — well under the 5% budget.

**Implementation sketch:**

```rust
// Add to PageFrame or PageTable slot:
pin_count: AtomicU32,  // 4 bytes per slot

// PageTable::pin_page():
pub fn pin_page(&self, page: Page) -> Option<PinnedPage<'_>> {
    let idx = self.frame_index(page);
    let ptr = self.frames[idx].load(Ordering::Acquire);
    if ptr.is_null() { return None; }
    
    // Increment pin count
    let frame = unsafe { &*ptr };
    let prev = frame.pin_count.fetch_add(1, Ordering::AcqRel);
    
    // Double-check frame wasn't evicted between load and pin
    let ptr2 = self.frames[idx].load(Ordering::Acquire);
    if ptr2 != ptr || frame.state() == PageState::Evicted {
        frame.pin_count.fetch_sub(1, Ordering::Release);
        return None;
    }
    
    Some(PinnedPage { frame, _pin: PagePinGuard { frame, idx } })
}

// try_evict_frame() modification:
pub fn try_evict_frame(&self, page: Page) -> Option<()> {
    let frame = ...;
    if frame.pin_count.load(Ordering::Acquire) > 0 {
        return None;  // Pinned — skip eviction
    }
    // ... existing CAS eviction logic
}
```

**Blast radius:**

| Component | Changes needed |
|-----------|---------------|
| `page.rs` | Add `pin_count` to PageFrame, `PinnedPage` struct, `pin_page()` method |
| `log_allocator.rs` | `get_physical_address()` returns `PinnedPage` instead of `*mut u8` |
| `record_ops.rs` | `from_log()` accepts `PinnedPage` reference |
| `operations.rs` | 6 call sites — pin before access, unpin on scope exit |
| `scan.rs` | 1 call site |
| `eviction.rs` | Check pin count before evicting |

~8 files, ~15 call sites. The changes are mechanical — replace `*mut u8` with
`PinnedPage` at each call site.

| Criterion | Assessment |
|-----------|------------|
| Prevents ABA? | ✅ Yes — pinned page cannot be evicted or recycled |
| Performance | ✅ <3% overhead (amortized over records per page) |
| Blast radius | ~8 files, ~15 call sites |
| Maintenance burden | **Low** — compiler enforces pin scope via RAII drop |

**Verdict: RECOMMENDED as the long-term solution.**

### Option D: Safe Wrapper with Validation (Minimal Change)

```rust
pub fn read_record_safe(
    allocator: &HybridLogAllocator,
    addr: LogicalAddress,
    record_size: u32,
) -> Option<RecordInfo> {
    if !allocator.is_in_memory(addr) { return None; }
    let ptr = allocator.get_physical_address(addr)?;
    // Read into stack-local copy
    let info = unsafe { core::ptr::read(ptr as *const RecordInfo) };
    // Re-validate after read
    if !allocator.is_in_memory(addr) { return None; }
    Some(info)
}
```

**Correctness: ⚠️ DEFENSE-IN-DEPTH, NOT SOUND**

The double-check (read-validate-re-check) narrows the TOCTOU window but doesn't close it.
Between `ptr::read` and the re-check, the data is already on the stack — we can't un-read
it. If the page was freed during the read, we consumed garbage bytes (though they were
valid memory at the instant of the read, since `free()` doesn't zero memory immediately).

This is **strictly better than the current code** but **not formally sound**. It reduces
the race window from "entire operation duration" to "~5ns during memcpy."

**Performance: ✅ ACCEPTABLE**

| Operation | Cost | Notes |
|-----------|------|-------|
| Double bounds check | ~10ns (2 atomic loads) | Same cache line |
| Stack copy of record header | ~5ns (24 bytes) | L1 cache |
| **Total** | **~15ns** | <2% at 40M ops/s |

The copy cost is higher for full records (variable length), but the scanner already copies
key/value data into `ScanRecord` structs, so this aligns with existing patterns.

**Blast radius: MINIMAL**

| Component | Changes needed |
|-----------|---------------|
| `log_allocator.rs` | Add `read_record_info_safe()` helper |
| `record_ops.rs` | Add `from_log_validated()` alternative to `from_log()` |
| `scan.rs` | Switch to validated path |
| `compaction/scanner.rs` | Switch to validated path (0 unsafe — already `#[deny(unsafe_code)]`) |

~4 files. Existing call sites in operations.rs can remain unchanged (they're epoch-protected
and access mutable-region pages that can't be evicted).

| Criterion | Assessment |
|-----------|------------|
| Prevents ABA? | ⚠️ Narrows window but doesn't eliminate it |
| Performance | ✅ <2% overhead |
| Blast radius | ~4 files, ~6 call sites |
| Maintenance burden | **Very low** — new function alongside existing one |

**Verdict: RECOMMENDED as an immediate hardening step.**

---

## 4. Recommendation

### Dual-track approach: D now, C next

**Phase 1 (this week): Option D — Safe Wrapper with Validation**

Ship `read_record_info_validated()` and `from_log_validated()` as alternatives that add
double-check + copy semantics. Apply to:
- Compaction scanner (the code that crashed)
- Log scan iterator
- Any `from_log()` caller that doesn't hold epoch protection

This is a ~200-line change with minimal risk. It doesn't achieve formal soundness but
eliminates the practical crash scenario with near-zero performance cost.

**Phase 2 (next sprint): Option C — Scoped Page Pin**

Implement `PinnedPage` with per-slot pin counts. This achieves formal soundness:
- Pinned pages cannot be evicted
- RAII drop ensures pins are released
- Compiler enforces that page access cannot outlive the pin
- Eviction gracefully skips pinned pages

Estimated effort: ~800 lines across 8 files. Should include:
1. `PinnedPage<'a>` struct with RAII unpin
2. `PageTable::pin_page()` method
3. Pin-count check in `try_evict_frame()`
4. Migration of all `get_physical_address()` callers to `pin_page()` + offset
5. Deprecation of raw `get_physical_address()` (or make it `pub(crate)` only)

**Phase 3 (future): Epoch-deferred frame deallocation (Option A enhancement)**

If we want belt-and-suspenders safety, we can additionally defer frame deallocation to
the epoch drain system. This would make even the unpinned code paths safe, at the cost of
increased memory pressure (frames stay alive until all epochs advance past eviction).

This is not urgent if Phase 2 is done correctly, but it would close the "forgot to pin"
bug class entirely.

### Why not Option A alone?

Option A is tempting because it's zero-cost, but it requires changing the epoch system's
semantics from "callback coordinator" to "memory protector." This is a deep architectural
change that affects every epoch-using code path. If we get it wrong, we introduce new
deadlock risks (eviction stalled by slow epoch advancement) or memory leaks (frames never
freed because a thread holds an old epoch).

Option C is better because it's **local** — pin counts are per-page-slot, not global. A slow
scanner pins 4 pages; eviction skips those 4 and evicts the other 12. No global coordination
needed.

### Why not Option B?

Arc refcount contention at 40M+ ops/s on 16 threads is a non-starter. Each
`get_physical_address()` call would add ~30ns of atomic contention. With 3+ calls per
operation (prefetch + access + chain walk), that's ~100ns/op overhead on a system where
the entire operation budget is ~25ns. This would cut throughput by 30-50%.

---

## 5. Incremental Migration Path

```
Week 1: Option D (defense-in-depth)
  ├─ Add read_record_info_validated() to log_allocator.rs
  ├─ Add from_log_validated() to record_ops.rs
  ├─ Migrate scanner + scan iterator to validated paths
  ├─ Add #[cfg(debug_assertions)] double-check to get_physical_address()
  └─ All 1726 tests pass, benchmark within 2% of baseline

Week 3-4: Option C (formal soundness)
  ├─ Add pin_count: AtomicU32 to PageFrame
  ├─ Implement PinnedPage<'a> + PagePinGuard
  ├─ Implement PageTable::pin_page()
  ├─ Add pin-count check to try_evict_frame()
  ├─ Migrate get_physical_address() callers to pin_page()
  ├─ Deprecate raw get_physical_address() (or restrict to pub(crate))
  └─ New property: "pinned page cannot be evicted" tested with dedicated concurrent test

Future: Option A enhancement (optional)
  ├─ Defer Box::from_raw() in try_evict_frame() to epoch drain callback
  ├─ Changes try_evict_frame() to null the slot but NOT free the memory
  ├─ Epoch drain fires when safe_epoch advances past eviction epoch
  └─ Belt-and-suspenders: even code that forgets to pin is safe
```

### Compatibility notes

- Option D is **additive** — new functions alongside existing ones. No breaking changes.
- Option C changes the return type of the page access path. All callers must migrate.
  However, the migration is mechanical (replace `*mut u8` with `PinnedPage` at each site).
- Both options preserve the `#[deny(unsafe_code)]` invariant on compaction modules.
- Neither option changes the public API (`FasterKv`, `FasterSession`, `UnsafeContext`).

---

## Appendix A: The Epoch Gap — Why Comments Are Wrong

Multiple code comments claim epoch protection prevents page eviction:

```rust
// compaction/scanner.rs:23-26:
/// The caller **must** hold epoch protection for the duration of the scan.
/// This ensures that pages are not evicted while the scanner reads them.

// compaction/copier.rs:6-12:
/// Like the scanner, the caller **must** hold epoch protection for the
/// duration of the copy. Source records reside in in-memory pages that
/// could be evicted without epoch guards.
```

**These comments are incorrect.** Epoch protection prevents drain callbacks from firing,
not page eviction. The eviction path (`eviction.rs:89-120` → `page.rs:501-532`) checks
page state and address boundaries, not epoch status. Frame memory is freed via
`Box::from_raw()` immediately on successful eviction CAS — no epoch deferral.

**Action item:** These comments should be corrected regardless of which option we implement.
The scanner's safety relies on Sam's head-address bounds check, not on epoch protection.

## Appendix B: Pin Count Design Detail

The pin-count approach has a subtle race that must be handled:

```
Thread A (pin_page)              Thread B (eviction)
────────────────────             ─────────────────────
1. load frames[idx] → ptr
                                 2. CAS frames[idx] → null
                                 3. Box::from_raw(ptr) → FREE
4. pin_count.fetch_add(1)        
   → USE-AFTER-FREE on pin_count 💥
```

**Solution:** The pin must be validated with a double-check:

```rust
pub fn pin_page(&self, page: Page) -> Option<PinnedPage<'_>> {
    let idx = self.frame_index(page);
    let ptr = self.frames[idx].load(Ordering::Acquire);
    if ptr.is_null() { return None; }
    
    let frame = unsafe { &*ptr };
    frame.pin_count.fetch_add(1, Ordering::AcqRel);
    
    // Re-check: was the frame evicted between our load and pin?
    let ptr2 = self.frames[idx].load(Ordering::Acquire);
    if ptr2 != ptr {
        // Frame was swapped out. But we incremented pin_count on freed memory!
        // This is the crux: we need to ensure the frame memory is still alive.
        //
        // Solution: eviction must check pin_count BEFORE CAS-nulling the slot.
        // If pin_count > 0, eviction backs off. This creates the invariant:
        //   "If pin_count > 0, frames[idx] still points to this frame."
        // So if our re-check sees a different pointer, we know our pin was
        // on a frame that had pin_count == 0 at eviction time — meaning
        // we incremented it AFTER eviction checked, which is the race.
        //
        // Fix: eviction uses CAS on pin_count: 0 → EVICTING sentinel.
        // Pin uses CAS on pin_count: N → N+1 (fails if EVICTING).
        frame.pin_count.fetch_sub(1, Ordering::Release);
        return None;
    }
    
    Some(PinnedPage { frame, _pin: PagePinGuard { ... } })
}
```

**Better approach:** Use a combined state+pin atomic:
```rust
// Pack state (3 bits) + pin_count (29 bits) into a single AtomicU32
// Eviction CAS: (Flushed, 0) → (Evicted, 0)  — only succeeds if pin_count == 0
// Pin CAS: (state, N) → (state, N+1)          — only succeeds if state != Evicted
```

This eliminates the TOCTOU between pin increment and state check by making them a single
atomic operation. The 29-bit pin count supports up to 536 million concurrent pins — far
more than any realistic thread count.

This is the implementation detail that will need careful design during Phase 2.
