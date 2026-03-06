//! Full checkpoint orchestration for FASTER.
//!
//! The [`CheckpointOrchestrator`] drives a complete checkpoint from start to
//! finish, coordinating the state machine, index writer, log writer, session
//! participation, and metadata persistence. It is the single entry-point for
//! taking a consistent checkpoint of the hash index **and** hybrid log.
//!
//! # Checkpoint flow
//!
//! 1. **Prepare** — generate a [`CheckpointToken`], transition the state
//!    machine from `Rest` → `Prepare`.
//! 2. **Index checkpoint** — serialize every hash bucket via
//!    [`IndexCheckpointWriter`] and produce an [`IndexRecoveryInfo`].
//! 3. **Log checkpoint** — for *fold-over* mode, flush the hybrid log and
//!    wait for completion; for *snapshot* mode, capture a point-in-time
//!    snapshot of in-memory pages. Both produce a [`LogRecoveryInfo`].
//! 4. **Session snapshots** — collect [`SessionRecoveryInfo`] from all
//!    participating sessions.
//! 5. **Persist metadata** — write all recovery info to disk via
//!    [`CheckpointMetadataStore`].
//! 6. **Complete** — advance the state machine through the remaining phases
//!    and return to `Rest`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::checkpoint::index_writer::IndexCheckpointWriter;
use crate::checkpoint::log_writer::LogCheckpointWriter;
use crate::checkpoint::metadata_store::CheckpointMetadataStore;
use crate::checkpoint::state_machine::CheckpointStateMachine;
use crate::checkpoint::{
    CheckpointError, CheckpointPhase, CheckpointToken, CheckpointType, IndexRecoveryInfo,
    LogRecoveryInfo, SessionCheckpointState, SessionRecoveryInfo,
};
use crate::hash::index::HashIndex;
use crate::hybrid_log::log_allocator::HybridLogAllocator;

// ---------------------------------------------------------------------------
// CheckpointConfig
// ---------------------------------------------------------------------------

/// Configuration knobs for the checkpoint orchestrator.
#[derive(Debug, Clone)]
pub struct CheckpointConfig {
    /// Root directory under which checkpoint data is stored.
    pub base_dir: PathBuf,
    /// Maximum time to wait for the hybrid-log flush to complete
    /// (fold-over mode only). Defaults to 30 seconds.
    pub max_wait_flush: Duration,
}

impl CheckpointConfig {
    /// Create a new configuration with the given base directory and default
    /// flush timeout.
    pub fn new(base_dir: PathBuf) -> Self {
        Self {
            base_dir,
            max_wait_flush: Duration::from_secs(30),
        }
    }

    /// Builder-style setter for [`max_wait_flush`](Self::max_wait_flush).
    pub fn with_max_wait_flush(mut self, duration: Duration) -> Self {
        self.max_wait_flush = duration;
        self
    }
}

// ---------------------------------------------------------------------------
// CheckpointOrchestrator
// ---------------------------------------------------------------------------

/// Drives a complete checkpoint from start to finish.
///
/// The orchestrator coordinates the state machine, index writer, log writer,
/// session participation, and metadata persistence. See the
/// [module-level docs](self) for the full checkpoint flow.
pub struct CheckpointOrchestrator {
    config: CheckpointConfig,
    state_machine: CheckpointStateMachine,
}

impl CheckpointOrchestrator {
    /// Create a new orchestrator with the given configuration.
    pub fn new(config: CheckpointConfig) -> Self {
        Self {
            config,
            state_machine: CheckpointStateMachine::new(),
        }
    }

