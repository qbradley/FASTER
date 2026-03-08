# EPVS Architecture Design — Epoch-Protected Version Scheme

**Author:** Gandalf (Lead / System Architect)  
**Task:** W2-03  
**Proposal:** P-02  
**Date:** 2026-03-06  
**Status:** Design — Ready for Implementation  
**Implementer:** Sam

---

## 1. Current State Analysis

### 1.1 How Phase and Version Are Tracked Today

The Rust FASTER implementation stores checkpoint state across **two separate atomic fields in different structs**:

| Field | Location | Type | Purpose |
|-------|----------|------|---------|
| `phase` | `CheckpointStateMachine` (`checkpoint/state_machine.rs:119`) | `AtomicU8` | Current checkpoint phase (Rest through Completed) |
| `version` | `HashIndex` (`hash/index.rs:108`) | `AtomicU32` | Hash table version counter (toggled on grow, set during checkpoint) |

Additionally, **per-record version tracking** exists:

| Field | Location | Type | Bits |
|-------|----------|------|------|
| RecordInfo version | `record/record_info.rs:48-49` | 13-bit field in `u64` | Bits 48–60 |

### 1.2 Phase Values

**CheckpointPhase** (`checkpoint/state_machine.rs:33-46`):
```
Rest = 0, Prepare = 1, InProgress = 2, WaitFlush = 3, WaitCompletion = 4, Completed = 5
```

**GrowPhase** (`grow/state_machine.rs:67-78`):
```
Rest = 0, Prepare = 1, InProgress = 2, WaitCompletion = 3, Completed = 4
```

Both state machines currently have **independent** phase fields and CAS operations.

### 1.3 Race Conditions

The fundamental problem: **phase and version are not atomically coupled.**

**Race Window 1 — Checkpoint writes version unsafely:**
In `checkpoint/orchestrator.rs:235`, the checkpoint reads `index.version()` via `AtomicU32::load(Ordering::Acquire)`. This read is **not synchronized** with the phase transition that initiated the checkpoint. A concurrent grow operation could change `index.version` between the phase transition to `InProgress` and the version read.

**Race Window 2 — Observer torn reads:**
Any thread that reads both phase and version (e.g., to decide whether a record belongs to the current or previous checkpoint version) performs two separate atomic loads. Between those loads, another thread could advance the state machine, causing the observer to see `(old_phase, new_version)` or `(new_phase, old_version)`.

**Race Window 3 — Grow vs. Checkpoint interference:**
The grow state machine and checkpoint state machine are entirely independent. Both can be active simultaneously with no coordination on the version field they share.

### 1.4 Current Atomic Operation Count Per Checkpoint

A full checkpoint cycle requires these separate atomic operations on state:

| Transition | Operation | Ordering |
|-----------|-----------|----------|
| Rest → Prepare | `compare_exchange` on `phase` | AcqRel/Acquire |
| Prepare → InProgress | `compare_exchange` on `phase` | AcqRel/Acquire |
| (version bump) | `store` on `version` | Release |
| InProgress → WaitFlush | `compare_exchange` on `phase` | AcqRel/Acquire |
| WaitFlush → WaitCompletion | `compare_exchange` on `phase` | AcqRel/Acquire |
| WaitCompletion → Completed | `compare_exchange` on `phase` | AcqRel/Acquire |
| Completed → Rest | `compare_exchange` on `phase` | AcqRel/Acquire |

That's **6 CAS operations on phase** plus **1 separate store on version** — and the version store is not atomic with any phase CAS. This is the window that EPVS closes.

---

## 2. EPVS Bit Layout

### 2.1 Proposed Layout

Pack `(phase, version)` into a single `AtomicU64`:

```
Bit 63                                                    Bit 0
┌────────────────┬────────────────────────────────────────────────┐
│  Phase (8 bits)│              Version (56 bits)                 │
│  bits 56–63    │              bits 0–55                         │
└────────────────┴────────────────────────────────────────────────┘

Masks:
  PHASE_MASK   = 0xFF00_0000_0000_0000  (bits 56–63)
  VERSION_MASK = 0x00FF_FFFF_FFFF_FFFF  (bits 0–55)
  PHASE_SHIFT  = 56
```

