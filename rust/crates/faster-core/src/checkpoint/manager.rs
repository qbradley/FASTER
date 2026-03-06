//! Checkpoint coordination: trait, status types, and default implementation.
//!
//! The [`CheckpointManager`] trait abstracts the orchestration of full or
//! incremental checkpoints. The FasterKv store calls into a checkpoint manager
//! to initiate, track, and finalise checkpoints.
//!
//! [`CheckpointManagerImpl`] provides a thread-safe, in-memory implementation
//! suitable for single-node deployments. Pluggable backends (e.g. remote
//! storage) can implement the trait directly.

use std::collections::VecDeque;
use std::fmt;
use std::sync::{Arc, Mutex};

use super::{CheckpointToken, CheckpointType};

// ---------------------------------------------------------------------------
// CheckpointStatus
// ---------------------------------------------------------------------------

/// Current status of a checkpoint operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckpointStatus {
    /// The checkpoint is still being written.
    InProgress,
    /// The checkpoint completed successfully.
    Completed,
    /// The checkpoint was explicitly aborted by the caller.
    Aborted,
    /// The checkpoint failed with the given reason.
    Failed(String),
}

impl fmt::Display for CheckpointStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InProgress => write!(f, "InProgress"),
            Self::Completed => write!(f, "Completed"),
            Self::Aborted => write!(f, "Aborted"),
            Self::Failed(reason) => write!(f, "Failed: {reason}"),
        }
    }
}

// ---------------------------------------------------------------------------
// CheckpointError
// ---------------------------------------------------------------------------

/// Errors that can occur during checkpoint management operations.
#[derive(Debug)]
pub enum CheckpointError {
    /// A checkpoint is already in progress; only one at a time is allowed.
    AlreadyInProgress,
    /// The referenced checkpoint token was not found.
    NotFound,
    /// The operation is invalid for the checkpoint's current state.
    InvalidState(String),
    /// An underlying I/O error.
    IoError(std::io::Error),
}

impl fmt::Display for CheckpointError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyInProgress => write!(f, "a checkpoint is already in progress"),
            Self::NotFound => write!(f, "checkpoint not found"),
            Self::InvalidState(msg) => write!(f, "invalid checkpoint state: {msg}"),
            Self::IoError(err) => write!(f, "checkpoint I/O error: {err}"),
        }
    }
}

impl std::error::Error for CheckpointError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::IoError(err) => Some(err),
            _ => None,
        }
    }
}

impl From<std::io::Error> for CheckpointError {
    fn from(err: std::io::Error) -> Self {
        Self::IoError(err)
    }
}

// ---------------------------------------------------------------------------
// CheckpointManager trait
// ---------------------------------------------------------------------------

/// Trait abstracting checkpoint coordination.
///
/// The store calls into a `CheckpointManager` to begin, complete, abort, and
/// query checkpoints. At most one checkpoint may be active at a time.
///
/// Implementations **must** be [`Send`] + [`Sync`] so they can be shared
/// across worker threads.
pub trait CheckpointManager: Send + Sync {
    /// Initiate a new checkpoint of the given type.
    ///
    /// Returns a unique [`CheckpointToken`] that identifies this checkpoint
    /// throughout its lifecycle. Fails with [`CheckpointError::AlreadyInProgress`]
    /// if a checkpoint is already active.
    fn begin_checkpoint(
        &self,
        checkpoint_type: CheckpointType,
    ) -> Result<CheckpointToken, CheckpointError>;

    /// Finalise a previously started checkpoint.
    ///
    /// The token must reference an in-progress checkpoint; otherwise an error
    /// is returned.
    fn complete_checkpoint(&self, token: &CheckpointToken) -> Result<(), CheckpointError>;

    /// Abort an in-progress checkpoint.
    ///
    /// After this call the checkpoint is marked [`CheckpointStatus::Aborted`]
    /// and a new checkpoint may be started.
    fn abort_checkpoint(&self, token: &CheckpointToken) -> Result<(), CheckpointError>;

    /// Query the status of a checkpoint identified by `token`.
    ///
    /// Returns [`None`] if the token is unknown.
    fn checkpoint_status(&self, token: &CheckpointToken) -> Option<CheckpointStatus>;

