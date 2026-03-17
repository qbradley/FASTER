# Log Truncation & GC: Cross-Reference Analysis

**Author:** Gandalf (System Architect)  
**Date:** 2026-03-17  
**Status:** Decision — Recommendation for Rust implementation  
**Requested by:** qbradley

---

## Problem Statement

The Rust disk stress test OOMs after ~280 seconds:
- 48 GB of log segment files accumulate on disk
- RSS spikes from 2.9 GB to 27+ GB
- In **lossless mode** (the default), `truncate_until()` is **never called**
- Compaction is disabled (`auto_compact: false`)
- Result: unbounded disk growth → eventual OOM or disk exhaustion

## Root Cause

In `rust/crates/faster-core/src/store/kv.rs:1757-1759`, the lossless-mode
`maintenance()` path evicts in-memory pages but **never advances
`begin_address`** and **never calls `device.truncate_until()`**. Segment
files accumulate indefinitely. Only lossy mode (line 1714-1756) and
manual compaction (line 1834-1846) perform device truncation.

---

## Cross-Reference: How Each Implementation Handles This

### Address Model (Common to All)

All FASTER variants use the same 4-address hybrid log model:

```
begin_address ≤ head_address ≤ read_only_address ≤ tail_address
   │               │                  │                  │
   │               │                  │                  └─ Next write position
   │               │                  └─ Mutable/immutable boundary
   │               └─ In-memory / on-disk boundary
   └─ Start of valid data (everything before is truncated/GC'd)
```

The key insight: **moving `begin_address` forward is the mechanism that
enables truncation.** Head/RO/tail management bounds memory; begin-address
management bounds disk.

---

### Design Comparison Table

| Aspect | C++ FASTER | C# FASTER | Tsavorite (Garnet) | Our Rust |
|--------|------------|-----------|-------------------|----------|
| **Truncation trigger** | `ShiftBeginAddress()` API or auto-compaction thread | `ShiftBeginAddress()` API or checkpoint completion | Timer-based (`CompactionFrequencySecs`) + segment count threshold (`CompactionMaxSegments`) | **Lossy mode only** — lossless never truncates |
| **Safe address computation** | Must be ≤ `safe_head_address`; epoch-protected via `GarbageCollectSetup()` | Must be ≤ `SafeReadOnlyAddress`; epoch bump ensures visibility | Inherited from C# + Garnet server policies | `try_advance_begin(head)` — only in lossy path |
| **Segment file deletion** | `TruncateSegments()` → epoch callback → `file.Close()` + `file.Delete()` per segment (`file_system_disk.h:389-450`) | `RemoveSegment()` → `Native32.DeleteFileW()` per segment, epoch-protected (`StorageDeviceBase.cs:218-256`) | Same as C# (inherited) | `registry.remove_segment()` → `fs::remove_file()` (`sync_file_device.rs:163-171`) — **works, just never called in lossless** |
| **Compaction strategy** | `CompactWithLookup()`: multi-threaded scan, conditional insert to tail, shift begin, GC index (`faster.h:3585-3798`) | `CompactLog()`: scan or lookup mode, copy live records to tail (`FASTERCompaction.cs:35-149`) | 4 modes: None / Shift / Scan / Lookup. Shift is lightweight (no record copy) | 4-phase pipeline: scan → copy → pointer swing → begin-address advance (`compaction/mod.rs`) — **complete but auto_compact=false** |
| **Memory bounding** | `HlogCompactionConfig` with `hlog_size_budget` (default 1 GiB hot, 8 GiB cold); auto-compaction thread (`faster.h:4459-4502`) | Caller-driven via `EmptyPageCount` + `FlushAndEvict()` — **no automatic budget** | Garnet server manages memory budgets externally | `EvictionPolicy` bounds in-memory pages — **works correctly** |
| **Checkpoint interaction** | `log_begin_address` stored in `IndexMetadata`; recovery resets to checkpoint begin; can truncate past checkpoint if not needed | Checkpoint `WriteHybridLogMetaInfo()` immediately calls `ShiftBeginAddress(beginAddress, truncateLog: true)` (`Checkpoint.cs:56`) | Segments not deleted until next checkpoint (or `CompactionForceDelete`) | **No checkpoint impl yet** — no interaction to worry about |
| **Auto-compaction** | Yes — background thread with configurable budget, trigger%, compact% (`config.h:48-57`) | **No** — entirely caller-driven | **Yes** — Garnet server runs compaction on timer + segment count threshold | `maybe_compact()` exists but disabled by default (`auto_compact: false`) |

---

## Detailed Implementation References

### C++ FASTER

**Auto-compaction thread** (`cc/src/core/faster.h:4459-4502`):
- Background thread checks `Size() >= hlog_size_budget * trigger_pct` periodically
- Compacts `min(compact_pct * size, max_compacted_size)` bytes from begin up to `safe_head_address`
- Config: `hlog_size_budget` (hard limit), `trigger_pct`, `compact_pct`, `check_interval`
- Default budget: 1 GiB hot store, 8 GiB cold store

