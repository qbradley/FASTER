//! Higher-level concurrent hash index composing [`HashTable`] + [`EpochTable`].
//!
//! [`HashIndex`] is the component that FASTER's store uses for concurrent
//! hash-based record lookup. It wraps the low-level latch-free [`HashTable`]
//! with epoch-based reclamation ([`EpochTable`]) and provides:
//!
//! - **Epoch-aware lookups and inserts** — callers hold an [`EpochGuard`] for
//!   the duration of an operation batch; individual operations delegate to the
//!   underlying hash table without acquiring guards.
//! - **GC integration** — [`invalidate_entries_in_range`](HashIndex::invalidate_entries_in_range)
//!   scans the index and zeros entries whose addresses fall within a reclaimed
//!   range, enabling hybrid-log page eviction.
//! - **Maintenance** — [`cleanup_tentative_entries_for_recovery`](HashIndex::cleanup_tentative_entries_for_recovery)
//!   removes stale tentative entries left by crashed or aborted inserts
//!   (recovery-only; not safe while insert sessions are active).
//!
//! # Epoch discipline
//!
//! `find`, `find_or_create`, and `update` assume the caller already holds an
//! [`EpochGuard`]. Epoch protection is acquired per-session (once per operation
//! batch), NOT per-operation — matching FASTER's session model. Creating a
//! guard per-operation would be needlessly expensive and break the reentrant
//! epoch semantics.
//!
//! # C++ correspondence
//!
//! | Rust                            | C++ (`mem_index.h` / `internal_hash_table.h`)       |
//! |---------------------------------|------------------------------------------------------|
//! | `HashIndex`                     | `MemHashIndex` + `InternalHashTable<D>`              |
//! | `find()`                        | `FindEntry()`                                        |
//! | `find_or_create()`              | `FindOrCreateEntry()`                                |
//! | `update()`                      | CAS on `AtomicHashBucketEntry`                       |
//! | `invalidate_entries_in_range()` | `InternalHashTable::InvalidateEntries()`             |
//! | `register_thread()`             | `LightEpoch::register_thread()`                      |
//! | `cleanup_tentative_entries_for_recovery()` | Recovery-phase tentative cleanup             |
//!
//! # Memory ordering
//!
//! All ordering choices are documented inline. Summary:
//!
//! | Operation                     | Ordering  | Rationale                                  |
//! |-------------------------------|-----------|--------------------------------------------|
//! | `find` / `find_or_create`     | (delegated) | See [`HashTable`] documentation           |
//! | `update` CAS                  | `AcqRel`  | Publish new entry, see prior writes        |
//! | `invalidate` CAS to EMPTY     | `AcqRel`  | Ensure zeroed entry visible to readers     |
//! | `cleanup_tentative` CAS       | `AcqRel`  | Same as invalidate                         |
//!
//! # Thread safety
//!
//! `HashIndex` is `Send + Sync`. All operations are lock-free on the data
//! path. Thread registration uses a mutex internally (rare, not hot path).

use std::sync::Arc;

use crate::address::LogicalAddress;
use crate::epoch::{EpochTable, EpochThread};
use crate::hash::KeyHash;
use crate::hash_bucket::{AtomicHashBucketEntry, BUCKET_NUM_ENTRIES, HashBucketEntry};
use crate::hash_table::{FindOrCreateResult, HashTable};

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

// ---------------------------------------------------------------------------
// HashIndex
// ---------------------------------------------------------------------------

/// A concurrent hash index backed by epoch-based reclamation.
///
/// Composes [`HashTable`] (the latch-free bucket array) with [`EpochTable`]
/// (epoch coordination and drain callbacks) into the user-facing hash index
/// that FASTER's store operates against.
///
/// # Construction
///
/// ```
/// use faster_core::hash_index::HashIndex;
///
/// // Create a hash index with 2^14 = 16384 buckets.
/// let index = HashIndex::new(14);
/// assert_eq!(index.num_buckets(), 16384);
/// ```
///
/// # Usage pattern
///
/// ```
/// use faster_core::hash_index::HashIndex;
/// use faster_core::hash::KeyHash;
/// use faster_core::address::LogicalAddress;
///
/// let index = HashIndex::new(10);
///
/// // 1. Register thread for epoch participation
/// let thread = index.register_thread().expect("register");
///
/// // 2. Acquire epoch guard for the operation batch
/// let _guard = thread.protect();
///
/// // 3. Perform operations (find, find_or_create, update)
/// let hash = KeyHash::new(0xDEAD_BEEF_1234_5678);
/// let result = index.find_or_create(hash, LogicalAddress::INVALID);
/// assert!(result.created);
/// ```
pub struct HashIndex {
    /// The latch-free concurrent hash table.
    table: HashTable,

    /// Epoch coordination table for safe memory reclamation.
    /// Wrapped in `Arc` because [`EpochTable::register`] requires `&Arc<Self>`.
    epoch: Arc<EpochTable>,

    /// Current table version (0 or 1). Toggled on each grow completion.
    version: AtomicU32,

    /// Approximate count of live (non-empty) entries. O(1) tracking.
    live_entry_count: AtomicU64,
}

impl HashIndex {
    /// Creates a new **standalone** hash index with `2^log2_size` buckets.
    ///
    /// Initializes both the hash table and a fresh [`EpochTable`]. All buckets
    /// start empty, the global epoch starts at 1.
    ///
    /// Use this constructor when the hash index owns its own epoch lifecycle
    /// (tests, benchmarks, standalone usage). When embedding the index inside
    /// a larger system such as `FasterKv`, prefer [`with_epoch`](Self::with_epoch)
    /// so that all subsystems share a single epoch table.
    ///
    /// # Panics
    ///
    /// Panics if `log2_size` is outside [`HashTable::MIN_LOG2_SIZE`]..=[`HashTable::MAX_LOG2_SIZE`].
    ///
    /// # Examples
    ///
    /// ```
    /// use faster_core::hash_index::HashIndex;
    ///
    /// let index = HashIndex::new(8); // 256 buckets
    /// assert_eq!(index.num_buckets(), 256);
    /// assert_eq!(index.log2_buckets(), 8);
    /// assert_eq!(index.entry_count(), 0);
    /// ```
    pub fn new(log2_size: u32) -> Self {
        Self::with_epoch(log2_size, Arc::new(EpochTable::new()))
    }

