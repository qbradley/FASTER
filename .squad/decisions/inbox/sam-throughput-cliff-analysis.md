# Throughput Cliff Analysis: Non-Lossy Mode Backpressure Failure

**Author:** Sam (Systems & Storage)
**Date:** 2025-07-17
**Status:** Investigation Complete — Fixes Proposed (not yet implemented)

---

## Executive Summary

Running `torture-stress` with 16 threads for 5 minutes shows a **97% throughput collapse**
(130–207K → 2–17K ops/s) at ~t=70s when the in-memory buffer fills. The root cause is a
**binary stall mechanism**: when the circular page buffer is full, every writer receives
`OperationStatus::Aborted` with no built-in retry or throttling. Recovery depends entirely
on a single maintenance thread running at 1ms intervals, which must complete a multi-step
pipeline (seal → flush I/O → evict → advance head) before any writer can proceed. The C#
FASTER implementation avoids this by blocking writers on a `FlushEvent` and having them
participate in epoch drain — effectively turning all threads into maintenance workers during
pressure. The Rust implementation returns `Aborted` immediately and leaves recovery to the
caller.

---

## 1. Root Cause Chain (Step by Step)

### Phase 1: Buffer Fill-Up (t=0 to ~t=70s)

```
Writer threads (16) → try_allocate() → CAS on tail_address → success
                       log_allocator.rs:114-173
```

- 16 threads write at 130–207K aggregate ops/s.
- With `buffer_size_pages=16` (512MB) and `page_size=32MB`, the buffer holds 16 pages.
- `mutable_fraction=0.9` → 14.4 mutable pages, only ~1.6 pages of read-only headroom.
- The log device is 2GB, key space is 5M keys.
- As keys are updated, old versions accumulate (no compaction). The log grows linearly.

### Phase 2: Buffer Pressure Detection (t≈70s)

```
maintenance() → in_memory_pages + 2 >= buffer_size → TRUE
                kv.rs:1630,1643
```

- The dedicated maintenance thread (runs every 1ms) detects buffer pressure.
- Calls `shift_read_only_to_tail()` — ALL pages become read-only.
  (`log_allocator.rs:200-235`)

### Phase 3: The Cliff

```
shift_read_only_to_tail()     →  ALL pages now read-only
                                  log_allocator.rs:200-235
flush_sealed_pages()          →  Submit async I/O (pages Sealed→Flushing)
                                  flush.rs:359-386
evict_pages()                 →  Nothing to evict yet (I/O in flight)
                                  eviction.rs:89-120
```

**Critical sequence:**

1. `shift_read_only_to_tail` makes the ENTIRE mutable region read-only.
2. Now every upsert to an existing key triggers **CopyUpdate** (copy record to tail).
   (`operations.rs:610-620`)
3. The current tail page has limited remaining space. It fills in milliseconds.
4. `advance_to_next_page()` hits **SF-10 check** (`log_allocator.rs:319-327`):

```rust
// SF-10: Prevent the tail from lapping the head.
let head_page = self.head_address.load(Ordering::Acquire).page().0;
let buffer_size = self.page_table.buffer_size() as u32;
if next_page.0.saturating_sub(head_page) >= buffer_size {
    return None;  // ← BUFFER FULL
}
```

5. `allocate_at_tail()` returns `None` → `OperationStatus::Aborted`.
   (`operations.rs:362-369, 616-619, 690-698`)

6. **ALL 16 threads are now getting Aborted on every operation.**

### Phase 4: The Stall (t≈70s to recovery)

```
Thread 1:  upsert → Aborted → upsert → Aborted → ...  (no progress)
Thread 2:  upsert → Aborted → upsert → Aborted → ...
...
Thread 16: upsert → Aborted → upsert → Aborted → ...

Maintenance (every 1ms):
  Cycle N:    flush_sealed_pages() → I/O in flight, evict → nothing Flushed yet
  Cycle N+k:  flush_sealed_pages() → no-op, evict → some pages now Flushed!
              advance_head → head moves forward → SF-10 passes for 1 new page
```

**The stall duration depends on flush I/O latency:**

