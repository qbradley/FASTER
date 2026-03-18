//! Phase-based state machine that drives checkpoint execution.
//!
//! The checkpoint lifecycle is modelled as a linear sequence of phases:
//!
//! ```text
//! Rest → Prepare → InProgress → WaitFlush → WaitCompletion → Completed → Rest
//! ```
//!
//! Only the transitions listed above are legal; any other transition returns an
//! error. The current phase (and version) are stored as a single
//! [`AtomicSystemState`] (packed `AtomicU64`) so they can be queried atomically
//! from any thread, and phase transitions use compare-and-swap to prevent races.
//!
//! This design implements the EPVS (Epoch-Protected Version Scheme) and
//! mirrors the C# Tsavorite `VersionSchemeState`.

use core::sync::atomic::Ordering;
use serde::{Deserialize, Serialize};

use super::CheckpointToken;
use crate::error::FasterError;
use crate::state::{AtomicSystemState, Phase, SystemState};
use crate::sync::Mutex;

// ---------------------------------------------------------------------------
// CheckpointPhase
// ---------------------------------------------------------------------------

/// The phase a checkpoint is currently in.
///
/// Retained for backward compatibility. Internally maps to the unified
/// [`Phase`] enum in the [`state`](crate::state) module.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(u8)]
pub enum CheckpointPhase {
    /// No checkpoint in progress.
    Rest = 0,
    /// Sessions have been notified; preparation is underway.
    Prepare = 1,
    /// Actively taking the checkpoint (index + log).
    InProgress = 2,
    /// Waiting for the log flush to complete.
    WaitFlush = 3,
    /// Waiting for all sessions to acknowledge completion.
    WaitCompletion = 4,
    /// Checkpoint finished successfully.
    Completed = 5,
}

impl CheckpointPhase {
    /// Converts a raw `u8` to a [`CheckpointPhase`], returning `None` for
    /// values that do not correspond to any variant.
    #[inline]
    pub const fn from_u8(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Rest),
            1 => Some(Self::Prepare),
            2 => Some(Self::InProgress),
            3 => Some(Self::WaitFlush),
            4 => Some(Self::WaitCompletion),
            5 => Some(Self::Completed),
            _ => None,
        }
    }

    /// Returns the `u8` discriminant of this phase.
    #[inline]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// Returns `true` if `self → next` is a legal checkpoint transition.
    #[inline]
    const fn can_advance_to(self, next: Self) -> bool {
        matches!(
            (self as u8, next as u8),
            (0, 1) | (1, 2) | (2, 3) | (3, 4) | (4, 5) | (5, 0)
        )
    }

    /// Convert to the unified [`Phase`] enum.
    #[inline]
    pub const fn to_phase(self) -> Phase {
        match self {
            Self::Rest => Phase::Rest,
            Self::Prepare => Phase::Prepare,
            Self::InProgress => Phase::InProgress,
            Self::WaitFlush => Phase::WaitFlush,
            Self::WaitCompletion => Phase::WaitCompletion,
            Self::Completed => Phase::PersistenceCallback,
        }
    }

    /// Convert from the unified [`Phase`] enum.
    /// Returns `None` if the phase is not a checkpoint phase.
    #[inline]
    pub const fn from_phase(phase: Phase) -> Option<Self> {
        match phase {
            Phase::Rest => Some(Self::Rest),
            Phase::Prepare => Some(Self::Prepare),
            Phase::InProgress => Some(Self::InProgress),
            Phase::WaitFlush => Some(Self::WaitFlush),
            Phase::WaitCompletion => Some(Self::WaitCompletion),
            Phase::PersistenceCallback => Some(Self::Completed),
            _ => None,
        }
    }
}

impl core::fmt::Display for CheckpointPhase {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let name = match self {
            Self::Rest => "Rest",
            Self::Prepare => "Prepare",
            Self::InProgress => "InProgress",
            Self::WaitFlush => "WaitFlush",
            Self::WaitCompletion => "WaitCompletion",
            Self::Completed => "Completed",
        };
        f.write_str(name)
    }
}

// ---------------------------------------------------------------------------
// CheckpointStateMachine
// ---------------------------------------------------------------------------

/// Thread-safe state machine that drives a checkpoint through its phases.
///
/// Internally backed by an [`AtomicSystemState`] — a single `AtomicU64`
/// that packs both phase and version atomically (EPVS).
///
/// # Example
///
/// ```
/// use faster_core::checkpoint::{CheckpointToken, CheckpointPhase, CheckpointStateMachine};
///
/// let sm = CheckpointStateMachine::new();
/// assert_eq!(sm.phase(), CheckpointPhase::Rest);
///
/// let token = CheckpointToken::new(1);
/// sm.start(token).unwrap();
/// assert_eq!(sm.phase(), CheckpointPhase::Prepare);
/// ```
pub struct CheckpointStateMachine {
    /// Packed (phase, version) state word (EPVS).
    state: AtomicSystemState,
    /// Token of the currently-active checkpoint.
    token: Mutex<Option<CheckpointToken>>,
}

