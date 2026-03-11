# Log Prefix Coupling Fix

**Date:** 2026-03-11
**Author:** Sam (Systems & Storage Expert)
**Status:** Implemented

## Problem

The `SyncFileDevice` accepts a configurable filename prefix (passed to `new()`), but the recovery code in `log_recovery.rs` hardcoded `"log."` as the segment filename format. If someone created a `SyncFileDevice` with a different prefix (e.g., `"data."`), recovery would silently fail because it looked for `log.0`, `log.1`, etc.

**Affected locations:**
- `rust/crates/faster-core/src/sync_file_device.rs:733` — creates with `"log."` prefix
- `rust/crates/faster-core/src/recovery/log_recovery.rs:399, 534, 612, 857` — hardcoded `"log.{seg_idx}"` references

## Solution

**Chosen approach:** Pass the prefix as a parameter to recovery functions.

Evaluated three options:
a. Store the prefix in checkpoint metadata
b. **Pass the prefix to recovery functions as a parameter (CHOSEN)**
c. Have recovery discover the prefix by scanning directory for segment files

Option (b) was chosen as the simplest correct approach. It:
- Makes recovery explicit and predictable
- Avoids persisting device configuration in checkpoints (layering violation)
- Avoids filesystem scanning (fragile, slow, can match wrong files)
- Keeps the recovery API clean and stateless

## Implementation

1. Added `log_prefix: &str` parameter to:
   - `LogRecoveryEngine::recover_fold_over()`
   - `LogRecoveryEngine::recover_snapshot()`
   - Internal validation functions: `validate_log_file()`, `validate_log_file_for_head()`, `validate_page_checksums()`

2. Replaced all hardcoded `format!("log.{seg_idx}")` with `format!("{log_prefix}{seg_idx}")`

3. Updated `FasterKv::recover()` in `kv.rs` to pass `"log."` as the default prefix

## Testing

- Library builds successfully
- All 1719 existing tests pass
- Custom prefix support verified manually (device with "data." prefix can be recovered)

## Decision Rationale

The parameterized approach is preferred over metadata storage because:
- The prefix is a **device configuration**, not checkpoint state
- Recovery should not depend on runtime device configuration
- The caller (FasterKv) already knows what device it created, so passing the prefix is natural
- Future extensibility: allows different checkpoints to use different devices/prefixes

## Related Work

- **poll_completions:** No work needed. Device trait already has default implementation (returns 0) added during the deadlock fix (commit `75cba9ea`). All Device implementations either use the default or override it.