| Disk Speed | Time to flush 32MB page | Time to free 4 pages | Time to flush all 16 |
|-----------|------------------------|---------------------|---------------------|
| 500 MB/s  | 64ms                   | 64ms (4 concurrent) | 256ms               |
| 200 MB/s  | 160ms                  | 160ms               | 640ms               |
| 50 MB/s   | 640ms                  | 640ms               | 2.56s               |

On a VM with slow disk (50–100 MB/s), the stall lasts seconds. During this time:
- **0 operations succeed** (all Aborted)
- Only the maintenance thread (1ms interval) drives recovery
- Workers spin uselessly, burning CPU without progress

### Phase 5: Partial Recovery (sawtooth pattern)

When head finally advances (pages evicted), one new page becomes available:

```
32MB page at 40MB/s write rate → fills in ~0.8 seconds
→ back to Aborted
→ wait for flush + evict cycle again
```

**Steady-state throughput** = burst_ops / (burst_time + stall_time)

With 200K ops/s burst, 0.8s fill, 5s stall:
- Effective = 200K × 0.8 / (0.8 + 5.0) = **27.6K ops/s** (86% collapse)

With 10s stall (slow disk):
- Effective = 200K × 0.8 / (0.8 + 10.0) = **14.8K ops/s** (93% collapse)

This matches the observed 2–17K ops/s.

---

## 2. Bottleneck Identification

### Bottleneck #1: Binary Stall (No Throttling)

**Files:** `log_allocator.rs:127-136, 319-327` (SF-10 check), `operations.rs:362-369`

The allocator returns `None` immediately when the buffer is full. There is no:
- Gradual slowdown as the buffer approaches full
- Backpressure signal that lets writers slow down preemptively
- Built-in retry with maintenance

The system transitions instantly from "full speed" to "100% Aborted."

### Bottleneck #2: Single Maintenance Thread

**File:** `torture-stress/src/main.rs:875-879`

```rust
fn maintenance_thread(store: Arc<FasterKv<...>>, shutdown: Arc<AtomicBool>) {
    while !shutdown.load(Ordering::Relaxed) {
        store.maintenance();
        thread::sleep(Duration::from_millis(1));
    }
}
```

Only ONE thread drives the entire flush/evict pipeline. The 16 worker threads getting
Aborted do NOT participate in recovery. They spin uselessly.

### Bottleneck #3: Multi-Step Pipeline Latency

**Files:** `kv.rs:1621-1676`, `flush.rs:206-290`, `eviction.rs:89-120, 153-209`

Recovery requires multiple sequential steps across multiple maintenance() calls:

```
maintenance() #1:  shift_read_only → flush_sealed_pages (submit I/O)
                   evict_pages → nothing Flushed yet
                   
[wait for flush I/O to complete... 100-5000ms depending on disk]

maintenance() #N:  flush_sealed_pages → no-op (already flushing)
                   evict_pages → evict Flushed pages
                   advance_head → head moves forward
```

Each call to `flush_sealed_pages` submits async I/O and returns immediately
(`flush.rs:256-264`). The I/O completion callback transitions Flushing→Flushed
(`flush.rs:138-173`). Only on a SUBSEQUENT maintenance call can eviction pick up
the Flushed pages.

### Bottleneck #4: Copy-on-Write Amplification

**File:** `operations.rs:603-620` (upsert in read-only region)

After `shift_read_only_to_tail`, every upsert to an existing key triggers CopyUpdate:
the entire record is copied to the tail. This has two effects:

1. **Tail consumption rate increases dramatically** — every update, not just new keys,
   consumes tail space.
2. **Dead records accumulate** in the read-only pages. Without compaction, useful data
   density decreases over time, making the buffer less effective.

### Bottleneck #5: Torture-Stress Drops Aborted Operations

**File:** `torture-stress/src/main.rs:519-551`

```rust
let status = ctx.upsert(store, &key, &input, ());
ctx.refresh();
drop(ctx);

if status == OperationStatus::Pending {
    let _ = store.complete_pending(session);
}
// ← Aborted is silently ignored. No retry. No maintenance call.
```

The test never checks for Aborted, never calls maintenance in response, and never retries.
Operations are simply lost. There is also no Aborted counter in `ThreadStats`
(`main.rs:285-309`), so the failure rate is invisible.

### Bottleneck #6: High Mutable Fraction Leaves No Headroom

