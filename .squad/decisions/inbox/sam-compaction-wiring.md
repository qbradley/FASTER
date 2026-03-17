# Decision: Wire Log Compaction into maintenance() for Bounded Disk Operation

**Author:** Sam (Systems & Storage)  
**Date:** 2026-03-17  
**Status:** Implemented  
**Commit:** `762bc3bd`  
**Branch:** `rust`

---

## Problem

`stress_disk` OOMs after ~280s because:
1. `maintenance()` never called `maybe_compact()` — compaction pipeline was fully built but never triggered
2. `begin_address` never advanced in lossless mode — on-disk log grew without bound
3. RSS exploded as I/O traffic against 48 GB of accumulated segments overwhelmed memory

## What Was Done

### 1. Added `LogSizeBudgetPolicy` (`compaction/policy.rs`)

New compaction policy that triggers when `total_log_bytes > budget_bytes`. This bypasses the known limitation in `collect_stats()` which sets `live_data_bytes = total_log_bytes` (making `SpaceAmplificationPolicy` always return ratio 1.0 and never fire).

Follows C++ FASTER's `hlog_size_budget` model: trigger compaction based on raw log size, not live-data ratio.

### 2. Wired `maybe_compact()` into `maintenance()` (`store/kv.rs`)

Added step 5 at the end of `maintenance()`:
```rust
if self.config.auto_compact {
    self.maybe_compact();
}
```

This was the critical missing link. The doc comment on `maintenance()` already described step 4 (compaction check) but the code never implemented it. Now the code matches the docs.

Placement rationale: after flush + evict + poll, so buffer space is available for compaction's copy phase.

### 3. Updated `stress_disk.rs` to enable compaction

- `auto_compact: true` (was `false`)
- Budget: `4 × buffer_size_pages × (1 << OFFSET_BITS)` = 2 GB with default 16 × 32 MB config
- Imports `LogSizeBudgetPolicy` from `faster_core::compaction::policy`

## Truncation Chain Verification

The end-to-end chain works:
1. `maintenance()` → `maybe_compact()` → policy check → `compact()`
2. `compact()` → `CompactionOrchestrator::run()` → scan → copy → pointer swing → epoch drain
3. → `BeginAddressAdvancer::advance(until)` → `try_advance_begin()` + `device.truncate_until()`
4. `SyncFileDevice::truncate_until()` → deletes segment files below threshold

## Safety

- Compaction only advances `begin_address` to the compacted region's end (`until = safe_read_only_address`)
- Pointer swing ensures all hash entries point to new (tail) copies before truncation
- Epoch drain ensures no reader holds stale old-address references
- Compaction is serialized by `compaction_lock` mutex — no concurrent compaction races

## What This Does NOT Do

- Does **not** fix `collect_stats()` to provide accurate live-data estimates (would need a full log scan; deferred)
- Does **not** add checkpoint-aware truncation (no checkpoint impl yet)
- Does **not** add multi-threaded compaction (single-threaded is sufficient for now)
- Does **not** change lossy mode behavior (lossy already truncates inline in maintenance)

## Impact

- `stress_disk` should now run indefinitely with bounded disk usage and RSS
- Any user of `FasterKv` with `auto_compact: true` + a policy now gets automatic compaction through `maintenance()`
- 1726 tests pass, clippy clean, all doc tests pass

## Files Changed

| File | Change |
|------|--------|
| `rust/crates/faster-core/src/compaction/policy.rs` | Added `LogSizeBudgetPolicy` + 4 unit tests + doc test |
| `rust/crates/faster-core/src/store/kv.rs` | Added `maybe_compact()` call in `maintenance()` step 5 |
| `rust/crates/faster-core/examples/stress_disk.rs` | Enabled auto_compact, set LogSizeBudgetPolicy |
