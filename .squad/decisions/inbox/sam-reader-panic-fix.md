# Decision: Graceful NotFound on Evicted Pages (Reader Panic Fix)

**Author:** Sam  
**Date:** 2026-03-10  
**Branch:** `sam/reader-panic-fix`  
**Status:** Implemented  

## Context

In lossy mode (`FasterKvConfig::lossy = true`), a separate maintenance thread
calls `evict_and_truncate()` which advances `begin_address` and frees page
frames. A concurrent reader can hold a stale `AddressInfo` snapshot from before
the eviction. The timeline:

1. Reader takes snapshot → classifies address as in-memory
2. `find_record_for_key` walks chain → finds matching record via `read_header_and_match_key`
3. **Maintenance thread evicts the page** (between steps 2 and 4)
4. Reader calls `get_record()` → returns `None` (page frame freed)
5. `.expect()` **panics**

This was triggered by the page-cache sample: 1 writer + 2 readers + lossy mode
+ linear distribution.

## Decision

Replace all three `.expect()` calls in the read/rmw/delete hot paths with
graceful `None` handling:

| Operation | Path | Old Behavior | New Behavior |
|-----------|------|-------------|-------------|
| `internal_read` | Mutable/FuzzyRegion/ReadOnly | Panic | `NotFound` |
| `internal_rmw` | FuzzyRegion/ReadOnly | Panic | `Aborted` (caller retries) |
| `internal_delete` | FuzzyRegion/ReadOnly | Panic | `NotFound` |

The Mutable paths for RMW and Delete already handled `get_physical_address`
returning `None` by returning `Aborted`/`NotFound` — only the three read-value
sites used `.expect()`.

## Rationale

- **Readers must never panic.** In a lossy cache, data loss is expected. The
  correct response is `NotFound`, not a crash.
- **Aborted for RMW** is correct because the caller's retry loop will either
  find the key again or create a new record via `initial_updater`.
- **No performance impact.** The fix replaces a branch-on-`None` + `expect()`
  with a branch-on-`None` + `return`. Same cost.
- The misleading doc comment on `find_record_for_key` claiming "epoch
  protection guarantees pages won't be evicted" has been corrected.

## Verification

- 1631 tests pass (0 regressions)
- 238M reads over 60s with the exact reproduction command — zero panics
- Three new regression tests exercise the race for read, rmw, and delete paths