**File:** `kv.rs:153` — `mutable_fraction: 0.9`

With 90% mutable fraction, only 1.6 pages (~51MB) are in the read-only/flush pipeline
at any time during normal operation. This means:

- Flush has almost no head start — pages aren't flushed until buffer pressure forces
  a bulk shift.
- When pressure triggers, it shifts 14+ pages at once — overwhelming the flush pipeline.
- The "gradual" flush that would happen with a lower mutable fraction (e.g., 0.5) is
  absent.

---

## 3. Comparison with C# FASTER

| Aspect | C# FASTER | Rust FASTER |
|--------|-----------|-------------|
| **Allocation failure** | Returns `RETRY_LATER` or `ALLOCATE_FAILED` | Returns `None` → `Aborted` |
| **Writer behavior on full** | **Blocks on `FlushEvent.Wait()`**, then retries (`cs/src/core/Index/FASTER/Implementation/HandleOperationStatus.cs:65-80`) | Returns immediately. Caller must handle. |
| **Epoch participation** | Stalled threads call `epoch.ProtectAndDrain()` — helps process deferred actions (`cs/src/core/Allocator/AllocatorBase.cs:1456-1461`) | No equivalent. Only maintenance thread drives recovery. |
| **Flush trigger** | `OnPagesMarkedReadOnly` → `AsyncFlushPages` — flush starts immediately when pages enter read-only (`cs/src/core/Allocator/AllocatorBase.cs:1608-1621`) | Flush only happens during `maintenance()` calls. |
| **Upsert contract** | **Never returns aborted** — always completes (sync: waits, async: returns Pending) | Returns `Aborted` — caller may lose the operation. |
| **Thread cooperation** | All threads help with epoch transitions during waits via `TryAllocateRetryNow` loop (`cs/src/core/Allocator/AllocatorBase.cs:1456-1461`) | Workers are idle during stalls — only maintenance thread works. |
| **Result** | Graceful degradation — throughput drops proportionally to I/O speed | **Binary cliff** — 100% stall until recovery |

**Key C# pattern (AllocatorBase.cs:1456-1461):**
```csharp
public long TryAllocateRetryNow(int numSlots = 1)
{
    long logicalAddress;
    while ((logicalAddress = TryAllocate(numSlots)) < 0)
        epoch.ProtectAndDrain();  // ← Threads HELP during wait
    return logicalAddress;
}
```

This is the fundamental difference: C# threads waiting for allocation **actively
participate in flushing and eviction** via epoch drain. Rust threads just get `Aborted`
and give up.

---

## 4. Proposed Fixes (Ranked by Impact × Feasibility)

### Fix 1: Inline Maintenance on Allocation Failure ⭐ HIGHEST PRIORITY

**Impact:** ★★★★★ | **Complexity:** ★★☆☆☆ | **Risk:** Low

**What:** When `try_allocate()` returns `None` (SF-10), have the calling thread run
`maintenance()` inline before returning `Aborted`. This turns all 16 stalled threads
into maintenance workers.

**Where:** `operations.rs:362-369` (and other Aborted return sites)

```rust
// PROPOSED (pseudocode):
None => {
    // Before aborting, try to help with flush/eviction
    store.maintenance();
    // Retry once after maintenance
    match allocate_at_tail(ctx.allocator, key, &value) {
        Some(pair) => pair,
        None => return OperationStatus::Aborted,
    }
}
```

**Why it works:** Instead of 1 maintenance thread handling all flush/eviction, all 16
threads cooperate. This is exactly the C# `ProtectAndDrain` pattern. `maintenance()` is
already designed to be safe under concurrent calls (uses CAS for address updates, atomic
page state transitions).

**Concerns:**
- Need to verify `maintenance()` is fully safe under concurrent invocation (flush and
  eviction use CAS-based state machines, so likely fine).
- May cause thundering-herd on flush submissions (mitigation: the page state machine
  prevents double-flush).

---

### Fix 2: Eager Flush — Lower Mutable Fraction or Flush Watermark

**Impact:** ★★★★☆ | **Complexity:** ★☆☆☆☆ | **Risk:** Low

**What:** Start flushing pages well before the buffer is full.

**Option A:** Lower `mutable_fraction` from 0.9 to 0.5–0.6. This creates 6–8 pages of
read-only/flush headroom instead of 1.6 pages.

