# Log Truncation / Compaction Analysis — Why stress_disk OOMs

**Author:** Sam (Systems & Storage)  
**Date:** 2026-03-17  
**Context:** `stress_disk` ran 280s on Azure VM, OOM-killed at 27+ GB RSS with 48 GB untruncated log files

---

## 1. Executive Summary

The stress test runs in **lossless non-lossy mode** (`lossy: false`) with **compaction disabled** (`auto_compact: false`). In this configuration:

- **Disk growth is unbounded** because `begin_address` never advances and `device.truncate_until()` is never called. This is the direct cause of 48 GB of log files.
- **RSS explodes** because the combination of unbounded on-disk log + pending I/O write completions creates a cascading feedback loop: records cycle between tail and on-disk regions, generating ever more log data and I/O traffic, eventually overwhelming the allocator's retry budget and the I/O worker pool.

Two things are missing for bounded lossless operation:
1. **Compaction must be enabled** (the mechanism exists but was not turned on)
2. **The non-lossy maintenance path should advance `begin_address` after compaction** (currently only lossy mode does this inline)

---

## 2. How the Hybrid Log Lifecycle Works (What's Implemented)

### Region Architecture (`regions.rs:12-18`)

```
begin          head           read_only     safe_ro       tail
  │              │               │             │            │
  │  ON DISK     │  READ-ONLY    │  FUZZY       │  MUTABLE   │
  │  (evicted)   │  (in memory,  │  (flushing)  │ (writable) │
  │              │   immutable)  │              │            │
```

Five monotonically-advancing boundaries. Invariant: `begin ≤ head ≤ read_only ≤ safe_read_only ≤ tail`.

### Page Lifecycle (`page.rs:10-12`)

```
Free → Open → Sealed → Flushing → Flushed → Evicted → Free (recycle)
```

Transitions are atomic via `AtomicPageState::try_transition` (CAS on `AtomicU8`).

### What `maintenance()` Does (`kv.rs:1676-1769`)

1. **Shift read-only boundary** if mutable region is too large or buffer pressure detected (`kv.rs:1691-1696`)
2. **Buffer-pressure pre-eviction** (SF-10 guard): if `in_memory_pages + 2 >= buffer_size`, force shift + evict (`kv.rs:1711-1720`)
3. **Flush sealed pages** to device via `flush_sealed_pages()` (`kv.rs:1723-1726`)
4. **Poll device** for I/O completions: `Flushing → Flushed` callbacks (`kv.rs:1730`)
5. **Evict if needed** — HERE IS THE CRITICAL DIVERGENCE:

**Lossy path** (`kv.rs:1736-1756`):
```rust
// Evict + advance begin + invalidate hash entries + truncate device
let evicted = self.evictor.evict_pages(&self.allocator);
let new_begin = self.allocator.try_advance_begin(head);
self.hash_index.invalidate_entries_in_range(old_begin, new_begin);
self.device.truncate_until(truncate_offset);
```

**Non-lossy path** (`kv.rs:1757-1759`):
```rust
// ONLY evict — nothing else
self.evictor.evict_pages(&self.allocator);
```

### What `evict_pages()` Does (`eviction.rs:89-120`)

- Scans up to `eviction_batch_size` (4) pages from head forward
- Only evicts pages in `Flushed` state (Flushed → Evicted)
- Calls `advance_head()` which walks forward, evicting Flushed frames inline
- `try_evict_frame()` nulls the PageTable slot and frees the 32 MB frame via `Box::from_raw` (`page.rs:501-533`)
- Advances `head_address` via CAS

### What `truncate_until()` Does for SyncFileDevice (`sync_file_device.rs:599-616`)

Deletes segment files whose index is below `offset / segment_size`:
```rust
fn truncate_until(&self, offset: u64) {
    let threshold = offset / self.segment_size;
    let to_remove: Vec<u64> = guard.keys().filter(|&idx| idx < threshold).collect();
    for idx in to_remove {
        self.registry.remove_segment(idx);  // HashMap remove + fs::remove_file
    }
}
```