    /// Creates a hash index with `2^log2_size` buckets using an
    /// externally-provided [`EpochTable`].
    ///
    /// This constructor is intended for use inside composite structures like
    /// `FasterKv`, where a single [`EpochTable`] must be shared across the
    /// hash index, hybrid log, and other subsystems. The caller retains an
    /// `Arc` clone and passes one in here; all components then participate in
    /// the same epoch lifecycle.
    ///
    /// # Panics
    ///
    /// Panics if `log2_size` is outside [`HashTable::MIN_LOG2_SIZE`]..=[`HashTable::MAX_LOG2_SIZE`].
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Arc;
    /// use faster_core::epoch::EpochTable;
    /// use faster_core::hash_index::HashIndex;
    ///
    /// let shared_epoch = Arc::new(EpochTable::new());
    /// let index = HashIndex::with_epoch(8, Arc::clone(&shared_epoch));
    ///
    /// // The index uses the same epoch table, not a private copy.
    /// assert!(Arc::ptr_eq(&index.epoch_arc(), &shared_epoch));
    /// ```
    pub fn with_epoch(log2_size: u32, epoch: Arc<EpochTable>) -> Self {
        Self {
            table: HashTable::new(log2_size),
            epoch,
            version: AtomicU32::new(0),
            live_entry_count: AtomicU64::new(0),
        }
    }

    // -----------------------------------------------------------------------
    // Thread registration
    // -----------------------------------------------------------------------

    /// Registers the calling thread for epoch participation.
    ///
    /// Returns an [`EpochThread`] handle that can create [`EpochGuard`]s.
    /// The handle holds an `Arc` to the epoch table and releases its slot
    /// on drop.
    ///
    /// # Returns
    ///
    /// `None` if all [`MAX_THREADS`](crate::epoch::MAX_THREADS) slots are occupied.
    ///
    /// # Examples
    ///
    /// ```
    /// use faster_core::hash_index::HashIndex;
    ///
    /// let index = HashIndex::new(8);
    /// let thread = index.register_thread().expect("should register");
    /// let _guard = thread.protect();
    /// ```
    pub fn register_thread(&self) -> Option<EpochThread> {
        self.epoch.register()
    }

    // -----------------------------------------------------------------------
    // Core operations (caller must hold EpochGuard)
    // -----------------------------------------------------------------------

    /// Looks up an existing committed entry by key hash.
    ///
    /// Scans the primary bucket and overflow chain for a committed (non-tentative)
    /// entry whose tag matches `hash.tag()`. Returns `None` if not found.
    ///
    /// # Epoch requirement
    ///
    /// The caller **must** hold an [`EpochGuard`] for the duration of this call
    /// and any subsequent use of the returned slot reference. The guard is NOT
    /// acquired internally — FASTER acquires epoch protection per operation batch,
    /// not per individual lookup.
    ///
    /// # Examples
    ///
    /// ```
    /// use faster_core::hash_index::HashIndex;
    /// use faster_core::hash::KeyHash;
    ///
    /// let index = HashIndex::new(10);
    /// let thread = index.register_thread().unwrap();
    /// let _guard = thread.protect();
    ///
    /// let hash = KeyHash::new(0xABCD_0000_0000_1234);
    /// assert!(index.find(hash).is_none()); // empty table
    /// ```
    #[inline]
    pub fn find(&self, hash: KeyHash) -> Option<(HashBucketEntry, &AtomicHashBucketEntry)> {
        self.table.find_entry(hash)
    }

    /// Issues a software prefetch hint for the hash bucket that `hash` maps to.
    ///
    /// Call this between computing the key hash and calling [`find`](Self::find)
    /// or [`find_or_create`](Self::find_or_create) so the CPU can begin pulling
    /// the bucket cache line into L1 while other work (e.g. record layout
    /// computation) proceeds.
    ///
    /// The hint is purely advisory and never affects correctness.
    ///
    /// # Examples
    ///
    /// ```
    /// use faster_core::hash_index::HashIndex;
    /// use faster_core::hash::KeyHash;
    ///
    /// let index = HashIndex::new(10);
    /// let hash = KeyHash::new(0xCAFE);
    /// index.prefetch(hash); // non-blocking hint
    /// // … do other work (compute layout, etc.) …
    /// let _result = index.find(hash);
    /// ```
    #[inline(always)]
    pub fn prefetch(&self, hash: KeyHash) {
        self.table.prefetch_bucket(hash);
    }