**ShiftBeginAddress** (`cc/src/core/faster.h:3491-3509`):
- Updates `hlog.begin_address` monotonically
- Calls `hash_index_.GarbageCollectSetup()` to scan and fix index entries
- Epoch-protected: no thread reads below begin after epoch completes

**Segment deletion** (`cc/src/device/file_system_disk.h:389-450`):
- `TruncateSegments()` deletes files via epoch callback
- For each obsolete segment: `file.Close()` then `file.Delete()`
- New segment bundle atomically replaces old one

### C# FASTER

**ShiftBeginAddress** (`cs/src/core/Allocator/AllocatorBase.cs:1517-1567`):
- Monotonic update of `BeginAddress`
- Cascading shift: shifts ReadOnly → Head if needed, waits for flush
- `truncateLog: true` triggers `TruncateUntilAddress()` via epoch callback

**CompactLog** (`cs/src/core/Compaction/FASTERCompaction.cs:35-149`):
- Two strategies: Lookup-based (random probe per record) or Scan-based (temp KV for dedup)
- Both copy live records to tail, then `ShiftBeginAddress(untilAddress, truncateLog: false)`
- **Caller must explicitly call `Log.Truncate()` or checkpoint to delete files**

**LogAccessor** (`cs/src/core/Index/FASTER/LogAccessor.cs`):
- Full API: `ShiftBeginAddress()`, `ShiftHeadAddress()`, `Flush()`, `FlushAndEvict()`, `Truncate()`
- `Truncate()` = `ShiftBeginAddress(BeginAddress, truncateLog: true)` — idempotent disk cleanup
- `EmptyPageCount` controls memory pressure

### Tsavorite / Garnet

**Key improvement over C# FASTER:** Garnet wraps Tsavorite with **server-level orchestration**:
- `CompactionFrequencySecs`: periodic compaction check timer
- `CompactionMaxSegments`: segment count threshold trigger
- `CompactionType`: None / Shift / Scan / Lookup
- `CompactionForceDelete`: immediate segment deletion (risky without checkpoint)