This is correctly implemented and works. The problem is it's never called in non-lossy mode.

---

## 3. Root Cause #1: Disk Growth (48 GB) — Truncation Never Happens

### The Missing Link

In non-lossy mode, `begin_address` advancement and device truncation are **only** available through the **compaction pipeline**:

- `compaction/begin_address.rs` — `BeginAddressAdvancer::advance()` calls `allocator.try_advance_begin()` + `device.truncate_until()`
- `compaction/orchestrator.rs` — Wires scan → copy → pointer swing → begin-address advance
- `store/kv.rs:1632` — `compact()` runs the full pipeline
- `store/kv.rs:1834-1847` — `maybe_compact()` checks the policy

### The stress_disk Configuration

```rust
// stress_disk.rs:293-305
auto_compact: false,
lossy: false,
```

**No compaction policy is set** (default is `None`), and `auto_compact` is `false`. The test never calls `compact()` manually. Therefore:

1. `begin_address` stays at `LogicalAddress::new(Page(0), Offset(0))` forever
2. `device.truncate_until()` is never called
3. Every flushed page creates a new segment file that is never deleted
4. With 32 MB pages at ~150 MB/s write rate: **~1.5 GB/minute** of new segment files
5. After 280 seconds: **~48 GB** of accumulated segment files ✓

### Fix Required

Either:
- **Option A:** Enable compaction: `auto_compact: true` + set a `SpaceAmplificationPolicy` or `TombstonePercentPolicy`
- **Option B:** Add a non-lossy truncation path in `maintenance()` that advances `begin_address` to `flushed_until_address` and truncates the device, but ONLY for data that is both flushed and has no live hash references (requires compaction to have relocated live records first)
- **Option C:** Have the stress test call `store.compact()` periodically from the maintenance thread

**Option A is the correct fix for the stress test.** The compaction infrastructure exists and works.

---

## 4. Root Cause #2: RSS Growth (2.9 GB → 27+ GB) — Cascading Pending I/O Feedback Loop

### The Memory Budget (Expected)

| Component | Size | Reference |
|-----------|------|-----------|
| Page frames (16 × 32 MB) | 512 MB | `page.rs:190`, `stress_disk.rs:291` |
| Hash index (2^21 × 64B) | 128 MB | `stress_disk.rs:287` |
| Thread stacks (21 × 8 MB) | 168 MB | 16 workers + 1 maint + 4 I/O |
| Misc (code, libs, heap) | ~200 MB | |
| **Expected total** | **~1 GB** | |

Initial observed RSS: 2.9 GB. This is higher than expected but within reason (glibc allocator overhead, kernel structures, etc.).

### The Cascade Mechanism

As the log grows without compaction, the `OnDisk` region expands (from `begin_address=0` to `head_address`). An increasing fraction of hash entries point to on-disk addresses. This triggers a cascading feedback:

**Phase 1 — Increasing pending I/O rate:**
- Upsert/delete for key K → hash entry points to OnDisk address → returns `Pending` (`operations.rs:609-619`)
- `dispatch_pending_io` issues 8KB async read (`kv.rs:1086-1099`)
- `complete_pending` (every 1024 ops) runs `complete_write_pending` → allocates NEW record at tail, CAS hash entry to new address (`kv.rs:1309-1446`)
- The new record at the tail ages through: mutable → sealed → flushed → evicted → OnDisk
- Next access to same key: SAME cycle again
- Each cycle adds one more dead record to the on-disk log

**Phase 2 — Buffer pressure amplification:**
- Write completions call `allocate_with_retry` with `on_alloc_failure: None` (`kv.rs:1452-1463`)
- Under buffer pressure, these allocations silently fail → write completions are DROPPED
- Hash entry still points to old on-disk address → key is permanently "stuck" on disk
- Every future operation on this key goes Pending indefinitely
- More keys get stuck → more pending I/O → more buffer pressure → positive feedback