This matches the C# SystemState layout exactly (`cs/src/core/Index/Synchronization/StateTransitions.cs:65-90`) and the Tsavorite VersionSchemeState (`cs/src/core/Epochs/EpochProtectedVersionScheme.cs:15-151`).

### 2.2 Phase — 8 Bits (Sufficient)

All current phase values fit easily:

| Phase Value | Name | Context |
|-------------|------|---------|
| 0 | REST | Idle state |
| 1 | PREPARE | Checkpoint/Grow preparation |
| 2 | IN_PROGRESS | Active operation |
| 3 | WAIT_FLUSH | Checkpoint: log flush |
| 4 | WAIT_COMPLETION | Waiting for session acknowledgment |
| 5 | COMPLETED | Terminal (before reset) |
| 6 | PREP_INDEX_CHECKPOINT | (Future: C# has this) |
| 7 | WAIT_INDEX_CHECKPOINT | (Future: C# has this) |
| 8 | PERSISTENCE_CALLBACK | (Future: C# has this) |
| 9 | PREPARE_GROW | (Future: separate grow phases) |
| 10 | IN_PROGRESS_GROW | (Future: separate grow phases) |
| 128 (0x80) | INTERMEDIATE | Bit 7 set = transitioning |

C# uses 10 distinct phases + the INTERMEDIATE marker (bit 7). With 8 bits we have room for **127 normal phases** plus the intermediate bit. This is vastly more than sufficient.

**Design decision:** Reserve bit 7 (0x80) of the phase byte as the **INTERMEDIATE marker**. When set, the state machine is mid-transition. Threads must spin-wait until the intermediate bit clears. This matches the C# pattern exactly.

### 2.3 Version — 56 Bits (Sufficient)

56 bits provides `2^56 = 72,057,594,037,927,936` versions.

At 1 checkpoint per second, this overflows in **~2.28 billion years**. At 1,000 checkpoints per second (absurdly aggressive), overflow in ~2.28 million years. 56 bits is more than sufficient for any conceivable deployment.

For comparison: the current Rust implementation uses `AtomicU32` (4 billion versions) and the per-record version field is only 13 bits (8,191 versions). EPVS's 56 bits is a strict upgrade.

### 2.4 Reserved Bits — None Needed

All 64 bits are allocated (8 phase + 56 version). No reserved bits are needed because:
- 8 phase bits provide ample room for future phase values (127 phases + intermediate)
- 56 version bits provide effectively infinite version space
- The bit layout matches C# exactly, aiding cross-implementation consistency
- If we ever need additional state, we can shrink version to 48 bits (still 281 trillion versions) and use the freed 8 bits — but this is not foreseeable

---

## 3. Core Data Type

### 3.1 SystemState Struct

```rust
/// Packed checkpoint/version state in a single atomic word.
///
/// Layout: `[phase: u8 | version: u56]`
/// - Bits 56–63: phase (with bit 63 = intermediate marker)
/// - Bits 0–55: monotonic version counter
///
/// All state transitions are single 64-bit CAS operations,
/// eliminating torn reads between phase and version.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct SystemState(u64);

impl SystemState {
    const PHASE_SHIFT: u32 = 56;
    const PHASE_MASK: u64 = 0xFF00_0000_0000_0000;
    const VERSION_MASK: u64 = 0x00FF_FFFF_FFFF_FFFF;
    const INTERMEDIATE_BIT: u8 = 0x80; // Bit 7 of phase byte

    /// Create a new SystemState from phase and version.
    pub const fn new(phase: Phase, version: u64) -> Self {
        Self(((phase as u8 as u64) << Self::PHASE_SHIFT) | (version & Self::VERSION_MASK))
    }

    /// Extract the phase.
    pub const fn phase(self) -> Phase {
        Phase::from_u8((self.0 >> Self::PHASE_SHIFT) as u8 & !Self::INTERMEDIATE_BIT)
    }

    /// Extract the raw phase byte (including intermediate bit).
    pub const fn phase_raw(self) -> u8 {
        (self.0 >> Self::PHASE_SHIFT) as u8
    }

    /// Extract the version.
    pub const fn version(self) -> u64 {
        self.0 & Self::VERSION_MASK
    }

    /// Check if the state is in an intermediate (mid-transition) state.
    pub const fn is_intermediate(self) -> bool {
        (self.phase_raw() & Self::INTERMEDIATE_BIT) != 0
    }

    /// Create the intermediate form of this state (sets bit 7 of phase).
    pub const fn make_intermediate(self) -> Self {
        Self(self.0 | ((Self::INTERMEDIATE_BIT as u64) << Self::PHASE_SHIFT))
    }

    /// The raw u64 word (for CAS operations).
    pub const fn word(self) -> u64 {
        self.0
    }

    /// Construct from a raw u64 word.
    pub const fn from_word(word: u64) -> Self {
        Self(word)
    }
}
```

### 3.2 Unified Phase Enum

Unify `CheckpointPhase` and `GrowPhase` into a single `Phase` enum since EPVS carries one phase value at a time:

```rust
/// All phases for the FASTER state machine.
///
/// Values 0–63 are normal phases. Bit 7 (0x80) is reserved for
/// the INTERMEDIATE marker in SystemState.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(u8)]
pub enum Phase {
    // --- Quiescent ---
    Rest = 0,

    // --- Checkpoint phases ---
    Prepare = 1,
    InProgress = 2,
    WaitFlush = 3,
    WaitCompletion = 4,
    PersistenceCallback = 5,

    // --- Grow phases ---
    PrepareGrow = 8,
    InProgressGrow = 9,
    WaitCompletionGrow = 10,
}
```

**Numbering rationale:**
- Checkpoint phases: 1–7 (matching C# numbering where useful)
- Grow phases: 8–15 (separate range for clarity)
- Values 16–63: reserved for future state machine tasks (P-09 composable tasks)
- The current `Completed` phase (5 in `CheckpointPhase`, 4 in `GrowPhase`) is replaced by `PersistenceCallback` (5) which directly transitions back to `Rest`. The "completed" state was always transient — the caller that CAS'd to Completed immediately resets to Rest. Making this explicit as `PersistenceCallback → Rest` matches the C# pattern.

### 3.3 Atomic State Container

```rust
/// Thread-safe atomic container for SystemState.
///
/// All reads and transitions go through this. Replaces both
/// CheckpointStateMachine::phase (AtomicU8) and HashIndex::version (AtomicU32).
pub struct AtomicSystemState {
    word: AtomicU64,
}

impl AtomicSystemState {
    pub fn new(initial: SystemState) -> Self {
        Self {
            word: AtomicU64::new(initial.word()),
        }
    }

    /// Load the current state.
    pub fn load(&self, ordering: Ordering) -> SystemState {
        SystemState::from_word(self.word.load(ordering))
    }

    /// Attempt an atomic state transition.
    /// Returns Ok(old) on success, Err(actual) on failure.
    pub fn compare_exchange(
        &self,
        expected: SystemState,
        new: SystemState,
        success: Ordering,
        failure: Ordering,
    ) -> Result<SystemState, SystemState> {
        self.word
            .compare_exchange(expected.word(), new.word(), success, failure)
            .map(SystemState::from_word)
            .map_err(SystemState::from_word)
    }
}
```

---

## 4. State Machine Transitions

### 4.1 Two-Phase Transition Protocol

Every state machine transition uses the **intermediate state pattern** from C#:

```
Step 1: CAS(current_state → intermediate_state)     // Claim transition
Step 2: Execute before-entering hooks                // Safe: nobody else can transition
Step 3: CAS(intermediate_state → next_state)         // Publish new state (guaranteed)
```

**Why intermediate?** Without it, two threads could race to advance the state. The intermediate marker acts as a lightweight exclusive lock on state transitions. Other threads that observe the intermediate state **spin-wait** until it clears.

```rust
/// Attempt to transition from `expected` to `next`.
///
/// Uses intermediate state to serialize concurrent transition attempts.
/// Returns true if this thread won the transition.
pub fn try_transition(
    &self,
    expected: SystemState,
    next: SystemState,
    before_hooks: impl FnOnce(),
) -> bool {
    let intermediate = expected.make_intermediate();

    // Step 1: Claim the transition via CAS to intermediate.
    if self.state.compare_exchange(
        expected,
        intermediate,
        Ordering::AcqRel,
        Ordering::Acquire,
    ).is_err() {
        return false; // Another thread won
    }

    // Step 2: Execute hooks (safe — we hold the intermediate lock).
    before_hooks();

    // Step 3: Publish the new state. Guaranteed to succeed
    // because only we can transition from intermediate.
    let result = self.state.compare_exchange(
        intermediate,
        next,
        Ordering::AcqRel,
        Ordering::Acquire,
    );
    debug_assert!(result.is_ok(), "intermediate → next must succeed");

    true
}
```

### 4.2 Valid Checkpoint Phase Transitions

All transitions as single CAS operations on the unified `AtomicSystemState`:

```
Rest(v) ──CAS──→ Prepare(v)
    One thread wins; losers see CAS failure and do nothing.
    Hook: initialize checkpoint token, capture begin_address.

Prepare(v) ──CAS──→ InProgress(v+1)
    VERSION BUMP happens here atomically with phase change.
    Hook: epoch bump to ensure all threads see new version.
    This is the key improvement: old code did phase CAS + separate version store.

InProgress(v+1) ──CAS──→ WaitFlush(v+1)
    Hook: capture final_logical_address, begin flushing.

WaitFlush(v+1) ──CAS──→ WaitCompletion(v+1)
    Hook: wait for flush I/O completion.

WaitCompletion(v+1) ──CAS──→ PersistenceCallback(v+1)
    Hook: write checkpoint metadata to disk.

PersistenceCallback(v+1) ──CAS──→ Rest(v+1)
    Hook: cleanup, dispose checkpoint resources.
```

### 4.3 Valid Grow Phase Transitions

```
Rest(v) ──CAS──→ PrepareGrow(v)
    Hook: allocate new (2×) hash table.

PrepareGrow(v) ──CAS──→ InProgressGrow(v)
    Hook: begin bucket splitting. No version bump for grow.

InProgressGrow(v) ──CAS──→ WaitCompletionGrow(v)
    Hook: all chunks split, wait for session acknowledgment.

WaitCompletionGrow(v) ──CAS──→ Rest(v)
    Hook: reclaim old table via epoch drain.
```

### 4.4 How Single-Word CAS Eliminates the Race

**Before (current code):**
```
Thread A (checkpoint):                Thread B (observer):
  CAS(phase, Prepare, InProgress)       load(phase)  → InProgress  ✓
  ...                                   load(version) → OLD!       ✗ TORN READ
  store(version, v+1)
```

**After (EPVS):**
```
Thread A (checkpoint):                Thread B (observer):
  CAS(word,                            load(word) → either:
      Prepare|v,                          Prepare|v    (old, consistent) ✓
      InProgress|v+1)                     InProgress|v+1 (new, consistent) ✓
                                        Never sees InProgress|v — impossible.
```

The 64-bit CAS is atomic on all modern x86-64 and AArch64 targets. Phase and version are always observed consistently.

### 4.5 CAS Failure Semantics

On CAS failure, the caller receives the **actual current state**:

```rust
match state.compare_exchange(expected, next, AcqRel, Acquire) {
    Ok(_old) => {
        // We won the transition.
    }
    Err(actual) => {
        if actual.is_intermediate() {
            // Another thread is mid-transition. Spin-wait.
            // After spin, re-read and retry or bail.
        } else {
            // State has moved on. The expected state is stale.
            // For checkpoint: another thread already advanced. Nothing to do.
            // For grow: same — idempotent, just return.
        }
    }
}
```

**Spin-wait on intermediate:** A thread that observes intermediate state should spin (with `core::hint::spin_loop()`) and reload. The intermediate window is very short — just the time to execute before-entering hooks.

### 4.6 Thread Safety Analysis

| Transition | Contention | Safety |
|-----------|-----------|--------|
| Rest → Prepare | Multiple threads may race to start checkpoint | CAS ensures exactly one winner. Losers fail harmlessly. |
| Prepare → InProgress(v+1) | Single orchestrator thread | Only checkpoint orchestrator calls this. Intermediate state blocks observers. |
| InProgress → WaitFlush | Single orchestrator thread | Same as above. |
| WaitFlush → WaitCompletion | Single orchestrator thread | Same as above. |
| WaitCompletion → PersistenceCallback | Single orchestrator thread | Same as above. |
| PersistenceCallback → Rest | Single orchestrator thread | Same as above. |
| Intermediate spin-wait | All observer threads | Observers never modify state; they just reload and retry. No ABA because version is monotonic. |

**ABA prevention:** Version is monotonic (never decremented). The 56-bit version ensures that the same `(phase, version)` pair never recurs. A thread that loses a CAS and later retries will never see the old state re-appear.

---

## 5. Integration Points

### 5.1 Files That Need Changes

#### Core State Module (NEW)

| File | Action | Description |
|------|--------|-------------|
| `src/state/mod.rs` | **Create** | New module for `SystemState`, `Phase`, `AtomicSystemState` |
| `src/state/system_state.rs` | **Create** | `SystemState` struct and bit manipulation |
| `src/state/phase.rs` | **Create** | Unified `Phase` enum (replaces `CheckpointPhase` + `GrowPhase`) |

#### Checkpoint State Machine

| File | Action | Description |
|------|--------|-------------|
| `src/checkpoint/state_machine.rs` | **Major rewrite** | Replace `AtomicU8 phase` with `AtomicSystemState`. Remove `CheckpointPhase` (moved to unified `Phase`). Rewrite `try_advance()` to use two-phase CAS protocol with intermediate state. |
| `src/checkpoint/orchestrator.rs` | **Moderate change** | Remove separate `index.version()` reads. Read version from `SystemState` instead. Transition logic uses new `try_transition()`. |
| `src/checkpoint/metadata.rs` | **Minor change** | `IndexRecoveryInfo::version` and `LogRecoveryInfo::version` change from `u32` to `u64` to match EPVS width. |
| `src/checkpoint/metadata_store.rs` | **Minor change** | Serialization unchanged (serde handles u32→u64). Add format version marker for compatibility detection. |
| `src/checkpoint/participant.rs` | **Moderate change** | `on_checkpoint_phase()` receives `SystemState` instead of `CheckpointPhase`. |
| `src/checkpoint/session_state.rs` | **Minor change** | Phase tracking uses `Phase` enum. |

#### Grow State Machine

| File | Action | Description |
|------|--------|-------------|
| `src/grow/state_machine.rs` | **Major rewrite** | Replace `AtomicU8 phase` with shared `AtomicSystemState` reference. Remove `GrowPhase` (moved to unified `Phase`). Grow and checkpoint share the same atomic word — only one can be active at a time, enforced by CAS. |
| `src/grow/mod.rs` | **Moderate change** | Update grow orchestration to use `SystemState` transitions. |

#### Hash Index

| File | Action | Description |
|------|--------|-------------|
| `src/hash/index.rs` | **Remove field** | Remove `version: AtomicU32`. Version now lives in `AtomicSystemState`. Provide accessor that reads version from the shared state. |

#### Epoch Module

| File | Action | Description |
|------|--------|-------------|
| `src/epoch/entry.rs` | **Minor change** | `phase_finished` array indexing uses `Phase` discriminant instead of `CheckpointPhase`. Array size may increase to accommodate unified phases (16 entries). |
| `src/epoch/mod.rs` | **Minor change** | `PHASE_COUNT` constant updated from 8 to 16 (or dynamically derived from `Phase` enum). |

#### Store/KV

| File | Action | Description |
|------|--------|-------------|
| `src/store/kv.rs` | **Moderate change** | Add `system_state: Arc<AtomicSystemState>` field. Remove version reads from `hash_index`. Thread refresh reads phase from system state. |
| `src/store/session.rs` | **Minor change** | Session can read current `SystemState` atomically for checkpoint coordination. |

#### Record Info

| File | Action | Description |
|------|--------|-------------|
| `src/record/record_info.rs` | **No change in this PR** | The 13-bit per-record version field remains for now. P-22 (RecordInfo Bit Layout Optimization) is a separate task that depends on EPVS landing first. Sam should NOT change RecordInfo in the EPVS implementation — it will be handled in W2-04. |

### 5.2 How Functions Trait Callbacks See Phase/Version

Currently, Functions trait callbacks (`on_read`, `on_upsert`, etc.) do **not** receive phase or version information. This remains unchanged. The callbacks operate on records and don't need system state.

When checkpoint-aware operations are needed (e.g., CPR's "create pending if InProgress"), the store checks `SystemState` internally before dispatching to the callback. The callback contract is:

```rust
// No change to the Functions trait signature.
// SystemState is internal to the store's operation dispatch.
trait Functions {
    fn on_read(&self, key: &Self::Key, value: &Self::Value, output: &mut Self::Output);
    fn on_upsert(&self, key: &Self::Key, value: &Self::Value, existing: Option<&Self::Value>);
    // etc.
}
```

### 5.3 Impact on RecordInfo (W2-04 Connection)

EPVS does NOT change RecordInfo directly. However, EPVS **enables** P-22:

- With EPVS, the global version is tracked in `SystemState`, not per-record.
- The 13-bit `version` field in RecordInfo (bits 48–60) becomes redundant for checkpoint coordination.
- P-22 proposes reclaiming those 13 bits for: `Sealed` (P-03), `Filler` (P-08), `Dirty`, and future flags.
- W2-04 should be planned as a follow-up to EPVS.

**For this PR:** Leave RecordInfo unchanged. Continue writing version into records via `RecordInfo::with_checkpoint_version()`. The version will come from `SystemState::version()` instead of `HashIndex::version()`.

### 5.4 Impact on Checkpoint Metadata on Disk

**Format change:** `version` field changes from `u32` to `u64`.

**JSON compatibility:**
```json
// Old format (version as u32):
{ "version": 42, "table_size": 65536, ... }

// New format (version as u64):
{ "format_version": 2, "version": 42, "table_size": 65536, ... }
```

- serde `u64` serializes as a JSON number. Values ≤ 2^53 are exact in JSON/JavaScript (not a concern for checkpoint metadata read by Rust).
- Add `format_version` field to recovery info structs for forward compatibility.
- **Phase is NOT serialized** — checkpoints are always taken when the state machine reaches the persistence phase. The recovered state always starts in `Rest`.

### 5.5 Impact on Recovery

**Reading old format:** Detect by absence of `format_version` field:
```rust
impl IndexRecoveryInfo {
    fn from_json(value: &serde_json::Value) -> Result<Self, CheckpointError> {
        match value.get("format_version") {
            Some(v) if v.as_u64() == Some(2) => {
                // New EPVS format: version is u64
                serde_json::from_value(value.clone()).map_err(...)
            }
            _ => {
                // Legacy format: version is u32, widen to u64
                let legacy: LegacyIndexRecoveryInfo = serde_json::from_value(value.clone())?;
                Ok(Self {
                    format_version: 1,
                    version: legacy.version as u64,
                    // ... copy other fields
                })
            }
        }
    }
}
```

---

## 6. Migration Strategy

### 6.1 Recommended Approach: Soft Migration with Format Version

This is **NOT a hard breaking change**. We can support both formats:

1. **Write new format always:** New checkpoints include `format_version: 2` and `version` as `u64`.

2. **Read both formats:** Recovery code checks for `format_version`:
   - Absent or 1: legacy format, widen `version: u32` → `u64`.
   - 2: EPVS format, read `version` as `u64`.

3. **One-way migration:** Once EPVS is deployed, old binaries cannot read new checkpoints (they don't know `format_version`). This is acceptable — we don't support downgrade.

### 6.2 Rollout Sequence

```
Phase 1 (this PR): Introduce SystemState, AtomicSystemState, unified Phase.
         Wire into checkpoint and grow state machines.
         Version field widened to u64 in metadata.
         Recovery reads both formats.

Phase 2 (P-22 / W2-04): Reclaim RecordInfo version bits.
         Requires EPVS to be stable first.

Phase 3 (P-09): Composable state machine tasks.
         Built on EPVS's clean atomic transitions.
```

### 6.3 Format Version Constants

```rust
/// Checkpoint metadata format versions.
pub const FORMAT_VERSION_LEGACY: u64 = 1; // Pre-EPVS, version is u32
pub const FORMAT_VERSION_EPVS: u64 = 2;   // EPVS, version is u64
pub const FORMAT_VERSION_CURRENT: u64 = FORMAT_VERSION_EPVS;
```

---

## 7. Grow + Checkpoint Mutual Exclusion

### 7.1 Shared State Word

With EPVS, the grow state machine and checkpoint state machine **share the same `AtomicSystemState`**. This means:

- Only one operation (checkpoint OR grow) can be active at a time.
- A grow cannot start while a checkpoint is in progress (CAS from `Rest` will fail because the state is in a checkpoint phase).
- A checkpoint cannot start while a grow is in progress (same reason).

This is a **simplification** over the current code, where two independent state machines could theoretically interfere. The C# implementation uses this exact pattern.

### 7.2 Future: Concurrent Grow + Checkpoint

If we later need concurrent grow and checkpoint (unlikely, but possible), we can:
- Use two `AtomicSystemState` words (one per state machine)
- Or encode both states in a single 128-bit CAS (x86-64 supports `cmpxchg16b`)
- Or use the composable state machine (P-09) approach

For now, mutual exclusion is the correct design. It matches C# FASTER's behavior.

---

## 8. Epoch Integration

### 8.1 Version Bump via Epoch

When transitioning `Prepare → InProgress(v+1)`, the version bump must be visible to all threads before any thread operates in the new version. This requires an **epoch barrier**:

```rust
// In checkpoint orchestrator:
fn advance_to_in_progress(&self) {
    let current = self.state.load(Acquire);
    assert_eq!(current.phase(), Phase::Prepare);

    let next = SystemState::new(Phase::InProgress, current.version() + 1);

    self.try_transition(current, next, || {
        // Before-entering hook: bump epoch to create happens-before edge.
        // All threads that enter epoch protection after this point
        // will see the new version.
        self.epoch_table.bump_current_epoch(|| {
            // Callback fires when all pre-bump threads have exited.
            // This is where we know the old version is fully quiesced.
        });
    });
}
```

### 8.2 Phase-Finished Array

The `EpochEntry::phase_finished` array (`epoch/entry.rs:50`) should index by `Phase` discriminant. Increase `PHASE_COUNT` to 16 (from 8) to accommodate the unified phase enum:

```rust
pub(crate) const PHASE_COUNT: usize = 16;
```

---

## 9. Test Plan

### 9.1 Key Correctness Properties

| Property | How to Verify |
|----------|---------------|
| **Atomic consistency:** Phase and version are always seen together | Load `SystemState`, verify `(phase, version)` is a valid combination |
| **Monotonic version:** Version never decreases | Property test: record all observed versions across threads, verify monotonic |
| **No torn reads:** No thread ever sees `(new_phase, old_version)` | Concurrent test: N threads read state in a loop while another thread transitions |
| **CAS linearizability:** Exactly one thread wins each transition | N threads race to transition; verify exactly one succeeds |
| **Intermediate exclusion:** Normal reads never return intermediate state | Observer threads should never see `is_intermediate() == true` for more than a spin-loop |
| **Mutual exclusion:** Grow and checkpoint never overlap | Start both concurrently; verify only one enters non-Rest phase |

### 9.2 Concurrent Transition Scenarios

```rust
#[test]
fn test_single_transition() {
    // Basic: Rest → Prepare, verify version unchanged.
}

#[test]
fn test_version_bump_on_in_progress() {
    // Prepare(v) → InProgress(v+1), verify version incremented.
}

#[test]
fn test_cas_failure_on_stale_state() {
    // Two threads race to transition Rest → Prepare.
    // One wins, one gets Err(actual).
}

#[test]
fn test_full_checkpoint_cycle() {
    // Run complete Rest→...→Rest cycle, verify version incremented by 1.
}

#[test]
fn test_concurrent_observers() {
    // N reader threads + 1 writer thread doing checkpoint cycle.
    // Verify no torn reads: every observed (phase, version) is valid.
}

#[test]
fn test_grow_checkpoint_mutual_exclusion() {
    // Thread A tries checkpoint, Thread B tries grow.
    // Exactly one succeeds; the other fails CAS from Rest.
}

#[test]
fn test_intermediate_not_observed() {
    // Fast reader loop + slow transition (with artificial delay in hook).
    // Readers should spin-wait, never return intermediate to callers.
}

#[test]
fn test_version_monotonic_under_contention() {
    // Many checkpoint cycles under thread contention.
    // All observed versions are monotonically increasing.
}
```

### 9.3 Recovery Scenarios

```rust
#[test]
fn test_recover_legacy_checkpoint() {
    // Create checkpoint in old format (version as u32, no format_version).
    // Recover with EPVS code. Verify version widened correctly.
}

#[test]
fn test_recover_epvs_checkpoint() {
    // Create checkpoint in new format (format_version: 2, version as u64).
    // Recover and verify.
}

#[test]
fn test_checkpoint_roundtrip() {
    // Take checkpoint with EPVS, recover, verify state matches.
}

#[test]
fn test_version_preserved_through_recovery() {
    // Run N checkpoint cycles, recover, verify version == N.
}
```

### 9.4 Property-Based Tests

```rust
// Using proptest or quickcheck:

#[proptest]
fn prop_system_state_roundtrip(phase: Phase, version: u56) {
    let state = SystemState::new(phase, version);
    assert_eq!(state.phase(), phase);
    assert_eq!(state.version(), version);
}

#[proptest]
fn prop_intermediate_preserves_phase_version(phase: Phase, version: u56) {
    let state = SystemState::new(phase, version);
    let inter = state.make_intermediate();
    assert!(inter.is_intermediate());
    assert_eq!(inter.version(), version);
    // Phase extraction strips intermediate bit
    assert_eq!(inter.phase(), phase);
}
```

---

## 10. Implementation Notes for Sam

### 10.1 Module Organization

Create `src/state/` module:
```
src/state/
  mod.rs            // pub mod phase; pub mod system_state; re-exports
  phase.rs          // Phase enum (unified)
  system_state.rs   // SystemState, AtomicSystemState
```

### 10.2 Migration Order

1. **Create `SystemState` + `Phase` types** (no existing code changes yet)
2. **Write unit tests** for `SystemState` (roundtrip, intermediate, ordering)
3. **Wire into `CheckpointStateMachine`** — replace `AtomicU8` with `AtomicSystemState`
4. **Wire into `GrowStateMachine`** — share the same `AtomicSystemState`
5. **Remove `HashIndex::version`** — read from `SystemState` instead
6. **Update checkpoint metadata** — `format_version`, `u64` version
7. **Update recovery** — format detection, legacy compatibility
8. **Integration tests** — full checkpoint + grow cycles
9. **Concurrent tests** — race conditions, mutual exclusion

### 10.3 Key Invariants to Maintain

- `SystemState::version()` is **monotonically increasing** (never set to a lower value)
- Phase transitions are **sequential** — no skipping phases
- The intermediate state is **never exposed** to code outside the state machine module
- Checkpoint metadata always includes `format_version` for forward compatibility
- `RecordInfo` version field is **unchanged** in this PR

### 10.4 Performance Considerations

- `AtomicU64` CAS on x86-64: single `lock cmpxchg8b` instruction — same cost as current `AtomicU8` CAS
- On AArch64: `ldxr`/`stxr` loop — same cost as current code
- Reading `SystemState`: single `ldr` (64-bit load) — actually **fewer** instructions than current code which loads phase and version separately
- No additional memory: we replace `AtomicU8 + AtomicU32` (13 bytes, padded to 16) with `AtomicU64` (8 bytes)
- Cache line usage: **improved** — one cache line access instead of potentially two

---

## Appendix A: C# Reference

The C# SystemState (`cs/src/core/Index/Synchronization/StateTransitions.cs`) uses the identical bit layout:
- `[StructLayout(LayoutKind.Explicit, Size = 8)]`
- Phase: bits 56–63 (`kPhaseShiftInWord = 56`)
- Version: bits 0–55 (`kVersionMaskInWord = 0x00FF_FFFF_FFFF_FFFF`)
- CAS via `Interlocked.CompareExchange(ref systemState.Word, ...)`

The Tsavorite `VersionSchemeState` (`cs/src/core/Epochs/EpochProtectedVersionScheme.cs`) is structurally identical, with the intermediate bit at 0x80 in the phase byte.

Our Rust implementation follows this layout exactly for cross-implementation consistency.

## Appendix B: Relationship to Other Proposals

| Proposal | Relationship | Notes |
|----------|-------------|-------|
| P-09 (Composable State Machine) | **Enabled by EPVS** | Composable tasks rely on atomic state transitions |
| P-10 (Incremental Snapshots) | **Enabled by EPVS** | Requires clean version tracking |
| P-11 (Streaming Snapshots) | **Enabled by EPVS** | Same as P-10 |
| P-22 (RecordInfo Bits) | **Unlocked by EPVS** | Can reclaim 13-bit version field after EPVS |
| W2-04 (RecordInfo Redesign) | **Follow-up** | Should land after EPVS stabilizes |