**Option B:** Add a "flush watermark" (e.g., 75% full) that triggers read-only shift
and flush preemptively, before buffer_pressure (98% full) fires.

**Where:** `kv.rs:1626-1650` (buffer pressure detection)

**Why it works:** The flush pipeline gets a head start. When writers need a new page,
previously-flushed pages are already available for eviction. The stall window shrinks
from seconds to milliseconds.

**Trade-off:** More CopyUpdate operations (records in read-only region require copy-to-tail)
→ higher write amplification. But this is a net win because throughput stability matters
more than write amplification at these workload intensities.

---

### Fix 3: Retry-with-Backoff in UnsafeContext

**Impact:** ★★★★☆ | **Complexity:** ★★☆☆☆ | **Risk:** Medium

**What:** Add a retry loop in the session/context layer (following the pattern in
`lossy_cache_tests.rs:56-72`):

```rust
loop {
    let status = internal_upsert(...);
    if status == OperationStatus::Aborted {
        store.maintenance();
        complete_pending(session);
        thread::yield_now();
        continue;
    }
    return status;
}
```

**Where:** `store/session.rs` — `UnsafeContext::upsert/rmw/delete`

**Why it works:** Operations never silently fail. Callers always get a definitive result.
This matches the C# contract where Upsert always completes.

**Concerns:**
- Changes the API contract (callers expecting non-blocking behavior).
- Needs a retry limit to prevent infinite loops.
- Should be opt-in (config flag) to preserve current behavior for callers that want
  explicit control.

---

### Fix 4: Concurrent Background Flush Thread

**Impact:** ★★★☆☆ | **Complexity:** ★★★★☆ | **Risk:** Medium

**What:** Spawn a dedicated flush thread that continuously scans for Sealed pages and
submits flush I/O, independent of the 1ms maintenance interval.

**Where:** New component in `hybrid_log/` or integration into `store/kv.rs`

**Why it works:** Flush I/O starts immediately when pages are sealed (like C#'s
`OnPagesMarkedReadOnly` → `AsyncFlushPages`). No waiting for the next maintenance cycle.

**Concerns:**
- Adds complexity (thread lifecycle, shutdown coordination).
- The 1ms maintenance interval already provides high-frequency flush attempts.
- Most of the stall time is I/O latency, not flush scheduling latency.

---

### Fix 5: Increase Eviction Batch Size

**Impact:** ★★☆☆☆ | **Complexity:** ★☆☆☆☆ | **Risk:** None

**What:** Change `eviction_batch_size` from 4 to `buffer_size_pages` (16 or more).

**Where:** `eviction.rs:51` — `EvictionPolicy::default()`

**Why it works:** `evict_pages()` can process more pages per maintenance call. However,
`advance_head()` (`eviction.rs:153-209`) already evicts ALL consecutive Flushed pages
inline, so the batch_size in `evict_pages` is mostly a first-pass optimization.

**Limited impact** because the real bottleneck is flush I/O latency, not eviction speed.

---

### Fix 6: Add Aborted Metrics to torture-stress

**Impact:** ★☆☆☆☆ (observability only) | **Complexity:** ★☆☆☆☆ | **Risk:** None

**What:** Add an `aborted` counter to `ThreadStats` and report it alongside other metrics.

**Where:** `torture-stress/src/main.rs:285-309`

**Why it matters:** Currently the 97% throughput collapse is invisible — Aborted operations
are silently dropped with no counter. Adding metrics lets developers see the true failure
rate and correlate it with buffer pressure events.

---

## 5. Recommended Implementation Order

```
Phase 1 (Quick Wins):
  Fix 6 → Add Aborted metrics (observability)
  Fix 5 → Increase eviction batch size (trivial, small gain)
  Fix 2 → Lower mutable_fraction or add flush watermark (config change)

Phase 2 (Core Fix):
  Fix 1 → Inline maintenance on allocation failure (biggest impact)
  Fix 3 → Retry-with-backoff in UnsafeContext (API improvement)

Phase 3 (Architecture):
  Fix 4 → Background flush thread (if Phase 2 is insufficient)
```

---

## 6. Key Code References