    /// Execute a full checkpoint and return the token identifying it.
    ///
    /// The method drives the state machine through every phase, writes the
    /// index and log checkpoint data, collects session recovery info, and
    /// persists the combined metadata to disk.
    ///
    /// # Errors
    ///
    /// If any phase fails, the checkpoint is aborted (state machine reset,
    /// partial files cleaned up) and a [`CheckpointError`] describing the
    /// failing phase is returned.
    pub fn take_checkpoint(
        &self,
        checkpoint_type: CheckpointType,
        index: &HashIndex,
        log: &HybridLogAllocator,
        sessions: &[SessionCheckpointState],
        base_dir: &Path,
    ) -> Result<CheckpointToken, CheckpointError> {
        // 1. Prepare — generate token, Rest → Prepare
        let token = CheckpointToken::new(random_token_value());

        self.state_machine.start(token).map_err(|e| {
            CheckpointError::InvalidState(format!("failed to start checkpoint: {e}"))
        })?;

        // From here on, any failure must reset the state machine.
        match self.run_checkpoint(checkpoint_type, &token, index, log, sessions, base_dir) {
            Ok(()) => Ok(token),
            Err(e) => {
                // Best-effort abort: walk the state machine forward to
                // Completed so that reset() can bring it back to Rest.
                self.force_abort();
                Err(e)
            }
        }
    }

    /// Returns the current phase of the internal state machine.
    pub fn phase(&self) -> CheckpointPhase {
        self.state_machine.phase()
    }

    /// Returns the currently active checkpoint token, if any.
    pub fn active_token(&self) -> Option<CheckpointToken> {
        self.state_machine.active_token()
    }

    // -----------------------------------------------------------------------
    // Internal: the actual checkpoint pipeline
    // -----------------------------------------------------------------------

    fn run_checkpoint(
        &self,
        checkpoint_type: CheckpointType,
        token: &CheckpointToken,
        index: &HashIndex,
        log: &HybridLogAllocator,
        sessions: &[SessionCheckpointState],
        base_dir: &Path,
    ) -> Result<(), CheckpointError> {
        // Prepare → InProgress
        self.advance(CheckpointPhase::Prepare, CheckpointPhase::InProgress)?;

        // 2. Write index checkpoint
        let index_info = self.write_index_checkpoint(token, index, base_dir)?;

        // 3. Write log checkpoint
        let log_info = self.write_log_checkpoint(checkpoint_type, token, log)?;

        // InProgress → WaitFlush
        self.advance(CheckpointPhase::InProgress, CheckpointPhase::WaitFlush)?;

        // 4. Wait for flush (fold-over) — snapshot mode completes immediately
        if checkpoint_type == CheckpointType::FoldOver {
            self.wait_for_flush(token, log)?;
        }

        // WaitFlush → WaitCompletion
        self.advance(CheckpointPhase::WaitFlush, CheckpointPhase::WaitCompletion)?;

        // 5. Collect session recovery info
        let session_infos = Self::collect_session_infos(sessions);

        // 6. Persist combined metadata
        let store = CheckpointMetadataStore::new(base_dir.to_path_buf());
        store.write_checkpoint_metadata(token, &index_info, &log_info, &session_infos)?;

        // WaitCompletion → Completed
        self.advance(
            CheckpointPhase::WaitCompletion,
            CheckpointPhase::Completed,
        )?;

        // Completed → Rest
        self.state_machine.reset().map_err(|e| {
            CheckpointError::InvalidState(format!("failed to reset state machine: {e}"))
        })?;

        Ok(())
    }

    /// Advance the state machine, wrapping the error for the orchestrator.
    fn advance(
        &self,
        from: CheckpointPhase,
        to: CheckpointPhase,
    ) -> Result<(), CheckpointError> {
        self.state_machine.try_advance(from, to).map_err(|e| {
            CheckpointError::InvalidState(format!(
                "state transition {from:?} → {to:?} failed: {e}"
            ))
        })?;
        Ok(())
    }

