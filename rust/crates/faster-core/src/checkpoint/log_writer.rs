//! Hybrid log checkpoint writer for fold-over mode.
//!
//! The [`LogCheckpointWriter`] coordinates the fold-over checkpoint protocol
//! for the hybrid log. In fold-over mode:
//!
//! 1. **Begin** — snapshot the current log addresses (the tail becomes the
//!    fold-over point).
//! 2. **Wait flush** — poll until all pages up to the checkpoint tail have
//!    been flushed to disk (`flushed_until >= checkpoint_tail`).
//! 3. **Complete** — finalize the checkpoint and produce a
//!    [`LogRecoveryInfo`] for serialization.
//!
//! This module does **not** perform the actual page I/O — that is handled by
//! the hybrid log's flush pipeline. The writer merely observes flush progress
//! and produces recovery metadata.

use std::time::Instant;

use crate::address::LogicalAddress;
use crate::hybrid_log::HybridLogAllocator;

use super::snapshot_writer::SnapshotCheckpointContext;
use super::{CheckpointError, CheckpointToken, CheckpointType, LogRecoveryInfo};

// ---------------------------------------------------------------------------
// LogCheckpointContext
// ---------------------------------------------------------------------------

/// Captures the hybrid log's address boundaries at checkpoint start.
///
/// Created by [`LogCheckpointWriter::begin_checkpoint`] and threaded through
/// the remaining checkpoint phases until [`LogCheckpointWriter::complete`]
/// consumes it to produce [`LogRecoveryInfo`].
#[derive(Debug)]
pub struct LogCheckpointContext {
    /// Unique token identifying this checkpoint.
    pub token: CheckpointToken,
    /// The checkpoint strategy in use.
    pub checkpoint_type: CheckpointType,
    /// Tail address at checkpoint start — the fold-over point.
    pub start_tail: LogicalAddress,
    /// Head address at checkpoint start.
    pub start_head: LogicalAddress,
    /// Begin address at checkpoint start.
    pub start_begin: LogicalAddress,
    /// Target flush point (same as `start_tail` for fold-over).
    pub flushed_until: LogicalAddress,
    /// Timestamp when the checkpoint began.
    pub started_at: Instant,
}

// ---------------------------------------------------------------------------
// LogCheckpointWriter
// ---------------------------------------------------------------------------

/// Coordinates the fold-over checkpoint protocol for the hybrid log.
///
/// The writer is stateless between calls — all per-checkpoint state lives in
/// the [`LogCheckpointContext`] that flows through the three protocol steps.
///
/// # Protocol
///
/// ```text
/// begin_checkpoint  ──►  wait_flush_complete  ──►  complete
///   (snapshot addrs)       (poll flush progress)     (emit LogRecoveryInfo)
/// ```
///
/// # Example (conceptual)
///
/// ```ignore
/// let writer = LogCheckpointWriter::new(CheckpointType::FoldOver);
/// let ctx = writer.begin_checkpoint(&log, &token)?;
/// // … trigger flush pipeline, poll …
/// writer.wait_flush_complete(&ctx, &log)?;
/// let recovery_info = writer.complete(ctx)?;
/// ```
pub struct LogCheckpointWriter {
    /// The checkpoint strategy this writer uses.
    checkpoint_type: CheckpointType,
}

impl LogCheckpointWriter {
    /// Creates a new log checkpoint writer for the given strategy.
    pub fn new(checkpoint_type: CheckpointType) -> Self {
        Self { checkpoint_type }
    }

    /// Returns the checkpoint type this writer uses.
    #[inline]
    pub fn checkpoint_type(&self) -> CheckpointType {
        self.checkpoint_type
    }