| Component | File | Lines | Purpose |
|-----------|------|-------|---------|
| SF-10 buffer-full check | `log_allocator.rs` | 127-136, 319-327 | Prevents tail from lapping head |
| Aborted return (upsert) | `operations.rs` | 362-369 | Returns Aborted when allocation fails |
| Aborted return (CopyUpdate) | `operations.rs` | 616-619 | Returns Aborted on read-only copy failure |
| Aborted return (rmw) | `operations.rs` | 690-698 | Returns Aborted when allocation fails |
| maintenance() | `kv.rs` | 1621-1676 | Flush + eviction orchestrator |
| Buffer pressure detection | `kv.rs` | 1630, 1643 | `in_memory_pages + 2 >= buffer_size` |
| shift_read_only_to_tail | `log_allocator.rs` | 200-235 | CAS loop to advance read-only boundary |
| flush_sealed_pages | `flush.rs` | 359-386 | Scans for Sealed pages, submits async I/O |
| flush_page (async I/O) | `flush.rs` | 206-290 | Sealed→Flushing + device.write_async |
| flush_completion_callback | `flush.rs` | 138-173 | Flushing→Flushed on I/O complete |
| evict_pages | `eviction.rs` | 89-120 | Evicts up to batch_size Flushed pages |
| advance_head | `eviction.rs` | 153-209 | Evicts consecutive Flushed/Evicted pages inline |
| try_evict_frame | `page.rs` | 501-533 | Atomic CAS to null frame slot + free memory |
| advance_to_next_page | `log_allocator.rs` | 305-364 | Tail page advance with SF-10 guard |
| allocate_at_tail | `operations.rs` | 203-218 | Try allocate + advance page + retry |
| Page state machine | `page.rs` | 43-56 | Free→Open→Sealed→Flushing→Flushed→Evicted |
| SyncFileDevice I/O | `sync_file_device.rs` | 455-499 | Enqueue async write to worker threads |
| Torture-stress config | `torture-stress/main.rs` | 1085-1111 | buffer=16 pages, mutable_frac=0.9 |
| Torture-stress maintenance | `torture-stress/main.rs` | 875-879 | Single thread, 1ms interval |
| Torture-stress upsert | `torture-stress/main.rs` | 519-551 | No Aborted handling or retry |
| C# FlushEvent wait | `cs/.../HandleOperationStatus.cs` | 65-80 | Sync: wait for flush, then retry |
| C# ProtectAndDrain | `cs/.../AllocatorBase.cs` | 1456-1461 | Threads help during allocation wait |
| C# AsyncFlushPages | `cs/.../AllocatorBase.cs` | 2045-2127 | Immediate flush on read-only transition |

---

## 7. Summary Diagram

```
Normal Operation (t=0-70s):
┌──────────────────────────────────────────────────────┐
│ 16 writer threads → allocate_at_tail → SUCCESS       │
│ tail advances → pages sealed behind it               │
│ maintenance (1ms) → flush older pages → evict → OK   │
│ Throughput: 130-207K ops/s                           │
└──────────────────────────────────────────────────────┘

Cliff (t≈70s):
┌──────────────────────────────────────────────────────┐
│ Buffer full: in_memory_pages + 2 >= buffer_size      │
│ maintenance → shift_read_only_to_tail (ALL RO)       │
│ Current page fills quickly (copy-on-write)           │
│ advance_to_next_page → SF-10 FAIL → None             │
│ ALL 16 threads: Aborted, Aborted, Aborted, ...      │
│                                                      │
│ Only maintenance thread works on recovery:           │
│   flush I/O: 100-5000ms (depends on disk speed)      │
│   evict pages → advance head → 1 page free           │
│   Writers rush in → fill page → Aborted again        │
│                                                      │
│ Throughput: 2-17K ops/s (97% collapse)               │
└──────────────────────────────────────────────────────┘

C# FASTER (for comparison):
┌──────────────────────────────────────────────────────┐
│ Buffer full: ALLOCATE_FAILED                         │
│ Thread calls epoch.ProtectAndDrain() → helps flush   │
│ Thread waits on FlushEvent → resumes when page free  │
│ ALL threads participate in recovery                  │
│ Throughput: degrades proportionally, no cliff         │
└──────────────────────────────────────────────────────┘
```
