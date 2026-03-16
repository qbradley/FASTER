# Decision: Throughput Cliff Detection in Deadlock Tests

**Author:** Aragorn (Rust Expert)
**Date:** 2026-07-22
**Status:** Implemented
**Commit:** 8e0ec6f4

## Summary

Added `ThroughputMonitor` helper to deadlock regression tests that detects "slow deadlocks" — situations where the system makes just enough progress to avoid timeout-based watchdogs but throughput has collapsed by 90%+ from peak.

## Motivation

Post-mortem on two escaped deadlocks found that existing tests checked "did it complete?" (correctness) but not "did throughput collapse?" (liveness). Both bugs manifested as throughput cliffs: ~14 ops/s instead of ~1M ops/s. The 5-second stall watchdog never fired because the system was technically making progress.

## Design Choices

| Decision | Rationale |
|----------|-----------|
| `AtomicU64` counter with `Relaxed` ordering | One atomic increment per op — negligible overhead at 40M ops/s |
| Window-based sampling (not per-op timestamps) | Per-op timestamps would be ~100ns/op overhead; window snapshots are free |
| Reuse existing watchdog/monitor threads | No extra thread spawns — `tick()` is called from existing monitoring loops |
| 10% of peak as cliff threshold | Catches catastrophic collapse while tolerating normal throughput variation |
| 2-window warmup period | First windows have JIT/setup overhead — skip them for cliff detection |
| `with_counter()` constructor for shared counters | Tests already have `total_ops: Arc<AtomicU64>` — reuse rather than double-count |

## Team Impact

- **Boromir (Testing):** ThroughputMonitor is available as a shared helper for any future stress tests.
- **All:** The pattern can be extracted to a test utility crate if needed beyond deadlock_tests.rs.

## Files Changed

- `rust/crates/faster-core/tests/deadlock_tests.rs` — ThroughputMonitor helper + integration into 3 tests
