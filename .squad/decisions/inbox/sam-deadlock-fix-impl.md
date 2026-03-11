# Decision: Multi-Writer Deadlock Fix — Implementation

**Author:** Sam (Systems & Storage Expert)  
**Date:** 2026-03-11  
**Status:** Implemented  
**Branch:** `sam/deadlock-fix`  
**Commits:** `5b50d994` (Fix A), `a35b7846` (Fix B), `75cba9ea` (Fix C)

## Summary

Implemented three coordinated fixes to close the multi-writer flush pipeline deadlock, following Aragorn's design.

## Changes

### Fix A: Flush Batch Abort on QueueFull
- New `FlushBatchResult { flushed: u32, queue_full: bool }` struct
- `flush_sealed_pages` breaks on QueueFull (was: `continue`)
- `maintenance()` now captures the `queue_full` flag (was: `let _ =`)

### Fix B: Writer-Side I/O Draining + Retry Loop
- `Device::poll_completions(&self) -> u32` with default returning 0
- `SyncFileDevice::poll_completions()` calls `thread::yield_now()`
- `allocate_at_tail` bounded retry loop (32 iterations) with yield between attempts

### Fix C: Yield Under Buffer Pressure
- `maintenance()` calls `poll_completions()` after every flush batch
- Under `queue_full` or `buffer_pressure`, yields + polls again before returning

## Verification

- **1728 tests pass** (cargo nextest, no regressions)
- **6 previously-ignored deadlock tests now pass**, including:
  - `multi_writer_forward_progress` (3 writers, SlowDevice, 10s runtime)
  - `flush_batch_aborts_on_queue_full`
  - `retry_loop_eventually_succeeds_after_queue_full_clears`
  - `poll_completions_default_returns_zero`
- Clippy clean (lib)

## Backward Compatibility

- `Device` trait: `poll_completions()` has default impl → existing custom devices don't break
- `flush_sealed_pages` return type changed from `Result<u32, FlushError>` to `Result<FlushBatchResult, FlushError>` → internal API, only 2 callers (both updated)
- `allocate_at_tail` behavior: now retries up to 32× instead of 1× → strictly more resilient, same Aborted outcome on permanent failure

## Open Items

- The `#[ignore]` annotations on the 6 deadlock tests can now be removed
- `SyncFileDevice::max_outstanding` remains dead code (not part of this fix scope)
- UringDevice (if added later) should implement `poll_completions()` to drain its completion queue
