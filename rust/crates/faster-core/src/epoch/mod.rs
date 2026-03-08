//! Epoch-based safe memory reclamation.
//!
//! Implements FASTER's custom epoch framework for coordinating concurrent
//! access to shared data structures without locks. The epoch system provides:
//!
//! - **Lock-free coordination**: threads announce which epoch they're operating
//!   in via a shared table. Memory and resources are safe to reclaim only when
//!   no thread could reference them.
//! - **Drain-list semantics**: deferred callbacks are queued with an epoch tag
//!   and execute automatically when all threads have moved past that epoch.
//! - **RAII protection**: [`EpochGuard`] ensures epoch protection is released
//!   even if the caller panics or forgets to call unprotect.
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────┐
//! │              EpochTable                          │
//! │  current_epoch: AtomicU64  (monotonic counter)   │
//! │  safe_to_reclaim_epoch: AtomicU64                │
//! ├─────────────────────────────────────────────────┤
//! │  Entry 0: [local_epoch | reentrant | thread_id] │
//! │  Entry 1: [local_epoch | reentrant | thread_id] │
//! │  ...            (cache-line padded)              │
//! │  Entry N: [local_epoch | reentrant | thread_id] │
//! ├─────────────────────────────────────────────────┤
//! │  DrainList: [(epoch, callback), ...]             │
//! └─────────────────────────────────────────────────┘
//! ```
//!
//! # Why Custom, Not crossbeam-epoch?
//!
//! crossbeam-epoch solves a different problem (deferred deallocation of
//! individual objects). FASTER's epoch system is a *coordination mechanism*
//! for multi-phase state machines: drain callbacks, checkpoint phase transitions,
//! and safe-to-reclaim computation. Shoehorning FASTER's semantics into
//! crossbeam's API would be more complex than the ~400-line custom system.
//!
//! We do use [`crossbeam_utils::CachePadded`] for cache-line padding.
//!
//! # Usage
//!
//! ```
//! use std::sync::Arc;
//! use faster_core::epoch::EpochTable;
//!
//! let table = Arc::new(EpochTable::new());
//! let thread = table.register().expect("register thread");
//!
//! // Enter a protected region
//! {
//!     let _guard = thread.protect();
//!     // Safe to read shared data — nothing can be reclaimed
//! }
//! // Guard dropped → epoch protection released
//! ```

mod drain; // contains unsafe: raw pointer manipulation in drain callbacks
#[deny(unsafe_code)]
mod entry;
#[deny(unsafe_code)]
mod guard;
#[deny(unsafe_code)]
mod table;

pub use self::entry::EpochEntry;
pub use self::guard::{EpochGuard, EpochThread};
pub use self::table::EpochTable;

/// Type alias matching the C++ FASTER naming convention (`LightEpoch`).
pub type LightEpoch = EpochTable;

/// Maximum number of concurrently registered threads.
///
/// Matches C++ FASTER's `kTableSize`. The table is pre-allocated at this
/// size; registration beyond this limit returns `None`.
pub const MAX_THREADS: usize = 256;

/// Number of checkpoint phase slots per entry.
///
/// Accommodates the unified [`Phase`](crate::state::Phase) enum which spans
/// both checkpoint phases (1–7) and grow phases (8–15). Each entry has this
/// many [`AtomicBool`](std::sync::atomic::AtomicBool) markers that other
/// subsystems can use to track per-thread phase completion.
pub(crate) const PHASE_COUNT: usize = 16;

/// Sentinel value indicating an inactive epoch entry.
///
/// When a thread's `local_current_epoch` is 0, it is not in a protected
/// region and does not participate in safe-epoch computation.
pub(crate) const INACTIVE_EPOCH: u64 = 0;

/// The initial value of the global epoch counter.
///
/// Starts at 1 because 0 is reserved as the "inactive" sentinel in
/// per-thread entries.
pub(crate) const INITIAL_EPOCH: u64 = 1;

#[cfg(test)]
mod tests;
