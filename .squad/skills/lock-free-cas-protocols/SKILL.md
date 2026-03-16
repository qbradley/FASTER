# Skill: Lock-Free CAS Protocols with ABA Prevention

## When to Use

When implementing lock-free state transitions using compare-and-swap (CAS) where:
- Multiple threads coordinate through shared atomic state
- State has version numbers or multi-field structures
- ABA problem could cause incorrect state transitions
- Two-phase commit or intermediate states are needed

## Pattern

### The ABA Problem

```
Thread A: reads state=1, prepares to CAS(1→2)
Thread B: CAS(1→3), then CAS(3→1)  [state back to 1]
Thread A: CAS(1→2) succeeds!  [❌ but missed state=3 transition]
```

### Solution: Two-Phase Intermediate CAS

From EPVS (Epoch Protection Version Scheme) implementation:

```rust
pub struct SystemState {
    pub phase: Phase,           // Prepare, InProgress, Complete
    pub version: u64,           // Bumps only on Prepare→InProgress
    pub checkpoint_cookie: u64,
}

// ✅ CORRECT: Prepare→InProgress uses intermediate value
pub fn start_checkpoint(&self, token: u64) -> bool {
    let current = self.state.load(Acquire);
    
    // Only start from Complete phase
    if current.phase != Phase::Complete {
        return false;
    }
    
    // CRITICAL: Prepare phase is intermediate state
    let prepare_state = SystemState {
        phase: Phase::Prepare,
        version: current.version,  // Don't bump yet!
        checkpoint_cookie: token,
    };
    
    // CAS into Prepare
    match self.state.compare_exchange(current, prepare_state, AcqRel, Acquire) {
        Ok(_) => {
            // Version bump happens ONLY on Prepare→InProgress
            let in_progress = SystemState {
                phase: Phase::InProgress,
                version: current.version + 1,  // NOW bump
                checkpoint_cookie: token,
            };
            self.state.store(in_progress, Release);
            true
        }
        Err(_) => false
    }
}
```

### Why It Works

1. **Prepare is observable** — other threads see it and won't start concurrent checkpoints
2. **Version bump delayed** — only happens on Prepare→InProgress, not on initial transition
3. **ABA prevented** — if thread suspends during Prepare→InProgress→Complete→Prepare cycle, the version number will differ

### Pattern Template

```rust
// Two-phase transition: A → Intermediate → B
// Version bumps only on Intermediate → B

pub fn transition(&self) -> bool {
    let current = self.load(Acquire);
    
    // Check preconditions
    if !can_transition(current) {
        return false;
    }
    
    // Phase 1: CAS into intermediate state (NO version bump)
    let intermediate = State {
        phase: Phase::Intermediate,
        version: current.version,  // Keep same
        ..current
    };
    
    match self.compare_exchange(current, intermediate, AcqRel, Acquire) {
        Ok(_) => {
            // Phase 2: Store final state (YES version bump)
            let final_state = State {
                phase: Phase::Final,
                version: current.version + 1,  // Bump NOW
                ..intermediate
            };
            self.store(final_state, Release);
            true
        }
        Err(_) => false
    }
}
```

## Confidence: High

## Learned From

- **EPVS Implementation (Iteration 6 A1)**: Two-phase intermediate CAS protocol
- **Charter principle**: "Lock-free algorithms get formal correctness arguments"
- **Backlog Sprint**: "Keep version bumps on Prepare→InProgress only"

## Key Considerations

1. **Intermediate state must be observable** — other threads must be able to see it and react
2. **Version bump timing matters** — too early creates ABA window, too late loses synchronization
3. **Memory ordering critical** — CAS uses AcqRel/Acquire, final store uses Release
4. **Precondition checks** — verify current state before attempting transition

## Revivification Example

```rust
// Single-phase CAS (simpler, no ABA risk for single bit flip)
pub fn try_revivify(&self, expected: RecordInfo) -> Result<RecordInfo, RecordInfo> {
    let new = expected.with_sealed_cleared();
    
    // Preconditions checked before CAS
    if !expected.is_sealed() {
        return Err(expected);
    }
    
    // Single CAS, AcqRel/Acquire ordering
    self.atomic_info.compare_exchange(
        expected.raw,
        new.raw,
        Ordering::AcqRel,
        Ordering::Acquire
    )
}
```

**Why no intermediate state?** Single bit flip is atomic, no multi-step protocol, no ABA window.

## Testing

```rust
#[test]
fn aba_prevention() {
    let state = AtomicSystemState::new();
    
    // Thread A starts checkpoint
    state.start_checkpoint(1);  // Complete→Prepare→InProgress (v1)
    
    // Thread B completes it
    state.complete_checkpoint(); // InProgress→Complete (v1)
    
    // Thread C starts new checkpoint
    state.start_checkpoint(2);  // Complete→Prepare→InProgress (v2)
    
    // Thread A's stale CAS should fail (version mismatch)
    let stale = SystemState { version: 1, .. };
    assert!(state.compare_exchange(stale, ..).is_err());
}
```

## Anti-Patterns

❌ **Bump version on first CAS** → creates ABA window between phases
❌ **Skip intermediate state** → other threads can't coordinate
❌ **Use Relaxed ordering** → breaks happens-before chains
❌ **No precondition checks** → invalid state transitions

## Key Files

- `rust/crates/faster-core/src/state/mod.rs` (SystemState, AtomicSystemState)
- `rust/crates/faster-core/src/checkpoint/state_machine.rs` (uses EPVS)
- `rust/crates/faster-core/tests/state_tests.rs` (48 unit tests)