    /// Begin a fold-over checkpoint by snapshotting the current log addresses.
    ///
    /// The tail address at this moment becomes the fold-over point: no new
    /// records should be appended beyond it for the duration of the
    /// checkpoint. The caller is responsible for sealing the log (or
    /// coordinating with sessions) before calling this method.
    ///
    /// # Errors
    ///
    /// Returns [`CheckpointError::InvalidState`] if the checkpoint type is
    /// not [`CheckpointType::FoldOver`]. Use
    /// [`begin_snapshot_checkpoint`](Self::begin_snapshot_checkpoint) for
    /// snapshot mode.
    pub fn begin_checkpoint(
        &self,
        log: &HybridLogAllocator,
        token: &CheckpointToken,
    ) -> Result<LogCheckpointContext, CheckpointError> {
        if self.checkpoint_type != CheckpointType::FoldOver {
            return Err(CheckpointError::InvalidState(
                "use begin_snapshot_checkpoint() for Snapshot mode".into(),
            ));
        }

        let snap = log.snapshot();

        Ok(LogCheckpointContext {
            token: *token,
            checkpoint_type: self.checkpoint_type,
            start_tail: snap.tail_address,
            start_head: snap.head_address,
            start_begin: snap.begin_address,
            flushed_until: snap.tail_address,
            started_at: Instant::now(),
        })
    }

    /// Begin a snapshot checkpoint by capturing the current log boundaries.
    ///
    /// Unlike fold-over, the log may continue accepting writes after this
    /// call. The `snapshot_tail` in the returned context records the tail at
    /// this instant — any records appended later are **not** part of the
    /// checkpoint.
    ///
    /// # Errors
    ///
    /// Returns [`CheckpointError::InvalidState`] if this writer is not
    /// configured for [`CheckpointType::Snapshot`].
    pub fn begin_snapshot_checkpoint(
        &self,
        log: &HybridLogAllocator,
        token: &CheckpointToken,
    ) -> Result<SnapshotCheckpointContext, CheckpointError> {
        if self.checkpoint_type != CheckpointType::Snapshot {
            return Err(CheckpointError::InvalidState(
                "begin_snapshot_checkpoint requires Snapshot mode".into(),
            ));
        }

        SnapshotCheckpointContext::begin(log, token)
    }

    /// Check whether the log has been flushed up to the checkpoint tail.
    ///
    /// For a fold-over checkpoint this means
    /// `log.flushed_until_address() >= ctx.start_tail`. The caller should
    /// poll this method (or use the flush pipeline's notifications) until it
    /// returns `Ok(())`.
    ///
    /// # Errors
    ///
    /// Returns [`CheckpointError::InvalidState`] if flushing has not yet
    /// reached the checkpoint tail.
    pub fn wait_flush_complete(
        &self,
        ctx: &LogCheckpointContext,
        log: &HybridLogAllocator,
    ) -> Result<(), CheckpointError> {
        let flushed = log.flushed_until_address();
        if flushed >= ctx.start_tail {
            Ok(())
        } else {
            Err(CheckpointError::InvalidState(format!(
                "flush incomplete: flushed_until={} < checkpoint_tail={}",
                flushed.raw(),
                ctx.start_tail.raw(),
            )))
        }
    }

    /// Finalize a fold-over checkpoint and produce recovery metadata.
    ///
    /// Consumes the [`LogCheckpointContext`] and returns a
    /// [`LogRecoveryInfo`] capturing the exact address boundaries needed to
    /// recover the hybrid log from the checkpoint file.
    ///
    /// # Errors
    ///
    /// Returns [`CheckpointError::InvalidState`] if the checkpoint type in
    /// the context does not match this writer's type.
    pub fn complete(&self, ctx: LogCheckpointContext) -> Result<LogRecoveryInfo, CheckpointError> {
        if ctx.checkpoint_type != self.checkpoint_type {
            return Err(CheckpointError::InvalidState(format!(
                "context type {:?} does not match writer type {:?}",
                ctx.checkpoint_type, self.checkpoint_type,
            )));
        }

        Ok(LogRecoveryInfo {
            version: 1,
            checkpoint_type: ctx.checkpoint_type,
            begin_address: ctx.start_begin,
            flushed_until_address: ctx.start_tail,
            final_address: ctx.start_tail,
            head_address: ctx.start_head,
            // Fold-over does not use a snapshot file.
            snapshot_start_address: LogicalAddress::ZERO,
            snapshot_final_address: LogicalAddress::ZERO,
            use_snapshot_file: false,
            object_log_segment_count: 0,
        })
    }