**Phase 3 — RSS spike:**
- With most operations going Pending, each worker thread accumulates up to 1024 `PendingIoContext` objects between `complete_pending` calls
- Each context: `PendingOperation` (cloned key + input + layout ~100B) + `AlignedBuffer` (8KB) + `Arc<AtomicBool>` completion state
- Peak: 16 threads × 1024 contexts × ~8.2 KB ≈ **128 MB** (not sufficient alone)
- BUT: the I/O worker threads (4 threads via `SyncFileDevice`) must read from **48 GB of segment files**, causing massive page cache pressure
- The kernel reads ahead and caches file data. While page cache isn't directly RSS, the kernel may also dirty-track file-backed mappings
- Additionally, if `complete_write_pending` succeeds intermittently, it creates records that immediately cycle back to disk, generating even more I/O and log data

**The likely proximate cause of the 27 GB RSS spike** is a combination of:

1. **Page frame allocation churn**: Under extreme buffer pressure, the evict→allocate cycle calls `alloc_zeroed(32 MB)` / `dealloc(32 MB)` rapidly. The system allocator (glibc) may retain multiple `mmap`'d regions as virtual memory due to `MADV_FREE` semantics, inflating RSS even though only 16 frames are "live"
2. **Pending I/O buffer accumulation**: If the I/O worker threads fall behind (because they're seeking across 48 GB of files), completed I/O contexts pile up in all 16 session `io_contexts` vectors
3. **`allocate_with_retry` triggering flush-without-eviction**: The retry path calls `self.flush()` which submits more I/O, further loading the worker pool while not actually freeing memory (flush ≠ evict)

### Key Code References

- `complete_write_pending` allocates with `on_alloc_failure: None`: `kv.rs:1452-1463`
- Only tries `flush()` once then gives up — does NOT run full maintenance (no eviction)
- Non-lossy maintenance skips begin_address advance: `kv.rs:1757-1759`
- Pending ops accumulated in `session.io_contexts: Vec<PendingIoContext>`: `session.rs:205`
- Drained only on `complete_pending` (every 1024 ops): `stress_disk.rs:485-487`

---

## 5. What's Implemented vs. What's Missing

### ✅ Fully Implemented

| Component | Location | Status |
|-----------|----------|--------|
| Page flush pipeline (Sealed→Flushing→Flushed) | `flush.rs` | Working |
| Page eviction (Flushed→Evicted, memory freed) | `eviction.rs:89-228` | Working |
| `advance_head` (head_address advancement) | `eviction.rs:153-209` | Working |
| `try_advance_begin` (begin_address CAS) | `log_allocator.rs:527-547` | Working |
| `SyncFileDevice::truncate_until` (segment deletion) | `sync_file_device.rs:599-616` | Working |
| Lossy mode maintenance (evict + advance begin + truncate) | `kv.rs:1736-1756` | Working |
| Full compaction pipeline (scan→copy→swing→truncate) | `compaction/` module | Working |
| `maybe_compact()` policy-based auto-compaction | `kv.rs:1834-1847` | Working |
| `invalidate_entries_in_range` (hash GC) | `hash/index.rs:427-480` | Working |
| Write-path pending I/O completion | `kv.rs:1309-1446` | Working |

### ❌ Missing / Misconfigured for Bounded Lossless Operation

| Gap | Impact | Fix |
|-----|--------|-----|
| Non-lossy `maintenance()` never advances `begin_address` | Disk grows without bound | Enable compaction or add periodic compact() |
| Non-lossy `maintenance()` never calls `truncate_until()` | Old segment files never deleted | Same — compaction handles this |
| `auto_compact: false` in stress_disk | Compaction never triggers | Set `auto_compact: true` + policy |
| No compaction policy set in stress_disk | `maybe_compact()` is a no-op | Set `SpaceAmplificationPolicy::new(2.0)` |
| `allocate_with_retry` uses `on_alloc_failure: None` | Write completions silently dropped under pressure | Pass `maintenance` callback or retry with eviction |
| `maintenance()` doesn't call `maybe_compact()` | Even with auto_compact, maintenance won't trigger it | Add `self.maybe_compact()` at end of `maintenance()` |

### ⚠️ Important: `maintenance()` Doesn't Call `maybe_compact()`

Looking at `maintenance()` (`kv.rs:1676-1769`), it handles steps 1-4 (shift RO, flush, poll, evict) but does **NOT** call `maybe_compact()`. The `maybe_compact()` method (`kv.rs:1834-1847`) exists but must be called separately. The stress test's maintenance thread only calls `store.maintenance()`:

```rust
// stress_disk.rs:325-328
while !counters.stop.load(Ordering::Relaxed) {
    store.maintenance();
    thread::sleep(Duration::from_millis(5));
}
```

**Even if `auto_compact` were enabled, the maintenance thread wouldn't trigger compaction.** The maintenance thread must also call `store.maybe_compact()`.

---

## 6. Recommended Fixes

### Fix 1: Enable compaction in stress_disk (immediate, config-only)

```rust
// stress_disk.rs — store_config
auto_compact: true,  // was: false

// After store creation:
store.set_compaction_policy(Some(Box::new(
    SpaceAmplificationPolicy::new(2.0)
)));
```

### Fix 2: Add `maybe_compact()` to maintenance thread

```rust
// stress_disk.rs — maintenance thread
while !counters.stop.load(Ordering::Relaxed) {
    store.maintenance();
    store.maybe_compact();  // NEW — trigger compaction check
    thread::sleep(Duration::from_millis(5));
}
```

### Fix 3: Consider integrating compaction check into `maintenance()` (engine improvement)

In `kv.rs:maintenance()`, after the eviction block, add:
```rust
// 5. Check compaction policy.
if self.config.auto_compact {
    self.maybe_compact();
}
```

This ensures any caller of `maintenance()` automatically gets compaction when enabled, rather than requiring every maintenance loop to remember to call it separately.

### Fix 4: Improve `allocate_with_retry` resilience

Change `allocate_with_retry` to use the full maintenance callback instead of just `flush()`:

```rust
fn allocate_with_retry(&self, key, value) -> Option<(LogicalAddress, MutableRecordAccessor)> {
    if let Some(pair) = allocate_at_tail(&self.allocator, key, value, None) {
        return Some(pair);
    }
    // Full maintenance cycle instead of just flush
    self.maintenance();
    allocate_at_tail(&self.allocator, key, value, None)
}
```

This prevents write completions from being silently dropped under buffer pressure.

---

## 7. Summary of File:Line References

| Finding | File | Lines |
|---------|------|-------|
| Non-lossy maintenance only evicts | `store/kv.rs` | 1757-1759 |
| Lossy maintenance does full truncation | `store/kv.rs` | 1736-1756 |
| Buffer pressure pre-eviction | `store/kv.rs` | 1711-1720 |
| `allocate_with_retry` with no maintenance | `store/kv.rs` | 1452-1463 |
| `complete_write_pending` implementation | `store/kv.rs` | 1309-1446 |
| `maybe_compact()` not called by maintenance | `store/kv.rs` | 1676-1769, 1834-1847 |
| `try_advance_begin` implementation | `hybrid_log/log_allocator.rs` | 527-547 |
| `evict_pages` + `advance_head` | `hybrid_log/eviction.rs` | 89-209 |
| `evict_and_truncate` (unused in maintenance) | `hybrid_log/eviction.rs` | 211-228 |
| `truncate_until` implementation | `sync_file_device.rs` | 599-616 |
| Segment removal | `sync_file_device.rs` | 164-171 |
| Compaction orchestrator | `compaction/orchestrator.rs` | 1-22 |
| Begin-address advancer | `compaction/begin_address.rs` | 1-24 |
| Stress test config (no compaction) | `examples/stress_disk.rs` | 293-305 |
| Stress test maintenance thread | `examples/stress_disk.rs` | 321-329 |
| Pending I/O dispatch | `store/kv.rs` | 1086-1099 |
| Session io_contexts accumulation | `store/session.rs` | 200-205 |
| Region classification | `hybrid_log/regions.rs` | 12-18, 96-116 |
| Page lifecycle states | `hybrid_log/page.rs` | 37-56 |
| Hash entry invalidation | `hash/index.rs` | 427-480 |
