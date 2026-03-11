//! Grow manager: coordinates hash table resize for [`FasterKv`].
//!
//! [`GrowManager`] orchestrates the full lifecycle of an online hash table
//! grow operation. It owns a [`GrowStateMachine`] and ties together the
//! phase transitions, chunk-based bucket splitting, and table swap into a
//! single, easy-to-drive API suitable for embedding in [`FasterKv`].
//!
//! # Cooperative splitting
//!
//! During a grow, the manager exposes [`process_chunks`](GrowManager::process_chunks)
//! which splits a bounded number of chunks per call. Session operations call
//! this incrementally so that the grow cost is amortized across all threads —
//! matching the C++/C# FASTER cooperative grow pattern.
//!
//! # C++ / C# correspondence
//!
//! | Rust                 | C++ / C#                              |
//! |----------------------|---------------------------------------|
//! | `GrowManager`        | Inlined in `FasterKV::InternalGrow()` |
//! | `GrowConfig`         | `kHashTableLoadFactor` constant       |
//! | `GrowProgress`       | (implicit counters)                   |
//!
//! [`FasterKv`]: crate::store::FasterKv

use crate::hash::index::HashIndex;
use crate::hash::table::HashTable;
use crate::sync::RwLock;

use super::splitter::{BucketSplitter, HashResolver, SplitResult};
use super::state_machine::{GrowError, GrowPhase, GrowStateMachine};

// ---------------------------------------------------------------------------
// GrowConfig
// ---------------------------------------------------------------------------

/// Configuration knobs for automatic and manual hash table grow.
///
/// # Examples
///
/// ```
/// use faster_core::grow::GrowConfig;
///
/// // Defaults
/// let cfg = GrowConfig::default();
/// assert!((cfg.load_factor_threshold - 0.75).abs() < f64::EPSILON);
/// assert_eq!(cfg.chunks_per_operation, 16);
/// assert!(cfg.enabled);
///
/// // Custom
/// let cfg = GrowConfig {
///     load_factor_threshold: 0.5,
///     chunks_per_operation: 8,
///     enabled: false,
/// };
/// assert!(!cfg.enabled);
/// ```
#[derive(Debug, Clone)]
pub struct GrowConfig {
    /// Load factor threshold above which a grow is triggered.
    ///
    /// Load factor is defined as `entry_count / (num_buckets * 7)` — each
    /// bucket holds up to 7 entries before overflow chains are needed.
    /// Default: `0.75`.
    pub load_factor_threshold: f64,

    /// Number of chunks to split per cooperative call to
    /// [`GrowManager::process_chunks`].
    ///
    /// Higher values finish the grow faster but increase per-operation
    /// latency during the grow. Default: `16`.
    pub chunks_per_operation: u32,

    /// Whether automatic grow (triggered by load factor) is enabled.
    ///
    /// When `false`, the grow can still be triggered manually via
    /// [`GrowManager::begin_grow`]. Default: `true`.
    pub enabled: bool,
}

impl Default for GrowConfig {
    fn default() -> Self {
        Self {
            load_factor_threshold: 0.75,
            chunks_per_operation: 16,
            enabled: true,
        }
    }
}

// ---------------------------------------------------------------------------
// GrowProgress
// ---------------------------------------------------------------------------

/// Progress snapshot of an in-progress grow operation.
///
/// Returned by [`GrowManager::process_chunks`] to report how far along the
/// bucket splitting is.
///
/// # Examples
///
/// ```
/// use faster_core::grow::GrowProgress;
///
/// let p = GrowProgress {
///     chunks_done: 3,
///     chunks_total: 4,
///     fraction: 0.75,
///     is_complete: false,
/// };
/// assert!(!p.is_complete);
/// ```
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GrowProgress {
    /// Number of chunks that have been fully split so far.
    pub chunks_done: u32,
    /// Total number of chunks for this grow operation.
    pub chunks_total: u32,
    /// Completion fraction (0.0 to 1.0).
    pub fraction: f64,
    /// `true` when all chunks have been processed.
    pub is_complete: bool,
}

impl GrowProgress {
    /// A progress value representing "no grow in progress".
    pub const IDLE: Self = Self {
        chunks_done: 0,
        chunks_total: 0,
        fraction: 0.0,
        is_complete: false,
    };
}

