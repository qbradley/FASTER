//! Central epoch coordination structure.
//!
//! [`EpochTable`] is the backbone of FASTER's lock-free concurrent
//! coordination. It manages a global epoch counter, a table of per-thread
//! epoch entries, and a drain list of deferred callbacks.

use crate::sync::{Arc, AtomicU64, AtomicUsize, Mutex, Ordering};

use crossbeam_utils::CachePadded;

use super::drain::DrainList;
use super::entry::EpochEntry;
use super::guard::EpochThread;
use super::{INACTIVE_EPOCH, INITIAL_EPOCH, MAX_THREADS};

/// The central epoch coordination table.
///
/// Manages a global epoch counter, per-thread epoch entries (cache-line
/// padded), and a drain list of deferred callbacks.
///
/// # Thread Safety
///
/// `EpochTable` is `Send + Sync`. Core operations (`protect`, `unprotect`,
/// `bump_current_epoch`) use atomic instructions. Thread registration uses
/// a mutex (rare operation, not on the hot path).
///
/// # Epoch Lifecycle
///
/// 1. [`register()`](EpochTable::register) → claim a slot, get [`EpochThread`]
/// 2. [`EpochThread::protect()`] → enter protected region, get [`EpochGuard`]
/// 3. While protected, the thread is "active" at the current epoch
/// 4. Guard dropped → thread exits protected region
/// 5. [`bump_current_epoch()`](EpochTable::bump_current_epoch) → advance global epoch
/// 6. When all threads move past an epoch, drain callbacks for that epoch fire
///
/// # Examples
///
/// ```
/// use std::sync::Arc;
/// use faster_core::epoch::EpochTable;
///
/// let table = Arc::new(EpochTable::new());
/// assert_eq!(table.current_epoch(), 1);
/// assert_eq!(table.safe_epoch(), 0);
/// ```
pub struct EpochTable {
    /// Global monotonically increasing epoch counter.
    ///
    /// Starts at [`INITIAL_EPOCH`] (1). Advanced by `bump_current_epoch`
    /// using `SeqCst` for total ordering with thread-local stores.
    pub(crate) current_epoch: CachePadded<AtomicU64>,

    /// The newest epoch that is safe to reclaim.
    ///
    /// No active thread references this epoch or anything earlier.
    /// Updated by `try_drain` when the minimum active epoch advances.
    pub(crate) safe_to_reclaim_epoch: CachePadded<AtomicU64>,

    /// Per-thread epoch entries, cache-line padded to avoid false sharing.
    ///
    /// Each entry is 64-byte aligned via [`CachePadded`], ensuring that
    /// two threads writing to adjacent entries don't contend on the same
    /// cache line.
    pub(crate) table: Box<[CachePadded<EpochEntry>]>,

    /// Deferred callbacks keyed by epoch.
    pub(crate) drain_list: DrainList,

    /// High-water mark: one past the highest index ever registered.
    ///
    /// `compute_safe_epoch()` scans only `0..scan_limit` instead of all
    /// [`MAX_THREADS`] slots. With 16 threads registered, this reduces
    /// the scan from 256 entries to ~16.
    scan_limit: CachePadded<AtomicUsize>,

    /// Free slot indices for thread registration.
    ///
    /// Protected by a mutex because registration/deregistration is rare
    /// (once per thread lifetime, not on the hot path).
    free_list: Mutex<Vec<usize>>,
}

impl EpochTable {
    /// Creates a new epoch table with [`MAX_THREADS`] slots.
    ///
    /// All slots start as free. The global epoch starts at 1 and the
    /// safe-to-reclaim epoch starts at 0.
    ///
    /// # Examples
    ///
    /// ```
    /// use faster_core::epoch::EpochTable;
    ///
    /// let table = EpochTable::new();
    /// assert_eq!(table.current_epoch(), 1);
    /// assert_eq!(table.safe_epoch(), 0);
    /// assert_eq!(table.registered_count(), 0);
    /// ```
    pub fn new() -> Self {
        let mut entries = Vec::with_capacity(MAX_THREADS);
        for _ in 0..MAX_THREADS {
            entries.push(CachePadded::new(EpochEntry::new()));
        }

        // Reverse so that pop() returns index 0 first (predictable ordering
        // for tests, though not required for correctness).
        let free_list: Vec<usize> = (0..MAX_THREADS).rev().collect();

        Self {
            current_epoch: CachePadded::new(AtomicU64::new(INITIAL_EPOCH)),
            safe_to_reclaim_epoch: CachePadded::new(AtomicU64::new(0)),
            table: entries.into_boxed_slice(),
            drain_list: DrainList::new(),
            scan_limit: CachePadded::new(AtomicUsize::new(0)),
            free_list: Mutex::new(free_list),
        }
    }

