//! Hash index subsystem for FASTER.
//!
//! This module groups the hash-related components:
//!
//! - [`hash`] — `FasterHash` helpers, [`KeyHash`], [`Hashable`] trait
//! - [`bucket`] — [`HashBucket`] and [`HashBucketEntry`] layout
//! - [`table`] — Latch-free concurrent [`HashTable`]
//! - [`index`] — High-level [`HashIndex`] (table + epoch integration)
//! - [`overflow`] — [`OverflowBucketPool`] for overflow bucket chains

pub mod bucket;
#[allow(clippy::module_inception)]
pub mod hash;
pub mod index;
pub mod overflow;
pub mod table;

// Re-export items from `hash/hash.rs` at this level so that
// `crate::hash::Hashable`, `crate::hash::KeyHash`, etc. keep working.
pub use self::hash::*;