// ---------------------------------------------------------------------------
// GrowManager
// ---------------------------------------------------------------------------

/// Coordinates a hash table grow (online resize) operation.
///
/// Owns the [`GrowStateMachine`] and provides a high-level API for:
/// - Checking whether a grow should be initiated ([`should_grow`](Self::should_grow))
/// - Starting a grow ([`begin_grow`](Self::begin_grow))
/// - Incrementally splitting buckets ([`process_chunks`](Self::process_chunks))
/// - Finalizing the grow ([`complete_grow`](Self::complete_grow))
///
/// # Thread safety
///
/// `GrowManager` is `Send + Sync`. Phase transitions use CAS internally.
///
/// # Examples
///
/// ```
/// use faster_core::grow::{GrowConfig, GrowManager};
/// use faster_core::hash_index::HashIndex;
///
/// let manager = GrowManager::new(GrowConfig::default());
/// let index = HashIndex::new(8); // 256 buckets
///
/// // No entries yet — should not grow.
/// assert!(!manager.should_grow(&index));
/// assert!(!manager.is_growing());
/// ```
pub struct GrowManager {
    /// The phase-based state machine driving grow lifecycle.
    state_machine: GrowStateMachine,
    /// Configuration for this manager.
    config: GrowConfig,
    /// The new (doubled) hash table allocated during grow. Only populated
    /// between `begin_grow` and `complete_grow`.
    new_table: RwLock<Option<HashTable>>,
}

impl GrowManager {
    /// Creates a new grow manager with the given configuration.
    pub fn new(config: GrowConfig) -> Self {
        Self {
            state_machine: GrowStateMachine::new(),
            config,
            new_table: RwLock::new(None),
        }
    }

    /// Returns `true` if the hash index load factor exceeds the configured
    /// threshold and automatic grow is enabled.
    ///
    /// Does **not** trigger a grow — the caller must call [`begin_grow`](Self::begin_grow)
    /// to actually start the resize.
    pub fn should_grow(&self, index: &HashIndex) -> bool {
        if !self.config.enabled {
            return false;
        }
        if self.is_growing() {
            return false;
        }
        index.load_factor() > self.config.load_factor_threshold
    }

    /// Returns `true` if a grow operation is currently in progress.
    ///
    /// A grow is "in progress" when the state machine is in any phase
    /// other than `Rest`.
    #[inline]
    pub fn is_growing(&self) -> bool {
        self.state_machine.phase() != GrowPhase::Rest
    }

    /// Begins a grow operation, doubling the hash table size.
    ///
    /// Transitions the state machine `Rest → Prepare → InProgress`,
    /// allocating a new table with double the current bucket count.
    ///
    /// # Errors
    ///
    /// - [`GrowError::AlreadyInProgress`] if a grow is already running.
    /// - [`GrowError::InvalidSize`] if the resulting table size exceeds limits.
    pub fn begin_grow(&self, index: &HashIndex) -> Result<(), GrowError> {
        let current_log2 = index.log2_buckets();
        let new_size_bits = (current_log2 + 1) as u8;
        let old_version = index.version();

        // Rest → Prepare
        self.state_machine.start(old_version, new_size_bits)?;

        // Allocate the new (doubled) table.
        let new_table = HashTable::new(current_log2 + 1);
        {
            let mut guard = self.new_table.write().unwrap();
            *guard = Some(new_table);
        }

        // Prepare → InProgress
        self.state_machine
            .try_advance(GrowPhase::Prepare, GrowPhase::InProgress)
            .map_err(|_| GrowError::NotInExpectedPhase {
                expected: GrowPhase::Prepare,
                actual: self.state_machine.phase(),
            })?;

        Ok(())
    }