    /// Registers a new thread and returns an [`EpochThread`] handle.
    ///
    /// Claims a slot in the epoch table. The slot is released when the
    /// returned `EpochThread` is dropped or
    /// [`EpochThread::unregister()`] is called.
    ///
    /// # Returns
    ///
    /// `None` if all [`MAX_THREADS`] slots are occupied.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Arc;
    /// use faster_core::epoch::EpochTable;
    ///
    /// let table = Arc::new(EpochTable::new());
    /// let thread = table.register().expect("should register");
    /// assert_eq!(table.registered_count(), 1);
    /// ```
    #[cfg(not(loom))]
    pub fn register(self: &Arc<Self>) -> Option<EpochThread> {
        Self::register_impl(self)
    }

    /// Claims a slot in the epoch table (loom-compatible associated function).
    ///
    /// Under loom, `&Arc<Self>` cannot be used as a `self` receiver because
    /// `loom::sync::Arc` doesn't implement `core::ops::Receiver`.
    #[cfg(loom)]
    pub fn register(this: &Arc<Self>) -> Option<EpochThread> {
        Self::register_impl(this)
    }

    fn register_impl(this: &Arc<Self>) -> Option<EpochThread> {
        let mut free_list = this.free_list.lock().expect("free list lock poisoned");
        let index = free_list.pop()?;

        let entry = &this.table[index];
        // Mark slot as occupied. We use (index + 1) as a simple non-zero
        // sentinel. A production implementation could use OS thread IDs,
        // but ThreadId doesn't expose a stable numeric value.
        // Release: visible to other threads checking is_occupied().
        entry.thread_id.store((index as u64) + 1, Ordering::Release);

        // Expand scan_limit so compute_safe_epoch covers this slot.
        this.scan_limit.fetch_max(index + 1, Ordering::Release);

        Some(EpochThread::new(Arc::clone(this), index))
    }

    /// Releases a thread's slot back to the free list.
    ///
    /// Called automatically by [`EpochThread::drop()`]. Resets the entry
    /// state and returns the index to the free list.
    pub(crate) fn deregister(&self, entry_index: usize) {
        let entry = &self.table[entry_index];
        entry.reset();

        let mut free_list = self.free_list.lock().expect("free list lock poisoned");
        free_list.push(entry_index);
    }

    /// Enters an epoch-protected region for the given thread.
    ///
    /// On first entry (reentrant count 0→1), stores the current global epoch
    /// to the thread's local slot.
    ///
    /// Reentrant: nested calls increment the counter without updating the
    /// epoch, preserving the snapshot epoch from the outermost protect.
    ///
    /// # Memory Ordering
    ///
    /// - `reentrant.fetch_add(1, Relaxed)`: only the owning thread modifies
    ///   this counter. No cross-thread synchronization needed.
    /// - `current_epoch.load(Relaxed)`: reading a stale (lower) epoch is
    ///   conservative — it makes `safe_epoch` lower, delaying drain actions.
    ///   We can never read a future epoch (atomics guarantee this).
    /// - `local_current_epoch.store(epoch, Release)`: ensures that when
    ///   `compute_safe_epoch` reads this with `Acquire`, it sees the epoch
    ///   value and all prior writes by this thread.
    #[inline]
    pub(crate) fn protect(&self, entry_index: usize) {
        let entry = &self.table[entry_index];

        // Relaxed: only the owning thread modifies reentrant.
        let prev = entry.reentrant.fetch_add(1, Ordering::Relaxed);

        if prev == 0 {
            // First entry: publish our epoch.
            let epoch = self.current_epoch.load(Ordering::Relaxed);
            // Release: pairs with Acquire in compute_safe_epoch.
            entry.local_current_epoch.store(epoch, Ordering::Release);
        }
        sim_yield!("epoch::after_protect");
    }