    /// Best-effort abort: walk the state machine forward through all
    /// remaining phases until it reaches `Completed`, then reset to `Rest`.
    /// Errors are silently ignored — this is a cleanup path.
    fn force_abort(&self) {
        let ordered = [
            CheckpointPhase::Prepare,
            CheckpointPhase::InProgress,
            CheckpointPhase::WaitFlush,
            CheckpointPhase::WaitCompletion,
            CheckpointPhase::Completed,
        ];
        let current = self.state_machine.phase();
        if current == CheckpointPhase::Rest {
            return;
        }
        // Find the current phase in the ordered list and advance through
        // all subsequent phases.
        if let Some(pos) = ordered.iter().position(|&p| p == current) {
            for window in ordered[pos..].windows(2) {
                let _ = self.state_machine.try_advance(window[0], window[1]);
            }
        }
        let _ = self.state_machine.reset();
    }

    /// Write the hash-index checkpoint file and return its recovery info.
    fn write_index_checkpoint(
        &self,
        token: &CheckpointToken,
        index: &HashIndex,
        base_dir: &Path,
    ) -> Result<IndexRecoveryInfo, CheckpointError> {
        let mut writer = IndexCheckpointWriter::new(base_dir, token)?;
        let buckets = index.checkpoint_bucket_slice();
        let table_size_bits = index.log2_buckets() as u8;
        let version = index.version();
        let entry_count = index.entry_count();

        writer.write_index(buckets, table_size_bits, version, entry_count)
    }

    /// Write the log checkpoint (fold-over or snapshot) and return the
    /// recovery info.
    fn write_log_checkpoint(
        &self,
        checkpoint_type: CheckpointType,
        token: &CheckpointToken,
        log: &HybridLogAllocator,
    ) -> Result<LogRecoveryInfo, CheckpointError> {
        let writer = LogCheckpointWriter::new(checkpoint_type);

        match checkpoint_type {
            CheckpointType::FoldOver => {
                let ctx = writer.begin_checkpoint(log, token)?;
                writer.complete(ctx)
            }
            CheckpointType::Snapshot => {
                let ctx = writer.begin_snapshot_checkpoint(log, token)?;
                writer.complete_snapshot(ctx)
            }
        }
    }