**Critical design point:** Tsavorite itself is still caller-driven (same as C# FASTER).
The "automatic" behavior lives in the **Garnet server layer**, not in the storage engine.
This is the model we should follow.

---

## Analysis: Why Each Model Evolved

| Generation | Model | Rationale |
|-----------|-------|-----------|
| C++ FASTER | Built-in auto-compaction thread | Research prototype; self-contained convenience |
| C# FASTER | Pure caller-driven API | Library design; consumers (Azure services) have diverse policies |
| Tsavorite/Garnet | Library API + server orchestration | Best of both: clean library boundary, operational automation |
| Our Rust | Library API exists, orchestration missing | We have the building blocks but no one calls them |

---

## Recommendation for Rust

### Minimum Viable Fix (Ship This Week)

**Goal:** `stress_disk` (and any lossless workload) runs forever without OOM.

**Change 1: Add `ShiftBeginAddress` to lossless maintenance**

In `kv.rs` `maintenance()`, after eviction in lossless mode (line 1757-1759),
add begin-address advancement and device truncation — but **only for pages
that have been both flushed AND evicted**:

```rust
// After: self.evictor.evict_pages(&self.allocator);
// In lossless mode, advance begin_address to head (pages are safely on disk)
// and truncate device segments below begin.
let new_begin = self.allocator.try_advance_begin(self.allocator.head_address());
if new_begin > old_begin {
    let truncate_offset = u64::from(new_begin.page().0) * u64::from(self.allocator.page_size());
    self.device.truncate_until(truncate_offset);
}
```

**Preconditions satisfied:**
- Pages below `head_address` are guaranteed flushed (head only advances past flushed pages)
- Pages below `head_address` are evicted from memory (eviction happened first)
- No live hash index entries point below `begin_address` after advancement
  - **Wait**: This is the critical gap. We need hash index invalidation in lossless mode too.

**Change 1b: Hash index GC for lossless truncation**

When advancing `begin_address` in lossless mode, stale hash entries that
still point to truncated addresses must be cleaned up. Two options:

- **Option A (fast):** `hash_index.invalidate_entries_in_range(old_begin, new_begin)` — same as lossy mode. Simple linear scan of hash buckets. This is what the C++ `GarbageCollectSetup()` does.
- **Option B (correct for lossless):** Only advance `begin_address` after compaction copies live records forward. This preserves all data.

**For the minimum fix, Option A is wrong for lossless semantics** — it would
silently discard data that hasn't been compacted. In a true lossless store,
we must compact first, then truncate.

### Revised Minimum Fix

**The correct minimum viable fix for lossless mode is to enable auto-compaction:**

1. In `stress_disk.rs`: set `auto_compact: true`
2. Implement a default `CompactionPolicy` that triggers when
   `(tail - begin) > N * buffer_size` (e.g., 4× the in-memory buffer)
3. `maybe_compact()` already exists and calls `compact()` which does the full
   4-phase pipeline including `device.truncate_until()`

This is safe because compaction:
- Copies live records to the log tail (preserves data)
- Swings hash index pointers to new locations (no stale entries)
- Advances `begin_address` past compacted region
- Calls `device.truncate_until()` to delete old segment files

### Medium-Term: Follow Tsavorite's Model

Build a **server-level orchestration layer** (not baked into the engine):

```rust
pub struct MaintenanceConfig {
    /// Run compaction when log size exceeds this multiple of buffer size.
    pub compaction_trigger_ratio: f64,       // e.g., 4.0
    /// Fraction of log to compact per cycle.
    pub compaction_fraction: f64,            // e.g., 0.25
    /// Maximum segment count before forced compaction.
    pub max_segments: Option<u32>,           // e.g., 16
    /// Check interval.
    pub check_interval: Duration,            // e.g., 1s
}
```

This keeps the core library clean (like C# FASTER) while providing
operational automation (like Garnet/Tsavorite).

### Long-Term: Checkpoint-Aware Truncation

Once checkpointing is implemented:
1. Store `begin_address` in checkpoint metadata (like C++/C#)
2. Only truncate segments that are below the checkpointed `begin_address`
3. After checkpoint commit, call `ShiftBeginAddress(checkpoint.begin, truncate: true)`
4. Recovery resets `begin_address` to the checkpoint value

### What NOT to Do

- **Don't make truncation automatic without compaction in lossless mode.**
  This would silently lose data — the defining property of lossless mode is
  that all records are accessible until explicitly compacted.
- **Don't embed complex policies in the engine.** Follow Tsavorite's lesson:
  keep the engine as a library with explicit APIs; put policies in the
  application/server layer.
- **Don't skip hash index cleanup.** Both C++ (`GarbageCollectSetup`) and C#
  (epoch-protected `ShiftBeginAddress`) clean up stale index entries. Our
  lossy path does this (`invalidate_entries_in_range`). Compaction does this
  (pointer swing phase). Any truncation path must do this too.

---

## Fastest Path to "Run Forever Without OOM"

| Step | Effort | Impact |
|------|--------|--------|
| 1. Set `auto_compact: true` in `stress_disk.rs` | 1 line | Enables existing compaction pipeline |
| 2. Add default `CompactionPolicy` (ratio-based trigger) | ~50 lines | `maybe_compact()` becomes operational |
| 3. Verify compaction calls `device.truncate_until()` | Already done | `begin_address.rs:120` does this |
| 4. Add segment count monitoring to stress test reporter | ~20 lines | Visibility into disk usage |
| **Total** | **~70 lines** | **Stress test runs indefinitely** |

The compaction pipeline already works. The segment deletion code already
works. The only missing piece is a policy that says "compact now" — and
the `maybe_compact()` hook is already wired into maintenance.

---

## Key File References

| Component | File | Lines |
|-----------|------|-------|
| **Rust maintenance loop** | `rust/crates/faster-core/src/store/kv.rs` | 1690-1769 |
| **Rust compaction entry** | `rust/crates/faster-core/src/store/kv.rs` | 1559-1620 |
| **Rust auto-compact hook** | `rust/crates/faster-core/src/store/kv.rs` | 1834-1847 |
| **Rust segment deletion** | `rust/crates/faster-core/src/sync_file_device.rs` | 599-616, 163-171 |
| **Rust begin-address advance** | `rust/crates/faster-core/src/compaction/begin_address.rs` | 79-120 |
| **C++ auto-compaction** | `cc/src/core/faster.h` | 4459-4502 |
| **C++ ShiftBeginAddress** | `cc/src/core/faster.h` | 3491-3509 |
| **C++ segment deletion** | `cc/src/device/file_system_disk.h` | 389-450 |
| **C++ memory budget** | `cc/src/core/config.h` | 48-57 |
| **C# ShiftBeginAddress** | `cs/src/core/Allocator/AllocatorBase.cs` | 1517-1567 |
| **C# CompactLog** | `cs/src/core/Compaction/FASTERCompaction.cs` | 35-149 |
| **C# LogAccessor API** | `cs/src/core/Index/FASTER/LogAccessor.cs` | 32-374 |
| **C# segment deletion** | `cs/src/core/Device/StorageDeviceBase.cs` | 218-256 |
| **C# checkpoint truncation** | `cs/src/core/Index/FASTER/Checkpoint.cs` | 47-69 |

---

## Decision

**Approach:** Enable auto-compaction with a ratio-based policy (follow C++
model for the quick fix, evolve toward Tsavorite's server-orchestration model).

**Not needed for MVP:** Checkpoint-aware truncation, multi-threaded compaction,
hot/cold tiering. These are post-checkpoint features.

**Owner:** Whoever picks up the stress-test-OOM fix.  
**Estimated effort:** 70 lines of Rust code. The hard engineering is already done.
