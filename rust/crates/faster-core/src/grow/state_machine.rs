//! Phase-based state machine for hash table grow operations.
//!
//! FASTER grows its hash index in a series of phases, driven by
//! [`GrowStateMachine`]. Each phase performs a specific step in the
//! online-resize protocol, allowing concurrent readers and writers to
//! continue operating throughout the process.
//!
//! # Phases
//!
//! | Phase            | Description                                        |
//! |------------------|----------------------------------------------------|
//! | `Rest`           | No grow in progress                                |
//! | `Prepare`        | Allocate new table, notify sessions                |
//! | `InProgress`     | Actively splitting buckets (chunk-based)            |
//! | `WaitCompletion` | Waiting for all sessions to acknowledge             |
//! | `Completed`      | Grow finished, old table can be reclaimed           |
//!
//! # Valid transitions
//!
//! ```text
//! Rest → Prepare → InProgress → WaitCompletion → Completed → Rest
//! ```
//!
//! Only forward transitions along this chain are allowed. Any other
//! transition returns a [`GrowError`].
//!
//! # Thread safety
//!
//! All phase transitions use compare-and-swap (CAS) via [`AtomicU8`],
//! making the state machine safe to query and advance from any thread.
//!
//! # C++ / C# correspondence
//!
//! | Rust                 | C++ (`grow_state.h`)        | C# (`IndexResizeStateMachine.cs`) |
//! |----------------------|-----------------------------|-----------------------------------|
//! | `GrowPhase`          | `Phase` enum values         | `Phase` enum                      |
//! | `GrowStateMachine`   | `GrowStateMachine`          | `IndexResizeStateMachine`         |
//! | `GrowError`          | (status codes)              | (exceptions)                      |

use core::fmt;
use core::sync::atomic::Ordering;

use crate::sync::{AtomicU8, RwLock, RwLockReadGuard};

use super::GrowState;

// ---------------------------------------------------------------------------
// GrowPhase
// ---------------------------------------------------------------------------

/// Phase of a hash table grow operation.
///
/// Grows proceed through phases in strict order:
/// `Rest → Prepare → InProgress → WaitCompletion → Completed → Rest`.
///
/// Each phase has a fixed `u8` discriminant for atomic storage.
///
/// # Examples
///
/// ```
/// use faster_core::grow::GrowPhase;
///
/// let phase = GrowPhase::Rest;
/// assert_eq!(u8::from(phase), 0);
/// assert_eq!(GrowPhase::try_from(0u8), Ok(GrowPhase::Rest));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum GrowPhase {
    /// No grow in progress.
    Rest = 0,
    /// Allocate new table, notify sessions.
    Prepare = 1,
    /// Actively splitting buckets, chunk by chunk.
    InProgress = 2,
    /// Waiting for all sessions to acknowledge completion.
    WaitCompletion = 3,
    /// Grow finished; old table can be reclaimed.
    Completed = 4,
}

impl GrowPhase {
    /// Returns the valid successor phase, or `None` for terminal states
    /// that require special methods (`Rest` has no predecessor in the
    /// forward chain; `Completed` loops back via `reset()`).
    fn next(self) -> Option<GrowPhase> {
        match self {
            GrowPhase::Rest => Some(GrowPhase::Prepare),
            GrowPhase::Prepare => Some(GrowPhase::InProgress),
            GrowPhase::InProgress => Some(GrowPhase::WaitCompletion),
            GrowPhase::WaitCompletion => Some(GrowPhase::Completed),
            GrowPhase::Completed => None, // must use reset()
        }
    }
}

impl From<GrowPhase> for u8 {
    #[inline]
    fn from(phase: GrowPhase) -> u8 {
        phase as u8
    }
}

impl TryFrom<u8> for GrowPhase {
    type Error = u8;

