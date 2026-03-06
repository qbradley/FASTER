//! Per-session checkpoint state tracking.
//!
//! Each active session maintains a [`SessionCheckpointState`] that records
//! where the session is in the current checkpoint protocol.  When a
//! checkpoint begins, the state is initialised; as the checkpoint
//! progresses the session marks each phase complete and snapshots its
//! serial number and pending-operation count.  Once all phases are done
//! the state can be converted to a [`SessionRecoveryInfo`] for
//! serialisation.

use super::metadata::SessionRecoveryInfo;
use super::state_machine::CheckpointPhase;
use super::CheckpointToken;

// ---------------------------------------------------------------------------
// SessionCheckpointState
// ---------------------------------------------------------------------------

/// Tracks a single session's progress through a checkpoint.
///
/// Created when a new checkpoint starts and carried until the session
/// finishes its part of the protocol.  The struct is intentionally
/// non-`Clone` so ownership stays with the session that created it.
///
/// # Lifecycle
///
/// 1. [`new`](Self::new) — bind the state to a session and checkpoint token.
/// 2. [`snapshot`](Self::snapshot) — capture the session's serial number and
///    pending operation count.
/// 3. [`mark_phase_complete`](Self::mark_phase_complete) — called once per
///    phase, in order, as the session finishes its work for that phase.
/// 4. [`mark_completed`](Self::mark_completed) — the session is done with the
///    entire checkpoint.
/// 5. [`to_recovery_info`](Self::to_recovery_info) — convert to the
///    serialisable [`SessionRecoveryInfo`].
#[derive(Debug)]
pub struct SessionCheckpointState {
    /// Identifier of the session this state belongs to.
    session_id: usize,
    /// Token of the checkpoint this state is participating in.
    checkpoint_token: CheckpointToken,
    /// The latest phase this session has completed.
    phase: CheckpointPhase,
    /// Serial number captured at snapshot time.
    serial_number: u64,
    /// Number of pending (in-flight) operations captured at snapshot time.
    pending_count: u32,
    /// Whether the session has fully finished its checkpoint work.
    completed: bool,
}

impl SessionCheckpointState {
    /// Creates a new checkpoint state for the given session and token.
    ///
    /// The state starts at [`CheckpointPhase::Rest`] with zero serial number
    /// and pending count and `completed` set to `false`.
    #[inline]
    pub fn new(session_id: usize, checkpoint_token: CheckpointToken) -> Self {
        Self {
            session_id,
            checkpoint_token,
            phase: CheckpointPhase::Rest,
            serial_number: 0,
            pending_count: 0,
            completed: false,
        }
    }

    /// Returns the session identifier.
    #[inline]
    pub fn session_id(&self) -> usize {
        self.session_id
    }

    /// Returns the checkpoint token this state is associated with.
    #[inline]
    pub fn checkpoint_token(&self) -> CheckpointToken {
        self.checkpoint_token
    }

    /// Returns the latest phase the session has completed.
    #[inline]
    pub fn phase(&self) -> CheckpointPhase {
        self.phase
    }

    /// Returns the serial number captured at snapshot time.
    #[inline]
    pub fn serial_number(&self) -> u64 {
        self.serial_number
    }

    /// Returns the pending operation count captured at snapshot time.
    #[inline]
    pub fn pending_count(&self) -> u32 {
        self.pending_count
    }

    /// Returns `true` if the session has fully completed its checkpoint work.
    #[inline]
    pub fn is_completed(&self) -> bool {
        self.completed
    }

    /// Advances the session's completed phase.
    ///
    /// Phases must be marked in order — the new phase's discriminant must be
    /// exactly one greater than the current phase.  Attempting to skip a phase
    /// or go backwards is a logic error and will panic in debug builds (and
    /// be silently ignored in release builds).
    pub fn mark_phase_complete(&mut self, phase: CheckpointPhase) {
        debug_assert!(
            phase.as_u8() == self.phase.as_u8() + 1,
            "phases must be marked in order: current={}, requested={}",
            self.phase,
            phase,
        );
        // In release builds, only advance if the requested phase is the
        // expected next phase so callers cannot corrupt the ordering.
        if phase.as_u8() == self.phase.as_u8() + 1 {
            self.phase = phase;
        }
    }

    /// Marks the session as having fully completed its checkpoint work.
    pub fn mark_completed(&mut self) {
        self.completed = true;
    }