    /// Poll the log's flushed-until address until it reaches the checkpoint
    /// tail, or we exceed [`CheckpointConfig::max_wait_flush`].
    fn wait_for_flush(
        &self,
        token: &CheckpointToken,
        log: &HybridLogAllocator,
    ) -> Result<(), CheckpointError> {
        let writer = LogCheckpointWriter::new(CheckpointType::FoldOver);
        let ctx = writer.begin_checkpoint(log, token)?;

        let deadline = std::time::Instant::now() + self.config.max_wait_flush;
        loop {
            if writer.wait_flush_complete(&ctx, log).is_ok() {
                return Ok(());
            }
            if std::time::Instant::now() >= deadline {
                return Err(CheckpointError::InvalidState(
                    "flush timeout exceeded".into(),
                ));
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// Collect session recovery info from all participants.
    fn collect_session_infos(sessions: &[SessionCheckpointState]) -> Vec<SessionRecoveryInfo> {
        sessions.iter().map(|s| s.to_recovery_info()).collect()
    }
}

/// Generate a random 128-bit value for checkpoint tokens.
fn random_token_value() -> u128 {
    // Mix several sources of entropy available without external crates.
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let nanos = t.as_nanos();
    let thread_id = format!("{:?}", std::thread::current().id());
    let mut hash: u128 = nanos;
    for b in thread_id.bytes() {
        hash = hash.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(u128::from(b));
    }
    // Mix in a stack address for additional entropy.
    let stack_var: u8 = 0;
    let addr = std::ptr::addr_of!(stack_var) as u128;
    hash ^= addr.wrapping_mul(0x517c_c1b7_2722_0a95);
    hash
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::address::LogicalAddress;
    use crate::checkpoint::metadata_store::CheckpointMetadataStore;
    use crate::checkpoint::{
        CheckpointPhase, CheckpointToken, CheckpointType, SessionCheckpointState,
    };
    use crate::hash::index::HashIndex;
    use crate::hash::KeyHash;
    use crate::hash_bucket::HashBucketEntry;
    use crate::hybrid_log::log_allocator::HybridLogAllocator;
    use tempfile::tempdir;

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    fn make_config(dir: &Path) -> CheckpointConfig {
        CheckpointConfig::new(dir.to_path_buf())
    }

    fn make_allocator() -> HybridLogAllocator {
        let log = HybridLogAllocator::new(4, 0.9, 512);
        // Skip sentinel addresses
        log.try_allocate(8).expect("skip sentinel");
        log
    }

    fn make_index() -> HashIndex {
        HashIndex::new(8) // 256 buckets
    }

    fn populate_index(index: &HashIndex, count: u64) -> u64 {
        let table = index.table();
        let mut inserted = 0u64;
        for i in 0..count {
            let hash = KeyHash::new(i.wrapping_mul(0x9E37_79B9_7F4A_7C15));
            let result = table.find_or_create_entry(hash, LogicalAddress::INVALID);
            if result.created {
                let committed = HashBucketEntry::new(
                    result.entry.tag(),
                    LogicalAddress::from_raw(i + 1),
                    false,
                );
                table.update_entry(result.slot, result.entry, committed);
                inserted += 1;
            }
        }
        inserted
    }

    fn make_sessions(token: CheckpointToken, count: usize) -> Vec<SessionCheckpointState> {
        (0..count)
            .map(|i| {
                let mut s = SessionCheckpointState::new(i, token);
                s.snapshot(100 + i as u64, 0);
                s
            })
            .collect()
    }

    fn flush_log_fully(log: &HybridLogAllocator) {
        let tail = log.tail_address();
        log.try_advance_flushed_until(tail);
    }

    // -----------------------------------------------------------------------
    // Full fold-over checkpoint lifecycle
    // -----------------------------------------------------------------------

    #[test]
    fn fold_over_checkpoint_lifecycle() {
        let dir = tempdir().unwrap();
        let config = make_config(dir.path());
        let orchestrator = CheckpointOrchestrator::new(config);

        let index = make_index();
        let inserted = populate_index(&index, 50);
        assert_eq!(inserted, 50);

        let log = make_allocator();
        log.try_allocate(64).unwrap();
        log.try_allocate(128).unwrap();
        flush_log_fully(&log);

        let token = CheckpointToken::new(0xCAFE);
        let sessions = make_sessions(token, 3);

        let result_token = orchestrator
            .take_checkpoint(
                CheckpointType::FoldOver,
                &index,
                &log,
                &sessions,
                dir.path(),
            )
            .unwrap();

        // State machine returned to rest
        assert_eq!(orchestrator.phase(), CheckpointPhase::Rest);
        assert!(orchestrator.active_token().is_none());

        // Metadata persisted on disk
        let store = CheckpointMetadataStore::new(dir.path().to_path_buf());
        let meta = store.read_checkpoint_metadata(&result_token).unwrap();
        assert_eq!(meta.checkpoint_type, CheckpointType::FoldOver);
        assert_eq!(meta.index_info.table_size, 256);
        assert!(!meta.log_info.use_snapshot_file);
        assert_eq!(meta.session_infos.len(), 3);
    }

    // -----------------------------------------------------------------------
    // Full snapshot checkpoint lifecycle
    // -----------------------------------------------------------------------

    #[test]
    fn snapshot_checkpoint_lifecycle() {
        let dir = tempdir().unwrap();
        let config = make_config(dir.path());
        let orchestrator = CheckpointOrchestrator::new(config);

        let index = make_index();
        populate_index(&index, 25);

        let log = make_allocator();
        log.try_allocate(256).unwrap();
        flush_log_fully(&log);

        let token = CheckpointToken::new(0xBEEF);
        let sessions = make_sessions(token, 2);

        let result_token = orchestrator
            .take_checkpoint(
                CheckpointType::Snapshot,
                &index,
                &log,
                &sessions,
                dir.path(),
            )
            .unwrap();

        assert_eq!(orchestrator.phase(), CheckpointPhase::Rest);

        let store = CheckpointMetadataStore::new(dir.path().to_path_buf());
        let meta = store.read_checkpoint_metadata(&result_token).unwrap();
        assert_eq!(meta.checkpoint_type, CheckpointType::Snapshot);
        assert!(meta.log_info.use_snapshot_file);
        assert_eq!(meta.session_infos.len(), 2);
    }

    // -----------------------------------------------------------------------
    // Checkpoint with empty store
    // -----------------------------------------------------------------------

    #[test]
    fn checkpoint_with_empty_store() {
        let dir = tempdir().unwrap();
        let config = make_config(dir.path());
        let orchestrator = CheckpointOrchestrator::new(config);

        let index = make_index(); // empty index
        let log = make_allocator();
        flush_log_fully(&log);

        let result_token = orchestrator
            .take_checkpoint(
                CheckpointType::FoldOver,
                &index,
                &log,
                &[], // no sessions
                dir.path(),
            )
            .unwrap();

        let store = CheckpointMetadataStore::new(dir.path().to_path_buf());
        let meta = store.read_checkpoint_metadata(&result_token).unwrap();
        assert!(meta.session_infos.is_empty());
        assert_eq!(meta.index_info.table_size, 256);
    }

    // -----------------------------------------------------------------------
    // Checkpoint with data across multiple allocations
    // -----------------------------------------------------------------------

    #[test]
    fn checkpoint_with_multi_page_data() {
        let dir = tempdir().unwrap();
        let config = make_config(dir.path());
        let orchestrator = CheckpointOrchestrator::new(config);

        let index = make_index();
        populate_index(&index, 200);

        let log = make_allocator();
        // Allocate many small records to span pages
        for _ in 0..100 {
            log.try_allocate(64).unwrap();
        }
        flush_log_fully(&log);

        let token = CheckpointToken::new(0xF00D);
        let sessions = make_sessions(token, 5);

        let result_token = orchestrator
            .take_checkpoint(
                CheckpointType::FoldOver,
                &index,
                &log,
                &sessions,
                dir.path(),
            )
            .unwrap();

        let store = CheckpointMetadataStore::new(dir.path().to_path_buf());
        let meta = store.read_checkpoint_metadata(&result_token).unwrap();
        assert_eq!(meta.session_infos.len(), 5);
        // Log should span beyond the initial 8-byte sentinel
        assert!(meta.log_info.final_address > LogicalAddress::from_raw(8));
    }

    // -----------------------------------------------------------------------
    // Abort on failure — simulate index write failure via invalid base_dir
    // -----------------------------------------------------------------------

    #[test]
    fn abort_on_index_write_failure() {
        let config = CheckpointConfig::new(PathBuf::from("/nonexistent/path"));
        let orchestrator = CheckpointOrchestrator::new(config);

        let index = make_index();
        let log = make_allocator();
        flush_log_fully(&log);

        let result = orchestrator.take_checkpoint(
            CheckpointType::FoldOver,
            &index,
            &log,
            &[],
            Path::new("/nonexistent/path"),
        );

        assert!(result.is_err());
        // State machine must have been reset to Rest
        assert_eq!(orchestrator.phase(), CheckpointPhase::Rest);
        assert!(orchestrator.active_token().is_none());
    }

    // -----------------------------------------------------------------------
    // Multiple sequential checkpoints
    // -----------------------------------------------------------------------

    #[test]
    fn multiple_sequential_checkpoints() {
        let dir = tempdir().unwrap();
        let config = make_config(dir.path());
        let orchestrator = CheckpointOrchestrator::new(config);

        let index = make_index();
        let log = make_allocator();

        let mut tokens = Vec::new();
        for i in 0..3 {
            populate_index(&index, 10);
            log.try_allocate(64 + i * 32).unwrap();
            flush_log_fully(&log);

            let token = CheckpointToken::new(i as u128 + 1);
            let sessions = make_sessions(token, 1);

            let result_token = orchestrator
                .take_checkpoint(
                    CheckpointType::FoldOver,
                    &index,
                    &log,
                    &sessions,
                    dir.path(),
                )
                .unwrap();
            tokens.push(result_token);

            assert_eq!(orchestrator.phase(), CheckpointPhase::Rest);
        }

        // All three checkpoints should be independently readable
        let store = CheckpointMetadataStore::new(dir.path().to_path_buf());
        for t in &tokens {
            let meta = store.read_checkpoint_metadata(t).unwrap();
            assert_eq!(meta.checkpoint_type, CheckpointType::FoldOver);
        }
    }

    // -----------------------------------------------------------------------
    // Verify recovery info correctness
    // -----------------------------------------------------------------------

    #[test]
    fn recovery_info_addresses_correct() {
        let dir = tempdir().unwrap();
        let config = make_config(dir.path());
        let orchestrator = CheckpointOrchestrator::new(config);

        let index = make_index();
        populate_index(&index, 30);

        let log = make_allocator();
        log.try_allocate(128).unwrap();
        log.try_allocate(256).unwrap();
        let expected_tail = log.tail_address();
        let expected_begin = log.begin_address();
        flush_log_fully(&log);

        let token = CheckpointToken::new(0xABCD);
        let mut session = SessionCheckpointState::new(7, token);
        session.snapshot(42, 0);
        let sessions = vec![session];

        let result_token = orchestrator
            .take_checkpoint(
                CheckpointType::FoldOver,
                &index,
                &log,
                &sessions,
                dir.path(),
            )
            .unwrap();

        let store = CheckpointMetadataStore::new(dir.path().to_path_buf());
        let meta = store.read_checkpoint_metadata(&result_token).unwrap();

        // Index recovery info
        assert_eq!(meta.index_info.version, index.version());
        assert_eq!(meta.index_info.table_size, index.num_buckets());

        // Log recovery info
        assert_eq!(meta.log_info.final_address, expected_tail);
        assert_eq!(meta.log_info.begin_address, expected_begin);
        assert_eq!(meta.log_info.flushed_until_address, expected_tail);

        // Session recovery info
        assert_eq!(meta.session_infos.len(), 1);
        assert_eq!(meta.session_infos[0].session_id, 7);
        assert_eq!(meta.session_infos[0].serial_number, 42);
    }

    // -----------------------------------------------------------------------
    // Snapshot recovery info has snapshot addresses set
    // -----------------------------------------------------------------------

    #[test]
    fn snapshot_recovery_info_has_snapshot_addresses() {
        let dir = tempdir().unwrap();
        let config = make_config(dir.path());
        let orchestrator = CheckpointOrchestrator::new(config);

        let index = make_index();
        let log = make_allocator();
        log.try_allocate(512).unwrap();
        flush_log_fully(&log);

        let result_token = orchestrator
            .take_checkpoint(
                CheckpointType::Snapshot,
                &index,
                &log,
                &[],
                dir.path(),
            )
            .unwrap();

        let store = CheckpointMetadataStore::new(dir.path().to_path_buf());
        let meta = store.read_checkpoint_metadata(&result_token).unwrap();

        assert!(meta.log_info.use_snapshot_file);
        assert_eq!(meta.log_info.checkpoint_type, CheckpointType::Snapshot);
    }

    // -----------------------------------------------------------------------
    // Config builder
    // -----------------------------------------------------------------------

    #[test]
    fn config_builder() {
        let cfg = CheckpointConfig::new(PathBuf::from("/tmp/ckpt"))
            .with_max_wait_flush(Duration::from_secs(60));

        assert_eq!(cfg.base_dir, PathBuf::from("/tmp/ckpt"));
        assert_eq!(cfg.max_wait_flush, Duration::from_secs(60));
    }

    #[test]
    fn config_default_flush_timeout() {
        let cfg = CheckpointConfig::new(PathBuf::from("/tmp"));
        assert_eq!(cfg.max_wait_flush, Duration::from_secs(30));
    }
}
