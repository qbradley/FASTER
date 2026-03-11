# Review: Multi-Writer Deadlock Fix

**Reviewer:** Aragorn
**Branch:** sam/deadlock-fix
**Date:** 2026-07-21

## Verdict: APPROVE WITH COMMENTS

The implementation provides a robust fix for the multi-writer deadlock by addressing the root cause (timing gap between flush submission and callback completion) and adding necessary back-pressure handling.

### Assessment

1.  **Fix A (FlushBatchResult):** Correct. `flush_sealed_pages` now returns `FlushBatchResult`, allowing `maintenance` to detect when the device is saturated.
2.  **Fix B (poll_completions + retry loop):** Correct.
    *   `Device::poll_completions` allows the store to cooperatively yield to I/O threads.
    *   `allocate_at_tail` now retries up to 32 times with yields, replacing the single-retry logic.
3.  **Fix C (yield under pressure):** Correct. `maintenance` yields when the queue is full or buffer pressure is high.
4.  **Thread Safety:** Safe. `yield_now()` is safe. `FlushBatchResult` is POD.
5.  **Test Quality:** High quality, BUT **all new tests are ignored**.

### Issues Found

1.  **[Major] Tests are Ignored:** The regression tests in `deadlock_tests.rs` are marked `#[ignore]`. They pass when run manually (verified), but they must be enabled to prevent future regressions.
2.  **[Minor] UringDevice Consistency:** `UringDevice` uses the default `poll_completions` (returns 0). While `maintenance` explicitly yields, implementing `poll_completions` to yield (like `SyncFileDevice`) or actually drain the completion queue would be more consistent with the trait intent.

### Recommendation

Merge the changes after enabling the tests in `deadlock_tests.rs`.