    /// Return the most recently **completed** checkpoint token, if any.
    fn latest_checkpoint(&self) -> Option<CheckpointToken>;
}

// ---------------------------------------------------------------------------
// Internal state
// ---------------------------------------------------------------------------

/// An entry in the checkpoint history.
#[derive(Debug, Clone)]
struct CheckpointRecord {
    token: CheckpointToken,
    /// Stored for future recovery-path use (e.g. deciding fold-over vs snapshot replay).
    #[allow(dead_code)]
    checkpoint_type: CheckpointType,
    status: CheckpointStatus,
}

/// Inner mutable state protected by a [`Mutex`].
#[derive(Debug)]
struct ManagerInner {
    /// The currently active (in-progress) checkpoint, if any.
    active: Option<CheckpointRecord>,
    /// History of completed/aborted/failed checkpoints (newest at back).
    history: VecDeque<CheckpointRecord>,
    /// Monotonically increasing counter used to generate unique tokens.
    next_token_value: u128,
}

// ---------------------------------------------------------------------------
// CheckpointManagerImpl
// ---------------------------------------------------------------------------

/// A thread-safe, in-memory [`CheckpointManager`] implementation.
///
/// Tracks at most one active checkpoint and maintains a bounded history of
/// finished checkpoints. Token values are generated from a monotonic counter
/// and are unique for the lifetime of this manager instance.
///
/// # Thread safety
///
/// All state is held behind an `Arc<Mutex<…>>`, so cloning the manager
/// yields a handle to the same shared state.
///
/// # Examples
///
/// ```
/// use faster_core::checkpoint::{CheckpointManagerImpl, CheckpointManager, CheckpointType};
///
/// let mgr = CheckpointManagerImpl::new();
/// let token = mgr.begin_checkpoint(CheckpointType::Snapshot).unwrap();
/// mgr.complete_checkpoint(&token).unwrap();
/// assert_eq!(mgr.latest_checkpoint(), Some(token));
/// ```
#[derive(Debug, Clone)]
pub struct CheckpointManagerImpl {
    inner: Arc<Mutex<ManagerInner>>,
}

impl Default for CheckpointManagerImpl {
    fn default() -> Self {
        Self::new()
    }
}

impl CheckpointManagerImpl {
    /// Create a new, empty checkpoint manager.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(ManagerInner {
                active: None,
                history: VecDeque::new(),
                next_token_value: 1,
            })),
        }
    }

    /// Return the number of checkpoints in the completed history.
    pub fn history_len(&self) -> usize {
        self.inner.lock().expect("lock poisoned").history.len()
    }
}

impl CheckpointManager for CheckpointManagerImpl {
    fn begin_checkpoint(
        &self,
        checkpoint_type: CheckpointType,
    ) -> Result<CheckpointToken, CheckpointError> {
        let mut inner = self.inner.lock().expect("lock poisoned");

        if inner.active.is_some() {
            return Err(CheckpointError::AlreadyInProgress);
        }

        let token = CheckpointToken::new(inner.next_token_value);
        inner.next_token_value += 1;

        inner.active = Some(CheckpointRecord {
            token,
            checkpoint_type,
            status: CheckpointStatus::InProgress,
        });

        Ok(token)
    }

    fn complete_checkpoint(&self, token: &CheckpointToken) -> Result<(), CheckpointError> {
        let mut inner = self.inner.lock().expect("lock poisoned");

        let active = inner.active.as_ref().ok_or(CheckpointError::NotFound)?;

        if active.token != *token {
            return Err(CheckpointError::NotFound);
        }
        if active.status != CheckpointStatus::InProgress {
            return Err(CheckpointError::InvalidState(format!(
                "checkpoint is {}, expected InProgress",
                active.status
            )));
        }

        let mut record = inner.active.take().expect("just checked");
        record.status = CheckpointStatus::Completed;
        inner.history.push_back(record);

        Ok(())
    }