    /// Converts a `u8` to a `GrowPhase`, returning `Err(value)` for
    /// unrecognised discriminants.
    #[inline]
    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(GrowPhase::Rest),
            1 => Ok(GrowPhase::Prepare),
            2 => Ok(GrowPhase::InProgress),
            3 => Ok(GrowPhase::WaitCompletion),
            4 => Ok(GrowPhase::Completed),
            other => Err(other),
        }
    }
}

impl fmt::Display for GrowPhase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GrowPhase::Rest => write!(f, "Rest"),
            GrowPhase::Prepare => write!(f, "Prepare"),
            GrowPhase::InProgress => write!(f, "InProgress"),
            GrowPhase::WaitCompletion => write!(f, "WaitCompletion"),
            GrowPhase::Completed => write!(f, "Completed"),
        }
    }
}

// ---------------------------------------------------------------------------
// GrowError
// ---------------------------------------------------------------------------

/// Error type for grow state machine operations.
///
/// These errors indicate invalid state transitions or API misuse — they are
/// never returned during normal grow operation.
///
/// # Examples
///
/// ```
/// use faster_core::grow::{GrowError, GrowPhase};
///
/// let err = GrowError::AlreadyInProgress;
/// assert!(err.to_string().contains("already in progress"));
///
/// let err = GrowError::NotInExpectedPhase {
///     expected: GrowPhase::Rest,
///     actual: GrowPhase::InProgress,
/// };
/// assert!(err.to_string().contains("Rest"));
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GrowError {
    /// A grow is already in progress; cannot start another.
    AlreadyInProgress,
    /// The state machine is not in the expected phase for this transition.
    NotInExpectedPhase {
        /// The phase that was required for the transition.
        expected: GrowPhase,
        /// The phase the state machine is actually in.
        actual: GrowPhase,
    },
    /// The requested table size is invalid.
    InvalidSize(String),
}

impl fmt::Display for GrowError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GrowError::AlreadyInProgress => {
                write!(f, "grow is already in progress")
            }
            GrowError::NotInExpectedPhase { expected, actual } => {
                write!(
                    f,
                    "expected phase {expected}, but state machine is in {actual}"
                )
            }
            GrowError::InvalidSize(reason) => {
                write!(f, "invalid grow target size: {reason}")
            }
        }
    }
}

impl std::error::Error for GrowError {}

// ---------------------------------------------------------------------------
// GrowStateMachine
// ---------------------------------------------------------------------------

/// Phase-based state machine that drives hash table grow operations.
///
/// The state machine tracks the current [`GrowPhase`] atomically and holds
/// the [`GrowState`] that records per-grow progress (chunk claims, version
/// numbers). Phase transitions use compare-and-swap for thread safety.
///
/// # Lifecycle
///
/// 1. Call [`start`](Self::start) to transition `Rest → Prepare`.
/// 2. Advance through phases with [`try_advance`](Self::try_advance).
/// 3. Monitor progress with [`progress`](Self::progress).
/// 4. After `Completed`, call [`reset`](Self::reset) to return to `Rest`.
///
/// # Examples
///
/// ```
/// use faster_core::grow::{GrowStateMachine, GrowPhase};
///
/// let sm = GrowStateMachine::new();
/// assert_eq!(sm.phase(), GrowPhase::Rest);
///
/// sm.start(0, 16).unwrap();
/// assert_eq!(sm.phase(), GrowPhase::Prepare);
/// assert!(sm.grow_state().is_some());
/// ```
pub struct GrowStateMachine {
    /// Current phase, stored as `u8` for atomic CAS.
    phase: AtomicU8,
    /// Grow state for the active operation. Only valid when phase ≠ Rest.
    /// Protected by the phase transitions (only set during `start`, only
    /// cleared during `reset`).
    state: RwLock<Option<GrowState>>,
    /// Target table size in log2 bits for the current grow.
    new_size_bits: AtomicU8,
}

