//! Checkpoint and recovery metadata for the FASTER hybrid log.
//!
//! This module defines the metadata structures persisted during checkpoint
//! and read back during recovery. The design mirrors the C++ `IndexMetadata` /
//! `LogMetadata` and C# `IndexRecoveryInfo` / `HybridLogRecoveryInfo` types.
//!
//! All types serialize to JSON via [`serde`] for human-readable checkpoint files.
//!
//! The [`CheckpointManager`] trait abstracts checkpoint coordination,
//! and [`CheckpointManagerImpl`] provides a thread-safe in-memory default.

mod manager;
mod metadata;
pub mod metadata_store;
mod participant;
mod session_state;
mod state_machine;

pub mod index_writer;
pub mod log_writer;
pub mod snapshot_writer;

pub use manager::{
    CheckpointError, CheckpointManager, CheckpointManagerImpl, CheckpointStatus,
};
pub use metadata::{
    CheckpointToken, CheckpointType, IndexRecoveryInfo, LogRecoveryInfo, SessionRecoveryInfo,
};
pub use metadata_store::{CheckpointMetadata, CheckpointMetadataStore};
pub use participant::CheckpointParticipant;
pub use session_state::SessionCheckpointState;
pub use state_machine::{CheckpointPhase, CheckpointStateMachine};
