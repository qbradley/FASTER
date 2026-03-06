# Decision: No thread::sleep in Test Code

**Author:** Rex (QA Engineer)
**Date:** 2026-03-06
**Task:** AI-7 — Audit thread::sleep in Tests
**Status:** Convention confirmed

## Context

The retrospective noted that `write_pending_completion` tests originally took 18.5s due to sleep-based synchronization and were sped up 20× to 0.9s. Convention established: no `thread::sleep` in tests unless explicitly justified.

## Audit Results

### Test & Benchmark Code: ✅ Clean
Zero `thread::sleep` occurrences in any test or benchmark file under `crates/faster-core/`.

### Production Code: 2 Justified Sites

| File | Line | Duration | Context | Verdict |
|------|------|----------|---------|---------|
| `src/checkpoint/orchestrator.rs` | 282 | 1ms | Poll loop waiting for flush I/O completion, bounded by `max_wait_flush` deadline | **Justified** — I/O polling with timeout |
| `src/store/kv.rs` | 263 | 1ms | Poll loop in `Drop` impl waiting for in-flight I/O before device close | **Justified** — cleanup path, no signal available |

## Convention

**For test code:** Use `Barrier`, `Condvar`, `mpsc::channel`, or `notify`-style primitives instead of `thread::sleep`. Sleep-based synchronization in tests is prohibited unless a comment documents the specific justification.

**For production code:** Short (≤1ms) bounded poll loops are acceptable when the underlying API provides no callback or notification mechanism. Always pair with a deadline/timeout to prevent unbounded waits.
