# DST v2.0 Internals Inventory — Simulation Requirements

**Author:** Sam (Systems & Storage Expert)  
**Requested by:** qbradley  
**Purpose:** Comprehensive inventory of FasterKv internal state machines, I/O paths, lock patterns, and resource growth for deterministic simulation testing framework design.

---

## Table of Contents

1. [Page State Machine](#1-page-state-machine)
2. [Flush Pipeline](#2-flush-pipeline)
3. [Eviction Subsystem](#3-eviction-subsystem)
4. [Allocator (HybridLogAllocator)](#4-allocator-hybridlogallocator)
5. [Allocator (MallocFixedPageSize)](#5-allocator-mallocfixedpagesize)
6. [Device Layer](#6-device-layer)
7. [Epoch System](#7-epoch-system)
8. [Checkpoint / Recovery](#8-checkpoint--recovery)
9. [Hash Index & Overflow](#9-hash-index--overflow)
10. [Pending I/O Tracking](#10-pending-io-tracking)
11. [Operation Retry Loops](#11-operation-retry-loops)
12. [Known Deadlock Traces](#12-known-deadlock-traces)
13. [Minimum Simulator State Machine](#13-minimum-simulator-state-machine)

---

## 1. Page State Machine

**Source:** `hybrid_log/page.rs:43-126`, `hybrid_log/eviction.rs`, `hybrid_log/flush.rs`

### A. States & Transitions

```
                          ┌──────────────────────────────────┐
                          │                                  │
    ┌──────┐   alloc    ┌──────┐  page-full   ┌────────┐   │
    │ Free │ ─────────→ │ Open │ ──────────→  │ Sealed │   │
    └──────┘            └──────┘              └────────┘   │
       ↑                   │                   │    ↑      │
       │                   │ force-seal        │    │      │
       │                   │ (eviction race)   │    │      │
       │                   └───────────────────┘    │      │
       │                                            │      │
       │                                 flush_page │      │ revert on
       │                                            │      │ QueueFull/Error
       │                                            ↓      │
       │              ┌─────────┐  callback   ┌──────────┐ │
       │              │ Flushed │ ←────────── │ Flushing │─┘
       │              └─────────┘             └──────────┘
       │                   │
       │     evict_frame   │
       │                   ↓
       │              ┌─────────┐
       └───────────── │ Evicted │
         recycle      └─────────┘
```

| Transition | Trigger | Atomic? | Ordering | Source |
|---|---|---|---|---|
| Free → Open | `get_or_allocate_frame()` CAS | Yes (CAS) | AcqRel/Acquire | page.rs:447-457 |
| Open → Sealed | `try_allocate()` fills page exactly | Yes (CAS) | AcqRel/Acquire | log_allocator.rs:157-161 |
| Open → Sealed | `advance_head()` force-seal race recovery | Yes (CAS) | AcqRel/Acquire | eviction.rs:191 |
| Sealed → Flushing | `flush_page()` async initiation | Yes (CAS) | AcqRel/Acquire | flush.rs:238-240 |
| Sealed → Flushing | `flush_page_sync()` sync initiation | Yes (CAS) | AcqRel/Acquire | flush.rs:330-332 |
| Flushing → Flushed | I/O completion callback (success) | Yes (CAS) | AcqRel/Acquire | flush.rs:185-187 |
| Flushing → Flushed | `flush_page_sync()` after sync write | Yes (CAS) | AcqRel/Acquire | flush.rs:358-359 |
| Flushing → Sealed | QueueFull (revert for retry) | Yes (CAS) | AcqRel/Acquire | flush.rs:292-294 |
| Flushing → Sealed | I/O error (revert for retry) | Yes (CAS) | AcqRel/Acquire | flush.rs:301-303 |
| Flushed → Evicted | `try_evict_frame()` | Yes (CAS) | AcqRel/Acquire | page.rs:501-533 |
| Evicted → Free | Implicit on recycling (`get_or_allocate_frame`) | Yes (CAS) | AcqRel/Acquire | page.rs:447 |

**All transitions are atomic CAS via `AtomicPageState::try_transition()`** (page.rs:116-125). No locks held for state transitions. Failed CAS means someone else won the race — caller retries or bails.

### B. Concurrency Patterns

- **AtomicPageState** is a `AtomicU8` (page.rs:88-90) — single-byte CAS, lock-free.
- **`try_transition()` uses `compare_exchange` with AcqRel/Acquire** (page.rs:120-123).
- **flushed_until** is `AtomicU32` updated with `fetch_max(bytes, Release)` (flush.rs:183-184).
- **Contention hot spots:** Open→Sealed transition when multiple threads fill the same page simultaneously (only one CAS wins; loser retries allocate_at_tail).
- **No circular dependency** in page state alone — dependencies arise from interactions with allocator + device.

### C. I/O Paths

- **Flushing:** `Device::write_async(page_data, offset, len, callback, context)` → callback fires `Flushing→Flushed` (flush.rs:264-283).
- **Short-write detection:** Callback checks `bytes_transferred < ctx.bytes_flushed` and leaves in Flushing state for retry (flush.rs:171-181).
- **Error on callback:** Page stays in Flushing state (flush.rs:190) — recovery logic must handle.

### D. Resource Growth

- **PageFrame** data: Fixed allocation per page (32 MB default, `1 << OFFSET_BITS`). Bounded by `buffer_size`.
- **PageTable frames array:** `Box<[AtomicPtr<PageFrame>]>` of fixed `buffer_size` — no growth after init.
- **No unbounded growth** in page state machine itself.

---

## 2. Flush Pipeline

**Source:** `hybrid_log/flush.rs`

### A. State Machine

The flusher is **stateless** (flush.rs:199-204) — only holds `sector_size: u32` and `page_size: u32`. All state lives in PageTable/AtomicPageState.

**flush_sealed_pages()** algorithm (flush.rs:381-419):
1. Scan range: `[head_page, read_only_page)` — forward iteration
2. For each page: load state with `Acquire` (flush.rs:396)
3. If `Sealed`: call `flush_page()` to submit I/O
4. On `QueueFull`: **break immediately**, return `FlushBatchResult { flushed, queue_full: true }` (flush.rs:401-407)
5. On success: increment `flushed_count`

**FlushBatchResult** (flush.rs:53-66):
```rust
pub struct FlushBatchResult {
    pub flushed: u32,      // pages successfully submitted
    pub queue_full: bool,  // device rejected further I/O
}
```

### B. Concurrency Patterns

- **No locks** — all coordination through atomic PageState CAS.
- **Callback context:** Heap-allocated `TypedIoContext<FlushCallbackContext>` passed as raw `*mut u8` to device (flush.rs:257-261). Reclaimed in callback via `from_raw()` (flush.rs:160).
- **Unsafe contract:** PageTable pointer stored in context must outlive the I/O operation (guaranteed by SF-10: buffer can't be freed while I/O outstanding).

### C. I/O Paths

| Path | Operation | Device Method | Completion |
|---|---|---|---|
| Async flush | Write page to device | `write_async()` | Callback: `flush_completion_callback` |
| Sync flush | Write page to device | `write_sync()` | Inline transition |
| Completion | Transition Flushing→Flushed | — | `try_transition()` CAS |

**Back-pressure chain:**
```
Device::write_async() returns QueueFull
  → flush_page() reverts Flushing→Sealed, returns FlushError::QueueFull
    → flush_sealed_pages() breaks loop, returns FlushBatchResult{queue_full: true}
      → maintenance() gets signal, polls completions + yields
        → allocate retry loop calls maintenance + yield
```

### D. Resource Growth

- **Callback contexts:** One `Box<FlushCallbackContext>` per in-flight flush (~64 bytes each). Bounded by `max_outstanding_io()`. Reclaimed on callback.
- **No unbounded growth** — bounded by device queue depth.

---

## 3. Eviction Subsystem

**Source:** `store/kv.rs:1676-1769`, `hybrid_log/eviction.rs`

### A. State Machine

**EvictionPolicy** (eviction.rs:36-54):
```rust
pub struct EvictionPolicy {
    pub max_in_memory_pages: u32,    // Default: 256
    pub eviction_batch_size: u32,    // Default: 16
}
```

**Eviction triggers** (in maintenance() order):

| Condition | Where | What Happens |
|---|---|---|
| Buffer pressure: `in_memory + 2 >= buffer_size` | kv.rs:1698-1720 | Pre-flush eviction (critical path) |
| Eager flush: `in_memory * 4 >= buffer_size * 3` (75%) | kv.rs:1689 | Shift read-only + evict |
| Policy threshold: `in_memory > max_in_memory_pages` | kv.rs:1734 | Main eviction block |

**Lossy vs Non-Lossy paths:**

| Operation | Lossy | Non-Lossy |
|---|---|---|
| `evict_pages()` | ✓ | ✓ |
| `try_advance_begin(head)` | ✓ | ✗ |
| `hash_index.invalidate_entries_in_range()` | ✓ | ✗ |
| `device.truncate_until()` | ✓ (post-flush only) | ✗ |

### B. Concurrency Patterns

- **`evict_pages()`** — lock-free: scans pages head→read_only, CAS each Flushed→Evicted (eviction.rs:89-117).
- **`advance_head()`** — CAS loop advancing `head_address` atomically (eviction.rs:144-199).
- **Hash invalidation** — O(n) scan of all hash buckets + overflow chains, CAS each entry to EMPTY (hash/index.rs:427-480). **This is a contention point** — runs while writers continue inserting.
- **Critical lock insight (Bug 2 fix):** Pre-flush eviction block **never** calls `truncate_until()` to avoid device write lock. Truncation deferred to main eviction block, after flush completes.

### C. I/O Paths

- **`device.truncate_until(offset)`** — called only in lossy mode, only in main eviction block (kv.rs:1755).
- **InMemoryDevice:** truncate_until is a **no-op** (device.rs:557-565). The previous O(n) zeroing implementation caused Bug 2.
- **SyncFileDevice:** Would perform actual file truncation.

### D. Resource Growth

- **Hash invalidation scan** is O(total_buckets + overflow_chains). As hash table grows, this cost grows linearly.
- **In lossy mode:** Eviction churn creates sustained copy-to-tail pressure. Keys evicted → hash entries cleared → keys re-inserted at tail → more eviction. Effective data rate ~50 MB/s for 1M keys with 5% delete rate.

---

## 4. Allocator (HybridLogAllocator)

**Source:** `hybrid_log/log_allocator.rs`

### A. State Machine

**Address boundaries** (all `AtomicLogicalAddress`, log_allocator.rs:42-53):

```
Invariant: begin ≤ head ≤ read_only ≤ safe_read_only ≤ tail

begin_address        head_address     read_only_address   safe_read_only     tail_address
     │                    │                  │                  │                  │
     ▼                    ▼                  ▼                  ▼                  ▼
┌─────────────────┬───────────────┬──────────────────┬─────────────────┬──────────────┐
│   Truncated     │    On-disk    │    Read-only     │  Fuzzy Region   │   Mutable    │
│  (below begin)  │  (evicted)   │  (flushed/flush) │ (sealing/flush) │  (writable)  │
└─────────────────┴───────────────┴──────────────────┴─────────────────┴──────────────┘
```

**Boundary advancement (all CAS loops, no locks):**

| Boundary | Advanced By | Condition | Source |
|---|---|---|---|
| `tail_address` | `try_allocate()` CAS | Record allocation | log_allocator.rs:143-148 |
| `read_only_address` | `shift_read_only_to_tail()` | Maintenance triggers | log_allocator.rs:200-217 |
| `safe_read_only_address` | `shift_read_only_to_tail()` | Follows read_only | log_allocator.rs:220-234 |
| `head_address` | `try_advance_head()` | Eviction completes | log_allocator.rs:509-525 |
| `flushed_until_address` | `try_advance_flushed_until()` | Flush callback | log_allocator.rs:397-413 |
| `begin_address` | `try_advance_begin()` | Lossy eviction | log_allocator.rs:531-547 |

### B. Concurrency Patterns

**try_allocate()** (log_allocator.rs:105-174) — the hottest path:
1. Load `tail_address` with `Acquire` (line 115)
2. Compute new offset (line 117)
3. **SF-10 buffer-full check** (lines 132-137): `(next_page - head_page) >= buffer_size` → return None
4. CAS `tail_address` with `AcqRel/Acquire` (lines 143-148)
5. On success: allocate page frame, seal if page filled
6. On failure: **tight retry loop** (no yield, no backoff) — assumes low contention

**Atomic orderings:**
- All boundary loads: `Acquire`
- All boundary CAS: `AcqRel` success, `Acquire` failure
- `sealed` flag: `store(Release)`, `load(Acquire)` (lines 110, 288-290)

**Contention hot spots:**
- `tail_address` CAS — every allocation touches this. With N writers, N-1 retries per attempt on average.
- `head_address` CAS — rare (once per eviction batch, not per-record).

### C. I/O Paths

The allocator itself does **no I/O**. It's pure in-memory state machine. I/O happens in flush/device layers.

### D. Resource Growth

- **PageTable frames:** Fixed `buffer_size` array of `AtomicPtr<PageFrame>`. No growth.
- **Each PageFrame:** 32 MB allocation (`alloc_zeroed`). Bounded by `buffer_size`.
- **Total memory:** `buffer_size × page_size` (e.g., 64 pages × 32 MB = 2 GB). Fixed at init.

---

## 5. Allocator (MallocFixedPageSize)

**Source:** `allocator.rs:269-317`

### A. State Machine

Used for hash overflow buckets and other fixed-size allocations. **Completely different from HybridLogAllocator.**

**Fields:**
- `dir: AtomicPtr<PageDir<T>>` — page directory (atomic swap on growth)
- `count: AtomicU64` — monotonic bump counter (starts at 2)
- `free_list: Box<AtomicU64>` — Treiber stack head with ABA tag: `[tag:16 | addr:48]`
- `grow_lock: Mutex<()>` — protects directory doubling
- `retired_dirs: Mutex<Vec<>>` — old directories awaiting drop
- `epoch: Option<Arc<EpochTable>>` — optional epoch-gated deferred frees

### B. Concurrency Patterns

- **Bump allocation** (allocator.rs:670-704): `count.fetch_add(1, Relaxed)` — lock-free, no retry, O(1).
- **Free list push/pop** (allocator.rs:740-804): Lock-free Treiber stack with 16-bit ABA tag. CAS with `AcqRel/Acquire`.
- **Page installation** (allocator.rs:172-199): `get_or_add_page()` uses CAS to install page pointer. Only one thread wins; losers discard their allocation.
- **Directory expansion** (allocator.rs:708-733): **Under `grow_lock` Mutex**. Creates new PageDir with doubled capacity, atomic swap. Old dirs stored in `retired_dirs`.

**Contention:** `count.fetch_add` on bump — single shared counter, but `Relaxed` ordering is cheap. Free list CAS is only contention point for workloads with lots of churn.

### D. Resource Growth

| Resource | Growth | Rate | Bounded? |
|---|---|---|---|
| Pages (data) | On-demand, lazy | 1 page per 1M items (64 MB each) | No — **unbounded** |
| PageDir slots | Doubling (16→32→64...) | Log₂(pages) | No — up to 2²⁴ |
| Retired dirs | One per expansion | Log₂(pages) | No — freed on Drop |
| Free list chain | Per freed item | Workload-dependent | No — **unbounded** |

**⚠️ Simulator must model:** Overflow bucket allocator grows unboundedly under skewed workloads. Each overflow page = 64 MB. Under high hash collision rates, chains grow without bound.

---

## 6. Device Layer

**Source:** `device.rs`

### A. State Machine

Devices are **stateless** from the caller's perspective — no state machine. Just a synchronous API with async completion callbacks.

**Device trait** (device.rs:229-301):

| Method | Purpose |
|---|---|
| `write_async(source, offset, len, callback, context)` → `IoRequestResult` | Submit async write |
| `read_async(offset, dest, len, callback, context)` → `IoRequestResult` | Submit async read |
| `write_sync(offset, source)` → `io::Result<u32>` | Blocking write |
| `read_sync(offset, dest)` → `io::Result<u32>` | Blocking read |
| `truncate_until(offset)` | Reclaim space below offset |
| `poll_completions()` → `u32` | Drive completion callbacks |
| `max_outstanding_io()` → `u32` | Queue depth limit |

**IoRequestResult** (device.rs:51-60):
```
Submitted       — I/O queued, callback fires later
CompletedSync   — I/O done, callback already fired
QueueFull       — Device busy, caller should retry
Error(io::Error)— Submission failed
```

### B. Concurrency (InMemoryDevice)

**Fields** (device.rs:417-420):
- `data: RwLock<Vec<u8>>` — **the single contention point**
- `sector: u32`, `segment: u64` — immutable config

| Operation | Lock | Duration | Can Block? |
|---|---|---|---|
| `read_async()` | Read lock | memcpy duration | Yes (write lock held) |
| `write_async()` | Write lock | ensure_capacity + memcpy | Yes (blocks all) |
| `truncate_until()` | **No-op** | Zero | No |
| `read_sync()` | Read lock | memcpy duration | Yes |
| `write_sync()` | Write lock | ensure_capacity + memcpy | Yes |

**Critical design decision:** `truncate_until()` is explicitly a no-op (device.rs:557-565) to prevent Bug 2 deadlock. Comment explains: data below begin_address is never read, zeroing is cosmetic.

**CompletedSync pattern:** Both InMemoryDevice and NullDevice invoke callbacks **synchronously** within `write_async()`/`read_async()` and return `CompletedSync`. This means:
- The callback (Flushing→Flushed) fires **while the device write lock is held** (for InMemoryDevice).
- **Simulator must model this** — completion timing depends on device implementation.

### C. I/O Paths

**Callback type** (device.rs:35-36):
```rust
type IoCompletionCallback = unsafe fn(context: *mut u8, status: IoStatus, bytes_transferred: u32);
```

**Flow:**
1. Caller creates `TypedIoContext<T>` on heap, converts to `*mut u8` (device.rs:120-140)
2. Passes to `write_async()`/`read_async()`
3. Device invokes callback with context pointer when I/O completes
4. Callback reclaims context via `from_raw()`, processes result

### D. Resource Growth

**InMemoryDevice `data: Vec<u8>`:**
- Grows via `ensure_capacity()` → `Vec::resize()` (device.rs:444-448)
- **Grows monotonically** — never shrinks (truncate_until is no-op)
- Growth rate: proportional to `tail_address` advancement
- **⚠️ Unbounded:** In lossy mode, data below begin_address is dead but still occupies memory

**NullDevice:** `high_water: AtomicU64` — single counter, no memory growth.

---

## 7. Epoch System

**Source:** `epoch/table.rs`, `epoch/entry.rs`, `epoch/drain.rs`

### A. State Machine

**Per-thread epoch entry** (entry.rs:24-51):
- `local_current_epoch: AtomicU64` — 0 = inactive, nonzero = protected at that epoch
- `reentrant: AtomicU32` — nesting counter for protect/unprotect
- `thread_id: AtomicU64` — slot owner (0 = free)
- `phase_finished: [AtomicBool; 16]` — checkpoint phase markers

**States:**
```
Inactive (epoch=0, reentrant=0)
  → protect() → Protected (epoch=current, reentrant=1)
    → protect() again → Protected (reentrant=2, epoch unchanged)
    → unprotect() → Protected (reentrant=1)
  → unprotect() → Inactive (epoch=0, reentrant=0) → try_drain()
```

### B. Concurrency Patterns

| Operation | Atomic | Ordering | Source |
|---|---|---|---|
| `protect()` reentrant increment | `fetch_add(1, Relaxed)` | Single-writer | table.rs:205 |
| `protect()` store epoch | `store(epoch, Release)` | Publish to scanners | table.rs:211 |
| `protect()` load current epoch | `load(Relaxed)` | Stale reads safe | table.rs:209 |
| `unprotect()` reentrant decrement | `fetch_sub(1, Relaxed)` | Single-writer | table.rs:232 |
| `unprotect()` store inactive | `store(0, Release)` | Publish to scanners | table.rs:240 |
| `bump_current_epoch()` | `fetch_add(1, SeqCst)` | **Total ordering** | table.rs:282 |
| `compute_safe_epoch()` scan | `load(Acquire)` per entry | See protect() stores | table.rs:376 |
| DrainList push | CAS head `AcqRel/Acquire` | Treiber stack | drain.rs:94-109 |
| DrainList drain | `swap(null, AcqRel)` | Claim chain | drain.rs:181 |

**Table capacity:** 256 entries × 64 bytes (CachePadded) = **16 KB** (mod.rs:77).

**`compute_safe_epoch()` — O(n) scan** (table.rs:367-389):
1. Load `current_epoch` with `Acquire`
2. Load `scan_limit` with `Acquire` — optimization: only scan `[0..scan_limit]`, not all 256
3. For each entry: load `local_current_epoch` with `Acquire`, track minimum
4. Return `min - 1`

**Fast-path optimization** (table.rs:329-333): If `drain_count == 0`, skip the entire 16 KB scan. Critical for pure-read workloads (~5ns vs ~100ns).

### C. I/O Paths

No I/O. Pure in-memory coordination.

### D. Resource Growth

| Resource | Growth | Bounded? |
|---|---|---|
| Epoch table entries | Fixed 256 slots | Yes |
| DrainList nodes | Per `on_epoch` callback | Yes — bounded by active callbacks |
| `drain_count` | Tracks pending callbacks | Yes |

**DrainList:** Treiber stack of heap-allocated `DrainNode` (~40 bytes each). Nodes freed after callback fires. Growth bounded by rate of epoch bumps × active deferred actions.

**⚠️ Simulator must model:** `compute_safe_epoch()` called on **every `unprotect()`**. At high thread counts (>64), the 256-entry scan becomes a hot path. With the fast-path (drain_count == 0), read-only workloads skip the scan.

---

## 8. Checkpoint / Recovery

**Source:** `checkpoint/`, `recovery/`, `state/`

### A. State Machine (EPVS)

**Checkpoint phases:** Rest → Prepare → InProgress → WaitFlush → WaitCompletion → Completed → Rest

**SystemState** packs `(phase: u8, version: u56)` into a single `AtomicU64` (state/system_state.rs:20-205). All transitions are CAS.

**Version bumping:** Increments **only on Prepare → InProgress** (checkpoint/state_machine.rs:221-226). Two-phase intermediate CAS prevents ABA.

### B. Concurrency Patterns

- **No blocking locks during long-running checkpoint phases.**
- Token Mutex (state_machine.rs:149): ~microseconds at state transitions only.
- Manager Mutex (manager.rs:183): ~microseconds, O(1) operations.
- **NOT locked during checkpoint:** Hash index, hybrid log pages, sessions, device I/O.

### C. I/O Paths

| Phase | I/O Operation | Target |
|---|---|---|
| Index checkpoint | Write hash buckets + CRC32 | `{token}.index` file |
| Log checkpoint (fold-over) | Snapshot tail address, wait for flush | (no new I/O — waits for async flush pipeline) |
| Log checkpoint (snapshot) | Copy in-memory pages | Separate snapshot file |
| Metadata | Write JSON metadata | `{token}.meta` file |

**Recovery flow (3 stages):**
1. **Index recovery:** Read index file, validate CRC32, load buckets via `ptr::copy_nonoverlapping`
2. **Log recovery:** Validate address consistency, verify page CRC-32C checksums, compute restored boundaries
3. **Session recovery:** Restore serial numbers and pending counts

### D. Resource Growth

- **Checkpoint files:** One set per checkpoint token. Not auto-cleaned.
- **During WaitFlush:** Orchestrator polls `flushed_until_address()` in a loop (30s timeout). No resource growth.

---

## 9. Hash Index & Overflow

**Source:** `hash/index.rs`, `hash/bucket.rs`, `hash/table.rs`, `hash/overflow.rs`

### A. State Machine

**No explicit state machine.** All operations are atomic CAS on 64-bit bucket entries.

**Entry format** (bucket.rs): `[48-bit address | 14-bit tag | 1-bit tentative | 1-bit reserved]`

**Two-phase insert protocol:**
1. Phase 1: CAS entry with `tentative=1` (reserve slot)
2. Write record to log
3. Phase 2: CAS to clear tentative bit (commit)
4. On abort: CAS entry back to EMPTY

### B. Concurrency Patterns

**Completely lock-free:**
- Entry load: `Acquire`
- Insert/commit CAS: `AcqRel/Acquire`
- Invalidation CAS: `AcqRel/Relaxed`
- `live_entry_count`: `Relaxed` — advisory only

**Bucket layout:** 64 bytes = 7 entries + 1 overflow pointer (cache-line aligned).

**Overflow chaining:** When all 7 slots full, allocate overflow bucket from `OverflowBucketPool` (backed by `MallocFixedPageSize<HashBucket>`).

### C. I/O Paths

No direct I/O. Hash index is purely in-memory. Persisted during checkpoint.

### D. Resource Growth

| Resource | Growth Pattern | Bounded? |
|---|---|---|
| Primary buckets | Fixed at init (power-of-2) | Yes |
| Overflow buckets | 1 per chain collision | **No — unbounded** |
| Overflow pages | 64 MB each, on-demand | **No — unbounded** |
| Hash table (on grow) | Doubles (2x buckets) | Up to 2³⁰ |

**Online resize (GrowManager):** Cooperative incremental split. Default 16 chunks per `process_chunks()` call. Load factor threshold: 0.75.

**`invalidate_entries_in_range()`:** O(total_buckets + overflow_chains) — scans ALL buckets. Called during lossy eviction. **Becomes expensive as hash table grows.**

**⚠️ Simulator must model:**
- Overflow chain growth under skewed key distributions
- `invalidate_entries_in_range()` cost scaling with table size
- Online resize doubling + incremental split

---

## 10. Pending I/O Tracking

**Source:** `store/session.rs`, `store/pending_io.rs`

### A. State Machine

**Per-session pending ops:**
- `pending_ops: Vec<PendingOperation<F>>` — un-dispatched operations
- `io_contexts: Vec<PendingIoContext<F>>` — in-flight I/O (~160 bytes each)

**Completion state (per I/O):**
- `Arc<AtomicBool>` — completion flag (`Release` by callback, `Acquire` by poller)
- `Arc<Mutex<Option<IoStatus>>>` — status
- `Arc<AtomicU32>` — bytes transferred

### B. Concurrency Patterns

- **Polling:** `take_completed_io()` — **O(n) linear scan** of `io_contexts` (session.rs:304-317). Checks each `AtomicBool` with `Acquire`.
- **Busy-wait:** `wait_for_all_io()` — **spins with `yield_now()`** until all I/O complete (session.rs:329-350).

### D. Resource Growth

| Resource | Growth | Bounded? |
|---|---|---|
| `pending_ops` Vec | Per pending operation | Bounded by session activity |
| `io_contexts` Vec | Per in-flight I/O | Bounded by `max_outstanding_io()` |
| Sector-aligned buffers | Per disk read (8 KB min) | Bounded by outstanding I/O |

**⚠️ O(n) scan:** `take_completed_io()` is O(pending_count). Under sustained async read workloads with many sessions, this linear scan becomes a hot spot.

---

## 11. Operation Retry Loops

**Source:** `store/operations.rs:208-256`, `store/kv.rs`

### A. State Machine

**Allocation retry on buffer-full (SF-10):**

```
try_allocate() returns None (buffer full)
  → for attempt in 0..MAX_ALLOC_RETRIES (32):
      → on_alloc_failure() → maintenance()
          → shift_read_only_to_tail()
          → flush_sealed_pages()
          → poll_completions()
          → evict_pages()
      → thread::yield_now()
      → retry try_allocate()
  → if all 32 fail: return None → operation Aborted
```

**Constants:**
- `MAX_ALLOC_RETRIES = 32` (operations.rs:208) — "≈ a few milliseconds of cooperative waiting"

**On allocation failure, operations return `OperationStatus::Aborted`:**
- Upsert: Reverts tentative hash entry (operations.rs:402-409)
- RMW: Reverts tentative entry (operations.rs:750-756)
- Delete: Returns Aborted (operations.rs:1251-1253)

### B. Concurrency

- **Tight CAS loop** in `try_allocate()` — no yield, no backoff (log_allocator.rs:114-173). Assumes low contention on tail_address.
- **Bounded retry with yield** in `allocate_at_tail()` — yields to I/O threads, calls maintenance to make progress (operations.rs:241-254).

---

## 12. Known Deadlock Traces

### Bug 1: Circular Buffer Deadlock (Multi-Writer)

**Minimum trace the simulator must reproduce:**

```
T=0s:   Writers allocate rapidly, tail advances
T=10s:  tail_page reaches head_page + buffer_size
        → try_allocate() returns None (SF-10 check, log_allocator.rs:132-137)

T=10s:  Writers enter retry loop (operations.rs:241-254)
        → calls maintenance() → flush_sealed_pages()

T=10s:  flush_sealed_pages() scans [head, read_only)
        → calls device.write_async() for each Sealed page

T=10s:  Device returns QueueFull (device I/O queue saturated)
        → flush_page() reverts Flushing→Sealed (flush.rs:292-294)
        → flush_sealed_pages() breaks with queue_full=true (flush.rs:401-407)

T=10s:  maintenance() polls completions
        → But: I/O worker threads need CPU time to fire callbacks
        → All CPU time consumed by writer retry loops

T=10s:  evict_pages() scans [head, read_only)
        → All pages are Sealed (not Flushed!) — cannot evict
        → head_address stays stuck

DEADLOCK:
  Writers blocked → SF-10 (tail lapped head)
  Head can't advance → needs contiguous Flushed pages
  Flush reverts → QueueFull, pages stay Sealed
  I/O workers starved → callbacks never fire → pages never reach Flushed
```

**State transitions involved:**
1. `Open → Sealed` (page fill)
2. `Sealed → Flushing` (flush attempt)
3. `Flushing → Sealed` (QueueFull revert)  ← **critical: revert breaks forward progress**
4. `Flushed → Evicted` (never reached)

**Fix applied (three-part):**
- (A) Break on QueueFull (flush.rs:401-407) — stop scanning on first rejection
- (B) poll_completions + yield in retry loop (operations.rs:243-244) — give I/O threads CPU time
- (C) poll_completions in maintenance (kv.rs:1730) — drive callbacks after flush batch

### Bug 2: Lossy Eviction O(n) Lock Deadlock

**Minimum trace the simulator must reproduce:**

```
T=0s:   Lossy mode, sustained writes, eviction active
        → maintenance() calls evict_and_truncate()

T=0s:   evict_and_truncate() calls device.truncate_until(offset)
        → InMemoryDevice::truncate_until() acquires WRITE LOCK on data: RwLock<Vec<u8>>
        → Zeros data[0..offset] with guard[..offset].fill(0)

T=50s:  offset has grown to ~2 GB (tail advancing at ~50 MB/s in lossy mode)
        → fill(0) takes O(2 GB) = ~0.5s under write lock

T=100s: offset grows to ~5 GB
        → fill(0) takes ~1.5s under write lock
        → During this time: ALL flush_page() write_async() calls BLOCKED on read/write lock
        → No pages transition Flushing→Flushed

T=150s: offset grows to ~9 GB
        → fill(0) takes ~3s under write lock
        → Flush pipeline completely starved
        → Eviction needs Flushed pages but none available
        → Writers hit SF-10, retry loop exhausted

T=200s: truncation time exceeds all retry budgets
        → 0 ops/s permanent freeze
        → RSS: 13 GB (Vec<u8> never shrinks)

DEADLOCK:
  truncate_until() holds device WRITE LOCK for O(n) seconds
    → write_async() blocked → flush callbacks can't fire
      → Flushing pages never reach Flushed
        → evict_pages() can't advance head
          → allocator stuck at SF-10
            → writers permanently stalled
```

**State transitions involved:**
1. `Sealed → Flushing` (flush attempt)
2. `device.write_async()` **BLOCKED** on RwLock (device.rs:515) — can't submit I/O
3. Flushing pages stuck — never reach Flushed
4. `evict_pages()` finds no Flushed pages — head stuck

**Fix applied (three-part):**
- (A) `truncate_until()` → no-op for InMemoryDevice (device.rs:557-565)
- (B) Pre-flush eviction uses `evict_pages()` only — no `truncate_until()` (kv.rs:1706-1720)
- (C) Main eviction separates evict/begin/hash from truncation — truncate only after flush (kv.rs:1732-1760)

---

## 13. Minimum Simulator State Machine

Based on the two deadlocks and subsystem analysis, here is the **minimum state machine** the DST simulator must model to catch both known bugs and variants:

### Required State Variables

```
// Page-level state (per page in circular buffer)
page_state[page_id]: Free | Open | Sealed | Flushing | Flushed | Evicted

// Address boundaries (global, monotonically advancing)
begin_address, head_address, read_only_address, safe_read_only_address, tail_address, flushed_until_address

// Device state
device_queue_depth: u32          // current outstanding I/O count
device_max_queue: u32            // maximum (triggers QueueFull)
device_write_lock_held: bool     // for InMemoryDevice RwLock modeling
device_data_size: u64            // for growth tracking

// Epoch state
current_epoch: u64
per_thread_epoch[thread_id]: u64  // 0 = inactive
safe_to_reclaim_epoch: u64
drain_list: Vec<(epoch, action)>

// Allocator state
buffer_full: bool                // (tail_page - head_page) >= buffer_size
retry_count[thread_id]: u32      // per-thread retry counter (0..32)

// Hash state (for lossy eviction modeling)
overflow_bucket_count: u64       // tracks unbounded growth
```

### Required Transitions (Ordered by Priority)

**P0 — Must model to catch Bug 1 (circular buffer deadlock):**

| # | Transition | Precondition | Effect |
|---|---|---|---|
| 1 | `allocate(thread, size)` | `!buffer_full ∧ !sealed` | Advance `tail_address`, maybe seal page (Open→Sealed) |
| 2 | `allocate_fail(thread)` | `buffer_full` | Enter retry loop, call maintenance |
| 3 | `flush_page(page)` | `page_state == Sealed` | CAS to Flushing, submit I/O |
| 4 | `flush_queue_full(page)` | `device_queue_depth >= max` | Revert Flushing→Sealed, signal QueueFull |
| 5 | `io_complete(page)` | `page_state == Flushing` | CAS to Flushed |
| 6 | `poll_completions()` | I/O callbacks pending | Drive Flushing→Flushed transitions |
| 7 | `evict_page(page)` | `page_state == Flushed` | CAS to Evicted, advance head |
| 8 | `maintenance()` | Any thread | shift_read_only + flush + poll + evict |
| 9 | `yield_now(thread)` | In retry loop | Allow other threads (I/O workers) to run |
| 10 | `shift_read_only_to_tail()` | Mutable fraction exceeded | Advance read_only to tail |

**P0 — Must model to catch Bug 2 (lossy eviction lock deadlock):**

| # | Transition | Precondition | Effect |
|---|---|---|---|
| 11 | `truncate_until(offset)` | Lossy mode eviction | **Model: acquires device write lock for O(offset) time** |
| 12 | `write_async_blocked(thread)` | `device_write_lock_held` | Thread cannot submit flush I/O |
| 13 | `advance_begin(addr)` | Lossy eviction | Advance begin_address |
| 14 | `invalidate_hash_range(begin, end)` | Lossy eviction | O(buckets) scan, CAS entries to EMPTY |

**P1 — Required for checkpoint/recovery simulation:**

| # | Transition | Precondition | Effect |
|---|---|---|---|
| 15 | `checkpoint_begin(token)` | Phase == Rest | CAS Rest→Prepare, version bump on Prepare→InProgress |
| 16 | `checkpoint_wait_flush()` | Phase == WaitFlush | Poll flushed_until ≥ checkpoint_tail |
| 17 | `checkpoint_complete()` | Phase == WaitCompletion | CAS to Completed→Rest |
| 18 | `recover(token)` | Store offline | Rebuild hash + log from device files |

**P2 — Required for epoch correctness simulation:**

| # | Transition | Precondition | Effect |
|---|---|---|---|
| 19 | `epoch_protect(thread)` | Thread active | Store current_epoch to thread's entry |
| 20 | `epoch_unprotect(thread)` | Thread protected | Store 0, try_drain() |
| 21 | `epoch_bump()` | Any thread | fetch_add(1, SeqCst), try_drain() |
| 22 | `epoch_drain_action(action)` | `action.epoch <= safe_epoch` | Execute deferred callback |

**P2 — Required for resource growth modeling:**

| # | Transition | Precondition | Effect |
|---|---|---|---|
| 23 | `overflow_bucket_alloc()` | Hash bucket full (7 entries) | Allocate 64-byte overflow bucket |
| 24 | `hash_table_grow()` | Load factor > 0.75 | Double bucket count, cooperative split |
| 25 | `device_data_grow(bytes)` | write_async to new offset | Vec resize (never shrinks) |

### Invariants the Simulator Must Verify

```
INV-1: begin ≤ head ≤ read_only ≤ safe_read_only ≤ tail     (address ordering)
INV-2: (tail_page - head_page) < buffer_size                  (no circular overflow)
INV-3: Flushed pages exist between head and read_only         (forward progress)
INV-4: No page stuck in Flushing indefinitely                 (I/O completion)
INV-5: Writer threads make progress (ops/s > 0) under load    (liveness)
INV-6: In lossy mode: eviction + begin advance completes      (no truncation deadlock)
INV-7: Epoch safe_to_reclaim monotonically advances            (no epoch stall)
INV-8: Checkpoint eventually completes (WaitFlush→Complete)    (checkpoint liveness)
INV-9: After recovery: all addresses consistent                (crash safety)
```

### Scheduling Model Requirements

The simulator must support:

1. **Cooperative scheduling with explicit yield points** — `yield_now()` in retry loops, `poll_completions()` in maintenance.
2. **I/O completion callbacks on caller's thread** (InMemoryDevice/NullDevice: CompletedSync) or **on I/O worker thread** (real devices: Submitted + later callback).
3. **RwLock modeling** — multiple concurrent readers OR single writer. Critical for InMemoryDevice write lock contention.
4. **CAS contention** — multiple threads racing on `tail_address`, page state transitions, hash entries.
5. **Bounded retry with abort** — 32 retries then operation Aborted. Simulator must test both "retries succeed" and "retries exhausted" paths.
6. **Time-proportional operations** — `truncate_until` cost proportional to offset. `invalidate_entries_in_range` cost proportional to table size. `compute_safe_epoch` cost proportional to `scan_limit`.

---

## Appendix: sync.rs Abstraction Layer

**Source:** `sync.rs`

The codebase already has a `sync.rs` module (sync.rs:1-143) with three tiers:

1. **Tier 1 (loom):** Deterministic concurrency model checking — swaps std atomics for loom atomics
2. **Tier 2 (simulation):** DST framework — currently re-exports std (Phase 1). **Phase 2 will swap to SimClock/SimChannel/scheduler.**
3. **Tier 3 (std):** Production builds

**cfg precedence:** `loom` > `simulation` > `std`

**Already gated behind `feature = "simulation"`:**
- All atomic types: `AtomicBool`, `AtomicU8`, `AtomicU32`, `AtomicU64`, `AtomicUsize`, `AtomicPtr`
- Sync primitives: `Arc`, `Mutex`, `RwLock`, `RwLockReadGuard`
- Thread: `thread` module
- Time: `Duration`, `Instant`, `SystemTime`, `UNIX_EPOCH` (Phase 2: SimClock)
- Channels: `Sender`, `Receiver`, `channel` (Phase 2: SimChannel)

**⚠️ Not yet intercepted (needed for DST):**
- `alloc::alloc_zeroed` — page frame allocation
- File I/O — `std::fs` operations in SyncFileDevice
- `thread::spawn` — not gated (threads are real in all tiers)

This abstraction layer is the **injection point** for the deterministic simulator.
