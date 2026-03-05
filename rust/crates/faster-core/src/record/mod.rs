//! Record format and inline variable-length key-value storage.
//!
//! Defines the on-disk and in-memory record layout for FASTER's hybrid log.
//! Records store keys and values contiguously for optimal cache locality,
//! following the C++ inline model (not the C# dual-log model).
//!
//! # Record structure
//!
//! ```text
//! ┌────────────────────┬───────────┬──────────────┬───────────┬────────────────┬──────────┐
//! │  RecordInfo (8B)   │  padding  │  Key (K B)   │  padding  │  Value (V B)   │  padding │
//! └────────────────────┴───────────┴──────────────┴───────────┴────────────────┴──────────┘
//! ```
//!
//! All records are 8-byte aligned. The [`RecordInfo`] header occupies the first
//! 8 bytes and encodes a version-chain pointer, checkpoint version, and status
//! flags as a packed `u64`.
//!
//! # Modules
//!
//! - [`record_info`] — [`RecordInfo`] header and [`AtomicRecordInfo`]
//! - [`traits`] — [`Key`] and [`Value`] serialization traits
//! - [`layout`] — [`RecordLayout`] offset computation, record read/write helpers

mod layout;
pub(crate) mod record_info;
mod traits;

// Re-export the public API at the `record` module level.
pub use layout::{
    RECORD_ALIGNMENT, RECORD_HEADER_SIZE, RecordLayout, pad_alignment, read_key, read_record_info,
    read_value, record_size, write_record,
};
pub use record_info::{AtomicRecordInfo, RecordInfo};
pub use traits::{FixedSizeKey, FixedSizeValue, Key, Value};
