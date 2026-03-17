# Decision: Disk-Backed Forever Stress Test

**Author:** Sam (Systems & Storage Expert)  
**Date:** 2026-03-16  
**Status:** Implemented  
**Commit:** `103a887a` on `rust` branch

## Context

The existing `stress_5min.rs` uses InMemoryDevice and OOMs after ~8 minutes due to unbounded memory growth. qbradley requested a production-realistic stress test that exercises real disk I/O and can run indefinitely.

## Decision

Created `stress_disk.rs` as a new example in faster-core using SyncFileDevice with a bounded working set pattern:

- **SyncFileDevice** on local disk instead of InMemoryDevice — exercises the full flush/evict/compaction pipeline with real I/O
- **Lossless mode** (`lossy: false`) — no lossy eviction, tests the production compaction path
- **Per-thread VecDeque** tracks live keys; oldest key is deleted before inserting new one when at capacity — naturally caps working set at `max_live_keys`
- **Dedicated maintenance thread** runs every 5ms independent of workers
- **libc SIGINT handler** for graceful Ctrl+C shutdown (added `libc` as dev-dependency)
- **process::exit(0)** on shutdown to avoid pre-existing SyncFileDevice Drop segfault (same workaround as page-cache sample)

## Key Design Choices

1. **Per-thread key partitioning** (not shared atomic counter) — eliminates cross-thread contention on key generation and allows per-thread live-key tracking without locks
2. **SimpleFunctions<u64, u64>** — 8-byte values are sufficient to exercise the I/O pipeline; value size doesn't meaningfully change the compaction behavior being tested
3. **buffer_size_pages=16, max_in_memory_pages=8** — keeps ~256 MB in memory, forces continuous flushing to disk
4. **Added `libc` as dev-dependency** — only used by examples, zero impact on library consumers

## Validation

- 30s smoke test: 20M+ ops/sec, live keys exactly at cap, RSS 200-370 MB, no errors
- `cargo clippy -p faster-core -- -D warnings` clean
- `cargo build --release -p faster-core --example stress_disk` clean
