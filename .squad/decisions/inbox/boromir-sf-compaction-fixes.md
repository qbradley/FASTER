# Decision: SF-3/5/8/9 Compaction Scanner Runtime Warnings & Tests

**Author:** Boromir (QA Engineer)
**Date:** 2026-03-07
**Status:** Implemented
**Commit:** 4c11175b

## Summary

Implemented 4 should-fix items from the final review synthesis of the variable-length compaction feature. These add runtime warning instrumentation and edge-case test coverage to the compaction scanner.

## Changes

### SF-3: Tombstone vector memory warning
- Warns via `log::warn!` when tombstone_records exceeds ~100 MB
- Checked every 1024 pushes (amortized cost)

### SF-5: Hash collision hop metrics  
- `is_current_version()` now returns `(bool, usize)` with hop count
- New `total_chain_hops` field on `CompactionPlan`
- Warns when cumulative hops exceed 1,000,000

### SF-8: Post-scan byte accounting validation
- Scanned bytes vs address-range span check (>5% threshold)
- Classified bytes vs total bytes exact equality check

### SF-9: Five new edge-case tests
- `record_nearly_fills_page` — record leaving <MIN_READABLE gap
- `moderate_corruption_plausible_wrong_size` — bit-flip corruption detection
- `version_chain_variable_length_different_sizes` — multi-depth chains with Vec<u8>
- `empty_key_record_size` — zero-length key handling
- `eq_from_bytes_trailing_garbage` — trailing bytes and boundary conditions

## Design Decisions

1. **`log` crate over `tracing`:** Added `log` as non-optional dependency. These are safety warnings that should always emit regardless of feature flags. `tracing` bridges to `log` so no conflict.

2. **`(bool, usize)` return type:** Breaking internal API change to `is_current_version()`. All call sites updated. Conservative — returns 0 hops for fast-path and early-exit cases.

3. **5% threshold for scan-vs-range check:** Generous because page-end gaps cause legitimate divergence. Exact equality for classified-bytes check.

## Risks

- `log::warn!` in hot scan path — negligible cost when no log subscriber is active (log facade compiles to no-op).
- Extra `usize` fields on `CompactionPlan` — trivial memory overhead. `#[non_exhaustive]` makes this non-breaking.