impl CheckpointStateMachine {
    /// Creates a new state machine in `Rest` phase with version 0.
    #[inline]
    pub fn new() -> Self {
        Self {
            state: AtomicSystemState::new(SystemState::INITIAL),
            token: Mutex::new(None),
        }
    }

    /// Creates a new state machine with a specific initial system state.
    /// Used during recovery to restore the version from a checkpoint.
    #[inline]
    pub fn with_state(initial: SystemState) -> Self {
        Self {
            state: AtomicSystemState::new(initial),
            token: Mutex::new(None),
        }
    }

    /// Returns the current checkpoint phase (lock-free atomic read).
    #[inline]
    pub fn phase(&self) -> CheckpointPhase {
        let sys = self.state.load(Ordering::Acquire);
        // intentionally discarded: phases outside the checkpoint set are treated as Rest (no checkpoint active)
        CheckpointPhase::from_phase(sys.phase()).unwrap_or(CheckpointPhase::Rest)
    }

    /// Returns the current [`SystemState`] (phase + version) atomically.
    #[inline]
    pub fn system_state(&self) -> SystemState {
        self.state.load(Ordering::Acquire)
    }

    /// Returns the current version from the packed state.
    #[inline]
    pub fn version(&self) -> u64 {
        self.state.load(Ordering::Acquire).version()
    }

    /// Returns a reference to the underlying [`AtomicSystemState`].
    #[inline]
    pub fn atomic_state(&self) -> &AtomicSystemState {
        &self.state
    }

    /// Attempts a CAS-based transition from `expected` to `next`.
    ///
    /// Version bump happens atomically on Prepare → InProgress.
    ///
    /// # Errors
    ///
    /// Returns [`FasterError::CheckpointError`] if `expected → next` is not
    /// a legal transition.
    pub fn try_advance(
        &self,
        expected: CheckpointPhase,
        next: CheckpointPhase,
    ) -> Result<bool, FasterError> {
        if !expected.can_advance_to(next) {
            return Err(FasterError::CheckpointError(format!(
                "illegal phase transition: {expected} → {next}"
            )));
        }

        let current = self.state.load(Ordering::Acquire);
        let version = current.version();
        let expected_state = SystemState::new(expected.to_phase(), version);

        // Version bump on Prepare → InProgress (the EPVS key improvement).
        let next_version =
            if expected == CheckpointPhase::Prepare && next == CheckpointPhase::InProgress {
                version + 1
            } else {
                version
            };
        let next_state = SystemState::new(next.to_phase(), next_version);

        // Two-phase intermediate CAS protocol.
        // AcqRel: publishes state change with happens-before edge.
        // Acquire on failure: consistent read of actual state.
        Ok(self.state.try_transition(expected_state, next_state, || {}))
    }

    /// Begins a new checkpoint (Rest → Prepare).
    pub fn start(&self, token: CheckpointToken) -> Result<(), FasterError> {
        let won = self.try_advance(CheckpointPhase::Rest, CheckpointPhase::Prepare)?;
        if !won {
            return Err(FasterError::CheckpointError(
                "checkpoint already in progress (phase is not Rest)".into(),
            ));
        }
        *self.token.lock().expect("token mutex poisoned") = Some(token);
        Ok(())
    }

    /// Resets the state machine to `Rest`, clearing the active token.
    pub fn reset(&self) -> Result<(), FasterError> {
        let current = self.phase();
        match current {
            CheckpointPhase::Rest => Ok(()),
            CheckpointPhase::Completed => {
                let won = self.try_advance(CheckpointPhase::Completed, CheckpointPhase::Rest)?;
                if !won {
                    return Ok(());
                }
                *self.token.lock().expect("token mutex poisoned") = None;
                Ok(())
            }
            other => Err(FasterError::CheckpointError(format!(
                "cannot reset from phase {other} (expected Completed or Rest)"
            ))),
        }
    }

    /// Returns the active checkpoint token, or `None` if in `Rest`.
    pub fn active_token(&self) -> Option<CheckpointToken> {
        if self.phase() == CheckpointPhase::Rest {
            return None;
        }
        *self.token.lock().expect("token mutex poisoned")
    }
}

impl Default for CheckpointStateMachine {
    fn default() -> Self {
        Self::new()
    }
}