impl GrowStateMachine {
    /// Creates a new state machine in the `Rest` phase.
    ///
    /// # Examples
    ///
    /// ```
    /// use faster_core::grow::{GrowStateMachine, GrowPhase};
    ///
    /// let sm = GrowStateMachine::new();
    /// assert_eq!(sm.phase(), GrowPhase::Rest);
    /// assert!(sm.grow_state().is_none());
    /// ```
    pub fn new() -> Self {
        Self {
            phase: AtomicU8::new(GrowPhase::Rest as u8),
            state: RwLock::new(None),
            new_size_bits: AtomicU8::new(0),
        }
    }

    /// Returns the current phase of the state machine.
    ///
    /// This is a lock-free atomic read.
    #[inline]
    pub fn phase(&self) -> GrowPhase {
        let raw = self.phase.load(Ordering::Acquire);
        GrowPhase::try_from(raw).expect("corrupted grow phase")
    }

    /// Attempts a CAS-based phase transition from `expected` to `next`.
    ///
    /// Returns `true` if this thread won the CAS race and the transition
    /// was applied, `false` if the current phase did not match `expected`.
    ///
    /// Only the valid forward transitions are permitted:
    /// `Prepare → InProgress → WaitCompletion → Completed`.
    /// For `Rest → Prepare` use [`start`](Self::start); for
    /// `Completed → Rest` use [`reset`](Self::reset).
    ///
    /// # Errors
    ///
    /// Returns [`GrowError::NotInExpectedPhase`] if `next` is not the
    /// valid successor of `expected` according to the grow protocol.
    ///
    /// # Examples
    ///
    /// ```
    /// use faster_core::grow::{GrowStateMachine, GrowPhase};
    ///
    /// let sm = GrowStateMachine::new();
    /// sm.start(0, 16).unwrap();
    ///
    /// assert!(sm.try_advance(GrowPhase::Prepare, GrowPhase::InProgress).unwrap());
    /// assert_eq!(sm.phase(), GrowPhase::InProgress);
    /// ```
    pub fn try_advance(&self, expected: GrowPhase, next: GrowPhase) -> Result<bool, GrowError> {
        // Validate that `next` is the valid successor of `expected`.
        let valid_next = expected.next().ok_or(GrowError::NotInExpectedPhase {
            expected,
            actual: self.phase(),
        })?;
        if next != valid_next {
            return Err(GrowError::NotInExpectedPhase {
                expected: valid_next,
                actual: next,
            });
        }

        let result = self.phase.compare_exchange(
            expected as u8,
            next as u8,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        Ok(result.is_ok())
    }

    /// Initiates a grow operation, transitioning from `Rest` to `Prepare`.
    ///
    /// `old_version` is the current hash table version (0 or 1).
    /// `new_size_bits` is the log₂ of the target bucket count (e.g., 17
    /// for 131 072 buckets). The old table size is `1 << (new_size_bits - 1)`.
    ///
    /// # Errors
    ///
    /// - [`GrowError::AlreadyInProgress`] if the state machine is not in `Rest`.
    /// - [`GrowError::InvalidSize`] if `new_size_bits` is too small (< 1) or
    ///   too large (> 62).
    ///
    /// # Examples
    ///
    /// ```
    /// use faster_core::grow::{GrowStateMachine, GrowPhase};
    ///
    /// let sm = GrowStateMachine::new();
    /// sm.start(0, 17).unwrap();
    /// assert_eq!(sm.phase(), GrowPhase::Prepare);
    /// ```
    pub fn start(&self, old_version: u32, new_size_bits: u8) -> Result<(), GrowError> {
        if !(1..=62).contains(&new_size_bits) {
            return Err(GrowError::InvalidSize(format!(
                "new_size_bits must be in 1..=62, got {new_size_bits}"
            )));
        }

        // CAS Rest → Prepare
        let result = self.phase.compare_exchange(
            GrowPhase::Rest as u8,
            GrowPhase::Prepare as u8,
            Ordering::AcqRel,
            Ordering::Acquire,
        );

        match result {
            Ok(_) => {
                // We won the CAS — set up grow state.
                let old_table_size = 1u64 << (new_size_bits - 1);
                let new_version = if old_version == 0 { 1 } else { 0 };
                let grow_state = GrowState::new(old_version, new_version, old_table_size);

                let mut guard = self.state.write().unwrap();
                *guard = Some(grow_state);
                self.new_size_bits.store(new_size_bits, Ordering::Release);
                Ok(())
            }
            Err(actual_raw) => {
                let actual = GrowPhase::try_from(actual_raw).expect("corrupted grow phase");
                if actual == GrowPhase::Rest {
                    // Spurious CAS failure — treat as contention.
                    Err(GrowError::AlreadyInProgress)
                } else {
                    Err(GrowError::AlreadyInProgress)
                }
            }
        }
    }

    /// Resets the state machine from `Completed` back to `Rest`.
    ///
    /// This clears the internal [`GrowState`], allowing a new grow to start.
    ///
    /// # Errors
    ///
    /// Returns [`GrowError::NotInExpectedPhase`] if the current phase is
    /// not `Completed`.
    ///
    /// # Examples
    ///
    /// ```
    /// use faster_core::grow::{GrowStateMachine, GrowPhase};
    ///
    /// let sm = GrowStateMachine::new();
    /// sm.start(0, 16).unwrap();
    /// sm.try_advance(GrowPhase::Prepare, GrowPhase::InProgress).unwrap();
    /// sm.try_advance(GrowPhase::InProgress, GrowPhase::WaitCompletion).unwrap();
    /// sm.try_advance(GrowPhase::WaitCompletion, GrowPhase::Completed).unwrap();
    /// sm.reset().unwrap();
    /// assert_eq!(sm.phase(), GrowPhase::Rest);
    /// assert!(sm.grow_state().is_none());
    /// ```
    pub fn reset(&self) -> Result<(), GrowError> {
        let result = self.phase.compare_exchange(
            GrowPhase::Completed as u8,
            GrowPhase::Rest as u8,
            Ordering::AcqRel,
            Ordering::Acquire,
        );

        match result {
            Ok(_) => {
                let mut guard = self.state.write().unwrap();
                *guard = None;
                self.new_size_bits.store(0, Ordering::Release);
                Ok(())
            }
            Err(actual_raw) => {
                let actual = GrowPhase::try_from(actual_raw).expect("corrupted grow phase");
                Err(GrowError::NotInExpectedPhase {
                    expected: GrowPhase::Completed,
                    actual,
                })
            }
        }
    }

    /// Returns a reference to the active [`GrowState`], or `None` if no
    /// grow is in progress (phase is `Rest`).
    ///
    /// The returned guard holds a read-lock on the internal state. Drop
    /// it promptly to avoid blocking `start()` / `reset()`.
    ///
    /// # Examples
    ///
    /// ```
    /// use faster_core::grow::GrowStateMachine;
    ///
    /// let sm = GrowStateMachine::new();
    /// assert!(sm.grow_state().is_none());
    ///
    /// sm.start(0, 16).unwrap();
    /// let state = sm.grow_state().unwrap();
    /// assert_eq!(state.old_version(), 0);
    /// ```
    pub fn grow_state(&self) -> Option<GrowStateGuard<'_>> {
        let guard = self.state.read().unwrap();
        if guard.is_some() {
            Some(GrowStateGuard { guard })
        } else {
            None
        }
    }

