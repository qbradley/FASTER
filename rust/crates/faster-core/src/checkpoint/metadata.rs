//! Checkpoint metadata structures for index, log, and session recovery.
//!
//! These types capture the state of a FASTER instance at checkpoint time
//! and are serialized to disk (JSON). During recovery the metadata is read
//! back to reconstruct the exact same state.

use serde::{Deserialize, Serialize};

use crate::address::LogicalAddress;

// ---------------------------------------------------------------------------
// CheckpointType
// ---------------------------------------------------------------------------

/// The strategy used to persist the hybrid log during a checkpoint.
///
/// - **FoldOver**: the log tail is frozen in place and all in-memory pages
///   are flushed to the main log file. Simple, but the log file grows
///   monotonically.
/// - **Snapshot**: the mutable region is written to a separate snapshot file,
///   leaving the main log file untouched. Enables incremental checkpoints.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CheckpointType {
    /// Flush all in-memory pages to the main log file.
    FoldOver,
    /// Write a snapshot of the mutable region to a separate file.
    Snapshot,
}

// ---------------------------------------------------------------------------
// CheckpointToken
// ---------------------------------------------------------------------------

/// A unique, opaque identifier for a checkpoint.
///
/// Wraps a `u128` that can represent a UUID (or any other 128-bit id scheme).
/// Two checkpoints are distinguished solely by their token value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CheckpointToken(pub u128);

impl CheckpointToken {
    /// Creates a new token from a raw 128-bit value.
    #[inline]
    pub const fn new(value: u128) -> Self {
        Self(value)
    }

    /// Returns the underlying 128-bit value.
    #[inline]
    pub const fn as_u128(self) -> u128 {
        self.0
    }
}

impl core::fmt::Display for CheckpointToken {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // Format as a UUID-style hex string for readability.
        let hi = (self.0 >> 64) as u64;
        let lo = self.0 as u64;
        write!(f, "{hi:016x}-{lo:016x}")
    }
}

// ---------------------------------------------------------------------------
// IndexRecoveryInfo
// ---------------------------------------------------------------------------

/// Metadata captured from the hash index at checkpoint time.
///
/// Corresponds to C++ `IndexMetadata` and C# `IndexRecoveryInfo`.
/// During recovery this tells us the exact size and shape of the hash table
/// as well as the log address range it referenced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexRecoveryInfo {
    /// System version at the time the index checkpoint was taken.
    pub version: u32,
    /// Number of hash-table buckets (main table only).
    pub table_size: u64,
    /// Size of the main hash table in bytes.
    pub num_ht_bytes: u64,
    /// Size of the overflow-bucket area in bytes.
    pub num_ofb_bytes: u64,
    /// Total bucket count including overflow buckets.
    pub num_buckets: u64,
    /// Earliest valid log address at checkpoint time.
    pub start_logical_address: LogicalAddress,
    /// Log tail address at checkpoint time.
    pub final_logical_address: LogicalAddress,
}

impl Default for IndexRecoveryInfo {
    fn default() -> Self {
        Self {
            version: 0,
            table_size: 0,
            num_ht_bytes: 0,
            num_ofb_bytes: 0,
            num_buckets: 0,
            start_logical_address: LogicalAddress::ZERO,
            final_logical_address: LogicalAddress::ZERO,
        }
    }
}

// ---------------------------------------------------------------------------
// LogRecoveryInfo
// ---------------------------------------------------------------------------

/// Metadata captured from the hybrid log at checkpoint time.
///
/// Corresponds to C++ `LogMetadata` and C# `HybridLogRecoveryInfo`.
/// Captures every address boundary needed to reconstruct the log state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogRecoveryInfo {
    /// System version at the time the log checkpoint was taken.
    pub version: u32,
    /// The checkpoint strategy that was used.
    pub checkpoint_type: CheckpointType,
    /// Earliest valid address in the log (begin pointer).
    pub begin_address: LogicalAddress,
    /// How far the log had been flushed to disk at checkpoint time.
    pub flushed_until_address: LogicalAddress,
    /// Tail of the log at checkpoint time.
    pub final_address: LogicalAddress,
    /// Head address — boundary between on-disk and in-memory pages.
    pub head_address: LogicalAddress,
    /// Start of the snapshot region (meaningful only for [`CheckpointType::Snapshot`]).
    pub snapshot_start_address: LogicalAddress,
    /// End of the snapshot region (exclusive).
    pub snapshot_final_address: LogicalAddress,
    /// Whether a separate snapshot file was used for this checkpoint.
    pub use_snapshot_file: bool,
    /// Number of object-log segments (for stores with object values).
    pub object_log_segment_count: u64,
}

impl Default for LogRecoveryInfo {
    fn default() -> Self {
        Self {
            version: 0,
            checkpoint_type: CheckpointType::FoldOver,
            begin_address: LogicalAddress::ZERO,
            flushed_until_address: LogicalAddress::ZERO,
            final_address: LogicalAddress::ZERO,
            head_address: LogicalAddress::ZERO,
            snapshot_start_address: LogicalAddress::ZERO,
            snapshot_final_address: LogicalAddress::ZERO,
            use_snapshot_file: false,
            object_log_segment_count: 0,
        }
    }
}

