//! RAII epoch protection guard and per-thread handle.
//!
//! [`EpochGuard`] ensures epoch protection is released even on panics.
//! [`EpochThread`] is the per-thread handle that creates guards and
//! manages the thread's lifecycle in the epoch table.

use std::marker::PhantomData;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use super::table::EpochTable;

/// RAII guard for epoch protection.
///
/// While this guard exists, the owning thread is in a protected epoch
/// region. No data referenced by this epoch can be reclaimed. On drop,
/// the thread exits the protected region and may trigger drain callbacks.
///
/// # Thread Affinity
///
/// `EpochGuard` is `!Send` and `!Sync` — it must be dropped on the same
/// thread that created it. This matches FASTER's session model where
/// operations are thread-affine.
///
/// # Reentrance
///
/// Multiple guards can be active simultaneously (nested). Only the
/// outermost guard sets/clears the epoch; inner guards just bump the
/// reentrance counter.
///
/// # Examples
///
/// ```
/// use std::sync::Arc;
/// use faster_core::epoch::EpochTable;
///
/// let table = Arc::new(EpochTable::new());
/// let thread = table.register().expect("register");
///
/// {
///     let guard = thread.protect();
///     assert!(guard.epoch() > 0);
/// }
/// // Guard dropped — no longer protected
/// ```
pub struct EpochGuard<'a> {
    table: &'a EpochTable,
    entry_index: usize,
    // !Send + !Sync: epoch guards are thread-affine.
    _not_send: PhantomData<*const ()>,
}

impl<'a> EpochGuard<'a> {
    /// Creates a new guard, entering the protected region.
    ///
    /// Called internally by [`EpochThread::protect()`].
    #[inline]
    pub(crate) fn new(table: &'a EpochTable, entry_index: usize) -> Self {
        table.protect(entry_index);
        Self {
            table,
            entry_index,
            _not_send: PhantomData,
        }
    }

    /// Returns the epoch this thread is protecting.
    ///
    /// This is the epoch stored in the thread's local slot when protection
    /// was first acquired (for the outermost guard in a reentrant stack).
    #[inline]
    pub fn epoch(&self) -> u64 {
        self.table.table[self.entry_index]
            .local_current_epoch
            .load(Ordering::Relaxed)
    }

    /// Refreshes this thread's local epoch to the current global epoch.
    ///
    /// Call this periodically during long-running operations to avoid
    /// stalling the safe-epoch computation. Without refresh, a thread
    /// holding a guard at an old epoch prevents drain callbacks from
    /// firing for newer epochs.
    ///
    /// # Memory Ordering
    ///
    /// - `Relaxed` load of current epoch (conservative: stale is safe).
    /// - `Release` store to local epoch (visible to `compute_safe_epoch`).
    #[inline]
    pub fn refresh(&self) {
        let epoch = self.table.current_epoch.load(Ordering::Relaxed);
        // Release: same reasoning as protect() — pairs with Acquire in
        // compute_safe_epoch.
        self.table.table[self.entry_index]
            .local_current_epoch
            .store(epoch, Ordering::Release);
    }
}

impl Drop for EpochGuard<'_> {
    #[inline]
    fn drop(&mut self) {
        self.table.unprotect(self.entry_index);
    }
}

/// Per-thread handle for epoch operations.
///
/// Created by [`EpochTable::register()`]. Holds an `Arc<EpochTable>` and
/// the index of this thread's entry in the epoch table.
///
/// On drop, the thread's slot is automatically released back to the free
/// list. Explicit cleanup is available via [`unregister()`](EpochThread::unregister).
///
/// # Examples
///
/// ```
/// use std::sync::Arc;
/// use faster_core::epoch::EpochTable;
///
/// let table = Arc::new(EpochTable::new());
/// let thread = table.register().expect("register");
///
/// // Protect/unprotect via RAII guard
/// let guard = thread.protect();
/// drop(guard);
///
/// // Slot released when `thread` is dropped
/// drop(thread);
/// assert_eq!(table.registered_count(), 0);
/// ```
pub struct EpochThread {
    table: Arc<EpochTable>,
    entry_index: usize,
    /// Tracks whether this thread has been explicitly unregistered,
    /// preventing double-deregistration in `Drop`.
    unregistered: bool,
}

impl EpochThread {
    /// Creates a new epoch thread handle (internal constructor).
    pub(crate) fn new(table: Arc<EpochTable>, entry_index: usize) -> Self {
        Self {
            table,
            entry_index,
            unregistered: false,
        }
    }

    /// Enters an epoch-protected region, returning an RAII guard.
    ///
    /// The guard ensures `unprotect` is called even if the caller panics.
    /// Reentrant: multiple guards can be active simultaneously (nested).
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Arc;
    /// use faster_core::epoch::EpochTable;
    ///
    /// let table = Arc::new(EpochTable::new());
    /// let thread = table.register().expect("register");
    ///
    /// let guard = thread.protect();
    /// // ... do work in protected region ...
    /// drop(guard);
    /// ```
    #[inline]
    pub fn protect(&self) -> EpochGuard<'_> {
        EpochGuard::new(&self.table, self.entry_index)
    }

    /// Updates this thread's local epoch to the current global epoch.
    ///
    /// Only has an effect if the thread is currently protected (an
    /// `EpochGuard` is active). If unprotected, this is a no-op.
    ///
    /// Prefer calling [`EpochGuard::refresh()`] directly when you have
    /// a guard reference.
    #[inline]
    pub fn refresh(&self) {
        let entry = &self.table.table[self.entry_index];
        // Only refresh if we're currently protected (reentrant > 0).
        // Relaxed: only this thread writes reentrant.
        if entry.reentrant.load(Ordering::Relaxed) > 0 {
            let epoch = self.table.current_epoch.load(Ordering::Relaxed);
            // Release: pairs with Acquire in compute_safe_epoch.
            entry.local_current_epoch.store(epoch, Ordering::Release);
        }
    }

    /// Explicitly releases this thread's epoch table slot.
    ///
    /// After calling this, the `EpochThread` is invalidated. Any
    /// subsequent call to `protect()` would use a deallocated slot.
    /// This is called automatically on drop.
    pub fn unregister(&mut self) {
        if !self.unregistered {
            self.table.deregister(self.entry_index);
            self.unregistered = true;
        }
    }

    /// Returns this thread's entry index in the epoch table.
    #[inline]
    pub fn entry_index(&self) -> usize {
        self.entry_index
    }

    /// Returns a reference to the underlying epoch table.
    #[inline]
    pub fn table(&self) -> &EpochTable {
        &self.table
    }
}

impl Drop for EpochThread {
    fn drop(&mut self) {
        self.unregister();
    }
}