    /// Finalize a snapshot checkpoint and produce recovery metadata.
    ///
    /// Consumes the [`SnapshotCheckpointContext`] and produces a
    /// [`LogRecoveryInfo`] with `use_snapshot_file = true`.
    ///
    /// # Errors
    ///
    /// Returns [`CheckpointError::InvalidState`] if this writer is not
    /// configured for [`CheckpointType::Snapshot`].
    pub fn complete_snapshot(
        &self,
        ctx: SnapshotCheckpointContext,
    ) -> Result<LogRecoveryInfo, CheckpointError> {
        if self.checkpoint_type != CheckpointType::Snapshot {
            return Err(CheckpointError::InvalidState(format!(
                "complete_snapshot requires Snapshot writer, got {:?}",
                self.checkpoint_type,
            )));
        }

        Ok(ctx.into_recovery_info())
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::address::{LogicalAddress, Offset, Page};

    const SECTOR_SIZE: usize = 512;
    const BUFFER_PAGES: usize = 4;
    const MUTABLE_FRAC: f64 = 0.5;

    fn make_allocator() -> HybridLogAllocator {
        HybridLogAllocator::new(BUFFER_PAGES, MUTABLE_FRAC, SECTOR_SIZE)
    }

    fn make_token(value: u128) -> CheckpointToken {
        CheckpointToken::new(value)
    }

    // -----------------------------------------------------------------------
    // Construction
    // -----------------------------------------------------------------------

    #[test]
    fn writer_new_fold_over() {
        let writer = LogCheckpointWriter::new(CheckpointType::FoldOver);
        assert_eq!(writer.checkpoint_type(), CheckpointType::FoldOver);
    }

    #[test]
    fn writer_new_snapshot() {
        let writer = LogCheckpointWriter::new(CheckpointType::Snapshot);
        assert_eq!(writer.checkpoint_type(), CheckpointType::Snapshot);
    }

    // -----------------------------------------------------------------------
    // begin_checkpoint — basic fold-over
    // -----------------------------------------------------------------------

    #[test]
    fn begin_checkpoint_empty_log() {
        let log = make_allocator();
        let writer = LogCheckpointWriter::new(CheckpointType::FoldOver);
        let token = make_token(1);

        let ctx = writer.begin_checkpoint(&log, &token).unwrap();

        assert_eq!(ctx.token, token);
        assert_eq!(ctx.checkpoint_type, CheckpointType::FoldOver);
        // Empty log: all addresses at zero.
        let zero = LogicalAddress::new(Page(0), Offset(0));
        assert_eq!(ctx.start_tail, zero);
        assert_eq!(ctx.start_head, zero);
        assert_eq!(ctx.start_begin, zero);
        assert_eq!(ctx.flushed_until, zero);
    }

    #[test]
    fn begin_checkpoint_with_data() {
        let log = make_allocator();
        // Allocate some records.
        log.try_allocate(64).unwrap();
        log.try_allocate(128).unwrap();

        let writer = LogCheckpointWriter::new(CheckpointType::FoldOver);
        let token = make_token(42);

        let ctx = writer.begin_checkpoint(&log, &token).unwrap();

        // Tail should be at offset 192 (64 + 128).
        assert_eq!(ctx.start_tail, LogicalAddress::new(Page(0), Offset(192)));
        // Head, begin still at zero (no eviction).
        let zero = LogicalAddress::new(Page(0), Offset(0));
        assert_eq!(ctx.start_head, zero);
        assert_eq!(ctx.start_begin, zero);
        // flushed_until target == tail for fold-over.
        assert_eq!(ctx.flushed_until, ctx.start_tail);
    }

    #[test]
    fn begin_checkpoint_captures_head_advance() {
        let log = make_allocator();
        log.try_allocate(64).unwrap();

        // Simulate head advance.
        let new_head = LogicalAddress::new(Page(0), Offset(32));
        log.try_advance_head(new_head);

        let writer = LogCheckpointWriter::new(CheckpointType::FoldOver);
        let token = make_token(7);
        let ctx = writer.begin_checkpoint(&log, &token).unwrap();

        assert_eq!(ctx.start_head, new_head);
    }

    #[test]
    fn begin_checkpoint_rejects_snapshot_type() {
        let log = make_allocator();
        let writer = LogCheckpointWriter::new(CheckpointType::Snapshot);
        let token = make_token(1);

        let err = writer.begin_checkpoint(&log, &token).unwrap_err();
        assert!(matches!(err, CheckpointError::InvalidState(_)));
    }

    // -----------------------------------------------------------------------
    // wait_flush_complete
    // -----------------------------------------------------------------------

    #[test]
    fn wait_flush_complete_empty_log() {
        let log = make_allocator();
        let writer = LogCheckpointWriter::new(CheckpointType::FoldOver);
        let token = make_token(1);

        let ctx = writer.begin_checkpoint(&log, &token).unwrap();
        // Empty log: flushed_until starts at zero == tail, so immediately done.
        writer.wait_flush_complete(&ctx, &log).unwrap();
    }

    #[test]
    fn wait_flush_complete_not_yet_flushed() {
        let log = make_allocator();
        log.try_allocate(64).unwrap();

        let writer = LogCheckpointWriter::new(CheckpointType::FoldOver);
        let token = make_token(1);
        let ctx = writer.begin_checkpoint(&log, &token).unwrap();

        // flushed_until is still at zero < tail (offset 64), should fail.
        let err = writer.wait_flush_complete(&ctx, &log).unwrap_err();
        assert!(matches!(err, CheckpointError::InvalidState(_)));
    }

    #[test]
    fn wait_flush_complete_after_flush_advance() {
        let log = make_allocator();
        log.try_allocate(64).unwrap();

        let writer = LogCheckpointWriter::new(CheckpointType::FoldOver);
        let token = make_token(1);
        let ctx = writer.begin_checkpoint(&log, &token).unwrap();

        // Simulate flush completing up to the checkpoint tail.
        log.try_advance_flushed_until(ctx.start_tail);

        writer.wait_flush_complete(&ctx, &log).unwrap();
    }

    #[test]
    fn wait_flush_complete_overshoot() {
        let log = make_allocator();
        log.try_allocate(64).unwrap();

        let writer = LogCheckpointWriter::new(CheckpointType::FoldOver);
        let token = make_token(1);
        let ctx = writer.begin_checkpoint(&log, &token).unwrap();

        // Advance flushed_until past the checkpoint tail.
        let beyond = LogicalAddress::new(Page(1), Offset(0));
        log.try_advance_flushed_until(beyond);

        writer.wait_flush_complete(&ctx, &log).unwrap();
    }

    // -----------------------------------------------------------------------
    // complete — recovery info generation
    // -----------------------------------------------------------------------

    #[test]
    fn complete_fold_over_empty_log() {
        let log = make_allocator();
        let writer = LogCheckpointWriter::new(CheckpointType::FoldOver);
        let token = make_token(99);
        let ctx = writer.begin_checkpoint(&log, &token).unwrap();

        let info = writer.complete(ctx).unwrap();

        assert_eq!(info.version, 1);
        assert_eq!(info.checkpoint_type, CheckpointType::FoldOver);
        let zero = LogicalAddress::ZERO;
        assert_eq!(info.begin_address, zero);
        assert_eq!(info.head_address, zero);
        assert_eq!(info.final_address, zero);
        assert_eq!(info.flushed_until_address, zero);
        assert_eq!(info.snapshot_start_address, zero);
        assert_eq!(info.snapshot_final_address, zero);
        assert!(!info.use_snapshot_file);
        assert_eq!(info.object_log_segment_count, 0);
    }

    #[test]
    fn complete_fold_over_with_data() {
        let log = make_allocator();
        log.try_allocate(64).unwrap();
        log.try_allocate(128).unwrap();

        let writer = LogCheckpointWriter::new(CheckpointType::FoldOver);
        let token = make_token(42);
        let ctx = writer.begin_checkpoint(&log, &token).unwrap();

        let info = writer.complete(ctx).unwrap();

        let expected_tail = LogicalAddress::new(Page(0), Offset(192));
        assert_eq!(info.final_address, expected_tail);
        assert_eq!(info.flushed_until_address, expected_tail);
        assert_eq!(info.begin_address, LogicalAddress::ZERO);
        assert_eq!(info.head_address, LogicalAddress::ZERO);
        assert!(!info.use_snapshot_file);
    }

    #[test]
    fn complete_recovery_info_serializable() {
        let log = make_allocator();
        log.try_allocate(64).unwrap();

        let writer = LogCheckpointWriter::new(CheckpointType::FoldOver);
        let token = make_token(1);
        let ctx = writer.begin_checkpoint(&log, &token).unwrap();
        let info = writer.complete(ctx).unwrap();

        // Round-trip through JSON.
        let json = serde_json::to_string(&info).unwrap();
        let back: LogRecoveryInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(info, back);
    }

    // -----------------------------------------------------------------------
    // complete — error paths
    // -----------------------------------------------------------------------

    #[test]
    fn complete_rejects_type_mismatch() {
        let log = make_allocator();
        let fold_writer = LogCheckpointWriter::new(CheckpointType::FoldOver);
        let token = make_token(1);
        let mut ctx = fold_writer.begin_checkpoint(&log, &token).unwrap();

        // Tamper with the context type.
        ctx.checkpoint_type = CheckpointType::Snapshot;

        let err = fold_writer.complete(ctx).unwrap_err();
        assert!(matches!(err, CheckpointError::InvalidState(_)));
    }

    // -----------------------------------------------------------------------
    // Full lifecycle
    // -----------------------------------------------------------------------

    #[test]
    fn full_fold_over_lifecycle() {
        let log = make_allocator();

        // Write some data.
        log.try_allocate(64).unwrap();
        log.try_allocate(128).unwrap();
        log.try_allocate(256).unwrap();

        let writer = LogCheckpointWriter::new(CheckpointType::FoldOver);
        let token = make_token(0xDEAD_BEEF);

        // 1. Begin
        let ctx = writer.begin_checkpoint(&log, &token).unwrap();
        let checkpoint_tail = ctx.start_tail;
        assert_eq!(checkpoint_tail, LogicalAddress::new(Page(0), Offset(448)));

        // 2. Flush not yet complete
        assert!(writer.wait_flush_complete(&ctx, &log).is_err());

        // 3. Simulate flush progress (partial)
        log.try_advance_flushed_until(LogicalAddress::new(Page(0), Offset(256)));
        assert!(writer.wait_flush_complete(&ctx, &log).is_err());

        // 4. Flush catches up
        log.try_advance_flushed_until(checkpoint_tail);
        writer.wait_flush_complete(&ctx, &log).unwrap();

        // 5. Complete
        let info = writer.complete(ctx).unwrap();
        assert_eq!(info.checkpoint_type, CheckpointType::FoldOver);
        assert_eq!(info.final_address, checkpoint_tail);
        assert_eq!(info.flushed_until_address, checkpoint_tail);
        assert!(!info.use_snapshot_file);
    }

    #[test]
    fn lifecycle_with_head_and_begin_advanced() {
        let log = make_allocator();

        // Allocate data across pages.
        let page_size = log.page_size();
        log.try_allocate(page_size - 64).unwrap();
        log.advance_to_next_page().unwrap();
        log.try_allocate(128).unwrap();

        // Advance head and begin to simulate eviction.
        let new_head = LogicalAddress::new(Page(0), Offset(512));
        let new_begin = LogicalAddress::new(Page(0), Offset(256));
        log.try_advance_head(new_head);
        log.try_advance_begin(new_begin);

        let writer = LogCheckpointWriter::new(CheckpointType::FoldOver);
        let token = make_token(7);

        let ctx = writer.begin_checkpoint(&log, &token).unwrap();
        assert_eq!(ctx.start_head, new_head);
        assert_eq!(ctx.start_begin, new_begin);

        // Simulate full flush.
        log.try_advance_flushed_until(ctx.start_tail);
        writer.wait_flush_complete(&ctx, &log).unwrap();

        let info = writer.complete(ctx).unwrap();
        assert_eq!(info.head_address, new_head);
        assert_eq!(info.begin_address, new_begin);
    }

    #[test]
    fn multiple_checkpoints_sequential() {
        let log = make_allocator();
        let writer = LogCheckpointWriter::new(CheckpointType::FoldOver);

        // First checkpoint (empty).
        let token1 = make_token(1);
        let ctx1 = writer.begin_checkpoint(&log, &token1).unwrap();
        let info1 = writer.complete(ctx1).unwrap();
        assert_eq!(info1.final_address, LogicalAddress::ZERO);

        // Add data.
        log.try_allocate(64).unwrap();

        // Second checkpoint.
        let token2 = make_token(2);
        let ctx2 = writer.begin_checkpoint(&log, &token2).unwrap();
        log.try_advance_flushed_until(ctx2.start_tail);
        writer.wait_flush_complete(&ctx2, &log).unwrap();
        let info2 = writer.complete(ctx2).unwrap();
        assert_eq!(
            info2.final_address,
            LogicalAddress::new(Page(0), Offset(64))
        );
    }

    // -----------------------------------------------------------------------
    // Context fields
    // -----------------------------------------------------------------------

    #[test]
    fn context_started_at_is_recent() {
        let log = make_allocator();
        let writer = LogCheckpointWriter::new(CheckpointType::FoldOver);
        let before = Instant::now();
        let ctx = writer.begin_checkpoint(&log, &make_token(1)).unwrap();
        let after = Instant::now();

        assert!(ctx.started_at >= before);
        assert!(ctx.started_at <= after);
    }

    #[test]
    fn context_token_preserved() {
        let log = make_allocator();
        let writer = LogCheckpointWriter::new(CheckpointType::FoldOver);
        let token = make_token(0xCAFE_BABE);
        let ctx = writer.begin_checkpoint(&log, &token).unwrap();
        assert_eq!(ctx.token, token);
    }

    #[test]
    fn context_debug_format() {
        let log = make_allocator();
        let writer = LogCheckpointWriter::new(CheckpointType::FoldOver);
        let ctx = writer.begin_checkpoint(&log, &make_token(1)).unwrap();
        let dbg = format!("{ctx:?}");
        assert!(dbg.contains("LogCheckpointContext"));
        assert!(dbg.contains("FoldOver"));
    }

    // -----------------------------------------------------------------------
    // Allocator flushed_until integration
    // -----------------------------------------------------------------------

    #[test]
    fn flushed_until_monotonic_advance() {
        let log = make_allocator();
        let zero = LogicalAddress::new(Page(0), Offset(0));
        assert_eq!(log.flushed_until_address(), zero);

        let addr1 = LogicalAddress::new(Page(0), Offset(100));
        let result = log.try_advance_flushed_until(addr1);
        assert_eq!(result, addr1);
        assert_eq!(log.flushed_until_address(), addr1);

        // Attempt to go backwards — should stay at addr1.
        let result = log.try_advance_flushed_until(zero);
        assert_eq!(result, addr1);
        assert_eq!(log.flushed_until_address(), addr1);

        // Advance further.
        let addr2 = LogicalAddress::new(Page(1), Offset(0));
        let result = log.try_advance_flushed_until(addr2);
        assert_eq!(result, addr2);
        assert_eq!(log.flushed_until_address(), addr2);
    }

    // -----------------------------------------------------------------------
    // Snapshot checkpoint via LogCheckpointWriter
    // -----------------------------------------------------------------------

    #[test]
    fn begin_snapshot_checkpoint_empty_log() {
        let log = make_allocator();
        let writer = LogCheckpointWriter::new(CheckpointType::Snapshot);
        let token = make_token(1);

        let ctx = writer.begin_snapshot_checkpoint(&log, &token).unwrap();

        assert_eq!(ctx.token, token);
        assert_eq!(ctx.snapshot_tail, LogicalAddress::ZERO);
        assert_eq!(ctx.snapshot_head, LogicalAddress::ZERO);
        assert_eq!(ctx.begin_address, LogicalAddress::ZERO);
    }

    #[test]
    fn begin_snapshot_checkpoint_with_data() {
        let log = make_allocator();
        log.try_allocate(64).unwrap();
        log.try_allocate(128).unwrap();

        let writer = LogCheckpointWriter::new(CheckpointType::Snapshot);
        let token = make_token(42);
        let ctx = writer.begin_snapshot_checkpoint(&log, &token).unwrap();

        assert_eq!(ctx.snapshot_tail, LogicalAddress::new(Page(0), Offset(192)));
        assert_eq!(ctx.snapshot_head, LogicalAddress::ZERO);
    }

    #[test]
    fn begin_snapshot_rejects_fold_over_writer() {
        let log = make_allocator();
        let writer = LogCheckpointWriter::new(CheckpointType::FoldOver);
        let token = make_token(1);

        let err = writer.begin_snapshot_checkpoint(&log, &token).unwrap_err();
        assert!(matches!(err, CheckpointError::InvalidState(_)));
    }

    #[test]
    fn complete_snapshot_empty_log() {
        let log = make_allocator();
        let writer = LogCheckpointWriter::new(CheckpointType::Snapshot);
        let token = make_token(99);
        let ctx = writer.begin_snapshot_checkpoint(&log, &token).unwrap();

        let info = writer.complete_snapshot(ctx).unwrap();

        assert_eq!(info.version, 1);
        assert_eq!(info.checkpoint_type, CheckpointType::Snapshot);
        assert!(info.use_snapshot_file);
        assert_eq!(info.final_address, LogicalAddress::ZERO);
    }

    #[test]
    fn complete_snapshot_with_data() {
        let log = make_allocator();
        log.try_allocate(64).unwrap();

        let writer = LogCheckpointWriter::new(CheckpointType::Snapshot);
        let token = make_token(42);
        let ctx = writer.begin_snapshot_checkpoint(&log, &token).unwrap();
        let info = writer.complete_snapshot(ctx).unwrap();

        let expected_tail = LogicalAddress::new(Page(0), Offset(64));
        assert_eq!(info.final_address, expected_tail);
        assert_eq!(info.snapshot_final_address, expected_tail);
        assert!(info.use_snapshot_file);
    }

    #[test]
    fn complete_snapshot_rejects_fold_over_writer() {
        let log = make_allocator();
        let fold_writer = LogCheckpointWriter::new(CheckpointType::FoldOver);
        let snap_writer = LogCheckpointWriter::new(CheckpointType::Snapshot);
        let token = make_token(1);

        let ctx = snap_writer.begin_snapshot_checkpoint(&log, &token).unwrap();
        let err = fold_writer.complete_snapshot(ctx).unwrap_err();
        assert!(matches!(err, CheckpointError::InvalidState(_)));
    }

    #[test]
    fn snapshot_recovery_info_serializable() {
        let log = make_allocator();
        log.try_allocate(64).unwrap();

        let writer = LogCheckpointWriter::new(CheckpointType::Snapshot);
        let token = make_token(1);
        let ctx = writer.begin_snapshot_checkpoint(&log, &token).unwrap();
        let info = writer.complete_snapshot(ctx).unwrap();

        let json = serde_json::to_string(&info).unwrap();
        let back: LogRecoveryInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(info, back);
    }

    #[test]
    fn full_snapshot_via_writer_lifecycle() {
        let log = make_allocator();
        log.try_allocate(64).unwrap();
        log.try_allocate(128).unwrap();

        let writer = LogCheckpointWriter::new(CheckpointType::Snapshot);
        let token = make_token(0xCAFE);

        // Begin snapshot.
        let ctx = writer.begin_snapshot_checkpoint(&log, &token).unwrap();
        let snapshot_tail = ctx.snapshot_tail;

        // Simulate continued writes after snapshot.
        log.try_allocate(256).unwrap();
        assert!(log.tail_address() > snapshot_tail);

        // Complete — snapshot_tail is preserved.
        let info = writer.complete_snapshot(ctx).unwrap();
        assert_eq!(info.final_address, snapshot_tail);
        assert_eq!(info.checkpoint_type, CheckpointType::Snapshot);
        assert!(info.use_snapshot_file);
    }
}