impl core::fmt::Debug for CheckpointStateMachine {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("CheckpointStateMachine")
            .field("phase", &self.phase())
            .field("version", &self.version())
            .field("token", &self.active_token())
            .finish()
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phase_u8_round_trip() {
        let phases = [
            CheckpointPhase::Rest,
            CheckpointPhase::Prepare,
            CheckpointPhase::InProgress,
            CheckpointPhase::WaitFlush,
            CheckpointPhase::WaitCompletion,
            CheckpointPhase::Completed,
        ];
        for (i, &phase) in phases.iter().enumerate() {
            assert_eq!(phase.as_u8(), i as u8);
            assert_eq!(CheckpointPhase::from_u8(i as u8), Some(phase));
        }
    }

    #[test]
    fn phase_from_u8_invalid() {
        assert_eq!(CheckpointPhase::from_u8(6), None);
        assert_eq!(CheckpointPhase::from_u8(255), None);
    }

    #[test]
    fn phase_display() {
        assert_eq!(CheckpointPhase::Rest.to_string(), "Rest");
        assert_eq!(CheckpointPhase::Completed.to_string(), "Completed");
    }

    #[test]
    fn phase_to_phase_round_trip() {
        for cp in [
            CheckpointPhase::Rest,
            CheckpointPhase::Prepare,
            CheckpointPhase::InProgress,
            CheckpointPhase::WaitFlush,
            CheckpointPhase::WaitCompletion,
            CheckpointPhase::Completed,
        ] {
            assert_eq!(CheckpointPhase::from_phase(cp.to_phase()), Some(cp));
        }
    }

    #[test]
    fn valid_transitions() {
        let valid = [
            (CheckpointPhase::Rest, CheckpointPhase::Prepare),
            (CheckpointPhase::Prepare, CheckpointPhase::InProgress),
            (CheckpointPhase::InProgress, CheckpointPhase::WaitFlush),
            (CheckpointPhase::WaitFlush, CheckpointPhase::WaitCompletion),
            (CheckpointPhase::WaitCompletion, CheckpointPhase::Completed),
            (CheckpointPhase::Completed, CheckpointPhase::Rest),
        ];
        for (from, to) in valid {
            assert!(from.can_advance_to(to), "{from} → {to} should be valid");
        }
    }

    #[test]
    fn invalid_transitions() {
        assert!(!CheckpointPhase::Rest.can_advance_to(CheckpointPhase::InProgress));
        assert!(!CheckpointPhase::Prepare.can_advance_to(CheckpointPhase::Rest));
        assert!(!CheckpointPhase::Completed.can_advance_to(CheckpointPhase::Prepare));
    }

    #[test]
    fn new_starts_at_rest() {
        let sm = CheckpointStateMachine::new();
        assert_eq!(sm.phase(), CheckpointPhase::Rest);
        assert!(sm.active_token().is_none());
        assert_eq!(sm.version(), 0);
    }

    #[test]
    fn default_starts_at_rest() {
        let sm = CheckpointStateMachine::default();
        assert_eq!(sm.phase(), CheckpointPhase::Rest);
    }

    #[test]
    fn version_bumps_on_prepare_to_in_progress() {
        let sm = CheckpointStateMachine::new();
        sm.start(CheckpointToken::new(1)).unwrap();
        assert_eq!(sm.version(), 0);
        sm.try_advance(CheckpointPhase::Prepare, CheckpointPhase::InProgress)
            .unwrap();
        assert_eq!(sm.version(), 1);
    }

    #[test]
    fn system_state_consistent() {
        let sm = CheckpointStateMachine::new();
        sm.start(CheckpointToken::new(1)).unwrap();
        sm.try_advance(CheckpointPhase::Prepare, CheckpointPhase::InProgress)
            .unwrap();
        let ss = sm.system_state();
        assert_eq!(ss.phase(), Phase::InProgress);
        assert_eq!(ss.version(), 1);
    }

    #[test]
    fn full_lifecycle() {
        let sm = CheckpointStateMachine::new();
        let token = CheckpointToken::new(0xDEAD);

        sm.start(token).unwrap();
        assert_eq!(sm.phase(), CheckpointPhase::Prepare);
        assert_eq!(sm.active_token(), Some(token));

        assert!(
            sm.try_advance(CheckpointPhase::Prepare, CheckpointPhase::InProgress)
                .unwrap()
        );
        assert!(
            sm.try_advance(CheckpointPhase::InProgress, CheckpointPhase::WaitFlush)
                .unwrap()
        );
        assert!(
            sm.try_advance(CheckpointPhase::WaitFlush, CheckpointPhase::WaitCompletion)
                .unwrap()
        );
        assert!(
            sm.try_advance(CheckpointPhase::WaitCompletion, CheckpointPhase::Completed)
                .unwrap()
        );
        assert_eq!(sm.active_token(), Some(token));

        sm.reset().unwrap();
        assert_eq!(sm.phase(), CheckpointPhase::Rest);
        assert!(sm.active_token().is_none());
        assert_eq!(sm.version(), 1);
    }

