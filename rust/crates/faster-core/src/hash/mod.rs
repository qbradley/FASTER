//! Hash index subsystem for FASTER.
//!
//! The hash index is a latch-free concurrent hash table that maps keys to
//! record addresses in the hybrid log. It is the primary lookup structure
//! and supports millions of concurrent point-lookups per second.
//!
//! Each hash bucket contains a fixed number of inline entries plus an
//! overflow chain for handling collisions. The index supports online resize
//! (grow) — see the [`grow`](crate::grow) module.
//!
//! # Submodules
//!
//! - [`hash`] — `FasterHash` helpers, [`KeyHash`], [`Hashable`] trait. All
//!   keys must implement `Hashable` to be usable with the store.
//! - [`bucket`] — `HashBucket` and `HashBucketEntry` — the physical layout
//!   of a single hash bucket (7 entries + 1 overflow pointer per 64-byte line).
//! - [`table`] — Latch-free concurrent `HashTable` with find/insert/update
//!   operations using atomic CAS.
//! - [`index`] — High-level `HashIndex` combining the table with epoch
//!   integration and overflow bucket management.
//! - [`overflow`] — `OverflowBucketPool` — pre-allocated pool for overflow
//!   bucket chains, avoiding allocation on the hot path.

#[deny(unsafe_code)]
pub mod bucket;
#[allow(clippy::module_inception)]
#[deny(unsafe_code)]
pub mod hash;
pub mod index; // contains unsafe: raw pointer slice for bucket serialization
#[deny(unsafe_code)]
pub mod overflow;
pub mod table; // contains unsafe: unchecked indexing for hot-path performance

// Re-export items from `hash/hash.rs` at this level so that
// `crate::hash::Hashable`, `crate::hash::KeyHash`, etc. keep working.
pub use self::hash::*;