    /// Returns the target table size in log₂ bits, or 0 if no grow is active.
    #[inline]
    pub fn new_size_bits(&self) -> u8 {
        self.new_size_bits.load(Ordering::Acquire)
    }

    /// Returns the fraction of chunks completed (0.0 to 1.0).
    ///
    /// Delegates to the active [`GrowState`]. Returns 0.0 if no grow is
    /// in progress.
    ///
    /// # Examples
    ///
    /// ```
    /// use faster_core::grow::GrowStateMachine;
    ///
    /// let sm = GrowStateMachine::new();
    /// assert_eq!(sm.progress(), 0.0);
    ///
    /// sm.start(0, 16).unwrap();
    /// assert!(sm.progress() >= 0.0 && sm.progress() <= 1.0);
    /// ```
    pub fn progress(&self) -> f64 {
        let guard = self.state.read().unwrap();
        match guard.as_ref() {
            Some(state) => {
                let total = state.num_chunks() as f64;
                if total == 0.0 {
                    return 1.0;
                }
                let pending = state.pending_chunks() as f64;
                (total - pending) / total
            }
            None => 0.0,
        }
    }
}

impl Default for GrowStateMachine {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for GrowStateMachine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GrowStateMachine")
            .field("phase", &self.phase())
            .field("new_size_bits", &self.new_size_bits())
            .field("state", &*self.state.read().unwrap())
            .finish()
    }
}

