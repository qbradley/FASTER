# Decision: EPVS Implementation Choices

**Author:** Sam (Systems Programming Expert)
**Date:** $(date +%Y-%m-%d)
**Context:** Iteration 6 Action Item A1 — EPVS Implementation

## Decisions Made

### 1. Phase Numbering Scheme
Checkpoint phases use values 0–5, grow phases use 8–10, matching the design doc.
`PHASE_COUNT` increased from 8 to 16 to accommodate all phases in epoch arrays.

### 2. Version Bump Location
Version increments atomically on Prepare→InProgress transition only.
No version bump for grow phases. This exactly matches C# Tsavorite behavior.

### 3. Checkpoint Metadata Backward Compatibility
Added `format_version` field (u64) with `#[serde(default)]` so existing JSON
metadata without this field deserializes as `FORMAT_VERSION_LEGACY` (1).
The version field widened from u32→u64 but recovery code casts back to u32
where needed for `set_version()` until HashIndex is updated.

### 4. Grow State Machine Deferred
The grow state machine still uses its own `AtomicU8`. Wiring it to the shared
`AtomicSystemState` is deferred to a future PR (depends on RecordInfo bits
and HashIndex version field removal).

### 5. Two-Phase CAS Protocol
`AtomicSystemState::try_transition()` uses intermediate states (bit 63 set)
to prevent ABA problems. Observers seeing an intermediate state know a
transition is in progress and can spin-wait.

## Open Questions
- Should `HashIndex::version` (AtomicU32) be removed in the same PR or deferred?
  → Deferred: removing it requires wiring grow to shared state first.
- Should `RecordInfo` sealed-bit changes land before or after grow wiring?
  → Per design doc, RecordInfo changes are separate (P-22 / W2-04).