    #[test]
    fn start_rejects_when_not_rest() {
        let sm = CheckpointStateMachine::new();
        sm.start(CheckpointToken::new(1)).unwrap();
        assert!(sm.start(CheckpointToken::new(2)).is_err());
    }

    #[test]
    fn reset_idempotent_from_rest() {
        let sm = CheckpointStateMachine::new();
        sm.reset().unwrap();
        assert_eq!(sm.phase(), CheckpointPhase::Rest);
    }

    #[test]
    fn reset_rejects_mid_checkpoint() {
        let sm = CheckpointStateMachine::new();
        sm.start(CheckpointToken::new(1)).unwrap();
        assert!(sm.reset().is_err());
    }

    #[test]
    fn try_advance_illegal_transition_returns_error() {
        let sm = CheckpointStateMachine::new();
        assert!(
            sm.try_advance(CheckpointPhase::Rest, CheckpointPhase::InProgress)
                .is_err()
        );
    }

    #[test]
    fn try_advance_wrong_expected_returns_false() {
        let sm = CheckpointStateMachine::new();
        sm.start(CheckpointToken::new(1)).unwrap();
        let won = sm
            .try_advance(CheckpointPhase::InProgress, CheckpointPhase::WaitFlush)
            .unwrap();
        assert!(!won);
        assert_eq!(sm.phase(), CheckpointPhase::Prepare);
    }

    #[test]
    fn concurrent_start_only_one_wins() {
        use std::sync::{Arc, Barrier};
        let sm = Arc::new(CheckpointStateMachine::new());
        let barrier = Arc::new(Barrier::new(8));
        let handles: Vec<_> = (0..8u128)
            .map(|i| {
                let sm = Arc::clone(&sm);
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    sm.start(CheckpointToken::new(i))
                })
            })
            .collect();
        let winners = handles
            .into_iter()
            .map(|h| h.join().unwrap())
            .filter(|r| r.is_ok())
            .count();
        assert_eq!(winners, 1);
        assert_eq!(sm.phase(), CheckpointPhase::Prepare);
    }

    #[test]
    fn concurrent_advance_only_one_wins() {
        use std::sync::{Arc, Barrier};
        let sm = Arc::new(CheckpointStateMachine::new());
        sm.start(CheckpointToken::new(42)).unwrap();
        let barrier = Arc::new(Barrier::new(8));
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let sm = Arc::clone(&sm);
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    sm.try_advance(CheckpointPhase::Prepare, CheckpointPhase::InProgress)
                })
            })
            .collect();
        let winners = handles
            .into_iter()
            .map(|h| h.join().unwrap())
            .filter(|r| matches!(r, Ok(true)))
            .count();
        assert_eq!(winners, 1);
        assert_eq!(sm.phase(), CheckpointPhase::InProgress);
    }

    #[test]
    fn version_increments_per_cycle() {
        let sm = CheckpointStateMachine::new();
        for cycle in 0u64..3 {
            sm.start(CheckpointToken::new(cycle as u128)).unwrap();
            sm.try_advance(CheckpointPhase::Prepare, CheckpointPhase::InProgress)
                .unwrap();
            assert_eq!(sm.version(), cycle + 1);
            sm.try_advance(CheckpointPhase::InProgress, CheckpointPhase::WaitFlush)
                .unwrap();
            sm.try_advance(CheckpointPhase::WaitFlush, CheckpointPhase::WaitCompletion)
                .unwrap();
            sm.try_advance(CheckpointPhase::WaitCompletion, CheckpointPhase::Completed)
                .unwrap();
            sm.reset().unwrap();
            assert_eq!(sm.version(), cycle + 1);
        }
    }

    #[test]
    fn with_state_restores_version() {
        let sm = CheckpointStateMachine::with_state(SystemState::new(Phase::Rest, 42));
        assert_eq!(sm.version(), 42);
        assert_eq!(sm.phase(), CheckpointPhase::Rest);
    }

    #[test]
    fn debug_format() {
        let sm = CheckpointStateMachine::new();
        let dbg = format!("{sm:?}");
        assert!(dbg.contains("Rest"));
        assert!(dbg.contains("CheckpointStateMachine"));
    }

    #[test]
    fn state_machine_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<CheckpointStateMachine>();
    }

    #[test]
    fn phase_serde_round_trip() {
        for phase in [
            CheckpointPhase::Rest,
            CheckpointPhase::Prepare,
            CheckpointPhase::InProgress,
            CheckpointPhase::WaitFlush,
            CheckpointPhase::WaitCompletion,
            CheckpointPhase::Completed,
        ] {
            let json = serde_json::to_string(&phase).unwrap();
            let back: CheckpointPhase = serde_json::from_str(&json).unwrap();
            assert_eq!(phase, back);
        }
    }
}