    fn abort_checkpoint(&self, token: &CheckpointToken) -> Result<(), CheckpointError> {
        let mut inner = self.inner.lock().expect("lock poisoned");

        let active = inner.active.as_ref().ok_or(CheckpointError::NotFound)?;

        if active.token != *token {
            return Err(CheckpointError::NotFound);
        }
        if active.status != CheckpointStatus::InProgress {
            return Err(CheckpointError::InvalidState(format!(
                "checkpoint is {}, expected InProgress",
                active.status
            )));
        }

        let mut record = inner.active.take().expect("just checked");
        record.status = CheckpointStatus::Aborted;
        inner.history.push_back(record);

        Ok(())
    }

    fn checkpoint_status(&self, token: &CheckpointToken) -> Option<CheckpointStatus> {
        let inner = self.inner.lock().expect("lock poisoned");

        // Check active checkpoint first.
        if let Some(ref active) = inner.active {
            if active.token == *token {
                return Some(active.status.clone());
            }
        }

        // Then search history (newest first for faster lookup of recent tokens).
        inner
            .history
            .iter()
            .rev()
            .find(|r| r.token == *token)
            .map(|r| r.status.clone())
    }

    fn latest_checkpoint(&self) -> Option<CheckpointToken> {
        let inner = self.inner.lock().expect("lock poisoned");
        inner
            .history
            .iter()
            .rev()
            .find(|r| r.status == CheckpointStatus::Completed)
            .map(|r| r.token)
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    fn new_manager() -> CheckpointManagerImpl {
        CheckpointManagerImpl::new()
    }

    // -----------------------------------------------------------------------
    // Normal lifecycle
    // -----------------------------------------------------------------------

    #[test]
    fn begin_and_complete_checkpoint() {
        let mgr = new_manager();

        let token = mgr.begin_checkpoint(CheckpointType::Snapshot).unwrap();
        assert_eq!(
            mgr.checkpoint_status(&token),
            Some(CheckpointStatus::InProgress)
        );

        mgr.complete_checkpoint(&token).unwrap();
        assert_eq!(
            mgr.checkpoint_status(&token),
            Some(CheckpointStatus::Completed)
        );
        assert_eq!(mgr.latest_checkpoint(), Some(token));
    }

    #[test]
    fn begin_and_complete_fold_over() {
        let mgr = new_manager();

        let token = mgr.begin_checkpoint(CheckpointType::FoldOver).unwrap();
        mgr.complete_checkpoint(&token).unwrap();
        assert_eq!(
            mgr.checkpoint_status(&token),
            Some(CheckpointStatus::Completed)
        );
    }

    #[test]
    fn multiple_sequential_checkpoints() {
        let mgr = new_manager();

        let t1 = mgr.begin_checkpoint(CheckpointType::Snapshot).unwrap();
        mgr.complete_checkpoint(&t1).unwrap();

        let t2 = mgr.begin_checkpoint(CheckpointType::FoldOver).unwrap();
        mgr.complete_checkpoint(&t2).unwrap();

        let t3 = mgr.begin_checkpoint(CheckpointType::Snapshot).unwrap();
        mgr.complete_checkpoint(&t3).unwrap();

        // Latest should be the last completed.
        assert_eq!(mgr.latest_checkpoint(), Some(t3));
        // All should be in history.
        assert_eq!(mgr.history_len(), 3);
    }

    // -----------------------------------------------------------------------
    // Abort flow
    // -----------------------------------------------------------------------

    #[test]
    fn abort_checkpoint() {
        let mgr = new_manager();

        let token = mgr.begin_checkpoint(CheckpointType::Snapshot).unwrap();
        mgr.abort_checkpoint(&token).unwrap();
        assert_eq!(
            mgr.checkpoint_status(&token),
            Some(CheckpointStatus::Aborted)
        );
        // Aborted checkpoint is not "completed", so latest_checkpoint ignores it.
        assert_eq!(mgr.latest_checkpoint(), None);
    }

    #[test]
    fn can_begin_after_abort() {
        let mgr = new_manager();

        let t1 = mgr.begin_checkpoint(CheckpointType::Snapshot).unwrap();
        mgr.abort_checkpoint(&t1).unwrap();

        // Should be able to start a new one.
        let t2 = mgr.begin_checkpoint(CheckpointType::FoldOver).unwrap();
        mgr.complete_checkpoint(&t2).unwrap();
        assert_eq!(mgr.latest_checkpoint(), Some(t2));
    }

    // -----------------------------------------------------------------------
    // Error cases
    // -----------------------------------------------------------------------

    #[test]
    fn begin_while_in_progress_fails() {
        let mgr = new_manager();

        let _token = mgr.begin_checkpoint(CheckpointType::Snapshot).unwrap();
        let err = mgr
            .begin_checkpoint(CheckpointType::FoldOver)
            .unwrap_err();
        assert!(matches!(err, CheckpointError::AlreadyInProgress));
    }

    #[test]
    fn complete_nonexistent_token_fails() {
        let mgr = new_manager();

        let bogus = CheckpointToken::new(9999);
        let err = mgr.complete_checkpoint(&bogus).unwrap_err();
        assert!(matches!(err, CheckpointError::NotFound));
    }

    #[test]
    fn complete_wrong_token_fails() {
        let mgr = new_manager();

        let _token = mgr.begin_checkpoint(CheckpointType::Snapshot).unwrap();
        let wrong = CheckpointToken::new(9999);
        let err = mgr.complete_checkpoint(&wrong).unwrap_err();
        assert!(matches!(err, CheckpointError::NotFound));
    }

    #[test]
    fn abort_nonexistent_token_fails() {
        let mgr = new_manager();

        let bogus = CheckpointToken::new(9999);
        let err = mgr.abort_checkpoint(&bogus).unwrap_err();
        assert!(matches!(err, CheckpointError::NotFound));
    }

    #[test]
    fn double_complete_fails() {
        let mgr = new_manager();

        let token = mgr.begin_checkpoint(CheckpointType::Snapshot).unwrap();
        mgr.complete_checkpoint(&token).unwrap();

        // Token is no longer active, so completing again should fail.
        let err = mgr.complete_checkpoint(&token).unwrap_err();
        assert!(matches!(err, CheckpointError::NotFound));
    }

    #[test]
    fn double_abort_fails() {
        let mgr = new_manager();

        let token = mgr.begin_checkpoint(CheckpointType::Snapshot).unwrap();
        mgr.abort_checkpoint(&token).unwrap();

        let err = mgr.abort_checkpoint(&token).unwrap_err();
        assert!(matches!(err, CheckpointError::NotFound));
    }

    #[test]
    fn status_of_unknown_token_is_none() {
        let mgr = new_manager();

        assert_eq!(mgr.checkpoint_status(&CheckpointToken::new(42)), None);
    }

    #[test]
    fn latest_checkpoint_empty() {
        let mgr = new_manager();
        assert_eq!(mgr.latest_checkpoint(), None);
    }

    // -----------------------------------------------------------------------
    // Display / Error impls
    // -----------------------------------------------------------------------

    #[test]
    fn checkpoint_status_display() {
        assert_eq!(CheckpointStatus::InProgress.to_string(), "InProgress");
        assert_eq!(CheckpointStatus::Completed.to_string(), "Completed");
        assert_eq!(CheckpointStatus::Aborted.to_string(), "Aborted");
        assert_eq!(
            CheckpointStatus::Failed("oops".into()).to_string(),
            "Failed: oops"
        );
    }

    #[test]
    fn checkpoint_error_display() {
        assert!(CheckpointError::AlreadyInProgress
            .to_string()
            .contains("already in progress"));
        assert!(CheckpointError::NotFound.to_string().contains("not found"));
        assert!(CheckpointError::InvalidState("bad".into())
            .to_string()
            .contains("bad"));

        let io_err = std::io::Error::new(std::io::ErrorKind::Other, "disk");
        assert!(CheckpointError::IoError(io_err)
            .to_string()
            .contains("disk"));
    }

    #[test]
    fn checkpoint_error_source() {
        let io_err = std::io::Error::new(std::io::ErrorKind::Other, "oops");
        let err = CheckpointError::IoError(io_err);
        assert!(std::error::Error::source(&err).is_some());

        assert!(std::error::Error::source(&CheckpointError::AlreadyInProgress).is_none());
        assert!(std::error::Error::source(&CheckpointError::NotFound).is_none());
        assert!(
            std::error::Error::source(&CheckpointError::InvalidState("x".into())).is_none()
        );
    }

    #[test]
    fn checkpoint_error_from_io() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "gone");
        let err: CheckpointError = io_err.into();
        assert!(matches!(err, CheckpointError::IoError(_)));
    }

    // -----------------------------------------------------------------------
    // Send + Sync
    // -----------------------------------------------------------------------

    #[test]
    fn manager_impl_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<CheckpointManagerImpl>();
    }

    #[test]
    fn checkpoint_error_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<CheckpointError>();
    }

    // -----------------------------------------------------------------------
    // Clone / Default
    // -----------------------------------------------------------------------

    #[test]
    fn manager_clone_shares_state() {
        let mgr1 = new_manager();
        let mgr2 = mgr1.clone();

        let token = mgr1.begin_checkpoint(CheckpointType::Snapshot).unwrap();
        // The clone should see the active checkpoint.
        assert_eq!(
            mgr2.checkpoint_status(&token),
            Some(CheckpointStatus::InProgress)
        );

        mgr2.complete_checkpoint(&token).unwrap();
        assert_eq!(mgr1.latest_checkpoint(), Some(token));
    }

    #[test]
    fn manager_default() {
        let mgr = CheckpointManagerImpl::default();
        assert_eq!(mgr.latest_checkpoint(), None);
        assert_eq!(mgr.history_len(), 0);
    }

    // -----------------------------------------------------------------------
    // Thread safety
    // -----------------------------------------------------------------------

    #[test]
    fn concurrent_begin_only_one_wins() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let mgr = Arc::new(new_manager());
        let success_count = Arc::new(AtomicUsize::new(0));
        let barrier = Arc::new(std::sync::Barrier::new(8));

        let handles: Vec<_> = (0..8)
            .map(|_| {
                let mgr = Arc::clone(&mgr);
                let cnt = Arc::clone(&success_count);
                let bar = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    bar.wait();
                    if mgr.begin_checkpoint(CheckpointType::Snapshot).is_ok() {
                        cnt.fetch_add(1, Ordering::Relaxed);
                    }
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }

        // Exactly one thread should have succeeded.
        assert_eq!(success_count.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn concurrent_lifecycle() {
        let mgr = Arc::new(new_manager());

        // Run 10 sequential checkpoints from different threads to exercise
        // lock contention on the complete path.
        for _ in 0..10 {
            let m = Arc::clone(&mgr);
            let token = m.begin_checkpoint(CheckpointType::FoldOver).unwrap();
            let m2 = Arc::clone(&mgr);
            let handle = std::thread::spawn(move || {
                m2.complete_checkpoint(&token).unwrap();
            });
            handle.join().unwrap();
        }

        assert_eq!(mgr.history_len(), 10);
        assert!(mgr.latest_checkpoint().is_some());
    }

    // -----------------------------------------------------------------------
    // latest_checkpoint skips aborted
    // -----------------------------------------------------------------------

    #[test]
    fn latest_checkpoint_skips_aborted() {
        let mgr = new_manager();

        let t1 = mgr.begin_checkpoint(CheckpointType::Snapshot).unwrap();
        mgr.complete_checkpoint(&t1).unwrap();

        let t2 = mgr.begin_checkpoint(CheckpointType::Snapshot).unwrap();
        mgr.abort_checkpoint(&t2).unwrap();

        // Latest should still be t1, not the aborted t2.
        assert_eq!(mgr.latest_checkpoint(), Some(t1));
    }

    // -----------------------------------------------------------------------
    // Debug formatting
    // -----------------------------------------------------------------------

    #[test]
    fn checkpoint_error_debug() {
        let err = CheckpointError::AlreadyInProgress;
        let dbg = format!("{err:?}");
        assert!(dbg.contains("AlreadyInProgress"));
    }

    #[test]
    fn checkpoint_status_debug() {
        let status = CheckpointStatus::InProgress;
        let dbg = format!("{status:?}");
        assert!(dbg.contains("InProgress"));
    }
}
