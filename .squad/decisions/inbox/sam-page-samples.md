# Decision: Disk-I/O Sample Buffer Sizing

**Author:** Sam (Systems & Storage Expert)
**Date:** 2026-03-09
**Branch:** `sam/page-cache-store`
**Status:** Implemented

## Context

Created `page-cache` and `page-store` sample crates that sustain writes beyond the in-memory buffer using FASTER's hybrid log eviction pipeline.

## Key Finding

FASTER's flush pipeline is asynchronous: `Sealed → Flushing → Flushed → Evicted`. The evictor can only reclaim `Flushed` pages (I/O completed). For sustained writes, the circular buffer needs **4× the target in-memory pages** as headroom — otherwise the tail laps the head while waiting for I/O callbacks, and all allocations abort.

Additionally, a **dedicated maintenance thread** (5ms interval) is required to pump the flush/evict cycle. Writer-driven maintenance-on-retry is insufficient because each maintenance call may only queue flushes without completing them.

## Team Impact

- **Anyone configuring FasterKv for disk-heavy workloads:** `EvictionPolicy::default()` (max_in_memory_pages=256) never triggers eviction for small buffers. Set `max_in_memory_pages` to `buffer_size_pages / 2` and allocate `buffer_size_pages = target * 4`.
- **Benchmarks:** `disk-io-bench` uses default eviction and works because it doesn't sustain writes beyond buffer. These new samples do.
- **Documentation:** Should document the buffer sizing formula somewhere in QUICKSTART.md or a tuning guide.