    /// Exits an epoch-protected region for the given thread.
    ///
    /// On last exit (reentrant count 1→0), clears the thread's local epoch
    /// and attempts to drain pending callbacks.
    ///
    /// # Memory Ordering
    ///
    /// - `reentrant.fetch_sub(1, Relaxed)`: same as protect — single-writer.
    /// - `local_current_epoch.store(0, Release)`: ensures all memory
    ///   operations within the protected region are visible before we signal
    ///   that we're no longer active.
    #[inline]
    pub(crate) fn unprotect(&self, entry_index: usize) {
        let entry = &self.table[entry_index];

        // Relaxed: only the owning thread modifies reentrant.
        let prev = entry.reentrant.fetch_sub(1, Ordering::Relaxed);
        debug_assert!(prev > 0, "unprotect called without matching protect");

        if prev == 1 {
            // Last exit: clear our epoch slot.
            // Release: makes writes within the protected region visible.
            entry
                .local_current_epoch
                .store(INACTIVE_EPOCH, Ordering::Release);

            // Opportunistically try to drain pending actions.
            self.try_drain();
        }
    }

    /// Advances the global epoch by one and queues a callback to execute
    /// when all threads have moved past the prior epoch.
    ///
    /// The callback fires when `safe_to_reclaim_epoch >= prior_epoch`,
    /// meaning no active thread is still at or before the prior epoch.
    ///
    /// # Memory Ordering
    ///
    /// Uses `SeqCst` for `fetch_add` because the epoch advance must be
    /// totally ordered with respect to other threads' `local_current_epoch`
    /// stores. Without `SeqCst`, a thread could read a stale
    /// `current_epoch` and store it to its local slot *after* this bump,
    /// creating a race where the safe epoch is computed too aggressively.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Arc;
    /// use std::sync::atomic::{AtomicBool, Ordering};
    /// use faster_core::epoch::EpochTable;
    ///
    /// let table = Arc::new(EpochTable::new());
    /// let fired = Arc::new(AtomicBool::new(false));
    /// let fired_clone = Arc::clone(&fired);
    ///
    /// table.bump_current_epoch(move || {
    ///     fired_clone.store(true, Ordering::Relaxed);
    /// });
    ///
    /// // No threads are active, so the callback fires immediately
    /// assert!(fired.load(Ordering::Relaxed));
    /// ```
    pub fn bump_current_epoch<F: FnOnce() + Send + 'static>(&self, callback: F) {
        trace_span!("epoch_bump");
        // SeqCst: total ordering required — see doc comment.
        let prior_epoch = self.current_epoch.fetch_add(1, Ordering::SeqCst);
        self.drain_list.push(prior_epoch, Box::new(callback));
        self.try_drain();
        sim_yield!("epoch::after_bump_current");
    }

    /// Advances the global epoch by one without queuing a callback.
    ///
    /// Useful when you need to advance the epoch for safe-epoch progress
    /// without any deferred work.
    pub fn bump_current_epoch_no_callback(&self) {
        // SeqCst: same reasoning as bump_current_epoch.
        self.current_epoch.fetch_add(1, Ordering::SeqCst);
        self.try_drain();
    }

    /// Queues a callback to execute when the current epoch becomes safe to
    /// reclaim, **without** advancing the global epoch.
    ///
    /// The callback fires when all currently-protected threads have exited
    /// their protected regions and the epoch has been bumped at least once
    /// (by another call to [`bump_current_epoch`](Self::bump_current_epoch)
    /// or the normal operation cycle).
    ///
    /// # When to Use
    ///
    /// Use this to defer work (e.g., free-list pushes) until it is safe,
    /// without paying the cost of an epoch bump per deferral. The epoch
    /// will be bumped by the normal FASTER operation cycle.
    pub fn defer<F: FnOnce() + Send + 'static>(&self, callback: F) {
        // SeqCst: must read the latest epoch to ensure the callback is not
        // tagged at an already-safe epoch (which would fire too early).
        let current = self.current_epoch.load(Ordering::SeqCst);
        self.drain_list.push(current, Box::new(callback));
    }

    /// Computes the safe-to-reclaim epoch and executes ready drain callbacks.
    ///
    /// Called automatically by `unprotect` and `bump_current_epoch`. Can
    /// also be called explicitly to force drain evaluation.
    ///
    /// # Fast-Path (drain_count == 0)
    ///
    /// When no deferred actions are pending (the common case in pure-read
    /// workloads), the entire scan is skipped. Cost drops from ~100ns
    /// (256-entry scan) to ~5ns (single atomic load). This matches the
    /// C# `drainCount == 0` fast-path in `ProtectAndDrain()`.
    pub(crate) fn try_drain(&self) {
        // Fast-path: no pending drain actions, skip the expensive scan.
        if !self.drain_list.has_pending() {
            return;
        }

        let safe = self.compute_safe_epoch();

        // Relaxed: comparing against a value we just computed locally.
        let old_safe = self.safe_to_reclaim_epoch.load(Ordering::Relaxed);

        if safe > old_safe {
            // Release: makes the new safe epoch visible to other threads.
            self.safe_to_reclaim_epoch.store(safe, Ordering::Release);
            self.drain_list.drain_up_to(safe);
        }
    }

    /// Computes the safe-to-reclaim epoch by scanning registered entries.
    ///
    /// Returns `min(active_epochs) - 1`, which is the newest epoch that no
    /// active thread could be referencing. If no threads are active, returns
    /// `current_epoch - 1`.
    ///
    /// # Scan Optimization
    ///
    /// Only scans `0..scan_limit` (the high-water mark of registered slots)
    /// instead of all [`MAX_THREADS`] entries. With 16 registered threads,
    /// this scans ~16 entries instead of 256.
    ///
    /// # Memory Ordering
    ///
    /// - Global epoch: `Acquire` to see all `Release` stores from threads'
    ///   `protect()` calls. `SeqCst` is unnecessary here \u2014 a stale (lower)
    ///   read of `current_epoch` is conservative (computes a lower safe
    ///   epoch, delaying drains).
    /// - Per-thread epochs: `Acquire` to see the latest `Release` store
    ///   from each thread's `protect()` call.
    fn compute_safe_epoch(&self) -> u64 {
        // Acquire: pairs with Release stores in protect() and
        // SeqCst fetch_add in bump_current_epoch.
        let current = self.current_epoch.load(Ordering::Acquire);
        let mut min = current;

        let limit = self.scan_limit.load(Ordering::Acquire);
        for entry in self.table[..limit].iter() {
            // Acquire: pairs with Release store in protect().
            let epoch = entry.local_current_epoch.load(Ordering::Acquire);

            if epoch != INACTIVE_EPOCH && epoch < min {
                min = epoch;
            }
        }
        sim_yield!("epoch::after_safe_scan");

        // Safe to reclaim everything strictly before the oldest active epoch.
        // If no threads are active, min == current, so safe = current - 1.
        // saturating_sub guards against underflow (shouldn't happen since we
        // start at INITIAL_EPOCH=1, but defense in depth).
        min.saturating_sub(1)
    }

    /// Returns the current global epoch value.
    ///
    /// Uses `Relaxed` ordering — suitable for monitoring and diagnostics,
    /// not for making reclamation decisions.
    #[inline]
    pub fn current_epoch(&self) -> u64 {
        self.current_epoch.load(Ordering::Relaxed)
    }

    /// Returns the current safe-to-reclaim epoch.
    ///
    /// Everything at or before this epoch can be safely reclaimed.
    #[inline]
    pub fn safe_epoch(&self) -> u64 {
        self.safe_to_reclaim_epoch.load(Ordering::Relaxed)
    }

    /// Returns the number of currently registered (occupied) thread slots.
    pub fn registered_count(&self) -> usize {
        let free_list = self.free_list.lock().expect("free list lock poisoned");
        MAX_THREADS - free_list.len()
    }

    /// Returns the number of threads currently in a protected region.
    pub fn active_count(&self) -> usize {
        self.table.iter().filter(|e| e.is_active()).count()
    }
}

impl Default for EpochTable {
    fn default() -> Self {
        Self::new()
    }
}
