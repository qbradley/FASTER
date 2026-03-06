//! Hash table online-resize (grow) data structures.
//!
//! FASTER grows its hash index online by doubling the bucket count while
//! concurrent readers and writers continue operating. The process splits
//! buckets in parallel, chunk by chunk, using a two-version scheme: the old
//! table remains readable while the new (double-sized) table is populated.
//!
//! This module provides the foundational types for grow — **not** the state
//! machine or the actual split logic. Those will be added in later items.
//!
//! # Key types
//!
//! - [`GrowState`] — per-grow metadata: version numbers, chunk progress.
//! - [`CHUNK_SIZE`] — number of buckets processed per split work unit.
//! - [`bucket_index_for_version`] — computes the bucket a key maps to in
//!   a table of a given size (used during split to decide left vs right).
//!
//! # C++ / C# correspondence
//!
//! | Rust                        | C++ (`grow_state.h`)            | C# (`IndexResizeStateMachine.cs`) |
//! |-----------------------------|---------------------------------|-----------------------------------|
//! | `GrowState`                 | `GrowState<H>`                 | `IndexResizeInfo`                 |
//! | `CHUNK_SIZE`                | `kHashTableChunkSize`           | `Constants.kSizeofChunk`          |
//! | `bucket_index_for_version`  | `key_hash_t::hash_table_index`  | `(hash & size_mask)`              |

use core::sync::atomic::{AtomicU32, Ordering};

/// Number of buckets processed per split work-unit during grow.
///
/// Each thread atomically claims a chunk via [`GrowState::next_chunk`],
/// then splits all `CHUNK_SIZE` buckets in that chunk from the old table
/// into the new (doubled) table.
///
/// Matches C++ `kHashTableChunkSize` and C# `Constants.kSizeofChunk`.
/// Value: 2^14 = 16 384 — chosen to balance per-chunk overhead against
/// granularity of parallel work distribution.
pub const CHUNK_SIZE: u32 = 16_384;

// ---------------------------------------------------------------------------
// GrowState
// ---------------------------------------------------------------------------

/// Metadata for a single in-progress hash table grow operation.
///
/// Created when a grow is initiated and consumed when all chunks have been
/// split. Threads cooperate by atomically claiming chunks via
/// [`claim_next_chunk`](Self::claim_next_chunk) and decrementing
/// [`num_pending_chunks`](Self::pending_chunks) on completion.
///
/// # Versions
///
/// FASTER uses two table slots (version 0 and version 1). A grow transitions
/// from `old_version` to `new_version`; on the next grow the roles swap.
/// This avoids reallocating the table array — only the bucket storage is
/// doubled.
///
/// # Example
///
/// ```
/// use faster_core::grow::GrowState;
///
/// // Growing a table that currently has 2^16 = 65 536 buckets.
/// let state = GrowState::new(0, 1, 65_536);
/// assert_eq!(state.old_version(), 0);
/// assert_eq!(state.new_version(), 1);
/// assert_eq!(state.num_chunks(), 4); // 65536 / 16384
/// assert_eq!(state.pending_chunks(), 4);
/// ```
pub struct GrowState {
    /// Version number of the table being grown FROM.
    old_version: u32,
    /// Version number of the table being grown TO.
    new_version: u32,
    /// Total number of chunks to split (`table_size / CHUNK_SIZE`).
    num_chunks: u32,
    /// How many chunks still need splitting. Starts at `num_chunks` and
    /// is decremented atomically as threads complete their chunks.
    num_pending_chunks: AtomicU32,
    /// Next chunk index for a thread to claim. Incremented atomically.
    next_chunk: AtomicU32,
}

impl GrowState {
    /// Creates a new `GrowState` for growing a table of `table_size` buckets.
    ///
    /// `table_size` must be a multiple of [`CHUNK_SIZE`]. If it is smaller
    /// than `CHUNK_SIZE`, a single chunk covers the entire table.
    ///
    /// # Panics
    ///
    /// Panics if `old_version == new_version`.
    pub fn new(old_version: u32, new_version: u32, table_size: u64) -> Self {
        assert_ne!(
            old_version, new_version,
            "old_version and new_version must differ"
        );
        let num_chunks = (table_size as u32).div_ceil(CHUNK_SIZE);
        Self {
            old_version,
            new_version,
            num_chunks,
            num_pending_chunks: AtomicU32::new(num_chunks),
            next_chunk: AtomicU32::new(0),
        }
    }

    /// Version of the table being grown FROM.
    #[inline]
    pub fn old_version(&self) -> u32 {
        self.old_version
    }

    /// Version of the table being grown TO.
    #[inline]
    pub fn new_version(&self) -> u32 {
        self.new_version
    }

