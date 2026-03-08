# Decision: EPVS (Epoch-Protected Version Scheme) Architecture

**Author:** Gandalf  
**Date:** 2026-03-06  
**Task:** W2-03  
**Status:** Proposed  

## Key Architectural Decisions

### D-EPVS-1: Pack (phase: u8, version: u56) into AtomicU64

**Decision:** System state is a single 64-bit atomic word with phase in bits 56–63 and version in bits 0–55. All state machine transitions are single CAS operations.

**Rationale:** Eliminates torn reads between phase and version. Matches C# FASTER's SystemState layout exactly. Reduces atomic operation count per checkpoint cycle.

**Impact:** Replaces `CheckpointStateMachine::phase (AtomicU8)` and `HashIndex::version (AtomicU32)` with a shared `AtomicSystemState`.

### D-EPVS-2: Unified Phase Enum

**Decision:** Merge `CheckpointPhase` and `GrowPhase` into a single `Phase` enum. Checkpoint phases: 0–7. Grow phases: 8–15. Bit 7 (0x80) of the phase byte is the INTERMEDIATE marker.

**Rationale:** A single state word can only hold one phase, so grow and checkpoint share the same word. This enforces mutual exclusion (only one active at a time), matching C# behavior.

**Impact:** Grow and checkpoint cannot run concurrently. This is the correct design for now; P-09 (composable tasks) can relax this later.

### D-EPVS-3: Two-Phase Intermediate Transition Protocol

**Decision:** Every state transition uses `current → intermediate → next` with two CAS operations. The intermediate state (phase bit 7 set) blocks other threads from observing inconsistent mid-transition state.

**Rationale:** Matches C# FASTER's `MakeTransition` pattern. Provides a safe window for before-entering hooks (epoch bumps, metadata capture) without races.

### D-EPVS-4: Checkpoint Metadata Format Version

**Decision:** Add `format_version` field to `IndexRecoveryInfo` and `LogRecoveryInfo`. New format (version 2) stores version as `u64`. Recovery code detects legacy format (no `format_version`) and widens `u32 → u64`.

**Rationale:** Soft migration — no hard break. New code reads old checkpoints. Old code cannot read new checkpoints (acceptable — no downgrade support).

### D-EPVS-5: RecordInfo Unchanged

**Decision:** The 13-bit per-record version field in RecordInfo is NOT changed in the EPVS PR. It will be addressed in P-22 / W2-04 after EPVS stabilizes.

**Rationale:** Minimizes blast radius. RecordInfo changes affect every record in every log page and every operation path. Decoupling keeps each PR reviewable and testable.

## Binding For

- **Sam:** Implement per this design. Module structure: `src/state/{mod,phase,system_state}.rs`.
- **All:** The unified `Phase` enum replaces both `CheckpointPhase` and `GrowPhase`.
- **Frodo:** W2-04 (RecordInfo) depends on EPVS landing first.
