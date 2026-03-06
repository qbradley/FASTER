//! Phase-based state machine that drives checkpoint execution.
//!
//! The checkpoint lifecycle is modelled as a linear sequence of phases:
//!
//! ```text
//! Rest → Prepare → InProgress → WaitFlush → WaitCompletion → Completed → Rest
//! ```
//!
//! Only the transitions listed above are legal; any other transition returns an
//! error. The current phase is stored as an [`AtomicU8`] so it can be queried
//! from any thread without locking, and phase transitions use compare-and-swap
//! to prevent races when multiple threads attempt a transition concurrently.
//!
//! This design mirrors the checkpoint state machines in the C++ and C# FASTER
//! implementations.

use core::sync::atomic::{AtomicU8, Ordering};
use serde::{Deserialize, Serialize};

use super::CheckpointToken;
use crate::error::FasterError;

// ---------------------------------------------------------------------------
// CheckpointPhase
// ---------------------------------------------------------------------------

/// The phase a checkpoint is currently in.
///
/// Variants are ordered to match the natural lifecycle of a checkpoint.
/// Each variant maps to a unique `u8` discriminant used for atomic storage.
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
/// The current phase is stored as an [`AtomicU8`] so it can be read from any
/// thread without locking. Phase transitions use [`compare_exchange`] (CAS) to
/// guarantee that exactly one thread wins a race to advance the phase.
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
    /// Current phase encoded as a `u8`. All reads/writes go through atomic
    /// operations so no `&mut self` is needed.
    phase: AtomicU8,
    /// Token of the currently-active checkpoint. Only meaningful when the
    /// phase is not `Rest`. We use `std::sync::Mutex` for interior
    /// mutability since the token is accessed infrequently (only at phase
    /// boundaries) and correctness matters more than throughput here.
    token: std::sync::Mutex<Option<CheckpointToken>>,
}

impl CheckpointStateMachine {
    /// Creates a new state machine in the [`CheckpointPhase::Rest`] phase.
    #[inline]
    pub fn new() -> Self {
        Self {
            phase: AtomicU8::new(CheckpointPhase::Rest.as_u8()),
            token: std::sync::Mutex::new(None),
        }
    }

    /// Returns the current checkpoint phase.
    ///
    /// This is a lock-free atomic read and is safe to call from any thread.
    #[inline]
    pub fn phase(&self) -> CheckpointPhase {
        let raw = self.phase.load(Ordering::Acquire);
        // SAFETY (logical): we only ever store values produced by `CheckpointPhase::as_u8`,
        // so `from_u8` will always succeed.
        CheckpointPhase::from_u8(raw).expect("corrupted phase discriminant")
    }

    /// Attempts a CAS-based transition from `expected` to `next`.
    ///
    /// Returns `true` if this thread won the race and the transition was
    /// applied, or `false` if the current phase did not match `expected`
    /// (another thread got there first, or the caller's expectation was
    /// stale).
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

        let won = self
            .phase
            .compare_exchange(
                expected.as_u8(),
                next.as_u8(),
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok();
        Ok(won)
    }

    /// Begins a new checkpoint, transitioning from `Rest` to `Prepare`.
    ///
    /// The supplied [`CheckpointToken`] identifies this checkpoint and can be
    /// retrieved via [`active_token`](Self::active_token) until the state
    /// machine is [`reset`](Self::reset).
    ///
    /// # Errors
    ///
    /// Returns [`FasterError::CheckpointError`] if the state machine is not
    /// currently in `Rest` (i.e., a checkpoint is already in progress).
    pub fn start(&self, token: CheckpointToken) -> Result<(), FasterError> {
        let won = self.try_advance(CheckpointPhase::Rest, CheckpointPhase::Prepare)?;
        if !won {
            return Err(FasterError::CheckpointError(
                "checkpoint already in progress (phase is not Rest)".into(),
            ));
        }
        // Store the token. The mutex is uncontended here because only the
        // thread that won the CAS above should be writing.
        *self.token.lock().expect("token mutex poisoned") = Some(token);
        Ok(())
    }