    /// Splits up to `chunks_per_call` chunks from the old table into the
    /// new table.
    ///
    /// Returns a [`GrowProgress`] snapshot. If no grow is in progress,
    /// returns [`GrowProgress::IDLE`].
    ///
    /// The `resolver` provides full key hashes for entries whose split
    /// direction cannot be determined from the truncated tag alone.
    pub fn process_chunks<R: HashResolver>(
        &self,
        index: &HashIndex,
        chunks_per_call: u32,
        resolver: &R,
    ) -> GrowProgress {
        if self.state_machine.phase() != GrowPhase::InProgress {
            return GrowProgress::IDLE;
        }

        let state_guard = match self.state_machine.grow_state() {
            Some(g) => g,
            None => return GrowProgress::IDLE,
        };

        let new_table_guard = self.new_table.read().unwrap();
        let new_table = match new_table_guard.as_ref() {
            Some(t) => t,
            None => return GrowProgress::IDLE,
        };

        let old_log2 = index.log2_buckets();
        let new_log2 = old_log2 + 1;
        let old_table = index.table();

        let splitter = BucketSplitter::new(old_table, new_table, resolver);

        let mut _split_result = SplitResult::default();
        for _ in 0..chunks_per_call {
            if let Some(chunk_idx) = state_guard.claim_next_chunk() {
                let chunk_result = splitter.split_chunk(chunk_idx, old_log2 as u8, new_log2 as u8);
                _split_result.merge(&chunk_result);
                state_guard.complete_chunk();
            } else {
                break;
            }
        }

        let total = state_guard.num_chunks();
        let pending = state_guard.pending_chunks();
        let done = total.saturating_sub(pending);
        let fraction = if total == 0 {
            1.0
        } else {
            done as f64 / total as f64
        };

        GrowProgress {
            chunks_done: done,
            chunks_total: total,
            fraction,
            is_complete: pending == 0,
        }
    }

    /// Convenience wrapper around [`process_chunks`](Self::process_chunks)
    /// using the configured [`GrowConfig::chunks_per_operation`].
    pub fn process_chunks_default<R: HashResolver>(
        &self,
        index: &HashIndex,
        resolver: &R,
    ) -> GrowProgress {
        self.process_chunks(index, self.config.chunks_per_operation, resolver)
    }

    /// Finalizes a completed grow operation.
    ///
    /// Transitions `InProgress → WaitCompletion → Completed → Rest`,
    /// swaps the hash index to use the new table, and updates the version.
    ///
    /// # Errors
    ///
    /// - [`GrowError::NotInExpectedPhase`] if the state machine is not in
    ///   `InProgress` (or the chunks are not yet complete).
    pub fn complete_grow(&self, index: &mut HashIndex) -> Result<(), GrowError> {
        // Verify all chunks are done.
        if let Some(state) = self.state_machine.grow_state() {
            if !state.is_complete() {
                return Err(GrowError::NotInExpectedPhase {
                    expected: GrowPhase::WaitCompletion,
                    actual: GrowPhase::InProgress,
                });
            }
        }

        // InProgress → WaitCompletion
        self.state_machine
            .try_advance(GrowPhase::InProgress, GrowPhase::WaitCompletion)?;

        // Swap the table: replace the index's table with the new one.
        let new_table = {
            let mut guard = self.new_table.write().unwrap();
            guard.take()
        };

        if let Some(table) = new_table {
            index.swap_table(table);

            // Toggle version.
            let old_version = index.version();
            let new_version = if old_version == 0 { 1 } else { 0 };
            index.set_version(new_version);
        }

        // WaitCompletion → Completed
        self.state_machine
            .try_advance(GrowPhase::WaitCompletion, GrowPhase::Completed)?;

        // Completed → Rest
        self.state_machine.reset()?;

        Ok(())
    }

    /// Returns the current phase of the grow state machine.
    #[inline]
    pub fn phase(&self) -> GrowPhase {
        self.state_machine.phase()
    }

    /// Returns a reference to the grow configuration.
    #[inline]
    pub fn config(&self) -> &GrowConfig {
        &self.config
    }
}