// ---------------------------------------------------------------------------
// GrowStateGuard — RAII handle for reading GrowState
// ---------------------------------------------------------------------------

/// RAII guard returned by [`GrowStateMachine::grow_state`].
///
/// Dereferences to `&GrowState`. The guard holds a read-lock; drop it
/// promptly to avoid blocking state transitions.
pub struct GrowStateGuard<'a> {
    guard: RwLockReadGuard<'a, Option<GrowState>>,
}

impl<'a> core::ops::Deref for GrowStateGuard<'a> {
    type Target = GrowState;

    #[inline]
    fn deref(&self) -> &GrowState {
        self.guard
            .as_ref()
            .expect("GrowStateGuard created with None state")
    }
}

impl<'a> fmt::Debug for GrowStateGuard<'a> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&**self, f)
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // GrowPhase conversions
    // -----------------------------------------------------------------------

    #[test]
    fn phase_u8_roundtrip() {
        for (phase, val) in [
            (GrowPhase::Rest, 0),
            (GrowPhase::Prepare, 1),
            (GrowPhase::InProgress, 2),
            (GrowPhase::WaitCompletion, 3),
            (GrowPhase::Completed, 4),
        ] {
            assert_eq!(u8::from(phase), val);
            assert_eq!(GrowPhase::try_from(val), Ok(phase));
        }
    }

    #[test]
    fn phase_invalid_u8() {
        assert_eq!(GrowPhase::try_from(5u8), Err(5));
        assert_eq!(GrowPhase::try_from(255u8), Err(255));
    }

    #[test]
    fn phase_display() {
        assert_eq!(GrowPhase::Rest.to_string(), "Rest");
        assert_eq!(GrowPhase::Prepare.to_string(), "Prepare");
        assert_eq!(GrowPhase::InProgress.to_string(), "InProgress");
        assert_eq!(GrowPhase::WaitCompletion.to_string(), "WaitCompletion");
        assert_eq!(GrowPhase::Completed.to_string(), "Completed");
    }

    #[test]
    fn phase_derives() {
        let a = GrowPhase::Rest;
        let b = a; // Copy
        let c = a; // Copy (GrowPhase implements Copy)
        assert_eq!(a, b);
        assert_eq!(b, c);

        // Hash
        use std::collections::HashSet;
        let mut set = HashSet::new();
        set.insert(GrowPhase::Rest);
        set.insert(GrowPhase::Prepare);
        assert_eq!(set.len(), 2);

        // Debug
        let dbg = format!("{a:?}");
        assert_eq!(dbg, "Rest");
    }

    // -----------------------------------------------------------------------
    // GrowError
    // -----------------------------------------------------------------------

    #[test]
    fn error_display_already_in_progress() {
        let err = GrowError::AlreadyInProgress;
        assert!(err.to_string().contains("already in progress"));
    }

    #[test]
    fn error_display_not_in_expected_phase() {
        let err = GrowError::NotInExpectedPhase {
            expected: GrowPhase::Rest,
            actual: GrowPhase::InProgress,
        };
        let msg = err.to_string();
        assert!(msg.contains("Rest"));
        assert!(msg.contains("InProgress"));
    }

    #[test]
    fn error_display_invalid_size() {
        let err = GrowError::InvalidSize("too small".into());
        assert!(err.to_string().contains("too small"));
    }

    #[test]
    fn error_implements_std_error() {
        let err: Box<dyn std::error::Error> = Box::new(GrowError::AlreadyInProgress);
        let _ = err.to_string();
    }

    // -----------------------------------------------------------------------
    // GrowStateMachine — construction
    // -----------------------------------------------------------------------

    #[test]
    fn new_starts_at_rest() {
        let sm = GrowStateMachine::new();
        assert_eq!(sm.phase(), GrowPhase::Rest);
        assert!(sm.grow_state().is_none());
        assert_eq!(sm.new_size_bits(), 0);
        assert_eq!(sm.progress(), 0.0);
    }

    #[test]
    fn default_same_as_new() {
        let sm = GrowStateMachine::default();
        assert_eq!(sm.phase(), GrowPhase::Rest);
    }

    #[test]
    fn debug_formatting() {
        let sm = GrowStateMachine::new();
        let dbg = format!("{sm:?}");
        assert!(dbg.contains("GrowStateMachine"));
        assert!(dbg.contains("Rest"));
    }

    // -----------------------------------------------------------------------
    // GrowStateMachine — normal lifecycle
    // -----------------------------------------------------------------------

    #[test]
    fn full_lifecycle() {
        let sm = GrowStateMachine::new();

        // Rest → Prepare
        sm.start(0, 16).unwrap();
        assert_eq!(sm.phase(), GrowPhase::Prepare);
        assert_eq!(sm.new_size_bits(), 16);

        // Check grow state
        {
            let gs = sm.grow_state().unwrap();
            assert_eq!(gs.old_version(), 0);
            assert_eq!(gs.new_version(), 1);
        }

        // Prepare → InProgress
        assert!(
            sm.try_advance(GrowPhase::Prepare, GrowPhase::InProgress)
                .unwrap()
        );
        assert_eq!(sm.phase(), GrowPhase::InProgress);

        // InProgress → WaitCompletion
        assert!(
            sm.try_advance(GrowPhase::InProgress, GrowPhase::WaitCompletion)
                .unwrap()
        );
        assert_eq!(sm.phase(), GrowPhase::WaitCompletion);

        // WaitCompletion → Completed
        assert!(
            sm.try_advance(GrowPhase::WaitCompletion, GrowPhase::Completed)
                .unwrap()
        );
        assert_eq!(sm.phase(), GrowPhase::Completed);

        // Completed → Rest
        sm.reset().unwrap();
        assert_eq!(sm.phase(), GrowPhase::Rest);
        assert!(sm.grow_state().is_none());
        assert_eq!(sm.new_size_bits(), 0);
    }

    #[test]
    fn multiple_lifecycles() {
        let sm = GrowStateMachine::new();

        for i in 0..3 {
            let version = i % 2;
            sm.start(version, 16).unwrap();
            sm.try_advance(GrowPhase::Prepare, GrowPhase::InProgress)
                .unwrap();
            sm.try_advance(GrowPhase::InProgress, GrowPhase::WaitCompletion)
                .unwrap();
            sm.try_advance(GrowPhase::WaitCompletion, GrowPhase::Completed)
                .unwrap();
            sm.reset().unwrap();
            assert_eq!(sm.phase(), GrowPhase::Rest);
        }
    }

    // -----------------------------------------------------------------------
    // GrowStateMachine — invalid transitions
    // -----------------------------------------------------------------------

    #[test]
    fn start_when_not_rest() {
        let sm = GrowStateMachine::new();
        sm.start(0, 16).unwrap();
        let err = sm.start(0, 17).unwrap_err();
        assert_eq!(err, GrowError::AlreadyInProgress);
    }

    #[test]
    fn advance_wrong_expected_phase() {
        let sm = GrowStateMachine::new();
        sm.start(0, 16).unwrap();
        // Current is Prepare, but we pass InProgress as expected
        let ok = sm
            .try_advance(GrowPhase::InProgress, GrowPhase::WaitCompletion)
            .unwrap();
        assert!(!ok); // CAS fails because actual is Prepare
    }

    #[test]
    fn advance_wrong_successor() {
        let sm = GrowStateMachine::new();
        sm.start(0, 16).unwrap();
        // Prepare → WaitCompletion is not valid (skips InProgress)
        let err = sm
            .try_advance(GrowPhase::Prepare, GrowPhase::WaitCompletion)
            .unwrap_err();
        assert!(matches!(err, GrowError::NotInExpectedPhase { .. }));
    }

    #[test]
    fn advance_from_completed_errors() {
        let sm = GrowStateMachine::new();
        sm.start(0, 16).unwrap();
        sm.try_advance(GrowPhase::Prepare, GrowPhase::InProgress)
            .unwrap();
        sm.try_advance(GrowPhase::InProgress, GrowPhase::WaitCompletion)
            .unwrap();
        sm.try_advance(GrowPhase::WaitCompletion, GrowPhase::Completed)
            .unwrap();

        // try_advance from Completed should error (must use reset())
        let err = sm
            .try_advance(GrowPhase::Completed, GrowPhase::Rest)
            .unwrap_err();
        assert!(matches!(err, GrowError::NotInExpectedPhase { .. }));
    }

    #[test]
    fn reset_when_not_completed() {
        let sm = GrowStateMachine::new();
        let err = sm.reset().unwrap_err();
        assert!(matches!(
            err,
            GrowError::NotInExpectedPhase {
                expected: GrowPhase::Completed,
                actual: GrowPhase::Rest,
            }
        ));

        sm.start(0, 16).unwrap();
        let err = sm.reset().unwrap_err();
        assert!(matches!(
            err,
            GrowError::NotInExpectedPhase {
                expected: GrowPhase::Completed,
                actual: GrowPhase::Prepare,
            }
        ));
    }

    // -----------------------------------------------------------------------
    // GrowStateMachine — start() validation
    // -----------------------------------------------------------------------

    #[test]
    fn start_invalid_size_zero() {
        let sm = GrowStateMachine::new();
        let err = sm.start(0, 0).unwrap_err();
        assert!(matches!(err, GrowError::InvalidSize(_)));
    }

    #[test]
    fn start_invalid_size_too_large() {
        let sm = GrowStateMachine::new();
        let err = sm.start(0, 63).unwrap_err();
        assert!(matches!(err, GrowError::InvalidSize(_)));
    }

    #[test]
    fn start_boundary_sizes() {
        // Minimum valid: 1 bit → table of 1 bucket
        let sm = GrowStateMachine::new();
        sm.start(0, 1).unwrap();
        assert_eq!(sm.phase(), GrowPhase::Prepare);

        sm.try_advance(GrowPhase::Prepare, GrowPhase::InProgress)
            .unwrap();
        sm.try_advance(GrowPhase::InProgress, GrowPhase::WaitCompletion)
            .unwrap();
        sm.try_advance(GrowPhase::WaitCompletion, GrowPhase::Completed)
            .unwrap();
        sm.reset().unwrap();

        // Maximum valid: 62 bits
        sm.start(0, 62).unwrap();
        assert_eq!(sm.phase(), GrowPhase::Prepare);
    }

    #[test]
    fn start_version_swap() {
        let sm = GrowStateMachine::new();
        sm.start(1, 16).unwrap();
        {
            let gs = sm.grow_state().unwrap();
            assert_eq!(gs.old_version(), 1);
            assert_eq!(gs.new_version(), 0);
        }
    }

    // -----------------------------------------------------------------------
    // GrowStateMachine — progress
    // -----------------------------------------------------------------------

    #[test]
    fn progress_no_grow() {
        let sm = GrowStateMachine::new();
        assert_eq!(sm.progress(), 0.0);
    }

    #[test]
    fn progress_tracks_chunks() {
        let sm = GrowStateMachine::new();
        // 2^15 = 32768 old table → 2 chunks (32768 / 16384)
        sm.start(0, 16).unwrap();
        assert_eq!(sm.progress(), 0.0);

        // Simulate completing one chunk
        {
            let gs = sm.grow_state().unwrap();
            assert_eq!(gs.num_chunks(), 2);
            gs.complete_chunk();
        }
        assert!((sm.progress() - 0.5).abs() < f64::EPSILON);

        // Complete the second chunk
        {
            let gs = sm.grow_state().unwrap();
            gs.complete_chunk();
        }
        assert!((sm.progress() - 1.0).abs() < f64::EPSILON);
    }

    // -----------------------------------------------------------------------
    // GrowStateMachine — concurrent transitions
    // -----------------------------------------------------------------------

    #[test]
    fn concurrent_start_only_one_wins() {
        use std::sync::{Arc, Barrier};

        let sm = Arc::new(GrowStateMachine::new());
        let barrier = Arc::new(Barrier::new(8));
        let mut handles = Vec::new();

        for _ in 0..8 {
            let sm = Arc::clone(&sm);
            let barrier = Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                sm.start(0, 16)
            }));
        }

        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let successes = results.iter().filter(|r| r.is_ok()).count();
        let failures = results
            .iter()
            .filter(|r| matches!(r, Err(GrowError::AlreadyInProgress)))
            .count();

        assert_eq!(successes, 1, "exactly one thread should win the CAS");
        assert_eq!(failures, 7, "the rest should get AlreadyInProgress");
        assert_eq!(sm.phase(), GrowPhase::Prepare);
    }

    #[test]
    fn concurrent_advance_only_one_wins() {
        use std::sync::{Arc, Barrier};

        let sm = Arc::new(GrowStateMachine::new());
        sm.start(0, 16).unwrap();

        let barrier = Arc::new(Barrier::new(8));
        let mut handles = Vec::new();

        for _ in 0..8 {
            let sm = Arc::clone(&sm);
            let barrier = Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                sm.try_advance(GrowPhase::Prepare, GrowPhase::InProgress)
            }));
        }

        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let successes = results.iter().filter(|r| **r == Ok(true)).count();
        let cas_failures = results.iter().filter(|r| **r == Ok(false)).count();

        assert_eq!(successes, 1, "exactly one thread should win the CAS");
        assert_eq!(cas_failures, 7, "the rest should lose the CAS");
        assert_eq!(sm.phase(), GrowPhase::InProgress);
    }

    #[test]
    fn concurrent_reset_only_one_wins() {
        use std::sync::{Arc, Barrier};

        let sm = Arc::new(GrowStateMachine::new());
        sm.start(0, 16).unwrap();
        sm.try_advance(GrowPhase::Prepare, GrowPhase::InProgress)
            .unwrap();
        sm.try_advance(GrowPhase::InProgress, GrowPhase::WaitCompletion)
            .unwrap();
        sm.try_advance(GrowPhase::WaitCompletion, GrowPhase::Completed)
            .unwrap();

        let barrier = Arc::new(Barrier::new(4));
        let mut handles = Vec::new();

        for _ in 0..4 {
            let sm = Arc::clone(&sm);
            let barrier = Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                sm.reset()
            }));
        }

        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let successes = results.iter().filter(|r| r.is_ok()).count();
        assert_eq!(successes, 1, "exactly one thread should win the reset CAS");
        assert_eq!(sm.phase(), GrowPhase::Rest);
    }

    // -----------------------------------------------------------------------
    // GrowStateGuard
    // -----------------------------------------------------------------------

    #[test]
    fn grow_state_guard_deref() {
        let sm = GrowStateMachine::new();
        sm.start(0, 16).unwrap();
        let guard = sm.grow_state().unwrap();
        // Deref to GrowState
        assert_eq!(guard.old_version(), 0);
        assert_eq!(guard.new_version(), 1);
        // Debug
        let dbg = format!("{guard:?}");
        assert!(dbg.contains("GrowState"));
    }
}
