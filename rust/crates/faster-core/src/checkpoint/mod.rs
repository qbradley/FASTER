//! Checkpoint and recovery metadata for the FASTER hybrid log.
//!
//! This module defines the metadata structures persisted during checkpoint
//! and read back during recovery. The design mirrors the C++ `IndexMetadata` /
//! `LogMetadata` and C# `IndexRecoveryInfo` / `HybridLogRecoveryInfo` types.
//!
//! All types serialize to JSON via [`serde`] for human-readable checkpoint files.

mod metadata;

pub use metadata::{
    CheckpointToken, CheckpointType, IndexRecoveryInfo, LogRecoveryInfo, SessionRecoveryInfo,
};
