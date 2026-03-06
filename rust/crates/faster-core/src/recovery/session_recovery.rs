//! Session recovery engine for FASTER.
//!
//! Restores per-session state from checkpoint data so that sessions can
//! resume from where they left off.  The [`SessionRecoveryEngine`] reads
//! [`SessionRecoveryInfo`] entries from a [`RecoveryPlan`] and produces a
//! [`RecoveredSession`] for each one, flagging sessions that had pending
//! (in-flight) operations at checkpoint time as needing replay.
//!
//! # Recovery protocol
//!
//! 1. Load the `session_infos` from the [`RecoveryPlan`].
//! 2. Validate uniqueness of session IDs.
//! 3. For each [`SessionRecoveryInfo`], compute `needs_replay` — `true` when
//!    `excluded_serial_numbers` is non-empty (operations were in-flight at
//!    checkpoint time and may have been lost).
//! 4. Return the list of [`RecoveredSession`]s.  The caller (`FasterKv`) is
//!    responsible for actual replay or user notification.
//!
//! [`SessionRecoveryInfo`]: crate::checkpoint::SessionRecoveryInfo

use std::collections::HashSet;

use crate::checkpoint::SessionRecoveryInfo;

use super::{RecoveryError, RecoveryPlan};

// ---------------------------------------------------------------------------
// RecoveredSession
// ---------------------------------------------------------------------------

/// A session whose state has been restored from a checkpoint.
///
/// Produced by [`SessionRecoveryEngine::recover_sessions`].  The caller can
/// inspect [`needs_replay`](Self::needs_replay) to decide whether the
/// session's pending operations must be replayed before it is safe to
/// resume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveredSession {
    /// Unique identifier for the session.
    pub session_id: usize,
    /// The serial number to resume from.
    pub serial_number: u64,
    /// Number of operations that were pending (in-flight) at checkpoint time.
    pub pending_count: u32,
    /// Whether this session has operations that may need to be replayed
    /// (`pending_count > 0`).
    pub needs_replay: bool,
}

// ---------------------------------------------------------------------------
// SessionRecoveryEngine
// ---------------------------------------------------------------------------

/// Restores per-session state from checkpoint data.
///
/// This engine is intentionally stateless — all information comes from the
/// [`RecoveryPlan`] (or individual [`SessionRecoveryInfo`] entries).
///
/// # Examples
///
/// ```
/// use faster_core::recovery::session_recovery::{SessionRecoveryEngine, RecoveredSession};
/// use faster_core::recovery::RecoveryPlan;
/// use faster_core::checkpoint::{
///     CheckpointToken, CheckpointType, IndexRecoveryInfo, LogRecoveryInfo,
///     SessionRecoveryInfo,
/// };
///
/// let plan = RecoveryPlan {
///     token: CheckpointToken::new(1),
///     checkpoint_type: CheckpointType::FoldOver,
///     index_info: IndexRecoveryInfo::default(),
///     log_info: LogRecoveryInfo::default(),
///     session_infos: vec![
///         SessionRecoveryInfo {
///             session_id: 0,
///             serial_number: 100,
///             excluded_serial_numbers: vec![],
///         },
///     ],
///     steps: vec![],
/// };
///
/// let engine = SessionRecoveryEngine::new();
/// let sessions = engine.recover_sessions(&plan).unwrap();
/// assert_eq!(sessions.len(), 1);
/// assert!(!sessions[0].needs_replay);
/// ```
pub struct SessionRecoveryEngine {
    _private: (),
}

impl SessionRecoveryEngine {
    /// Creates a new `SessionRecoveryEngine`.
    #[inline]
    pub fn new() -> Self {
        Self { _private: () }
    }