    /// Total number of chunks to process.
    #[inline]
    pub fn num_chunks(&self) -> u32 {
        self.num_chunks
    }

    /// Number of chunks still awaiting completion.
    #[inline]
    pub fn pending_chunks(&self) -> u32 {
        self.num_pending_chunks.load(Ordering::Acquire)
    }

    /// Returns `true` when all chunks have been processed.
    #[inline]
    pub fn is_complete(&self) -> bool {
        self.pending_chunks() == 0
    }

    /// Atomically claims the next chunk to process.
    ///
    /// Returns `Some(chunk_index)` if there is work remaining, or `None`
    /// if all chunks have already been claimed.
    #[inline]
    pub fn claim_next_chunk(&self) -> Option<u32> {
        let idx = self.next_chunk.fetch_add(1, Ordering::AcqRel);
        if idx < self.num_chunks {
            Some(idx)
        } else {
            None
        }
    }

    /// Records that one chunk has been fully split.
    ///
    /// Returns the number of chunks still pending **after** this decrement.
    #[inline]
    pub fn complete_chunk(&self) -> u32 {
        let prev = self.num_pending_chunks.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(prev > 0, "complete_chunk called more times than num_chunks");
        prev - 1
    }
}

impl core::fmt::Debug for GrowState {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("GrowState")
            .field("old_version", &self.old_version)
            .field("new_version", &self.new_version)
            .field("num_chunks", &self.num_chunks)
            .field("pending_chunks", &self.pending_chunks())
            .field(
                "next_chunk",
                &self.next_chunk.load(Ordering::Relaxed),
            )
            .finish()
    }
}

// ---------------------------------------------------------------------------
// bucket_index_for_version
// ---------------------------------------------------------------------------

/// Computes the bucket index for a key hash in a table of `table_size` buckets.
///
/// This is equivalent to [`KeyHash::index`](crate::hash::KeyHash::index) but
/// works on the raw `u64` hash value and accepts `u64` table size directly.
/// It is used during grow to determine whether a key maps to the **left**
/// (same index) or **right** (index + old_size) bucket in the doubled table.
///
/// # Split semantics
///
/// When growing from size `N` to `2N`:
///
/// ```text
///   old index = hash & (N - 1)
///   new index = hash & (2N - 1)
///
///   if new_index < N  ->  entry stays at bucket[old_index]       ("left")
///   if new_index >= N ->  entry moves to bucket[old_index + N]   ("right")
/// ```
///
/// The single new bit (bit position log2(N)) in the hash determines the
/// split direction.
///
/// # Panics
///
/// Debug-asserts that `table_size` is a non-zero power of two.
///
/// # Examples
///
/// ```
/// use faster_core::grow::bucket_index_for_version;
///
/// let hash = 0x0000_0000_0000_03FF_u64; // low 10 bits all set
/// assert_eq!(bucket_index_for_version(hash, 1024), 0x3FF); // 1023
/// assert_eq!(bucket_index_for_version(hash, 512), 0x1FF);  // 511
/// ```
#[inline]
pub fn bucket_index_for_version(hash: u64, table_size: u64) -> u64 {
    debug_assert!(
        table_size > 0 && table_size.is_power_of_two(),
        "table_size must be a non-zero power of two, got {table_size}"
    );
    hash & (table_size - 1)
}