    /// Resets the state machine back to [`CheckpointPhase::Rest`], clearing
    /// the active token.
    ///
    /// This is valid from `Completed` (normal finish) or `Rest` (idempotent
    /// no-op).
    ///
    /// # Errors
    ///
    /// Returns [`FasterError::CheckpointError`] if the current phase is
    /// neither `Completed` nor `Rest`.
    pub fn reset(&self) -> Result<(), FasterError> {
        let current = self.phase();
        match current {
            CheckpointPhase::Rest => Ok(()),
            CheckpointPhase::Completed => {
                let won = self.try_advance(CheckpointPhase::Completed, CheckpointPhase::Rest)?;
                if !won {
                    // Another thread beat us — that's fine, the machine is
                    // presumably already back at Rest.
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

    /// Returns the [`CheckpointToken`] of the active checkpoint, or `None`
    /// if no checkpoint is in progress (phase is `Rest`).
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

    // -----------------------------------------------------------------------
    // Phase ↔ u8 round-trip
    // -----------------------------------------------------------------------

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
        assert_eq!(CheckpointPhase::Prepare.to_string(), "Prepare");
        assert_eq!(CheckpointPhase::InProgress.to_string(), "InProgress");
        assert_eq!(CheckpointPhase::WaitFlush.to_string(), "WaitFlush");
        assert_eq!(
            CheckpointPhase::WaitCompletion.to_string(),
            "WaitCompletion"
        );
        assert_eq!(CheckpointPhase::Completed.to_string(), "Completed");
    }

    // -----------------------------------------------------------------------
    // Valid transition table
    // -----------------------------------------------------------------------

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
        let invalid = [
            (CheckpointPhase::Rest, CheckpointPhase::InProgress),
            (CheckpointPhase::Rest, CheckpointPhase::Completed),
            (CheckpointPhase::Prepare, CheckpointPhase::Rest),
            (CheckpointPhase::Prepare, CheckpointPhase::WaitFlush),
            (CheckpointPhase::InProgress, CheckpointPhase::Completed),
            (CheckpointPhase::WaitFlush, CheckpointPhase::Rest),
            (CheckpointPhase::WaitCompletion, CheckpointPhase::Rest),
            (CheckpointPhase::Completed, CheckpointPhase::Prepare),
        ];
        for (from, to) in invalid {
            assert!(!from.can_advance_to(to), "{from} → {to} should be invalid");
        }
    }

    // -----------------------------------------------------------------------
    // Construction & default
    // -----------------------------------------------------------------------

    #[test]
    fn new_starts_at_rest() {
        let sm = CheckpointStateMachine::new();
        assert_eq!(sm.phase(), CheckpointPhase::Rest);
        assert!(sm.active_token().is_none());
    }

    #[test]
    fn default_starts_at_rest() {
        let sm = CheckpointStateMachine::default();
        assert_eq!(sm.phase(), CheckpointPhase::Rest);
    }

    // -----------------------------------------------------------------------
    // Full lifecycle
    // -----------------------------------------------------------------------

    #[test]
    fn full_lifecycle() {
        let sm = CheckpointStateMachine::new();
        let token = CheckpointToken::new(0xDEAD);

        // Rest → Prepare (via start)
        sm.start(token).unwrap();
        assert_eq!(sm.phase(), CheckpointPhase::Prepare);
        assert_eq!(sm.active_token(), Some(token));

        // Prepare → InProgress
        assert!(
            sm.try_advance(CheckpointPhase::Prepare, CheckpointPhase::InProgress)
                .unwrap()
        );
        assert_eq!(sm.phase(), CheckpointPhase::InProgress);
        assert_eq!(sm.active_token(), Some(token));

        // InProgress → WaitFlush
        assert!(
            sm.try_advance(CheckpointPhase::InProgress, CheckpointPhase::WaitFlush)
                .unwrap()
        );
        assert_eq!(sm.phase(), CheckpointPhase::WaitFlush);

        // WaitFlush → WaitCompletion
        assert!(
            sm.try_advance(CheckpointPhase::WaitFlush, CheckpointPhase::WaitCompletion)
                .unwrap()
        );
        assert_eq!(sm.phase(), CheckpointPhase::WaitCompletion);

        // WaitCompletion → Completed
        assert!(
            sm.try_advance(CheckpointPhase::WaitCompletion, CheckpointPhase::Completed)
                .unwrap()
        );
        assert_eq!(sm.phase(), CheckpointPhase::Completed);
        assert_eq!(sm.active_token(), Some(token));

        // Completed → Rest (via reset)
        sm.reset().unwrap();
        assert_eq!(sm.phase(), CheckpointPhase::Rest);
        assert!(sm.active_token().is_none());
    }

    // -----------------------------------------------------------------------
    // start() / reset() error paths
    // -----------------------------------------------------------------------

    #[test]
    fn start_rejects_when_not_rest() {
        let sm = CheckpointStateMachine::new();
        sm.start(CheckpointToken::new(1)).unwrap();

        // Attempt to start again while in Prepare
        let err = sm.start(CheckpointToken::new(2)).unwrap_err();
        assert!(matches!(err, FasterError::CheckpointError(_)));
    }

    #[test]
    fn reset_idempotent_from_rest() {
        let sm = CheckpointStateMachine::new();
        sm.reset().unwrap(); // no-op
        assert_eq!(sm.phase(), CheckpointPhase::Rest);
    }

    #[test]
    fn reset_rejects_mid_checkpoint() {
        let sm = CheckpointStateMachine::new();
        sm.start(CheckpointToken::new(1)).unwrap();

        // In Prepare — reset should fail
        let err = sm.reset().unwrap_err();
        assert!(matches!(err, FasterError::CheckpointError(_)));
    }

    // -----------------------------------------------------------------------
    // try_advance error paths
    // -----------------------------------------------------------------------

    #[test]
    fn try_advance_illegal_transition_returns_error() {
        let sm = CheckpointStateMachine::new();
        let result = sm.try_advance(CheckpointPhase::Rest, CheckpointPhase::InProgress);
        assert!(result.is_err());
    }

    #[test]
    fn try_advance_wrong_expected_returns_false() {
        let sm = CheckpointStateMachine::new();
        sm.start(CheckpointToken::new(1)).unwrap();
        // Current phase is Prepare, but we expect InProgress
        let won = sm
            .try_advance(CheckpointPhase::InProgress, CheckpointPhase::WaitFlush)
            .unwrap();
        assert!(!won);
        // Phase unchanged
        assert_eq!(sm.phase(), CheckpointPhase::Prepare);
    }

    // -----------------------------------------------------------------------
    // Concurrent transition attempts
    // -----------------------------------------------------------------------

    #[test]
    fn concurrent_start_only_one_wins() {
        use std::sync::{Arc, Barrier};

        let sm = Arc::new(CheckpointStateMachine::new());
        let barrier = Arc::new(Barrier::new(8));
        let mut handles = Vec::new();

        for i in 0..8u128 {
            let sm = Arc::clone(&sm);
            let barrier = Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                sm.start(CheckpointToken::new(i))
            }));
        }

        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let winners: Vec<_> = results.iter().filter(|r| r.is_ok()).collect();
        assert_eq!(winners.len(), 1, "exactly one thread should win the start");
        assert_eq!(sm.phase(), CheckpointPhase::Prepare);
    }

    #[test]
    fn concurrent_advance_only_one_wins() {
        use std::sync::{Arc, Barrier};

        let sm = Arc::new(CheckpointStateMachine::new());
        sm.start(CheckpointToken::new(42)).unwrap();

        let barrier = Arc::new(Barrier::new(8));
        let mut handles = Vec::new();

        for _ in 0..8 {
            let sm = Arc::clone(&sm);
            let barrier = Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                sm.try_advance(CheckpointPhase::Prepare, CheckpointPhase::InProgress)
            }));
        }

        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let winners: usize = results.iter().filter(|r| matches!(r, Ok(true))).count();
        assert_eq!(winners, 1, "exactly one thread should win the CAS");
        assert_eq!(sm.phase(), CheckpointPhase::InProgress);
    }

    // -----------------------------------------------------------------------
    // Debug formatting
    // -----------------------------------------------------------------------

    #[test]
    fn debug_format() {
        let sm = CheckpointStateMachine::new();
        let dbg = format!("{sm:?}");
        assert!(dbg.contains("Rest"));
        assert!(dbg.contains("CheckpointStateMachine"));
    }

    // -----------------------------------------------------------------------
    // Send + Sync
    // -----------------------------------------------------------------------

    #[test]
    fn state_machine_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<CheckpointStateMachine>();
    }

    // -----------------------------------------------------------------------
    // Serde round-trip for CheckpointPhase
    // -----------------------------------------------------------------------

    #[test]
    fn phase_serde_round_trip() {
        let phases = [
            CheckpointPhase::Rest,
            CheckpointPhase::Prepare,
            CheckpointPhase::InProgress,
            CheckpointPhase::WaitFlush,
            CheckpointPhase::WaitCompletion,
            CheckpointPhase::Completed,
        ];
        for phase in phases {
            let json = serde_json::to_string(&phase).unwrap();
            let back: CheckpointPhase = serde_json::from_str(&json).unwrap();
            assert_eq!(phase, back);
        }
    }
}
