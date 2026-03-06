//! Checkpoint infrastructure for the FASTER hybrid log.
//!
//! FASTER supports two checkpoint strategies:
//!
//! - **Fold-over** — the in-memory portion of the log is "folded" into the
//!   on-disk portion by flushing dirty pages. This is fast but blocks new
//!   writes to flushed regions during the checkpoint.
//! - **Snapshot** — a point-in-time copy of the log is written to a separate
//!   file, allowing writers to continue unimpeded.
//!
//! Both strategies persist the hash **index** and the hybrid **log**
//! independently. Recovery reads these back to restore the store to a
//! consistent state (see the [`recovery`](crate::recovery) module).
//!
//! # Key Types
//!
//! | Type | Purpose |
//! |------|---------|
//! | [`CheckpointOrchestrator`] | Coordinates the multi-phase checkpoint process |
//! | [`CheckpointStateMachine`] | Phase-based state machine driving checkpoint lifecycle |
//! | [`CheckpointManager`] | Trait abstracting checkpoint coordination |
//! | [`CheckpointManagerImpl`] | Thread-safe in-memory default implementation |
//! | [`CheckpointToken`] | Unique identifier (UUID) for a checkpoint |
//! | [`CheckpointType`] | Enum: `FoldOver` or `Snapshot` |
//! | [`IndexRecoveryInfo`] | Persisted index metadata |
//! | [`LogRecoveryInfo`] | Persisted log metadata |
//! | [`CheckpointMetadataStore`] | Filesystem-backed metadata persistence |
//!
//! All metadata types serialize to JSON via [`serde`] for human-readable
//! checkpoint files. The design mirrors the C++ `IndexMetadata` /
//! `LogMetadata` and C# `IndexRecoveryInfo` / `HybridLogRecoveryInfo` types.

mod manager;
mod metadata;
pub mod metadata_store;
pub mod orchestrator;
mod participant;
mod session_state;
mod state_machine;

pub mod index_writer;
pub mod log_writer;
pub mod snapshot_writer;

pub use manager::{CheckpointError, CheckpointManager, CheckpointManagerImpl, CheckpointStatus};
pub use metadata::{
    CheckpointToken, CheckpointType, IndexRecoveryInfo, LogRecoveryInfo, SessionRecoveryInfo,
};
pub use metadata_store::{CheckpointMetadata, CheckpointMetadataStore};
pub use orchestrator::{CheckpointConfig, CheckpointOrchestrator};
pub use participant::CheckpointParticipant;
pub use session_state::SessionCheckpointState;
pub use state_machine::{CheckpointPhase, CheckpointStateMachine};