/// Determines whether a key hash splits to the **right** bucket during grow.
///
/// When growing from `old_size` to `2 * old_size`, returns `true` if the
/// hash maps to `bucket[idx + old_size]` in the new table (i.e. the bit at
/// position `log2(old_size)` in the hash is set).
///
/// # Panics
///
/// Debug-asserts that `old_size` is a non-zero power of two.
///
/// # Examples
///
/// ```
/// use faster_core::grow::splits_right;
///
/// let old_size = 1024u64;
/// // Hash with bit 10 clear -> stays left
/// assert!(!splits_right(0x0000_0000_0000_00FF, old_size));
/// // Hash with bit 10 set -> moves right
/// assert!(splits_right(0x0000_0000_0000_04FF, old_size));
/// ```
#[inline]
pub fn splits_right(hash: u64, old_size: u64) -> bool {
    debug_assert!(
        old_size > 0 && old_size.is_power_of_two(),
        "old_size must be a non-zero power of two, got {old_size}"
    );
    (hash & old_size) != 0
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grow_state_basic_construction() {
        let state = GrowState::new(0, 1, 65_536);
        assert_eq!(state.old_version(), 0);
        assert_eq!(state.new_version(), 1);
        assert_eq!(state.num_chunks(), 4);
        assert_eq!(state.pending_chunks(), 4);
        assert!(!state.is_complete());
    }

    #[test]
    fn grow_state_single_chunk() {
        let state = GrowState::new(1, 0, 1024);
        assert_eq!(state.num_chunks(), 1);
        assert_eq!(state.pending_chunks(), 1);
    }

    #[test]
    fn grow_state_exact_multiple() {
        let state = GrowState::new(0, 1, CHUNK_SIZE as u64 * 8);
        assert_eq!(state.num_chunks(), 8);
    }

    #[test]
    fn grow_state_non_multiple_rounds_up() {
        let state = GrowState::new(0, 1, CHUNK_SIZE as u64 + 1);
        assert_eq!(state.num_chunks(), 2);
    }

    #[test]
    #[should_panic(expected = "old_version and new_version must differ")]
    fn grow_state_same_version_panics() {
        let _ = GrowState::new(0, 0, 1024);
    }

    #[test]
    fn claim_all_chunks() {
        let state = GrowState::new(0, 1, CHUNK_SIZE as u64 * 3);
        assert_eq!(state.num_chunks(), 3);
        assert_eq!(state.claim_next_chunk(), Some(0));
        assert_eq!(state.claim_next_chunk(), Some(1));
        assert_eq!(state.claim_next_chunk(), Some(2));
        assert_eq!(state.claim_next_chunk(), None);
        assert_eq!(state.claim_next_chunk(), None);
    }

    #[test]
    fn complete_all_chunks() {
        let state = GrowState::new(0, 1, CHUNK_SIZE as u64 * 2);
        assert_eq!(state.pending_chunks(), 2);
        assert!(!state.is_complete());
        assert_eq!(state.complete_chunk(), 1);
        assert!(!state.is_complete());
        assert_eq!(state.complete_chunk(), 0);
        assert!(state.is_complete());
    }

    #[test]
    fn debug_formatting() {
        let state = GrowState::new(0, 1, CHUNK_SIZE as u64);
        let dbg = format!("{state:?}");
        assert!(dbg.contains("GrowState"));
        assert!(dbg.contains("old_version: 0"));
        assert!(dbg.contains("new_version: 1"));
    }

    #[test]
    fn bucket_index_basic() {
        assert_eq!(bucket_index_for_version(0xFF, 256), 0xFF);
        assert_eq!(bucket_index_for_version(0x1FF, 256), 0xFF);
        assert_eq!(bucket_index_for_version(0x100, 256), 0);
    }

    #[test]
    fn bucket_index_grow_semantics() {
        let old_size: u64 = 1024;
        let new_size: u64 = 2048;

        let hash_left: u64 = 0x0000_0000_0000_01AB;
        let old_idx = bucket_index_for_version(hash_left, old_size);
        let new_idx = bucket_index_for_version(hash_left, new_size);
        assert_eq!(old_idx, new_idx);
        assert!(new_idx < old_size);

        let hash_right: u64 = 0x0000_0000_0000_05AB;
        let old_idx_r = bucket_index_for_version(hash_right, old_size);
        let new_idx_r = bucket_index_for_version(hash_right, new_size);
        assert_eq!(new_idx_r, old_idx_r + old_size);
        assert!(new_idx_r >= old_size);
    }

    #[test]
    fn bucket_index_power_of_two_sizes() {
        let hash: u64 = 0xDEAD_BEEF_1234_5678;
        for log2 in 1..=20 {
            let size = 1u64 << log2;
            let idx = bucket_index_for_version(hash, size);
            assert!(idx < size);
        }
    }

    #[test]
    fn splits_right_basic() {
        let old_size = 1024u64;
        assert!(!splits_right(0x0000_0000_0000_00FF, old_size));
        assert!(!splits_right(0x0000_0000_0000_01FF, old_size));
        assert!(splits_right(0x0000_0000_0000_04FF, old_size));
        assert!(splits_right(0x0000_0000_0000_0400, old_size));
    }

    #[test]
    fn splits_right_various_sizes() {
        for log2 in 1..=20u32 {
            let old_size = 1u64 << log2;
            let hash_left = old_size - 1;
            assert!(!splits_right(hash_left, old_size));
            let hash_right = old_size | (old_size - 1);
            assert!(splits_right(hash_right, old_size));
        }
    }

    #[test]
    fn splits_right_consistent_with_bucket_index() {
        let old_size = 512u64;
        let new_size = 1024u64;
        for seed in 0..1000u64 {
            let hash = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15);
            let old_idx = bucket_index_for_version(hash, old_size);
            let new_idx = bucket_index_for_version(hash, new_size);
            if splits_right(hash, old_size) {
                assert_eq!(new_idx, old_idx + old_size);
            } else {
                assert_eq!(new_idx, old_idx);
            }
        }
    }
}