    /// Recovers all sessions described in the [`RecoveryPlan`].
    ///
    /// Returns one [`RecoveredSession`] per entry in
    /// [`RecoveryPlan::session_infos`].  Returns an error if any
    /// validation check fails (e.g. duplicate session IDs).
    pub fn recover_sessions(
        &self,
        plan: &RecoveryPlan,
    ) -> Result<Vec<RecoveredSession>, RecoveryError> {
        if plan.session_infos.is_empty() {
            return Ok(Vec::new());
        }

        Self::validate_session_infos(&plan.session_infos)?;

        plan.session_infos
            .iter()
            .map(|info| self.recover_session(info))
            .collect()
    }

    /// Processes a single [`SessionRecoveryInfo`] into a [`RecoveredSession`].
    ///
    /// The session is marked as needing replay when it had pending
    /// (excluded) operations at checkpoint time.
    pub fn recover_session(
        &self,
        info: &SessionRecoveryInfo,
    ) -> Result<RecoveredSession, RecoveryError> {
        Self::validate_session_info(info)?;

        let pending_count = info.excluded_serial_numbers.len() as u32;
        let needs_replay = pending_count > 0;

        Ok(RecoveredSession {
            session_id: info.session_id as usize,
            serial_number: info.serial_number,
            pending_count,
            needs_replay,
        })
    }

    // -----------------------------------------------------------------------
    // Validation helpers
    // -----------------------------------------------------------------------

    /// Validates the entire set of session infos for consistency.
    fn validate_session_infos(infos: &[SessionRecoveryInfo]) -> Result<(), RecoveryError> {
        let mut seen_ids = HashSet::with_capacity(infos.len());
        for info in infos {
            if !seen_ids.insert(info.session_id) {
                return Err(RecoveryError::CorruptMetadata(format!(
                    "duplicate session ID: {}",
                    info.session_id,
                )));
            }
        }
        Ok(())
    }

    /// Validates a single session info entry.
    fn validate_session_info(info: &SessionRecoveryInfo) -> Result<(), RecoveryError> {
        // Serial number 0 is valid only when the session has done no work.
        // We allow it but flag truly anomalous state: a non-zero
        // excluded-serial-numbers list paired with serial_number == 0 means
        // something went wrong during the checkpoint.
        if info.serial_number == 0 && !info.excluded_serial_numbers.is_empty() {
            return Err(RecoveryError::CorruptMetadata(format!(
                "session {} has excluded serial numbers but serial_number is 0",
                info.session_id,
            )));
        }
        Ok(())
    }
}

impl Default for SessionRecoveryEngine {
    fn default() -> Self {
        Self::new()
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checkpoint::{
        CheckpointToken, CheckpointType, IndexRecoveryInfo, LogRecoveryInfo, SessionRecoveryInfo,
    };

    // -- helpers ------------------------------------------------------------

    fn empty_plan() -> RecoveryPlan {
        RecoveryPlan {
            token: CheckpointToken::new(1),
            checkpoint_type: CheckpointType::FoldOver,
            index_info: IndexRecoveryInfo::default(),
            log_info: LogRecoveryInfo::default(),
            session_infos: vec![],
            steps: vec![],
        }
    }

    fn plan_with_sessions(sessions: Vec<SessionRecoveryInfo>) -> RecoveryPlan {
        RecoveryPlan {
            token: CheckpointToken::new(42),
            checkpoint_type: CheckpointType::Snapshot,
            index_info: IndexRecoveryInfo::default(),
            log_info: LogRecoveryInfo::default(),
            session_infos: sessions,
            steps: vec![],
        }
    }

    fn session_info(id: u32, serial: u64, excluded: Vec<u64>) -> SessionRecoveryInfo {
        SessionRecoveryInfo {
            session_id: id,
            serial_number: serial,
            excluded_serial_numbers: excluded,
        }
    }

    // -- recover_sessions: empty --------------------------------------------

    #[test]
    fn recover_zero_sessions() {
        let engine = SessionRecoveryEngine::new();
        let plan = empty_plan();
        let sessions = engine.recover_sessions(&plan).unwrap();
        assert!(sessions.is_empty());
    }

    // -- recover_sessions: single session -----------------------------------

    #[test]
    fn recover_single_session_no_pending() {
        let engine = SessionRecoveryEngine::new();
        let plan = plan_with_sessions(vec![session_info(1, 500, vec![])]);
        let sessions = engine.recover_sessions(&plan).unwrap();

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, 1);
        assert_eq!(sessions[0].serial_number, 500);
        assert_eq!(sessions[0].pending_count, 0);
        assert!(!sessions[0].needs_replay);
    }