    /// Finds an existing entry or creates a new tentative entry.
    ///
    /// Delegates to [`HashTable::find_or_create_entry`] — the two-phase tentative
    /// CAS protocol. If `created` is true in the result, the caller must:
    /// 1. Write the record to the hybrid log.
    /// 2. Call [`update`](Self::update) to clear the tentative bit.
    ///
    /// # Epoch requirement
    ///
    /// The caller **must** hold an [`EpochGuard`]. See [`find`](Self::find) for details.
    ///
    /// # Examples
    ///
    /// ```
    /// use faster_core::hash_index::HashIndex;
    /// use faster_core::hash::KeyHash;
    /// use faster_core::address::LogicalAddress;
    ///
    /// let index = HashIndex::new(10);
    /// let thread = index.register_thread().unwrap();
    /// let _guard = thread.protect();
    ///
    /// let hash = KeyHash::new(0x1234_0000_0000_ABCD);
    /// let result = index.find_or_create(hash, LogicalAddress::INVALID);
    /// assert!(result.created);
    /// assert!(result.entry.is_tentative());
    /// ```
    #[inline]
    pub fn find_or_create(
        &self,
        hash: KeyHash,
        initial_address: LogicalAddress,
    ) -> FindOrCreateResult<'_> {
        trace_span!("hash_lookup");
        let result = self.table.find_or_create_entry(hash, initial_address);
        if result.created {
            self.live_entry_count.fetch_add(1, Ordering::Relaxed);
        }
        result
    }

    /// Atomically updates a hash index entry via CAS.
    ///
    /// Common use cases:
    /// - **Commit:** CAS from tentative to committed (clear tentative bit).
    /// - **Update address:** Point an existing entry to a new log record.
    /// - **Invalidate:** CAS a tentative entry back to `EMPTY` on abort.
    ///
    /// Returns `true` if the CAS succeeded.
    ///
    /// # Epoch requirement
    ///
    /// The caller **must** hold an [`EpochGuard`]. See [`find`](Self::find) for details.
    ///
    /// # Memory ordering
    ///
    /// Delegates to [`HashTable::update_entry`] which uses `AcqRel` / `Acquire`.
    ///
    /// # Examples
    ///
    /// ```
    /// use faster_core::hash_index::HashIndex;
    /// use faster_core::hash::KeyHash;
    /// use faster_core::hash_bucket::HashBucketEntry;
    /// use faster_core::address::{LogicalAddress, Page, Offset};
    ///
    /// let index = HashIndex::new(10);
    /// let thread = index.register_thread().unwrap();
    /// let _guard = thread.protect();
    ///
    /// let hash = KeyHash::new(0x1234_0000_0000_ABCD);
    /// let result = index.find_or_create(hash, LogicalAddress::INVALID);
    /// assert!(result.created);
    ///
    /// // Commit the tentative entry with a real address.
    /// let new_addr = LogicalAddress::new(Page(1), Offset(256));
    /// let committed = HashBucketEntry::new(result.entry.tag(), new_addr, false);
    /// assert!(index.update(result.slot, result.entry, committed));
    /// ```
    #[inline]
    pub fn update(
        &self,
        slot: &AtomicHashBucketEntry,
        old: HashBucketEntry,
        new: HashBucketEntry,
    ) -> bool {
        trace_span!("hash_update");
        let success = self.table.update_entry(slot, old, new);
        if success && !old.is_empty() && new.is_empty() {
            self.live_entry_count.fetch_sub(1, Ordering::Relaxed);
        }
        success
    }

    // -----------------------------------------------------------------------
    // GC: entry invalidation for page eviction
    // -----------------------------------------------------------------------

    /// Scans all buckets and invalidates entries whose address falls in `[begin, end)`.
    ///
    /// For each non-empty entry with `begin <= entry.address() < end`, attempts
    /// a CAS to [`HashBucketEntry::EMPTY`]. CAS failures are expected and harmless
    /// — they indicate a concurrent modification (insert or another invalidation).
    ///
    /// Returns the count of successfully invalidated entries.
    ///
    /// # Epoch requirement
    ///
    /// This method does **NOT** require epoch protection. It is designed to be
    /// called from epoch drain callbacks when the hybrid log evicts pages. The
    /// method only touches hash index entries (CAS to EMPTY) and does not
    /// dereference any log addresses.
    ///
    /// # Concurrency
    ///
    /// Safe to call concurrently with `find`, `find_or_create`, and `update`.
    /// The CAS ensures atomicity: readers either see the old entry or EMPTY,
    /// never a torn value.
    ///
    /// # Memory ordering
    ///
    /// - Entries are loaded with `Acquire` to see the latest committed value.
    /// - CAS uses `AcqRel` / `Relaxed`: `Release` on success ensures the
    ///   zeroed entry is visible to subsequent readers; `Relaxed` on failure
    ///   because we simply skip failed CAS attempts.
    ///
    /// # Performance
    ///
    /// O(total_entries) — walks every primary bucket and overflow chain.
    /// Intended for batch page eviction, not the per-operation hot path.
    ///
    /// # Examples
    ///
    /// ```
    /// use faster_core::hash_index::HashIndex;
    /// use faster_core::hash::KeyHash;
    /// use faster_core::hash_bucket::HashBucketEntry;
    /// use faster_core::address::{LogicalAddress, Page, Offset};
    ///
    /// let index = HashIndex::new(8);
    /// let thread = index.register_thread().unwrap();
    ///
    /// // Insert and commit an entry at address Page(1), Offset(100).
    /// let hash = KeyHash::new(0x1234_0000_0000_ABCD);
    /// {
    ///     let _guard = thread.protect();
    ///     let r = index.find_or_create(hash, LogicalAddress::INVALID);
    ///     let addr = LogicalAddress::new(Page(1), Offset(100));
    ///     let committed = HashBucketEntry::new(r.entry.tag(), addr, false);
    ///     index.update(r.slot, r.entry, committed);
    /// }
    ///
    /// // Invalidate all entries in page range [Page(0)..Page(2)).
    /// let begin = LogicalAddress::new(Page(0), Offset(0));
    /// let end = LogicalAddress::new(Page(2), Offset(0));
    /// let count = index.invalidate_entries_in_range(begin, end);
    /// assert_eq!(count, 1);
    /// ```
    pub fn invalidate_entries_in_range(&self, begin: LogicalAddress, end: LogicalAddress) -> u64 {
        let begin_raw = begin.raw();
        let end_raw = end.raw();
        let mut invalidated = 0u64;
        let pool = self.table.overflow_pool();

        for bucket_idx in 0..self.table.num_buckets() {
            let bucket = &self.table_buckets()[bucket_idx as usize];
            let mut current = bucket;

            loop {
                for i in 0..BUCKET_NUM_ENTRIES {
                    // Acquire: see the latest committed value.
                    let entry = current.entry(i).load(Ordering::Acquire);
                    if entry.is_empty() {
                        continue;
                    }

                    let addr_raw = entry.address().raw();
                    if addr_raw >= begin_raw && addr_raw < end_raw {
                        // CAS to EMPTY. AcqRel on success: publish the zeroed
                        // entry to concurrent readers. Relaxed on failure:
                        // another thread modified this entry concurrently —
                        // that's fine, skip it.
                        if current
                            .entry(i)
                            .compare_exchange(
                                entry,
                                HashBucketEntry::EMPTY,
                                Ordering::AcqRel,
                                Ordering::Relaxed,
                            )
                            .is_ok()
                        {
                            invalidated += 1;
                        }
                    }
                }

                // Follow overflow chain.
                let overflow_addr = current.overflow_address().load(Ordering::Acquire);
                if overflow_addr == LogicalAddress::ZERO {
                    break;
                }
                current = pool.get(overflow_addr);
            }
        }

        if invalidated > 0 {
            self.live_entry_count
                .fetch_sub(invalidated, Ordering::Relaxed);
        }
        invalidated
    }

    // -----------------------------------------------------------------------
    // Maintenance: tentative entry cleanup
    // -----------------------------------------------------------------------

    /// Scans all buckets and removes stale tentative entries left by crashed
    /// or aborted insert operations during recovery.
    ///
    /// Any entry with the tentative bit set is CAS'd to [`HashBucketEntry::EMPTY`].
    /// Returns the count of successfully cleaned entries.
    ///
    /// # Safety — recovery-only precondition
    ///
    /// **This method MUST only be called during recovery, when no insert
    /// sessions are active.** It is NOT safe to call concurrently with
    /// active [`find_or_create`](Self::find_or_create) operations.
    ///
    /// The two-phase insert protocol (see [`HashTable::find_or_create_entry`])
    /// deliberately leaves entries in the tentative state between phase 1
    /// (reservation) and phase 2 (commit). An entry being tentative does NOT
    /// mean it is abandoned — it may belong to an in-flight insert that has
    /// not yet called [`update`](Self::update) to clear the tentative bit.
    ///
    /// If this method runs concurrently with active inserters, the CAS can
    /// race with an in-flight insert and delete a live tentative entry,
    /// causing **silent data loss**: the inserter's subsequent commit CAS
    /// will fail (the slot is now EMPTY), and the record will be lost from
    /// the index with no error reported.
    ///
    /// The CAS-failure guard (Relaxed on failure) only protects against
    /// entries that were *committed* between load and CAS — it cannot
    /// distinguish a still-tentative in-flight entry from a genuinely
    /// abandoned one.
    ///
    /// # When to call
    ///
    /// - During recovery after a crash — no sessions are active, so all
    ///   remaining tentative entries are genuinely orphaned.
    /// - After draining all sessions and ensuring no in-flight operations
    ///   remain (e.g., during controlled shutdown).
    ///
    /// Do **not** use as periodic background maintenance while the store is
    /// serving operations — there is no way to distinguish live in-flight
    /// tentative entries from abandoned ones without session-level tracking.
    ///
    /// # Performance
    ///
    /// O(total_entries) full-table scan. Not on the hot path.
    ///
    /// # Memory ordering
    ///
    /// - Load with `Acquire`: see the latest value including tentative bit.
    /// - CAS with `AcqRel` / `Relaxed`: same rationale as `invalidate_entries_in_range`.
    ///
    /// # Examples
    ///
    /// ```
    /// use faster_core::hash_index::HashIndex;
    /// use faster_core::hash::KeyHash;
    /// use faster_core::address::LogicalAddress;
    ///
    /// let index = HashIndex::new(8);
    /// let thread = index.register_thread().unwrap();
    /// let _guard = thread.protect();
    ///
    /// // Create a tentative entry (simulating a partial insert).
    /// let hash = KeyHash::new(0xAAAA_0000_0000_BBBB);
    /// let r = index.find_or_create(hash, LogicalAddress::INVALID);
    /// assert!(r.created);
    /// assert!(r.entry.is_tentative());
    ///
    /// // In recovery (no active sessions), clean up tentative entries.
    /// let count = index.cleanup_tentative_entries_for_recovery();
    /// assert_eq!(count, 1);
    ///
    /// // Entry is now gone.
    /// assert!(index.find(hash).is_none());
    /// ```
    pub fn cleanup_tentative_entries_for_recovery(&self) -> u64 {
        let mut cleaned = 0u64;
        let pool = self.table.overflow_pool();

        for bucket_idx in 0..self.table.num_buckets() {
            let bucket = &self.table_buckets()[bucket_idx as usize];
            let mut current = bucket;

            loop {
                for i in 0..BUCKET_NUM_ENTRIES {
                    // Acquire: see the latest value including tentative bit.
                    let entry = current.entry(i).load(Ordering::Acquire);
                    if !entry.is_empty() && entry.is_tentative() {
                        // CAS to EMPTY. AcqRel on success to publish the
                        // removal. Relaxed on failure — the entry was
                        // concurrently modified (likely committed), which
                        // is fine.
                        if current
                            .entry(i)
                            .compare_exchange(
                                entry,
                                HashBucketEntry::EMPTY,
                                Ordering::AcqRel,
                                Ordering::Relaxed,
                            )
                            .is_ok()
                        {
                            cleaned += 1;
                        }
                    }
                }

                let overflow_addr = current.overflow_address().load(Ordering::Acquire);
                if overflow_addr == LogicalAddress::ZERO {
                    break;
                }
                current = pool.get(overflow_addr);
            }
        }

        if cleaned > 0 {
            self.live_entry_count.fetch_sub(cleaned, Ordering::Relaxed);
        }
        cleaned
    }

    // -----------------------------------------------------------------------
    // Accessors
    // -----------------------------------------------------------------------

    /// Returns a reference to the underlying [`HashTable`].
    ///
    /// Use this for advanced operations (direct bucket access, statistics)
    /// that are not covered by the `HashIndex` API.
    #[inline]
    pub fn table(&self) -> &HashTable {
        &self.table
    }

    /// Returns a reference to the underlying [`EpochTable`].
    ///
    /// Use this for advanced epoch management (bumping epochs, drain callbacks,
    /// monitoring epoch progress).
    #[inline]
    pub fn epoch(&self) -> &EpochTable {
        &self.epoch
    }

    /// Returns a clone of the `Arc<EpochTable>` for shared ownership.
    ///
    /// Useful when components need to independently hold a reference to the
    /// epoch table (e.g., for registering threads from other subsystems).
    #[inline]
    pub fn epoch_arc(&self) -> Arc<EpochTable> {
        Arc::clone(&self.epoch)
    }

    /// Replaces the underlying hash table with `new_table`.
    ///
    /// Used by the grow manager after all buckets have been split into
    /// the new (doubled) table. The old table is dropped.
    ///
    /// # Concurrency
    ///
    /// This must only be called when no concurrent operations are accessing
    /// the old table (i.e., after all sessions have acknowledged the grow
    /// completion).
    pub fn swap_table(&mut self, new_table: HashTable) {
        self.table = new_table;
    }

    /// Returns the number of primary buckets (2^log2_size).
    #[inline]
    pub fn num_buckets(&self) -> u64 {
        self.table.num_buckets()
    }

    /// Returns the log2 of the number of primary buckets.
    #[inline]
    pub fn log2_buckets(&self) -> u32 {
        self.table.log2_buckets()
    }

    /// Returns the current table version (0 or 1).
    ///
    /// Toggled on each completed grow operation.
    #[inline]
    pub fn version(&self) -> u32 {
        self.version.load(Ordering::Acquire)
    }

    /// Sets the table version. Used by the grow state machine.
    #[inline]
    pub fn set_version(&self, v: u32) {
        self.version.store(v, Ordering::Release);
    }

    /// Returns the approximate live entry count (O(1)).
    ///
    /// Maintained incrementally via atomic counter.
    /// For an exact count, use [`scan_entry_count`](Self::scan_entry_count).
    #[inline]
    pub fn entry_count(&self) -> u64 {
        self.live_entry_count.load(Ordering::Relaxed)
    }

    /// Returns an exact count of non-empty entries via full-table scan.
    ///
    /// **WARNING:** O(n) scan. Use only for diagnostics and testing.
    #[inline]
    pub fn scan_entry_count(&self) -> u64 {
        self.table.entry_count()
    }

    /// Returns the approximate load factor: `entry_count / (num_buckets * 7)`.
    ///
    /// Each bucket holds up to 7 entries (before overflow), so the
    /// denominator is the total slot capacity.
    #[inline]
    pub fn load_factor(&self) -> f64 {
        let entries = self.entry_count() as f64;
        let slots = (self.num_buckets() * BUCKET_NUM_ENTRIES as u64) as f64;
        if slots == 0.0 {
            return 0.0;
        }
        entries / slots
    }

    /// Returns the number of overflow buckets allocated.
    #[inline]
    pub fn overflow_count(&self) -> u64 {
        self.table.overflow_count()
    }

    // -----------------------------------------------------------------------
    // Private helpers
    // -----------------------------------------------------------------------

    /// Returns a reference to the raw bucket slice for direct iteration.
    ///
    /// This accesses the internal bucket array via the public `HashTable`
    /// API by using the `bucket()` method with synthesized hashes, but for
    /// full-table scans we need direct slice access. We reconstruct the
    /// bucket reference via pointer arithmetic from bucket(0).
    ///
    /// # Safety argument
    ///
    /// `HashTable::bucket(hash)` with `hash.index(num_buckets)` == `idx`
    /// returns `&buckets[idx]`. We know the buckets are a contiguous
    /// `Box<[HashBucket]>` of length `num_buckets`. Getting bucket 0 and
    /// constructing a slice from it is sound because:
    /// - All buckets are contiguous in memory.
    /// - We only read within `num_buckets` bounds.
    /// - `HashBucket` has `#[repr(C, align(64))]` so stride is correct.
    fn table_buckets(&self) -> &[crate::hash_bucket::HashBucket] {
        let first = self.table.bucket(KeyHash::new(0));
        let len = self.table.num_buckets() as usize;
        // SAFETY: `first` points to the start of a contiguous `Box<[HashBucket]>`
        // of length `num_buckets`. The slice we construct is within bounds.
        // `HashBucket` is `#[repr(C, align(64))]` with size == align == 64,
        // so `std::slice::from_raw_parts` with stride 64 is correct.
        // The lifetime is tied to `&self` which borrows the HashTable.
        unsafe { std::slice::from_raw_parts(first, len) }
    }

    /// Returns a shared slice of all primary hash buckets for checkpoint
    /// serialization.
    ///
    /// The slice covers `num_buckets()` contiguous [`HashBucket`] values
    /// starting at bucket index 0. Overflow buckets are **not** included.
    ///
    /// # Safety contract
    ///
    /// The returned slice borrows `&self`, so the index cannot be mutated
    /// while the slice is live.
    pub fn checkpoint_bucket_slice(&self) -> &[crate::hash_bucket::HashBucket] {
        self.table_buckets()
    }
}

