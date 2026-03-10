# Multi-Writer Flush Pipeline Deadlock

**Filed by:** Legolas (Performance Guru)
**Date:** 2026-03-10
**Severity:** Critical — makes multi-writer workloads unusable
**Affects:** `faster-core` hybrid log

## Problem

With 2+ concurrent writers doing upserts (linear/all-new-key distribution), the system permanently stalls after filling the circular page buffer. Single writer sustains 218K ops/s (852 MB/s). Two writers hit zero throughput after an initial burst of ~1M writes.

## Root Cause (3-Point Deadlock)

1. **SF-10 buffer overflow check** (`hybrid_log/log_allocator.rs:313-320`): When `next_page - head_page >= buffer_size`, all allocations return `None` → writers stall.

2. **Contiguous head advancement** (`hybrid_log/eviction.rs:171`): `advance_head()` scans pages from head forward and stops at the FIRST non-Flushed page. Cannot skip over pages in Sealed or Flushing state.

3. **Device back-pressure breaks flush loop** (`hybrid_log/flush.rs:379`): When `write_async()` returns `QueueFull`, the page is reverted to Sealed (line 275) and the flush loop breaks early. Remaining Sealed pages are never retried in that maintenance cycle.

When all three conditions align: buffer fills → maintenance hits QueueFull → some pages stuck in Sealed → head can't advance past them → SF-10 remains true → permanent stall.

## Proposed Fixes (needs team discussion)

**Option A (minimal):** Don't break the flush loop on QueueFull — retry remaining pages on next maintenance call. Change `flush.rs:379` to continue rather than break.

**Option B (robust):** Allow head to advance past Sealed pages that have been stuck beyond a timeout. Add a "stuck page" detector in eviction.

**Option C (structural):** Decouple the SF-10 check from head advancement. Use a "reclaimable pages" counter instead of `tail - head >= buffer`.

## Impact

- All multi-writer workloads with high insert rates are affected
- Single-writer workloads are NOT affected (flush pipeline keeps up)
- Zipf distribution with updates to existing keys may be less affected (fewer new allocations)

## Who Should Own This

Sam (Memory Architect) for the allocator/eviction pipeline, with input from Aragorn (Rust specialist) on the lock-free state machine.
