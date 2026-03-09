# Decision: Checkpoint-Based Testing Pattern for Compaction & Hybrid Log

**Author:** Boromir (QA)
**Date:** 2025-06-24
**Status:** Proposed

## Context

Testing the compaction pipeline and hybrid log management requires data in the read-only region. With 32MB pages and small test records (~24 bytes), natural region advancement via `maintenance()` is impractical — you'd need ~1.4M records to fill a single page.

## Decision

Use `checkpoint(tempdir, CheckpointType::FoldOver)` as the canonical method to force data into the read-only region for integration tests.

### Why This Works

`checkpoint()` internally calls `shift_read_only_to_tail()` + synchronous flush, but does NOT evict pages from memory. Pages end up in `Flushed` state — still readable by the compaction scanner.

This is superior to alternatives:
- `flush_and_evict()` evicts pages, making them unreadable by the scanner
- `maintenance()` requires filling entire 32MB pages before it shifts RO
- Directly calling `shift_read_only_to_tail()` is impossible — `allocator` is `pub(crate)`

### Helper Pattern

```rust
fn force_read_only_u64(store: &FasterKv<U64Key, U64Value, InMemoryDevice>) {
    let dir = tempfile::tempdir().unwrap();
    store.checkpoint(dir.path(), CheckpointType::FoldOver).unwrap();
}
```

## Impact

- Enables realistic compaction testing (K1→K4 pipeline) with small record counts
- Enables hybrid log region boundary testing without massive datasets
- Pattern should be reused by any future tests that need read-only region data

## Risks

- Depends on `checkpoint()` internal behavior (shift RO + no eviction) — if that changes, tests break
- Creates temp directories (cleaned up automatically by `tempfile`)