impl core::fmt::Debug for HashIndex {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("HashIndex")
            .field("num_buckets", &self.num_buckets())
            .field("log2_buckets", &self.log2_buckets())
            .field("version", &self.version())
            .field("entry_count", &self.entry_count())
            .field("load_factor", &self.load_factor())
            .field("overflow_count", &self.overflow_count())
            .field("epoch_current", &self.epoch.current_epoch())
            .field("epoch_safe", &self.epoch.safe_epoch())
            .finish()
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::address::{LogicalAddress, Offset, Page};
    use crate::hash::KeyHash;
    use crate::hash_bucket::HashBucketEntry;

    /// Helper: create a hash index with a small table for testing.
    fn test_index() -> HashIndex {
        HashIndex::new(8) // 256 buckets
    }

    /// Helper: create a distinct KeyHash from a seed.
    fn make_hash(seed: u64) -> KeyHash {
        // Shift seed into the tag bits (48..61) and lower bits for index.
        // This ensures different seeds produce different tags AND different buckets.
        KeyHash::new(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15))
    }

    /// Helper: create a valid LogicalAddress from simple components.
    fn make_addr(page: u32, offset: u32) -> LogicalAddress {
        LogicalAddress::new(Page(page), Offset(offset))
    }

    // -----------------------------------------------------------------------
    // Construction tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_new_creates_empty_index() {
        let index = test_index();
        assert_eq!(index.num_buckets(), 256);
        assert_eq!(index.log2_buckets(), 8);
        assert_eq!(index.entry_count(), 0);
        assert_eq!(index.overflow_count(), 0);
    }

    #[test]
    fn test_new_various_sizes() {
        // Cap at 2^14 to keep test under 1s; 2^16 = 4MB allocation.
        for log2 in [1, 4, 8, 12, 14] {
            let index = HashIndex::new(log2);
            assert_eq!(index.num_buckets(), 1u64 << log2);
            assert_eq!(index.log2_buckets(), log2);
        }
    }

    #[test]
    fn test_with_epoch_shares_epoch_table() {
        let shared_epoch = Arc::new(EpochTable::new());
        let index = HashIndex::with_epoch(8, Arc::clone(&shared_epoch));

        // The index must use the exact same allocation, not a copy.
        assert!(Arc::ptr_eq(&index.epoch_arc(), &shared_epoch));

        // Operations through the index are visible via the shared handle.
        let thread = index.register_thread().expect("should register");
        assert_eq!(shared_epoch.registered_count(), 1);
        drop(thread);
        assert_eq!(shared_epoch.registered_count(), 0);
    }

    #[test]
    #[should_panic]
    fn test_new_too_small() {
        let _ = HashIndex::new(0);
    }

    #[test]
    #[should_panic]
    fn test_new_too_large() {
        let _ = HashIndex::new(31);
    }

    // -----------------------------------------------------------------------
    // Thread registration
    // -----------------------------------------------------------------------

    #[test]
    fn test_register_thread() {
        let index = test_index();
        let thread = index.register_thread().expect("should register");
        assert_eq!(index.epoch().registered_count(), 1);
        drop(thread);
        assert_eq!(index.epoch().registered_count(), 0);
    }

    // -----------------------------------------------------------------------
    // Find on empty table
    // -----------------------------------------------------------------------

    #[test]
    fn test_find_empty_table() {
        let index = test_index();
        let thread = index.register_thread().unwrap();
        let _guard = thread.protect();

        assert!(index.find(make_hash(1)).is_none());
        assert!(index.find(make_hash(42)).is_none());
        assert!(index.find(make_hash(u64::MAX)).is_none());
    }

    // -----------------------------------------------------------------------
    // Find-or-create round-trip
    // -----------------------------------------------------------------------

    #[test]
    fn test_find_or_create_new_entry() {
        let index = test_index();
        let thread = index.register_thread().unwrap();
        let _guard = thread.protect();

        let hash = make_hash(100);
        let result = index.find_or_create(hash, LogicalAddress::INVALID);

        assert!(result.created);
        assert!(result.entry.is_tentative());
        assert_eq!(result.entry.tag(), hash.tag());
        assert_eq!(index.entry_count(), 1);
    }

    #[test]
    fn test_find_or_create_returns_existing() {
        let index = test_index();
        let thread = index.register_thread().unwrap();
        let _guard = thread.protect();

        let hash = make_hash(200);
        let addr = make_addr(5, 1024);

        // Create and commit.
        let r1 = index.find_or_create(hash, LogicalAddress::INVALID);
        assert!(r1.created);
        let committed = HashBucketEntry::new(r1.entry.tag(), addr, false);
        assert!(index.update(r1.slot, r1.entry, committed));

        // Second find_or_create should return the existing committed entry.
        let r2 = index.find_or_create(hash, LogicalAddress::INVALID);
        assert!(!r2.created);
        assert!(!r2.entry.is_tentative());
        assert_eq!(r2.entry.address(), addr);
    }

    // -----------------------------------------------------------------------
    // Find after commit
    // -----------------------------------------------------------------------

    #[test]
    fn test_find_after_commit() {
        let index = test_index();
        let thread = index.register_thread().unwrap();
        let _guard = thread.protect();

        let hash = make_hash(300);
        let addr = make_addr(10, 2048);

        // Tentative entries are NOT visible to find.
        let r = index.find_or_create(hash, LogicalAddress::INVALID);
        assert!(index.find(hash).is_none());

        // Commit the entry.
        let committed = HashBucketEntry::new(r.entry.tag(), addr, false);
        assert!(index.update(r.slot, r.entry, committed));

        // Now find should return it.
        let (found, _slot) = index.find(hash).expect("should find committed entry");
        assert_eq!(found.address(), addr);
        assert_eq!(found.tag(), hash.tag());
        assert!(!found.is_tentative());
    }

    // -----------------------------------------------------------------------
    // Update entry
    // -----------------------------------------------------------------------

    #[test]
    fn test_update_address() {
        let index = test_index();
        let thread = index.register_thread().unwrap();
        let _guard = thread.protect();

        let hash = make_hash(400);
        let addr1 = make_addr(1, 100);
        let addr2 = make_addr(2, 200);

        // Create and commit with addr1.
        let r = index.find_or_create(hash, LogicalAddress::INVALID);
        let committed1 = HashBucketEntry::new(r.entry.tag(), addr1, false);
        assert!(index.update(r.slot, r.entry, committed1));

        // Update to addr2.
        let committed2 = HashBucketEntry::new(r.entry.tag(), addr2, false);
        assert!(index.update(r.slot, committed1, committed2));

        // Verify the update.
        let (found, _) = index.find(hash).unwrap();
        assert_eq!(found.address(), addr2);
    }

    #[test]
    fn test_update_fails_on_stale() {
        let index = test_index();
        let thread = index.register_thread().unwrap();
        let _guard = thread.protect();

        let hash = make_hash(500);
        let addr = make_addr(3, 300);

        // Create and commit.
        let r = index.find_or_create(hash, LogicalAddress::INVALID);
        let committed = HashBucketEntry::new(r.entry.tag(), addr, false);
        assert!(index.update(r.slot, r.entry, committed));

        // Try to update with stale expected value (the tentative entry).
        let stale = r.entry; // still the tentative version
        let new_entry = HashBucketEntry::new(r.entry.tag(), make_addr(99, 99), false);
        assert!(!index.update(r.slot, stale, new_entry));
    }

    // -----------------------------------------------------------------------
    // Multiple entries
    // -----------------------------------------------------------------------

    #[test]
    fn test_multiple_entries() {
        let index = test_index();
        let thread = index.register_thread().unwrap();
        let _guard = thread.protect();

        let n = 50u64;
        let mut hashes = Vec::with_capacity(n as usize);

        for i in 0..n {
            let hash = make_hash(1000 + i);
            let addr = make_addr(i as u32, (i * 64) as u32);

            let r = index.find_or_create(hash, LogicalAddress::INVALID);
            assert!(r.created);
            let committed = HashBucketEntry::new(r.entry.tag(), addr, false);
            assert!(index.update(r.slot, r.entry, committed));
            hashes.push((hash, addr));
        }

        assert_eq!(index.entry_count(), n);

        // Verify all entries are findable.
        for (hash, addr) in &hashes {
            let (found, _) = index.find(*hash).expect("should find entry");
            assert_eq!(found.address(), *addr);
        }
    }

    // -----------------------------------------------------------------------
    // GC: invalidate_entries_in_range
    // -----------------------------------------------------------------------

    #[test]
    fn test_invalidate_empty_table() {
        let index = test_index();
        let count = index.invalidate_entries_in_range(LogicalAddress::ZERO, LogicalAddress::MAX);
        assert_eq!(count, 0);
    }

    #[test]
    fn test_invalidate_all_entries_in_range() {
        let index = test_index();
        let thread = index.register_thread().unwrap();

        // Insert entries at page 1 and page 2.
        {
            let _guard = thread.protect();
            for i in 0..5u64 {
                let hash = make_hash(2000 + i);
                let addr = make_addr(1, (i * 100) as u32);
                let r = index.find_or_create(hash, LogicalAddress::INVALID);
                let committed = HashBucketEntry::new(r.entry.tag(), addr, false);
                index.update(r.slot, r.entry, committed);
            }
            for i in 0..3u64 {
                let hash = make_hash(3000 + i);
                let addr = make_addr(2, (i * 100) as u32);
                let r = index.find_or_create(hash, LogicalAddress::INVALID);
                let committed = HashBucketEntry::new(r.entry.tag(), addr, false);
                index.update(r.slot, r.entry, committed);
            }
        }

        assert_eq!(index.entry_count(), 8);

        // Invalidate only page 1.
        let begin = make_addr(1, 0);
        let end = make_addr(2, 0);
        let invalidated = index.invalidate_entries_in_range(begin, end);
        assert_eq!(invalidated, 5);
        assert_eq!(index.entry_count(), 3);

        // Page 2 entries should still be there.
        let _guard = thread.protect();
        for i in 0..3u64 {
            let hash = make_hash(3000 + i);
            assert!(index.find(hash).is_some());
        }
    }

    #[test]
    fn test_invalidate_no_match() {
        let index = test_index();
        let thread = index.register_thread().unwrap();

        {
            let _guard = thread.protect();
            let hash = make_hash(4000);
            let addr = make_addr(10, 500);
            let r = index.find_or_create(hash, LogicalAddress::INVALID);
            let committed = HashBucketEntry::new(r.entry.tag(), addr, false);
            index.update(r.slot, r.entry, committed);
        }

        // Invalidate range that doesn't include page 10.
        let count = index.invalidate_entries_in_range(make_addr(0, 0), make_addr(5, 0));
        assert_eq!(count, 0);
        assert_eq!(index.entry_count(), 1);
    }

    #[test]
    fn test_invalidate_includes_tentative_entries() {
        let index = test_index();
        let thread = index.register_thread().unwrap();

        // Create a tentative entry (don't commit it).
        {
            let _guard = thread.protect();
            let hash = make_hash(5000);
            let r = index.find_or_create(hash, make_addr(3, 100));
            assert!(r.created);
            assert!(r.entry.is_tentative());
        }

        // Tentative entries should also be invalidated if in range.
        let count = index.invalidate_entries_in_range(make_addr(0, 0), make_addr(100, 0));
        // The tentative entry has address INVALID (raw value 1) which is in [0, huge_range).
        // But our entry was created with make_addr(3, 100), which has the address as the
        // initial_address embedded in the tentative entry.
        assert!(count >= 1);
        assert_eq!(index.entry_count(), 0);
    }

    // -----------------------------------------------------------------------
    // Tentative entry cleanup
    // -----------------------------------------------------------------------

    #[test]
    fn test_cleanup_tentative_empty_table() {
        let index = test_index();
        assert_eq!(index.cleanup_tentative_entries_for_recovery(), 0);
    }

    #[test]
    fn test_cleanup_tentative_removes_tentative() {
        let index = test_index();
        let thread = index.register_thread().unwrap();
        let _guard = thread.protect();

        // Create 3 tentative entries (don't commit).
        for i in 0..3u64 {
            let hash = make_hash(6000 + i);
            let r = index.find_or_create(hash, LogicalAddress::INVALID);
            assert!(r.created);
        }

        assert_eq!(index.entry_count(), 3);
        let cleaned = index.cleanup_tentative_entries_for_recovery();
        assert_eq!(cleaned, 3);
        assert_eq!(index.entry_count(), 0);
    }

    #[test]
    fn test_cleanup_tentative_preserves_committed() {
        let index = test_index();
        let thread = index.register_thread().unwrap();
        let _guard = thread.protect();

        // Create and commit one entry.
        let hash_committed = make_hash(7000);
        let addr = make_addr(5, 256);
        let r = index.find_or_create(hash_committed, LogicalAddress::INVALID);
        let committed = HashBucketEntry::new(r.entry.tag(), addr, false);
        index.update(r.slot, r.entry, committed);

        // Create one tentative entry.
        let hash_tentative = make_hash(7001);
        let r2 = index.find_or_create(hash_tentative, LogicalAddress::INVALID);
        assert!(r2.created);

        assert_eq!(index.entry_count(), 2);

        // Cleanup should only remove the tentative one.
        let cleaned = index.cleanup_tentative_entries_for_recovery();
        assert_eq!(cleaned, 1);
        assert_eq!(index.entry_count(), 1);

        // Committed entry still findable.
        let (found, _) = index.find(hash_committed).unwrap();
        assert_eq!(found.address(), addr);
    }

    // -----------------------------------------------------------------------
    // Accessors
    // -----------------------------------------------------------------------

    #[test]
    fn test_table_accessor() {
        let index = test_index();
        assert_eq!(index.table().num_buckets(), 256);
    }

    #[test]
    fn test_epoch_accessor() {
        let index = test_index();
        assert_eq!(index.epoch().current_epoch(), 1);
        assert_eq!(index.epoch().safe_epoch(), 0);
    }

    #[test]
    fn test_debug_impl() {
        let index = test_index();
        let debug = format!("{:?}", index);
        assert!(debug.contains("HashIndex"));
        assert!(debug.contains("num_buckets"));
    }

    // -----------------------------------------------------------------------
    // Concurrent operations (multi-threaded)
    // -----------------------------------------------------------------------

    #[test]
    fn test_concurrent_inserts() {
        use std::sync::Arc;
        use std::thread;

        let index = Arc::new(HashIndex::new(12)); // 4096 buckets
        let num_threads = 4;
        let ops_per_thread = 200;

        let handles: Vec<_> = (0..num_threads)
            .map(|t| {
                let idx = Arc::clone(&index);
                thread::spawn(move || {
                    let thread = idx.register_thread().unwrap();
                    let _guard = thread.protect();

                    let mut created_count = 0u64;
                    for i in 0..ops_per_thread {
                        let seed = t * 10_000 + i;
                        let hash = make_hash(seed);
                        let addr = make_addr(t as u32, i as u32);

                        let r = idx.find_or_create(hash, LogicalAddress::INVALID);
                        if r.created {
                            let committed = HashBucketEntry::new(r.entry.tag(), addr, false);
                            idx.update(r.slot, r.entry, committed);
                            created_count += 1;
                        }
                    }
                    created_count
                })
            })
            .collect();

        let total: u64 = handles.into_iter().map(|h| h.join().unwrap()).sum();

        // Most inserts should succeed. A few may collide on tag+bucket
        // (different seeds producing identical 14-bit tag and bucket index),
        // causing find_or_create to return an existing entry for one thread
        // while the other still got `created = true` from its CAS. The
        // entry_count may be slightly below `total` due to these collisions.
        let expected = num_threads * ops_per_thread;
        assert!(
            total >= expected - 10,
            "expected ~{expected} creates, got {total}",
        );
        assert!(
            index.entry_count() >= expected - 10,
            "expected ~{expected} entries, got {}",
            index.entry_count(),
        );
    }

    #[test]
    fn test_concurrent_find_and_insert() {
        use std::sync::Arc;
        use std::thread;

        let index = Arc::new(HashIndex::new(10));

        // Pre-populate with committed entries.
        {
            let thread = index.register_thread().unwrap();
            let _guard = thread.protect();
            for i in 0..100u64 {
                let hash = make_hash(8000 + i);
                let addr = make_addr(1, i as u32);
                let r = index.find_or_create(hash, LogicalAddress::INVALID);
                let committed = HashBucketEntry::new(r.entry.tag(), addr, false);
                index.update(r.slot, r.entry, committed);
            }
        }

        // Concurrent readers: verify pre-populated entries.
        let reader_handles: Vec<_> = (0..4)
            .map(|_| {
                let idx = Arc::clone(&index);
                thread::spawn(move || {
                    let thread = idx.register_thread().unwrap();
                    let _guard = thread.protect();

                    let mut found = 0u64;
                    for i in 0..100u64 {
                        let hash = make_hash(8000 + i);
                        if idx.find(hash).is_some() {
                            found += 1;
                        }
                    }
                    found
                })
            })
            .collect();

        for h in reader_handles {
            assert_eq!(h.join().unwrap(), 100);
        }
    }

    #[test]
    fn test_concurrent_invalidate_with_readers() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
        use std::thread;

        let index = Arc::new(HashIndex::new(10));
        let done = Arc::new(AtomicBool::new(false));

        // Pre-populate: 50 entries on page 1, 50 on page 5.
        {
            let thread = index.register_thread().unwrap();
            let _guard = thread.protect();
            for i in 0..50u64 {
                let hash = make_hash(9000 + i);
                let addr = make_addr(1, i as u32);
                let r = index.find_or_create(hash, LogicalAddress::INVALID);
                let committed = HashBucketEntry::new(r.entry.tag(), addr, false);
                index.update(r.slot, r.entry, committed);
            }
            for i in 0..50u64 {
                let hash = make_hash(9500 + i);
                let addr = make_addr(5, i as u32);
                let r = index.find_or_create(hash, LogicalAddress::INVALID);
                let committed = HashBucketEntry::new(r.entry.tag(), addr, false);
                index.update(r.slot, r.entry, committed);
            }
        }

        // Barrier to ensure reader is running before we invalidate.
        let reader_started = Arc::new(AtomicBool::new(false));

        // Reader thread: continually look up page-5 entries.
        let idx_r = Arc::clone(&index);
        let done_r = Arc::clone(&done);
        let started_r = Arc::clone(&reader_started);
        let reader = thread::spawn(move || {
            let thread = idx_r.register_thread().unwrap();
            let mut iterations = 0u64;
            started_r.store(true, AtomicOrdering::Release);
            while !done_r.load(AtomicOrdering::Relaxed) {
                let _guard = thread.protect();
                for i in 0..50u64 {
                    let hash = make_hash(9500 + i);
                    // Page 5 entries should always be findable.
                    let _ = idx_r.find(hash);
                }
                iterations += 1;
            }
            iterations
        });

        // Wait for reader to start.
        while !reader_started.load(AtomicOrdering::Acquire) {
            std::hint::spin_loop();
        }

        // Invalidator: remove page 1 entries.
        let invalidated = index.invalidate_entries_in_range(make_addr(1, 0), make_addr(2, 0));
        assert_eq!(invalidated, 50);

        done.store(true, AtomicOrdering::Relaxed);
        let reader_iters = reader.join().unwrap();
        assert!(reader_iters > 0);

        // Page 5 entries are still intact.
        assert_eq!(index.entry_count(), 50);
    }

    // -----------------------------------------------------------------------
    // Version tracking
    // -----------------------------------------------------------------------

    #[test]
    fn test_version_default() {
        let index = test_index();
        assert_eq!(index.version(), 0);
    }

    #[test]
    fn test_version_set_and_get() {
        let index = test_index();
        index.set_version(1);
        assert_eq!(index.version(), 1);
        index.set_version(0);
        assert_eq!(index.version(), 0);
    }

    // -----------------------------------------------------------------------
    // Live entry count (atomic O(1) counter)
    // -----------------------------------------------------------------------

    #[test]
    fn test_entry_count_increments_on_create() {
        let index = test_index();
        let thread = index.register_thread().unwrap();
        let _guard = thread.protect();

        assert_eq!(index.entry_count(), 0);
        for i in 0..10u64 {
            let hash = make_hash(10_000 + i);
            let r = index.find_or_create(hash, LogicalAddress::INVALID);
            assert!(r.created);
        }
        assert_eq!(index.entry_count(), 10);
    }

    #[test]
    fn test_entry_count_no_double_count_on_existing() {
        let index = test_index();
        let thread = index.register_thread().unwrap();
        let _guard = thread.protect();

        let hash = make_hash(11_000);
        let addr = make_addr(1, 0);
        let r = index.find_or_create(hash, LogicalAddress::INVALID);
        assert!(r.created);
        let committed = HashBucketEntry::new(r.entry.tag(), addr, false);
        index.update(r.slot, r.entry, committed);
        assert_eq!(index.entry_count(), 1);

        let r2 = index.find_or_create(hash, LogicalAddress::INVALID);
        assert!(!r2.created);
        assert_eq!(index.entry_count(), 1);
    }

    #[test]
    fn test_entry_count_decrements_on_invalidate() {
        let index = test_index();
        let thread = index.register_thread().unwrap();
        {
            let _guard = thread.protect();
            for i in 0..5u64 {
                let hash = make_hash(12_000 + i);
                let addr = make_addr(3, (i * 64) as u32);
                let r = index.find_or_create(hash, LogicalAddress::INVALID);
                let committed = HashBucketEntry::new(r.entry.tag(), addr, false);
                index.update(r.slot, r.entry, committed);
            }
        }
        assert_eq!(index.entry_count(), 5);
        let invalidated = index.invalidate_entries_in_range(make_addr(3, 0), make_addr(4, 0));
        assert_eq!(invalidated, 5);
        assert_eq!(index.entry_count(), 0);
    }

    #[test]
    fn test_scan_entry_count_matches_atomic() {
        let index = test_index();
        let thread = index.register_thread().unwrap();
        let _guard = thread.protect();

        for i in 0..20u64 {
            let hash = make_hash(13_000 + i);
            let addr = make_addr(1, (i * 8) as u32);
            let r = index.find_or_create(hash, LogicalAddress::INVALID);
            if r.created {
                let committed = HashBucketEntry::new(r.entry.tag(), addr, false);
                index.update(r.slot, r.entry, committed);
            }
        }
        assert_eq!(index.entry_count(), index.scan_entry_count());
    }

    // -----------------------------------------------------------------------
    // Load factor
    // -----------------------------------------------------------------------

    #[test]
    fn test_load_factor_empty() {
        let index = test_index();
        assert_eq!(index.load_factor(), 0.0);
    }

    #[test]
    fn test_load_factor_with_entries() {
        let index = HashIndex::new(4); // 16 buckets, 16 * 7 = 112 slots
        let thread = index.register_thread().unwrap();
        let _guard = thread.protect();

        for i in 0..7u64 {
            let hash = make_hash(14_000 + i);
            let r = index.find_or_create(hash, LogicalAddress::INVALID);
            if r.created {
                let addr = make_addr(1, i as u32);
                let committed = HashBucketEntry::new(r.entry.tag(), addr, false);
                index.update(r.slot, r.entry, committed);
            }
        }

        let lf = index.load_factor();
        assert!(lf > 0.0);
        assert!(lf < 1.0);
        let expected = 7.0 / 112.0;
        assert!((lf - expected).abs() < 1e-10);
    }

    // ── Prefetch tests ──────────────────────────────────────────────────────

    #[test]
    fn prefetch_does_not_panic() {
        let index = HashIndex::new(10);
        let hash = KeyHash::new(0xDEAD_BEEF);
        index.prefetch(hash);
    }

    #[test]
    fn prefetch_before_find() {
        let index = HashIndex::new(10);
        let thread = index.register_thread().unwrap();
        let _guard = thread.protect();

        let hash = KeyHash::new(12345);
        let r = index.find_or_create(hash, LogicalAddress::INVALID);
        assert!(r.created);
        let addr = LogicalAddress::new(Page(1), Offset(42));
        let committed = HashBucketEntry::new(r.entry.tag(), addr, false);
        index.update(r.slot, r.entry, committed);

        index.prefetch(hash);
        let found = index.find(hash);
        assert!(found.is_some());
    }

    #[test]
    fn prefetch_miss_is_harmless() {
        let index = HashIndex::new(10);
        let hash = KeyHash::new(0xBAAD_F00D);
        index.prefetch(hash);
        assert!(index.find(hash).is_none());
    }
}