    #[test]
    fn recover_single_session_with_pending() {
        let engine = SessionRecoveryEngine::new();
        let plan = plan_with_sessions(vec![session_info(2, 1000, vec![998, 999])]);
        let sessions = engine.recover_sessions(&plan).unwrap();

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, 2);
        assert_eq!(sessions[0].serial_number, 1000);
        assert_eq!(sessions[0].pending_count, 2);
        assert!(sessions[0].needs_replay);
    }

    // -- recover_sessions: multiple sessions --------------------------------

    #[test]
    fn recover_multiple_sessions() {
        let engine = SessionRecoveryEngine::new();
        let plan = plan_with_sessions(vec![
            session_info(0, 100, vec![]),
            session_info(1, 200, vec![150]),
            session_info(2, 300, vec![]),
        ]);
        let sessions = engine.recover_sessions(&plan).unwrap();

        assert_eq!(sessions.len(), 3);

        // Session 0: no replay needed.
        assert_eq!(sessions[0].session_id, 0);
        assert!(!sessions[0].needs_replay);

        // Session 1: has pending → replay needed.
        assert_eq!(sessions[1].session_id, 1);
        assert!(sessions[1].needs_replay);
        assert_eq!(sessions[1].pending_count, 1);

        // Session 2: no replay needed.
        assert_eq!(sessions[2].session_id, 2);
        assert!(!sessions[2].needs_replay);
    }

    // -- needs_replay flag --------------------------------------------------

    #[test]
    fn needs_replay_true_when_pending_count_positive() {
        let engine = SessionRecoveryEngine::new();
        let info = session_info(5, 42, vec![40, 41]);
        let recovered = engine.recover_session(&info).unwrap();
        assert!(recovered.needs_replay);
        assert_eq!(recovered.pending_count, 2);
    }

    #[test]
    fn needs_replay_false_when_pending_count_zero() {
        let engine = SessionRecoveryEngine::new();
        let info = session_info(5, 42, vec![]);
        let recovered = engine.recover_session(&info).unwrap();
        assert!(!recovered.needs_replay);
        assert_eq!(recovered.pending_count, 0);
    }

    // -- validation: duplicate session IDs ----------------------------------

    #[test]
    fn duplicate_session_id_is_rejected() {
        let engine = SessionRecoveryEngine::new();
        let plan = plan_with_sessions(vec![
            session_info(1, 100, vec![]),
            session_info(1, 200, vec![]),
        ]);
        let err = engine.recover_sessions(&plan).unwrap_err();
        match &err {
            RecoveryError::CorruptMetadata(msg) => {
                assert!(msg.contains("duplicate session ID"), "got: {msg}");
            }
            other => panic!("expected CorruptMetadata, got {other:?}"),
        }
    }

    #[test]
    fn duplicate_session_id_among_many() {
        let engine = SessionRecoveryEngine::new();
        let plan = plan_with_sessions(vec![
            session_info(0, 10, vec![]),
            session_info(1, 20, vec![]),
            session_info(2, 30, vec![]),
            session_info(1, 40, vec![]), // duplicate of session 1
        ]);
        let err = engine.recover_sessions(&plan).unwrap_err();
        match &err {
            RecoveryError::CorruptMetadata(msg) => {
                assert!(msg.contains("duplicate session ID: 1"), "got: {msg}");
            }
            other => panic!("expected CorruptMetadata, got {other:?}"),
        }
    }

    // -- validation: anomalous state ----------------------------------------

    #[test]
    fn zero_serial_with_excluded_is_rejected() {
        let engine = SessionRecoveryEngine::new();
        let info = session_info(10, 0, vec![1, 2, 3]);
        let err = engine.recover_session(&info).unwrap_err();
        match &err {
            RecoveryError::CorruptMetadata(msg) => {
                assert!(msg.contains("serial_number is 0"), "got: {msg}");
            }
            other => panic!("expected CorruptMetadata, got {other:?}"),
        }
    }

    #[test]
    fn zero_serial_with_no_excluded_is_valid() {
        let engine = SessionRecoveryEngine::new();
        let info = session_info(10, 0, vec![]);
        let recovered = engine.recover_session(&info).unwrap();
        assert_eq!(recovered.session_id, 10);
        assert_eq!(recovered.serial_number, 0);
        assert!(!recovered.needs_replay);
    }

    // -- recover_session (individual) ---------------------------------------

    #[test]
    fn recover_session_preserves_serial_number() {
        let engine = SessionRecoveryEngine::new();
        let info = session_info(7, 999_999, vec![]);
        let recovered = engine.recover_session(&info).unwrap();
        assert_eq!(recovered.serial_number, 999_999);
    }

    #[test]
    fn recover_session_id_widened_from_u32() {
        let engine = SessionRecoveryEngine::new();
        let info = session_info(u32::MAX, 1, vec![]);
        let recovered = engine.recover_session(&info).unwrap();
        assert_eq!(recovered.session_id, u32::MAX as usize);
    }

    // -- Default impl -------------------------------------------------------

    #[test]
    fn default_creates_engine() {
        let engine = SessionRecoveryEngine::default();
        let plan = empty_plan();
        let sessions = engine.recover_sessions(&plan).unwrap();
        assert!(sessions.is_empty());
    }

    // -- end-to-end with plan containing sessions in recover_sessions -------

    #[test]
    fn recover_sessions_all_clean() {
        let engine = SessionRecoveryEngine::new();
        let plan = plan_with_sessions(vec![
            session_info(0, 50, vec![]),
            session_info(1, 100, vec![]),
            session_info(2, 150, vec![]),
        ]);
        let sessions = engine.recover_sessions(&plan).unwrap();
        assert_eq!(sessions.len(), 3);
        assert!(sessions.iter().all(|s| !s.needs_replay));
        assert!(sessions.iter().all(|s| s.pending_count == 0));
    }

    #[test]
    fn recover_sessions_mixed_replay_states() {
        let engine = SessionRecoveryEngine::new();
        let plan = plan_with_sessions(vec![
            session_info(0, 100, vec![]),         // clean
            session_info(1, 200, vec![190, 195]), // needs replay
            session_info(2, 300, vec![]),         // clean
            session_info(3, 400, vec![399]),      // needs replay
        ]);
        let sessions = engine.recover_sessions(&plan).unwrap();
        assert_eq!(sessions.len(), 4);

        assert!(!sessions[0].needs_replay);
        assert!(sessions[1].needs_replay);
        assert!(!sessions[2].needs_replay);
        assert!(sessions[3].needs_replay);
    }

    // -- RecoveredSession derives ------------------------------------------

    #[test]
    fn recovered_session_debug_format() {
        let s = RecoveredSession {
            session_id: 1,
            serial_number: 42,
            pending_count: 0,
            needs_replay: false,
        };
        let dbg = format!("{s:?}");
        assert!(dbg.contains("RecoveredSession"));
        assert!(dbg.contains("session_id"));
    }

    #[test]
    fn recovered_session_clone_eq() {
        let s = RecoveredSession {
            session_id: 1,
            serial_number: 42,
            pending_count: 3,
            needs_replay: true,
        };
        let cloned = s.clone();
        assert_eq!(s, cloned);
    }
}
