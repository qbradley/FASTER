# Decision: Deadlock Test Harness — Device Wrapper Strategy

**Author:** Boromir (QA Engineer)  
**Date:** 2026-03-11  
**Status:** Implemented  
**Branch:** `sam/deadlock-fix`

## Context

The multi-writer deadlock fix (Aragorn's design doc) introduces new behavior
in `flush_sealed_pages` (Fix A: batch abort on QueueFull) and `allocate_at_tail`
(Fix B: retry loop with `poll_completions`). We need test devices that can
trigger these paths deterministically.

## Decision

Created three new test device wrappers in `deadlock_tests.rs` instead of
extending `FaultInjectingDevice`:

1. **`QueueFullDevice`** — returns `IoRequestResult::QueueFull` from
   `write_async()` directly. This is fundamentally different from
   `FaultInjectingDevice`'s callback-level error injection. QueueFull
   is a submission-level rejection (no callback fires), while
   `FaultInjectingDevice` fires the callback with `IoStatus::Error`.

2. **`QueueFullThenSucceedDevice`** — separate device for the "transient
   QueueFull" scenario (retry loop testing). Simpler than making
   `QueueFullDevice` track two phases.

3. **`SlowDevice`** — background-thread callback delay. Returns `Submitted`
   (not `CompletedSync`), matching `SyncFileDevice`'s async behavior.
   Critical for reproducing the timing-dependent deadlock.

## Why Not Extend FaultInjectingDevice?

`FaultInjectingDevice` injects faults via the callback path — it always
returns `CompletedSync` and fires the callback with `IoStatus::Error`.
QueueFull is a completely different code path: `write_async()` returns
`QueueFull` **without** firing any callback. Mixing both patterns in one
device would make the fault policy confusing and error-prone.

## Test Strategy

- **7 tests run now** (device-level correctness, no dependency on Sam's changes)
- **6 tests ignored** with descriptive reasons (`#[ignore = "requires deadlock fix: ..."]`)
- Tests will compile immediately once Sam's changes land (written against expected API)
- The multi-writer forward progress test is the definitive regression test

## Risk

The ignored tests are written against the **expected** new API (`FlushBatchResult`,
`poll_completions()`). If Sam's implementation differs from the design doc,
the tests will need adjustment. The device wrappers themselves are stable.
