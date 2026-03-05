//! Per-thread epoch state.
//!
//! Each registered thread owns one [`EpochEntry`] in the [`EpochTable`]'s
//! table. Entries are cache-line padded via [`CachePadded`] to prevent
//! false sharing between threads writing to adjacent slots.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use super::{INACTIVE_EPOCH, PHASE_COUNT};

/// Per-thread epoch tracking entry.
///
/// Tracks:
/// - `local_current_epoch`: the epoch this thread last observed (0 = inactive)
/// - `reentrant`: nesting count for reentrant protect/unprotect calls
/// - `thread_id`: unique identifier for the thread occupying this slot (0 = free)
/// - `phase_finished`: per-phase completion markers for checkpoint coordination
///
/// # Memory Layout
///
/// Uses `#[repr(C)]` for a predictable layout. Each entry is wrapped in
/// [`CachePadded`](crossbeam_utils::CachePadded) in the table (64-byte
/// alignment) to avoid false sharing.
#[repr(C)]
pub struct EpochEntry {
    /// The epoch this thread last observed. 0 = not in a protected region.
    ///
    /// Written with `Release` ordering when entering protection, read with
    /// `Acquire` ordering by `compute_safe_epoch`.
    pub(crate) local_current_epoch: AtomicU64,

    /// Reentrance counter for nested protect/unprotect calls.
    ///
    /// Incremented on protect, decremented on unprotect. Only when this
    /// transitions 0→1 is `local_current_epoch` updated; only when it
    /// transitions 1→0 is `local_current_epoch` cleared.
    pub(crate) reentrant: AtomicU32,

    /// Padding for 8-byte alignment of the next field within `#[repr(C)]`.
    _pad: u32,

    /// Unique identifier for the thread occupying this slot. 0 = free.
    pub(crate) thread_id: AtomicU64,

    /// Per-phase completion markers for checkpoint state machine.
    ///
    /// Reserved for future use by the checkpoint subsystem. Each marker
    /// indicates whether this thread has completed a particular checkpoint
    /// phase.
    pub(crate) phase_finished: [AtomicBool; PHASE_COUNT],
}

impl EpochEntry {
    /// Creates a new, inactive epoch entry with all fields zeroed.
    pub(crate) fn new() -> Self {
        Self {
            local_current_epoch: AtomicU64::new(INACTIVE_EPOCH),
            reentrant: AtomicU32::new(0),
            _pad: 0,
            thread_id: AtomicU64::new(0),
            phase_finished: core::array::from_fn(|_| AtomicBool::new(false)),
        }
    }

    /// Returns `true` if this entry is currently in a protected region.
    ///
    /// Uses `Relaxed` ordering — suitable for diagnostics, not for
    /// making reclamation decisions.
    #[inline]
    pub fn is_active(&self) -> bool {
        self.local_current_epoch.load(Ordering::Relaxed) != INACTIVE_EPOCH
    }

    /// Returns `true` if this slot is occupied by a registered thread.
    #[inline]
    pub fn is_occupied(&self) -> bool {
        self.thread_id.load(Ordering::Relaxed) != 0
    }

    /// Resets this entry to its initial (unoccupied, inactive) state.
    ///
    /// Called during deregistration. Uses `Release` on epoch and thread_id
    /// stores so that subsequent `Acquire` loads in `compute_safe_epoch`
    /// and registration see the cleared state.
    pub(crate) fn reset(&self) {
        self.local_current_epoch
            .store(INACTIVE_EPOCH, Ordering::Release);
        self.reentrant.store(0, Ordering::Relaxed);
        self.thread_id.store(0, Ordering::Release);
        for phase in &self.phase_finished {
            phase.store(false, Ordering::Relaxed);
        }
    }
}
