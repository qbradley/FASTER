//! Async checkpoint and recovery helpers.
//!
//! # Checkpoint in the Async World
//!
//! FASTER's checkpoint operation is synchronous and CPU-intensive (it
//! flushes in-memory pages to disk, writes index metadata, etc.). In a
//! Tokio application, we run it inside `spawn_blocking` to avoid blocking
//! the async runtime.
//!
//! # Recovery Pattern
//!
//! Recovery requires a *mutable* reference to `FasterKv`, which means the
//! store cannot be shared via `Arc` during recovery. The pattern is:
//!
//! 1. Drop the old `AsyncFasterKv` (releases the Arc)
//! 2. Create a new `FasterKv` with the same config and device
//! 3. Call `store.recover()` on the mutable reference
//! 4. Wrap it back in `AsyncFasterKv`
//!
//! This mirrors how a real application would restart after a crash —
//! you create a fresh store and recover from the last checkpoint.
//!
//! # KEY DIFFERENCE from Thread Version
//!
//! The thread version can checkpoint inline (it owns the store directly).
//! The Tokio version must:
//! - Use `spawn_blocking` for the checkpoint call
//! - Use `Arc::try_unwrap` or fresh construction for recovery (since
//!   `recover` needs `&mut self`)
//!
//! Both versions use the same underlying checkpoint format and can
//! recover each other's checkpoints.

use std::path::Path;
use std::sync::Arc;

use faster_core::checkpoint::{CheckpointToken, CheckpointType};
use faster_core::store::{FasterKv, RecoveryInfo};

use crate::functions::CampaignFunctions;

/// Take a checkpoint of the FASTER store.
///
/// Runs the checkpoint operation inside `spawn_blocking` because it
/// involves synchronous disk I/O that would block Tokio's async workers.
///
/// # Arguments
///
/// * `store` — Arc-shared FASTER store
/// * `checkpoint_dir` — Directory to write checkpoint files
/// * `checkpoint_type` — FoldOver (flush pages) or Snapshot (copy-on-write)
///
/// # Returns
///
/// The checkpoint token (a UUID) that identifies this checkpoint for
/// later recovery. Returns `None` if the checkpoint failed.
///
/// # When to Use FoldOver vs Snapshot
///
/// | Type | Pros | Cons | Use when... |
/// |------|------|------|------------|
/// | FoldOver | Faster, less disk space | Mutates the main log | You can tolerate brief write pauses |
/// | Snapshot | Non-blocking for writers | Uses extra disk space | Writers must not stall |
///
/// This sample uses FoldOver for simplicity — it's the more common choice
/// for batch workloads where a brief pause is acceptable.
pub async fn take_checkpoint(
    store: Arc<FasterKv<CampaignFunctions>>,
    checkpoint_dir: &Path,
    checkpoint_type: CheckpointType,
) -> Option<CheckpointToken> {
    let dir = checkpoint_dir.to_path_buf();

    // spawn_blocking because checkpoint() does synchronous I/O:
    // - Flushes all in-memory pages to the device
    // - Writes index and log metadata files
    // - Creates checkpoint directory structure
    //
    // Without spawn_blocking, this would block the Tokio runtime thread
    // and starve all other async tasks (stats reporter, shutdown handler, etc.)
    let result = tokio::task::spawn_blocking(move || {
        store.checkpoint(&dir, checkpoint_type)
    })
    .await;

    match result {
        Ok(Ok(token)) => {
            eprintln!("  ✓ checkpoint taken: {token}");
            Some(token)
        }
        Ok(Err(e)) => {
            eprintln!("  ✗ checkpoint failed: {e:?}");
            None
        }
        Err(e) => {
            eprintln!("  ✗ checkpoint task panicked: {e}");
            None
        }
    }
}

/// Recover a FASTER store from a checkpoint.
///
/// Creates a fresh `FasterKv` instance and recovers state from the
/// specified checkpoint (or the most recent one if `token` is None).
///
/// # Why Create a Fresh Store?
///
/// `FasterKv::recover` requires `&mut self`, which is incompatible with
/// `Arc<FasterKv>`. This mirrors real crash recovery: after a process
/// crash, you construct a new store from scratch and recover into it.
///
/// # Arguments
///
/// * `checkpoint_dir` — Directory containing checkpoint files
/// * `token` — Specific checkpoint to recover (None = most recent)
/// * `config` — Store configuration (must match the original)
/// * `device` — Fresh device instance (pointed at the same files)
///
/// # Returns
///
/// `(FasterKv, RecoveryInfo)` on success, or an error string.
pub fn recover_store(
    checkpoint_dir: &Path,
    token: Option<CheckpointToken>,
    config: faster_core::store::FasterKvConfig,
    device: impl faster_core::Device,
) -> Result<(FasterKv<CampaignFunctions>, RecoveryInfo), String> {
    // Create a fresh store — all state will come from the checkpoint.
    let mut store = FasterKv::new(config, CampaignFunctions, device);

    // Recover from checkpoint. This:
    // 1. Reads index metadata and rebuilds the hash index
    // 2. Reads log metadata and restores address boundaries
    // 3. Loads pages from device into memory
    //
    // After recovery, the store is in the exact state it was at
    // checkpoint time. Any events written after the checkpoint are lost
    // (this is the expected behavior for crash recovery).
    let info = store
        .recover(checkpoint_dir, token)
        .map_err(|e| format!("recovery failed: {e:?}"))?;

    Ok((store, info))
}
