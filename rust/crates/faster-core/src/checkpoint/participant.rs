//! Trait for checkpoint participation.
//!
//! Any component that must react to checkpoint phase transitions implements
//! [`CheckpointParticipant`].  The primary implementor is the per-session
//! context ([`FasterKvSession`]), but the trait is kept generic so that
//! other subsystems (e.g. background compaction) can participate too.

use super::session_state::SessionCheckpointState;
use super::state_machine::CheckpointPhase;
use super::CheckpointToken;

// ---------------------------------------------------------------------------
// CheckpointParticipant
// ---------------------------------------------------------------------------

/// A component that participates in the checkpoint protocol.
///
/// When the [`CheckpointStateMachine`](super::CheckpointStateMachine)
/// transitions to a new phase, each registered participant is notified via
/// [`on_checkpoint_phase`](Self::on_checkpoint_phase).  The participant
/// performs whatever work is needed for that phase (e.g. flushing buffers,
/// snapshotting state) and records its progress in a
/// [`SessionCheckpointState`].
///
/// # Contract
///
/// * [`on_checkpoint_phase`](Self::on_checkpoint_phase) will be called
///   **at most once per phase** per checkpoint cycle, in strictly increasing
///   phase order.
/// * Implementations must not block indefinitely — long-running work should
///   be offloaded and reported asynchronously.
pub trait CheckpointParticipant {
    /// Called when the checkpoint enters a new phase.
    ///
    /// The participant should perform its work for `phase` and update its
    /// internal [`SessionCheckpointState`] accordingly.
    fn on_checkpoint_phase(&mut self, phase: CheckpointPhase, token: &CheckpointToken);

    /// Returns the participant's current checkpoint state, or `None` if no
    /// checkpoint is active for this participant.
    fn checkpoint_state(&self) -> Option<&SessionCheckpointState>;

    /// Returns `true` if this participant has finished all of its checkpoint
    /// work (i.e. [`SessionCheckpointState::is_completed`] is `true`).
    fn is_checkpoint_complete(&self) -> bool;
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal mock participant used to verify the trait contract.
    struct MockParticipant {
        state: Option<SessionCheckpointState>,
    }

    impl MockParticipant {
        fn new() -> Self {
            Self { state: None }
        }
    }

    impl CheckpointParticipant for MockParticipant {
        fn on_checkpoint_phase(&mut self, phase: CheckpointPhase, token: &CheckpointToken) {
            let state = self
                .state
                .get_or_insert_with(|| SessionCheckpointState::new(0, *token));

            state.mark_phase_complete(phase);

            // Automatically mark completed when we reach the final phase.
            if phase == CheckpointPhase::Completed {
                state.mark_completed();
            }
        }

        fn checkpoint_state(&self) -> Option<&SessionCheckpointState> {
            self.state.as_ref()
        }

        fn is_checkpoint_complete(&self) -> bool {
            self.state
                .as_ref()
                .map_or(false, |s| s.is_completed())
        }
    }

    // -----------------------------------------------------------------------
    // Basic trait usage
    // -----------------------------------------------------------------------

    #[test]
    fn mock_participant_starts_without_state() {
        let p = MockParticipant::new();
        assert!(p.checkpoint_state().is_none());
        assert!(!p.is_checkpoint_complete());
    }

    #[test]
    fn mock_participant_progresses_through_phases() {
        let mut p = MockParticipant::new();
        let token = CheckpointToken::new(1);

        let phases = [
            CheckpointPhase::Prepare,
            CheckpointPhase::InProgress,
            CheckpointPhase::WaitFlush,
            CheckpointPhase::WaitCompletion,
            CheckpointPhase::Completed,
        ];

        for (i, &phase) in phases.iter().enumerate() {
            p.on_checkpoint_phase(phase, &token);
            let state = p.checkpoint_state().unwrap();
            assert_eq!(state.phase(), phase);

            if i < phases.len() - 1 {
                assert!(!p.is_checkpoint_complete());
            }
        }

        assert!(p.is_checkpoint_complete());
    }

    #[test]
    fn mock_participant_full_cycle_then_new_cycle() {
        let mut p = MockParticipant::new();
        let token1 = CheckpointToken::new(10);

        // First cycle
        for &phase in &[
            CheckpointPhase::Prepare,
            CheckpointPhase::InProgress,
            CheckpointPhase::WaitFlush,
            CheckpointPhase::WaitCompletion,
            CheckpointPhase::Completed,
        ] {
            p.on_checkpoint_phase(phase, &token1);
        }
        assert!(p.is_checkpoint_complete());

        // Reset for second cycle
        let token2 = CheckpointToken::new(20);
        p.state = Some(SessionCheckpointState::new(0, token2));

        assert!(!p.is_checkpoint_complete());
        assert_eq!(
            p.checkpoint_state().unwrap().checkpoint_token(),
            token2
        );
    }

    // -----------------------------------------------------------------------
    // Trait object safety
    // -----------------------------------------------------------------------

    #[test]
    fn trait_is_object_safe() {
        // Verify that CheckpointParticipant can be used as a trait object.
        let mut p = MockParticipant::new();
        let participant: &mut dyn CheckpointParticipant = &mut p;
        let token = CheckpointToken::new(99);
        participant.on_checkpoint_phase(CheckpointPhase::Prepare, &token);
        assert!(!participant.is_checkpoint_complete());
        assert!(participant.checkpoint_state().is_some());
    }
}