impl std::fmt::Debug for GrowManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GrowManager")
            .field("phase", &self.state_machine.phase())
            .field("config", &self.config)
            .finish()
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::address::LogicalAddress;
    use crate::hash::KeyHash;
    use crate::hash_bucket::HashBucketEntry;

    /// A trivial [`HashResolver`] that returns the address bits as the hash.
    /// Good enough for testing split direction logic.
    struct TestResolver;

    impl HashResolver for TestResolver {
        fn resolve_hash(&self, entry: HashBucketEntry) -> Option<u64> {
            Some(entry.address().raw())
        }
    }

    // -- GrowConfig ----------------------------------------------------------

    #[test]
    fn config_defaults() {
        let cfg = GrowConfig::default();
        assert!((cfg.load_factor_threshold - 0.75).abs() < f64::EPSILON);
        assert_eq!(cfg.chunks_per_operation, 16);
        assert!(cfg.enabled);
    }

    #[test]
    fn config_custom() {
        let cfg = GrowConfig {
            load_factor_threshold: 0.5,
            chunks_per_operation: 4,
            enabled: false,
        };
        assert!((cfg.load_factor_threshold - 0.5).abs() < f64::EPSILON);
        assert_eq!(cfg.chunks_per_operation, 4);
        assert!(!cfg.enabled);
    }

    // -- GrowProgress --------------------------------------------------------

    #[test]
    fn progress_idle() {
        let p = GrowProgress::IDLE;
        assert_eq!(p.chunks_done, 0);
        assert_eq!(p.chunks_total, 0);
        assert!(!p.is_complete);
    }

    // -- GrowManager lifecycle -----------------------------------------------

    #[test]
    fn should_grow_empty_index() {
        let mgr = GrowManager::new(GrowConfig::default());
        let index = HashIndex::new(8);
        assert!(!mgr.should_grow(&index));
    }

    #[test]
    fn should_grow_disabled() {
        let cfg = GrowConfig {
            enabled: false,
            load_factor_threshold: 0.0, // would trigger on any entry
            ..GrowConfig::default()
        };
        let mgr = GrowManager::new(cfg);
        let index = HashIndex::new(8);
        assert!(!mgr.should_grow(&index));
    }

    #[test]
    fn is_growing_initially_false() {
        let mgr = GrowManager::new(GrowConfig::default());
        assert!(!mgr.is_growing());
    }

    #[test]
    fn begin_grow_transitions_to_in_progress() {
        let mgr = GrowManager::new(GrowConfig::default());
        let index = HashIndex::new(4); // 16 buckets
        mgr.begin_grow(&index).unwrap();
        assert!(mgr.is_growing());
        assert_eq!(mgr.phase(), GrowPhase::InProgress);
    }

    #[test]
    fn begin_grow_double_start_fails() {
        let mgr = GrowManager::new(GrowConfig::default());
        let index = HashIndex::new(4);
        mgr.begin_grow(&index).unwrap();
        assert!(mgr.begin_grow(&index).is_err());
    }

    #[test]
    fn full_grow_cycle_small_table() {
        // 16-bucket table -> 32-bucket table.
        let mgr = GrowManager::new(GrowConfig {
            chunks_per_operation: 1,
            ..GrowConfig::default()
        });
        let mut index = HashIndex::new(4);
        let thread = index.register_thread().unwrap();

        // Insert a few entries.
        {
            let _guard = thread.protect();
            for i in 0u64..8 {
                let hash = KeyHash::new(i);
                let result = index.find_or_create(hash, LogicalAddress::INVALID);
                assert!(result.created);
                // Commit the tentative entry.
                let committed =
                    HashBucketEntry::new(result.entry.tag(), result.entry.address(), false);
                index.update(result.slot, result.entry, committed);
            }
        }
        assert_eq!(index.entry_count(), 8);
        assert_eq!(index.num_buckets(), 16);

        // Begin grow.
        mgr.begin_grow(&index).unwrap();
        assert!(mgr.is_growing());

        // Process all chunks.
        let resolver = TestResolver;
        loop {
            let progress = mgr.process_chunks(&index, 1, &resolver);
            if progress.is_complete || progress == GrowProgress::IDLE {
                break;
            }
        }

        // Complete grow.
        mgr.complete_grow(&mut index).unwrap();
        assert!(!mgr.is_growing());
        assert_eq!(mgr.phase(), GrowPhase::Rest);

        // Table should now be doubled.
        assert_eq!(index.num_buckets(), 32);
        assert_eq!(index.log2_buckets(), 5);
    }

    #[test]
    fn process_chunks_when_not_growing_returns_idle() {
        let mgr = GrowManager::new(GrowConfig::default());
        let index = HashIndex::new(4);
        let resolver = TestResolver;
        let progress = mgr.process_chunks(&index, 16, &resolver);
        assert_eq!(progress, GrowProgress::IDLE);
    }

    #[test]
    fn process_chunks_incremental() {
        // Use a table large enough to have > 1 chunk.
        // With CHUNK_SIZE=16384 and log2=15 (32768 buckets), we get 2 chunks.
        let mgr = GrowManager::new(GrowConfig::default());
        let index = HashIndex::new(15); // 32768 buckets -> 2 chunks
        mgr.begin_grow(&index).unwrap();

        let resolver = TestResolver;

        // Process one chunk at a time.
        let p1 = mgr.process_chunks(&index, 1, &resolver);
        assert_eq!(p1.chunks_done, 1);
        assert_eq!(p1.chunks_total, 2);
        assert!(!p1.is_complete);

        let p2 = mgr.process_chunks(&index, 1, &resolver);
        assert_eq!(p2.chunks_done, 2);
        assert_eq!(p2.chunks_total, 2);
        assert!(p2.is_complete);
    }

    #[test]
    fn complete_grow_before_done_fails() {
        let mgr = GrowManager::new(GrowConfig::default());
        let mut index = HashIndex::new(15); // 2 chunks
        mgr.begin_grow(&index).unwrap();

        // Don't process any chunks -- complete should fail.
        let result = mgr.complete_grow(&mut index);
        assert!(result.is_err());
    }

    #[test]
    fn manual_grow_trigger() {
        // Even with enabled=false, begin_grow works (manual trigger).
        let cfg = GrowConfig {
            enabled: false,
            ..GrowConfig::default()
        };
        let mgr = GrowManager::new(cfg);
        let mut index = HashIndex::new(4); // 16 buckets
        assert!(!mgr.should_grow(&index));

        mgr.begin_grow(&index).unwrap();
        assert!(mgr.is_growing());

        let resolver = TestResolver;
        loop {
            let p = mgr.process_chunks(&index, 64, &resolver);
            if p.is_complete || p == GrowProgress::IDLE {
                break;
            }
        }

        mgr.complete_grow(&mut index).unwrap();
        assert_eq!(index.num_buckets(), 32);
    }

    #[test]
    fn grow_preserves_entries() {
        // Insert entries, grow, verify all entries are still findable.
        let mgr = GrowManager::new(GrowConfig::default());
        let mut index = HashIndex::new(4); // 16 buckets
        let thread = index.register_thread().unwrap();

        let keys: Vec<u64> = (0..12).collect();
        {
            let _guard = thread.protect();
            for &k in &keys {
                let hash = KeyHash::new(k);
                let result = index.find_or_create(hash, LogicalAddress::INVALID);
                assert!(result.created);
                let committed =
                    HashBucketEntry::new(result.entry.tag(), result.entry.address(), false);
                index.update(result.slot, result.entry, committed);
            }
        }

        assert_eq!(index.entry_count(), 12);

        // Grow.
        mgr.begin_grow(&index).unwrap();
        let resolver = TestResolver;
        loop {
            let p = mgr.process_chunks(&index, 64, &resolver);
            if p.is_complete || p == GrowProgress::IDLE {
                break;
            }
        }
        mgr.complete_grow(&mut index).unwrap();

        // All entries should still be findable in the new table.
        {
            let _guard = thread.protect();
            for &k in &keys {
                let hash = KeyHash::new(k);
                let found = index.find(hash);
                assert!(found.is_some(), "key {} not found after grow", k);
            }
        }
    }

    #[test]
    fn should_grow_triggers_above_threshold() {
        // Use a very low threshold to trigger on a small number of entries.
        let cfg = GrowConfig {
            load_factor_threshold: 0.001,
            ..GrowConfig::default()
        };
        let mgr = GrowManager::new(cfg);
        let index = HashIndex::new(4); // 16 buckets, 112 slots
        let thread = index.register_thread().unwrap();

        // Empty -- should not grow.
        assert!(!mgr.should_grow(&index));

        // Insert one entry to exceed 0.001 load factor.
        {
            let _guard = thread.protect();
            let hash = KeyHash::new(42);
            let r = index.find_or_create(hash, LogicalAddress::INVALID);
            let committed = HashBucketEntry::new(r.entry.tag(), r.entry.address(), false);
            index.update(r.slot, r.entry, committed);
        }

        assert!(mgr.should_grow(&index));
    }

    #[test]
    fn debug_formatting() {
        let mgr = GrowManager::new(GrowConfig::default());
        let dbg = format!("{mgr:?}");
        assert!(dbg.contains("GrowManager"));
        assert!(dbg.contains("Rest"));
    }
}