// ---------------------------------------------------------------------------
// SessionRecoveryInfo
// ---------------------------------------------------------------------------

/// Per-session state captured at checkpoint time.
///
/// Corresponds to the C# `CommitPoint` / continue-token map entries and the
/// C++ `monotonic_serial_nums` array. During recovery each session can resume
/// from its recorded serial number, skipping operations that were in-flight
/// (and therefore excluded).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SessionRecoveryInfo {
    /// Unique identifier for the session.
    pub session_id: u32,
    /// The monotonic serial number committed at checkpoint time.
    pub serial_number: u64,
    /// Serial numbers of operations that were pending (in-flight) at
    /// checkpoint time and therefore excluded from the persisted state.
    pub excluded_serial_numbers: Vec<u64>,
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::address::{LogicalAddress, Offset, Page};

    // -----------------------------------------------------------------------
    // Default construction
    // -----------------------------------------------------------------------

    #[test]
    fn checkpoint_type_clone_eq() {
        let a = CheckpointType::FoldOver;
        let b = a;
        assert_eq!(a, b);
        assert_ne!(CheckpointType::FoldOver, CheckpointType::Snapshot);
    }

    #[test]
    fn checkpoint_token_display() {
        let token = CheckpointToken::new(0x0001_0002_0003_0004_0005_0006_0007_0008);
        let s = format!("{token}");
        assert!(s.contains('-'));
    }

    #[test]
    fn checkpoint_token_round_trip() {
        let val: u128 = 42;
        let token = CheckpointToken::new(val);
        assert_eq!(token.as_u128(), val);
    }

    #[test]
    fn index_recovery_info_default() {
        let info = IndexRecoveryInfo::default();
        assert_eq!(info.version, 0);
        assert_eq!(info.table_size, 0);
        assert_eq!(info.num_ht_bytes, 0);
        assert_eq!(info.num_ofb_bytes, 0);
        assert_eq!(info.num_buckets, 0);
        assert_eq!(info.start_logical_address, LogicalAddress::ZERO);
        assert_eq!(info.final_logical_address, LogicalAddress::ZERO);
    }

    #[test]
    fn log_recovery_info_default() {
        let info = LogRecoveryInfo::default();
        assert_eq!(info.version, 0);
        assert_eq!(info.checkpoint_type, CheckpointType::FoldOver);
        assert!(!info.use_snapshot_file);
        assert_eq!(info.object_log_segment_count, 0);
    }

    #[test]
    fn session_recovery_info_default() {
        let info = SessionRecoveryInfo::default();
        assert_eq!(info.session_id, 0);
        assert_eq!(info.serial_number, 0);
        assert!(info.excluded_serial_numbers.is_empty());
    }

    // -----------------------------------------------------------------------
    // Round-trip serialization (JSON)
    // -----------------------------------------------------------------------

    #[test]
    fn checkpoint_type_serde_round_trip() {
        for ct in [CheckpointType::FoldOver, CheckpointType::Snapshot] {
            let json = serde_json::to_string(&ct).unwrap();
            let back: CheckpointType = serde_json::from_str(&json).unwrap();
            assert_eq!(ct, back);
        }
    }

    #[test]
    fn checkpoint_token_serde_round_trip() {
        let token = CheckpointToken::new(0xDEAD_BEEF_CAFE_BABE_1234_5678_9ABC_DEF0);
        let json = serde_json::to_string(&token).unwrap();
        let back: CheckpointToken = serde_json::from_str(&json).unwrap();
        assert_eq!(token, back);
    }

    #[test]
    fn index_recovery_info_serde_round_trip() {
        let info = IndexRecoveryInfo {
            version: 7,
            table_size: 1 << 20,
            num_ht_bytes: (1 << 20) * 64,
            num_ofb_bytes: 4096,
            num_buckets: (1 << 20) + 64,
            start_logical_address: LogicalAddress::new(Page(0), Offset(64)),
            final_logical_address: LogicalAddress::new(Page(100), Offset(8192)),
        };
        let json = serde_json::to_string_pretty(&info).unwrap();
        let back: IndexRecoveryInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(info, back);
    }

    #[test]
    fn log_recovery_info_serde_round_trip() {
        let info = LogRecoveryInfo {
            version: 3,
            checkpoint_type: CheckpointType::Snapshot,
            begin_address: LogicalAddress::new(Page(0), Offset(64)),
            flushed_until_address: LogicalAddress::new(Page(50), Offset(0)),
            final_address: LogicalAddress::new(Page(100), Offset(4096)),
            head_address: LogicalAddress::new(Page(10), Offset(0)),
            snapshot_start_address: LogicalAddress::new(Page(50), Offset(0)),
            snapshot_final_address: LogicalAddress::new(Page(100), Offset(4096)),
            use_snapshot_file: true,
            object_log_segment_count: 12,
        };
        let json = serde_json::to_string_pretty(&info).unwrap();
        let back: LogRecoveryInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(info, back);
    }

    #[test]
    fn session_recovery_info_serde_round_trip() {
        let info = SessionRecoveryInfo {
            session_id: 42,
            serial_number: 999_999,
            excluded_serial_numbers: vec![100, 200, 300],
        };
        let json = serde_json::to_string_pretty(&info).unwrap();
        let back: SessionRecoveryInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(info, back);
    }

    #[test]
    fn log_recovery_info_fold_over_round_trip() {
        let info = LogRecoveryInfo {
            version: 1,
            checkpoint_type: CheckpointType::FoldOver,
            begin_address: LogicalAddress::ZERO,
            flushed_until_address: LogicalAddress::new(Page(200), Offset(0)),
            final_address: LogicalAddress::new(Page(200), Offset(0)),
            head_address: LogicalAddress::new(Page(100), Offset(0)),
            snapshot_start_address: LogicalAddress::ZERO,
            snapshot_final_address: LogicalAddress::ZERO,
            use_snapshot_file: false,
            object_log_segment_count: 0,
        };
        let json = serde_json::to_string(&info).unwrap();
        let back: LogRecoveryInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(info, back);
    }

    #[test]
    fn session_recovery_info_empty_excluded() {
        let info = SessionRecoveryInfo {
            session_id: 1,
            serial_number: 500,
            excluded_serial_numbers: vec![],
        };
        let json = serde_json::to_string(&info).unwrap();
        let back: SessionRecoveryInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(info, back);
        assert!(back.excluded_serial_numbers.is_empty());
    }

    // -----------------------------------------------------------------------
    // Property-based tests: arbitrary → serialize → deserialize = identity
    // -----------------------------------------------------------------------

    mod prop {
        use super::*;
        use proptest::prelude::*;

        fn logical_address_strategy() -> impl Strategy<Value = LogicalAddress> {
            (
                0u32..=crate::address::MAX_PAGE,
                0u32..=crate::address::MAX_OFFSET,
            )
                .prop_map(|(p, o)| LogicalAddress::new(Page(p), Offset(o)))
        }

        fn checkpoint_type_strategy() -> impl Strategy<Value = CheckpointType> {
            prop_oneof![
                Just(CheckpointType::FoldOver),
                Just(CheckpointType::Snapshot),
            ]
        }

        proptest! {
            #[test]
            fn checkpoint_token_identity(val: u128) {
                let token = CheckpointToken::new(val);
                let json = serde_json::to_string(&token).unwrap();
                let back: CheckpointToken = serde_json::from_str(&json).unwrap();
                prop_assert_eq!(token, back);
            }

            #[test]
            fn index_recovery_info_identity(
                version: u32,
                table_size: u64,
                num_ht_bytes: u64,
                num_ofb_bytes: u64,
                num_buckets: u64,
                start in logical_address_strategy(),
                final_addr in logical_address_strategy(),
            ) {
                let info = IndexRecoveryInfo {
                    version,
                    table_size,
                    num_ht_bytes,
                    num_ofb_bytes,
                    num_buckets,
                    start_logical_address: start,
                    final_logical_address: final_addr,
                };
                let json = serde_json::to_string(&info).unwrap();
                let back: IndexRecoveryInfo = serde_json::from_str(&json).unwrap();
                prop_assert_eq!(info, back);
            }

            #[test]
            fn log_recovery_info_identity(
                version: u32,
                ct in checkpoint_type_strategy(),
                begin in logical_address_strategy(),
                flushed in logical_address_strategy(),
                final_addr in logical_address_strategy(),
                head in logical_address_strategy(),
                snap_start in logical_address_strategy(),
                snap_final in logical_address_strategy(),
                use_snapshot: bool,
                seg_count: u64,
            ) {
                let info = LogRecoveryInfo {
                    version,
                    checkpoint_type: ct,
                    begin_address: begin,
                    flushed_until_address: flushed,
                    final_address: final_addr,
                    head_address: head,
                    snapshot_start_address: snap_start,
                    snapshot_final_address: snap_final,
                    use_snapshot_file: use_snapshot,
                    object_log_segment_count: seg_count,
                };
                let json = serde_json::to_string(&info).unwrap();
                let back: LogRecoveryInfo = serde_json::from_str(&json).unwrap();
                prop_assert_eq!(info, back);
            }

            #[test]
            fn session_recovery_info_identity(
                session_id: u32,
                serial_number: u64,
                excluded in proptest::collection::vec(any::<u64>(), 0..20),
            ) {
                let info = SessionRecoveryInfo {
                    session_id,
                    serial_number,
                    excluded_serial_numbers: excluded,
                };
                let json = serde_json::to_string(&info).unwrap();
                let back: SessionRecoveryInfo = serde_json::from_str(&json).unwrap();
                prop_assert_eq!(info, back);
            }
        }
    }
}