    /// Captures the session's current serial number and pending operation count.
    ///
    /// This is typically called during the `Prepare` or `InProgress` phase
    /// so that the checkpoint records a consistent view of the session.
    pub fn snapshot(&mut self, serial_number: u64, pending_count: u32) {
        self.serial_number = serial_number;
        self.pending_count = pending_count;
    }

    /// Converts this checkpoint state into a [`SessionRecoveryInfo`] suitable
    /// for serialisation.
    ///
    /// The `session_id` is narrowed to `u32` (matching the recovery-info
    /// schema).  If the session id exceeds `u32::MAX` the value is truncated;
    /// callers should ensure session ids stay within range.
    pub fn to_recovery_info(&self) -> SessionRecoveryInfo {
        SessionRecoveryInfo {
            session_id: self.session_id as u32,
            serial_number: self.serial_number,
            excluded_serial_numbers: Vec::new(),
        }
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn token(value: u128) -> CheckpointToken {
        CheckpointToken::new(value)
    }

    // -----------------------------------------------------------------------
    // Construction
    // -----------------------------------------------------------------------

    #[test]
    fn new_state_has_correct_defaults() {
        let state = SessionCheckpointState::new(7, token(42));
        assert_eq!(state.session_id(), 7);
        assert_eq!(state.checkpoint_token(), token(42));
        assert_eq!(state.phase(), CheckpointPhase::Rest);
        assert_eq!(state.serial_number(), 0);
        assert_eq!(state.pending_count(), 0);
        assert!(!state.is_completed());
    }

    // -----------------------------------------------------------------------
    // Snapshot
    // -----------------------------------------------------------------------

    #[test]
    fn snapshot_captures_values() {
        let mut state = SessionCheckpointState::new(1, token(1));
        state.snapshot(1000, 5);
        assert_eq!(state.serial_number(), 1000);
        assert_eq!(state.pending_count(), 5);
    }

    #[test]
    fn snapshot_can_be_updated() {
        let mut state = SessionCheckpointState::new(1, token(1));
        state.snapshot(100, 2);
        state.snapshot(200, 3);
        assert_eq!(state.serial_number(), 200);
        assert_eq!(state.pending_count(), 3);
    }

    // -----------------------------------------------------------------------
    // Phase progression
    // -----------------------------------------------------------------------

    #[test]
    fn mark_phase_complete_advances_in_order() {
        let mut state = SessionCheckpointState::new(0, token(1));
        assert_eq!(state.phase(), CheckpointPhase::Rest);

        state.mark_phase_complete(CheckpointPhase::Prepare);
        assert_eq!(state.phase(), CheckpointPhase::Prepare);

        state.mark_phase_complete(CheckpointPhase::InProgress);
        assert_eq!(state.phase(), CheckpointPhase::InProgress);

        state.mark_phase_complete(CheckpointPhase::WaitFlush);
        assert_eq!(state.phase(), CheckpointPhase::WaitFlush);

        state.mark_phase_complete(CheckpointPhase::WaitCompletion);
        assert_eq!(state.phase(), CheckpointPhase::WaitCompletion);

        state.mark_phase_complete(CheckpointPhase::Completed);
        assert_eq!(state.phase(), CheckpointPhase::Completed);
    }

    #[test]
    fn mark_phase_complete_ignores_out_of_order_in_release() {
        // In release mode the guard is a no-op; the phase should not change.
        let mut state = SessionCheckpointState::new(0, token(1));
        // Try to skip straight to InProgress (skipping Prepare).
        // In release this is silently ignored.
        // We cannot test the debug_assert panic in a non-debug build, so
        // we only verify the state stays at Rest.
        if cfg!(not(debug_assertions)) {
            state.mark_phase_complete(CheckpointPhase::InProgress);
            assert_eq!(state.phase(), CheckpointPhase::Rest);
        }
    }

    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "phases must be marked in order")]
    fn mark_phase_complete_panics_on_skip_in_debug() {
        let mut state = SessionCheckpointState::new(0, token(1));
        // Skip Prepare, go straight to InProgress — should panic.
        state.mark_phase_complete(CheckpointPhase::InProgress);
    }

    // -----------------------------------------------------------------------
    // Completion
    // -----------------------------------------------------------------------

    #[test]
    fn mark_completed_sets_flag() {
        let mut state = SessionCheckpointState::new(0, token(1));
        assert!(!state.is_completed());
        state.mark_completed();
        assert!(state.is_completed());
    }

    #[test]
    fn mark_completed_is_idempotent() {
        let mut state = SessionCheckpointState::new(0, token(1));
        state.mark_completed();
        state.mark_completed();
        assert!(state.is_completed());
    }

    // -----------------------------------------------------------------------
    // to_recovery_info
    // -----------------------------------------------------------------------

    #[test]
    fn to_recovery_info_converts_correctly() {
        let mut state = SessionCheckpointState::new(42, token(99));
        state.snapshot(5000, 10);

        let info = state.to_recovery_info();
        assert_eq!(info.session_id, 42);
        assert_eq!(info.serial_number, 5000);
        assert!(info.excluded_serial_numbers.is_empty());
    }

    #[test]
    fn to_recovery_info_with_zero_values() {
        let state = SessionCheckpointState::new(0, token(0));
        let info = state.to_recovery_info();
        assert_eq!(info.session_id, 0);
        assert_eq!(info.serial_number, 0);
        assert!(info.excluded_serial_numbers.is_empty());
    }

    #[test]
    fn to_recovery_info_preserves_session_id() {
        let state = SessionCheckpointState::new(255, token(1));
        let info = state.to_recovery_info();
        assert_eq!(info.session_id, 255);
    }

    // -----------------------------------------------------------------------
    // Full lifecycle
    // -----------------------------------------------------------------------

    #[test]
    fn full_checkpoint_lifecycle() {
        let mut state = SessionCheckpointState::new(3, token(0xABCD));

        // Snapshot during prepare
        state.snapshot(42, 2);
        state.mark_phase_complete(CheckpointPhase::Prepare);
        assert_eq!(state.phase(), CheckpointPhase::Prepare);

        // Continue through phases
        state.mark_phase_complete(CheckpointPhase::InProgress);
        state.mark_phase_complete(CheckpointPhase::WaitFlush);
        state.mark_phase_complete(CheckpointPhase::WaitCompletion);
        state.mark_phase_complete(CheckpointPhase::Completed);
        state.mark_completed();

        assert!(state.is_completed());
        assert_eq!(state.phase(), CheckpointPhase::Completed);
        assert_eq!(state.serial_number(), 42);
        assert_eq!(state.pending_count(), 2);

        // Convert to recovery info
        let info = state.to_recovery_info();
        assert_eq!(info.session_id, 3);
        assert_eq!(info.serial_number, 42);
    }

    // -----------------------------------------------------------------------
    // Multiple checkpoint cycles (reuse after reset)
    // -----------------------------------------------------------------------

    #[test]
    fn multiple_checkpoint_cycles() {
        // Cycle 1
        let mut state = SessionCheckpointState::new(1, token(100));
        state.snapshot(10, 1);
        state.mark_phase_complete(CheckpointPhase::Prepare);
        state.mark_phase_complete(CheckpointPhase::InProgress);
        state.mark_phase_complete(CheckpointPhase::WaitFlush);
        state.mark_phase_complete(CheckpointPhase::WaitCompletion);
        state.mark_phase_complete(CheckpointPhase::Completed);
        state.mark_completed();

        let info1 = state.to_recovery_info();
        assert_eq!(info1.serial_number, 10);

        // Cycle 2 — create a fresh state for the same session with a new token
        let mut state = SessionCheckpointState::new(1, token(200));
        assert_eq!(state.phase(), CheckpointPhase::Rest);
        assert!(!state.is_completed());
        assert_eq!(state.serial_number(), 0);

        state.snapshot(50, 3);
        state.mark_phase_complete(CheckpointPhase::Prepare);
        state.mark_phase_complete(CheckpointPhase::InProgress);
        state.mark_phase_complete(CheckpointPhase::WaitFlush);
        state.mark_phase_complete(CheckpointPhase::WaitCompletion);
        state.mark_phase_complete(CheckpointPhase::Completed);
        state.mark_completed();

        let info2 = state.to_recovery_info();
        assert_eq!(info2.serial_number, 50);
        assert_eq!(state.checkpoint_token(), token(200));
    }

    // -----------------------------------------------------------------------
    // Debug formatting
    // -----------------------------------------------------------------------

    #[test]
    fn debug_format_includes_key_fields() {
        let state = SessionCheckpointState::new(5, token(0xFF));
        let dbg = format!("{state:?}");
        assert!(dbg.contains("SessionCheckpointState"));
        assert!(dbg.contains("session_id"));
    }
}
