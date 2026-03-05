//! Hash bucket types for the FASTER hash index.
//!
//! This module defines:
//!
//! - [`HashBucketEntry`] — the atomic 64-bit packed value stored in each slot
//!   of a hash bucket.
//! - [`AtomicHashBucketEntry`] — thread-safe atomic wrapper for lock-free
//!   concurrent access to a single entry.
//! - [`HashBucket`] — the cache-line-sized (64-byte) container that holds
//!   7 entries plus an overflow pointer, forming the core of the hash index.
//!
//! # Bit layout (matches C++ `hash_bucket.h` `HotLogIndexBucketEntryDef`)
//!
//! ```text
//!   63      62      61..48            47..0
//! ┌───────┬───────┬────────────────┬─────────────────────────────┐
//! │ Tent  │ Rsvd  │   Tag (14)     │   Address (48)              │
//! │  (1)  │  (1)  │                │                             │
//! └───────┴───────┴────────────────┴─────────────────────────────┘
//!    │       │          │                       │
//!    │       │          │                       └─ LogicalAddress in hybrid log
//!    │       │          └─ 14-bit hash fingerprint for fast rejection
//!    │       └─ Reserved (read-cache bit in v2)
//!    └─ Tentative: entry is being inserted, not yet committed
//! ```
//!
//! - **Address (48 bits):** A [`LogicalAddress`] identifying a record in the
//!   hybrid log. Encodes page (23 bits) + offset (25 bits).
//! - **Tag (14 bits):** A hash fingerprint stored alongside the address.
//!   During lookup, the tag is compared first — if it doesn't match, the
//!   entry is skipped without chasing the address into the log. This avoids
//!   cache misses on most non-matching entries (1-in-16384 false positive rate).
//! - **Reserved (1 bit, bit 62):** Reserved for future use (read-cache bit in v2).
//! - **Tentative (1 bit, bit 63):** Indicates that a concurrent insert is
//!   in progress. The two-phase insert protocol: (1) CAS entry with tentative
//!   set, (2) write record to log, (3) CAS entry without tentative. Lookups
//!   skip tentative entries so partially-inserted records are never visible.
//!
//! # C++ correspondence
//!
//! | Rust                       | C++ (`hash_bucket.h`)                    |
//! |----------------------------|------------------------------------------|
//! | `HashBucketEntry`          | `HotLogIndexBucketEntryDef` + `HashBucketEntry` |
//! | `AtomicHashBucketEntry`    | `AtomicHashBucketEntry`                  |
//! | `HashBucketEntry::EMPTY`   | `HashBucketEntry::kInvalidEntry`         |
//! | `TAG_BITS = 14`            | `HotLogIndexBucketEntryDef::kTagBits = 14` |
//!
//! # Performance
//!
//! This is THE hot path for concurrent hash lookups. Every hash table probe
//! loads an `AtomicHashBucketEntry`, extracts the tag, and compares it against
//! the search tag — all before touching any log memory. Bit manipulation here
//! is branchless by design: shift + mask operations compile to single
//! instructions on x86-64 and AArch64.

use core::fmt;
use core::sync::atomic::{AtomicU64, Ordering};

use crate::address::{AtomicLogicalAddress, LogicalAddress};

// ---------------------------------------------------------------------------
// Constants — match C++ HotLogIndexBucketEntryDef exactly
// ---------------------------------------------------------------------------

/// Number of bits used for the tag field.
/// Matches `HotLogIndexBucketEntryDef::kTagBits = 14`.
const TAG_BITS: u32 = 14;

/// Number of bits used for the address field.
const ADDRESS_BITS: u32 = 48;

/// Bit position where the tag field starts (immediately above the address).
const TAG_SHIFT: u32 = ADDRESS_BITS; // 48

/// Bitmask for extracting the 48-bit address (bits [47:0]).
const ADDRESS_MASK: u64 = (1u64 << ADDRESS_BITS) - 1;

/// Bitmask for extracting the 14-bit tag (bits [61:48]).
const TAG_MASK: u64 = ((1u64 << TAG_BITS) - 1) << TAG_SHIFT;

/// Maximum tag value: 2^14 - 1 = 16383.
const MAX_TAG: u16 = (1u16 << TAG_BITS) - 1;

/// The tentative bit (bit 63). Set during two-phase concurrent insert.
///
/// C++ layout: `uint64_t tentative : 1` is the MSB of the 64-bit word
/// (after address:48, tag:14, reserved:1, tentative:1).
const TENTATIVE_BIT: u64 = 1u64 << 63;

/// Reserved bit (bit 62). Unused in hot-log index; reserved for read-cache
/// support in v2. Matches C++ `reserved : kReservedBits` where
/// `kReservedBits = 64 - 48 - 14 - 1 = 1`.
#[allow(dead_code)]
const RESERVED_BIT: u64 = 1u64 << 62;

// ---------------------------------------------------------------------------
// HashBucketEntry
// ---------------------------------------------------------------------------

/// A 64-bit packed hash bucket entry.
///
/// Each slot in a [`HashBucket`] stores one of these entries. The entry packs
/// a 48-bit [`LogicalAddress`], a 14-bit tag (hash fingerprint), a reserved
/// bit, and a tentative bit into a single `u64`, enabling atomic CAS-based
/// concurrent access with zero locking.
///
/// # Bit layout
///
/// ```text
///   63      62      61..48            47..0
/// ┌───────┬───────┬────────────────┬─────────────────────────────┐
/// │ Tent  │ Rsvd  │   Tag (14)     │   Address (48)              │
/// └───────┴───────┴────────────────┴─────────────────────────────┘
/// ```
///
/// # Size guarantee
///
/// `HashBucketEntry` is `#[repr(transparent)]` over `u64` — exactly 8 bytes,
/// no padding, no indirection. This is essential for atomic CAS operations
/// and cache-line-aligned bucket layout.
///
/// # Examples
///
/// ```
/// use faster_core::address::{LogicalAddress, Page, Offset};
/// use faster_core::hash_bucket::HashBucketEntry;
///
/// let addr = LogicalAddress::new(Page(1), Offset(100));
/// let entry = HashBucketEntry::new(0x1234, addr, false);
///
/// assert_eq!(entry.tag(), 0x1234);
/// assert_eq!(entry.address(), addr);
/// assert!(!entry.is_tentative());
/// assert!(!entry.is_empty());
/// ```
#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(transparent)]
pub struct HashBucketEntry(u64);

impl HashBucketEntry {
    /// An empty entry — all zeros. This is the sentinel for "slot is free".
    ///
    /// Matches C++ `HashBucketEntry::kInvalidEntry = 0`.
    pub const EMPTY: Self = Self(0);

    /// Creates a new `HashBucketEntry` from a tag, address, and tentative flag.
    ///
    /// # Parameters
    ///
    /// - `tag`: 14-bit hash fingerprint (0..16383). Extracted from the key's
    ///   hash via [`KeyHash::tag()`](crate::hash::KeyHash::tag).
    /// - `address`: The [`LogicalAddress`] of the record in the hybrid log.
    /// - `tentative`: If `true`, marks the entry as being inserted (not yet
    ///   committed). Lookups should skip tentative entries.
    ///
    /// # Panics (debug only)
    ///
    /// Debug-asserts that `tag` fits in 14 bits and `address` fits in 48 bits.
    #[inline(always)]
    pub const fn new(tag: u16, address: LogicalAddress, tentative: bool) -> Self {
        debug_assert!(tag <= MAX_TAG, "tag exceeds 14 bits");
        debug_assert!(address.raw() <= ADDRESS_MASK, "address exceeds 48 bits");
        // Mask inputs to valid ranges for defense-in-depth in release builds.
        let masked_tag = (tag & MAX_TAG) as u64;
        let masked_addr = address.raw() & ADDRESS_MASK;
        let mut bits = masked_addr;
        bits |= masked_tag << TAG_SHIFT;
        if tentative {
            bits |= TENTATIVE_BIT;
        }
        Self(bits)
    }

    /// Creates a `HashBucketEntry` from a raw `u64` value.
    ///
    /// No validation is performed. Use when loading from an atomic or
    /// deserializing from a known-good source.
    #[inline(always)]
    pub const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    /// Returns the raw `u64` representation.
    #[inline(always)]
    pub const fn raw(self) -> u64 {
        self.0
    }

    /// Extracts the 48-bit [`LogicalAddress`] (bits [47:0]).
    ///
    /// This is a branchless shift+mask operation — a single `AND` instruction
    /// on x86-64.
    #[inline(always)]
    pub const fn address(self) -> LogicalAddress {
        LogicalAddress::from_raw(self.0 & ADDRESS_MASK)
    }

    /// Extracts the 14-bit tag (bits [61:48]).
    ///
    /// The tag is a hash fingerprint used for fast-reject filtering. During
    /// lookup, compare this against [`KeyHash::tag()`](crate::hash::KeyHash::tag)
    /// before chasing the address into the log.
    ///
    /// Returns a value in `0..=16383`.
    #[inline(always)]
    pub const fn tag(self) -> u16 {
        ((self.0 & TAG_MASK) >> TAG_SHIFT) as u16
    }

    /// Returns `true` if the tentative bit (bit 63) is set.
    ///
    /// A tentative entry is one that is being inserted concurrently but has
    /// not yet been committed. Lookups should skip tentative entries to avoid
    /// seeing partially-inserted records.
    #[inline(always)]
    pub const fn is_tentative(self) -> bool {
        (self.0 & TENTATIVE_BIT) != 0
    }

    /// Returns `true` if this entry is empty (all zeros).
    ///
    /// Empty entries are the sentinel for "slot is free" in the hash bucket.
    /// An empty entry has address=0, tag=0, tentative=false.
    #[inline(always)]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Returns a copy of this entry with the tentative bit set.
    ///
    /// Used in phase 1 of the two-phase insert protocol: CAS an entry with
    /// tentative set to reserve the slot before writing the record to the log.
    #[inline(always)]
    pub const fn with_tentative(self) -> Self {
        Self(self.0 | TENTATIVE_BIT)
    }

    /// Returns a copy of this entry with the tentative bit cleared.
    ///
    /// Used in phase 3 of the two-phase insert protocol: after the record
    /// has been written to the log, CAS the entry without tentative to make
    /// it visible to lookups.
    #[inline(always)]
    pub const fn without_tentative(self) -> Self {
        Self(self.0 & !TENTATIVE_BIT)
    }
}

impl Default for HashBucketEntry {
    /// Defaults to [`HashBucketEntry::EMPTY`].
    fn default() -> Self {
        Self::EMPTY
    }
}

impl fmt::Debug for HashBucketEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Entry(addr=0x{:012x}, tag=0x{:04x}, tent={})",
            self.address().raw(),
            self.tag(),
            self.is_tentative(),
        )
    }
}

impl fmt::Display for HashBucketEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_empty() {
            write!(f, "Entry(empty)")
        } else {
            write!(
                f,
                "Entry(addr={}, tag=0x{:04x}{})",
                self.address(),
                self.tag(),
                if self.is_tentative() {
                    ", tentative"
                } else {
                    ""
                },
            )
        }
    }
}

// ---------------------------------------------------------------------------
// AtomicHashBucketEntry
// ---------------------------------------------------------------------------

/// Thread-safe atomic wrapper around [`HashBucketEntry`].
///
/// This is the type actually stored in hash bucket arrays. Every concurrent
/// hash lookup or insert goes through `AtomicHashBucketEntry` — this is the
/// hottest path in the entire FASTER system.
///
/// # Memory ordering rationale
///
/// The correct memory ordering depends on the operation:
///
/// - **Lookup (read-only):** `Acquire` on `load()`. We need to see all writes
///   that happened-before the entry was stored (the record content in the log).
///   Without `Acquire`, we could read the entry's address but see stale log data.
///
/// - **Insert/update (read-modify-write):** `AcqRel` on `compare_exchange()`.
///   `Acquire` ensures we see prior writes to the slot. `Release` ensures our
///   newly-written record content is visible to threads that subsequently load
///   this entry with `Acquire`.
///
/// - **Initialization (single-threaded):** `Relaxed` is sufficient when filling
///   a freshly-allocated bucket before publishing it.
///
/// # Size guarantee
///
/// `AtomicHashBucketEntry` is `#[repr(transparent)]` over `AtomicU64` —
/// exactly 8 bytes on all platforms where `AtomicU64` is lock-free.
///
/// # Examples
///
/// ```
/// use faster_core::address::{LogicalAddress, Page, Offset};
/// use faster_core::hash_bucket::{HashBucketEntry, AtomicHashBucketEntry};
/// use core::sync::atomic::Ordering;
///
/// let atomic = AtomicHashBucketEntry::new(HashBucketEntry::EMPTY);
///
/// let addr = LogicalAddress::new(Page(1), Offset(100));
/// let entry = HashBucketEntry::new(0x1234, addr, false);
///
/// atomic.store(entry, Ordering::Release);
/// assert_eq!(atomic.load(Ordering::Acquire), entry);
/// ```
#[repr(transparent)]
pub struct AtomicHashBucketEntry(AtomicU64);

impl AtomicHashBucketEntry {
    /// Creates a new `AtomicHashBucketEntry` with the given initial value.
    pub const fn new(entry: HashBucketEntry) -> Self {
        Self(AtomicU64::new(entry.0))
    }

    /// Atomically loads the entry.
    ///
    /// # Ordering
    ///
    /// Use `Ordering::Acquire` for hash lookups — this ensures that the
    /// record content pointed to by the entry's address is visible after
    /// the load. Use `Ordering::Relaxed` only during single-threaded
    /// initialization or when the value is not yet published.
    #[inline(always)]
    pub fn load(&self, order: Ordering) -> HashBucketEntry {
        HashBucketEntry(self.0.load(order))
    }

    /// Atomically stores an entry.
    ///
    /// # Ordering
    ///
    /// Use `Ordering::Release` after writing record content to the log —
    /// this ensures the record is visible to threads that subsequently
    /// `load()` with `Acquire`. Use `Ordering::Relaxed` only during
    /// single-threaded initialization.
    #[inline(always)]
    pub fn store(&self, entry: HashBucketEntry, order: Ordering) {
        self.0.store(entry.0, order);
    }

    /// Atomic compare-and-exchange (strong).
    ///
    /// If the current value equals `current`, stores `new_val` and returns
    /// `Ok(current)`. Otherwise returns `Err(actual)` with the value that
    /// was actually found.
    ///
    /// # Ordering
    ///
    /// For hash index insert/update operations:
    /// - `success`: `Ordering::AcqRel` — acquire to see prior writes to
    ///   this slot, release to publish our new entry to subsequent readers.
    /// - `failure`: `Ordering::Acquire` — we still need to see the actual
    ///   current value so we can retry with correct expected value.
    ///
    /// # Examples
    ///
    /// ```
    /// use faster_core::address::{LogicalAddress, Page, Offset};
    /// use faster_core::hash_bucket::{HashBucketEntry, AtomicHashBucketEntry};
    /// use core::sync::atomic::Ordering;
    ///
    /// let atomic = AtomicHashBucketEntry::new(HashBucketEntry::EMPTY);
    /// let addr = LogicalAddress::new(Page(1), Offset(42));
    /// let new_entry = HashBucketEntry::new(0x0ABC, addr, false);
    ///
    /// let result = atomic.compare_exchange(
    ///     HashBucketEntry::EMPTY,
    ///     new_entry,
    ///     Ordering::AcqRel,
    ///     Ordering::Acquire,
    /// );
    /// assert_eq!(result, Ok(HashBucketEntry::EMPTY));
    /// assert_eq!(atomic.load(Ordering::Acquire), new_entry);
    /// ```
    #[inline(always)]
    pub fn compare_exchange(
        &self,
        current: HashBucketEntry,
        new_val: HashBucketEntry,
        success: Ordering,
        failure: Ordering,
    ) -> Result<HashBucketEntry, HashBucketEntry> {
        self.0
            .compare_exchange(current.0, new_val.0, success, failure)
            .map(HashBucketEntry)
            .map_err(HashBucketEntry)
    }

    /// Weak compare-and-exchange — may spuriously fail on some platforms
    /// (notably ARM), but is more efficient in CAS retry loops.
    ///
    /// Prefer this over [`compare_exchange`](Self::compare_exchange) when
    /// the CAS is already inside a loop, as the compiler can emit `LDXR/STXR`
    /// on ARM without the loop overhead of the strong variant.
    #[inline(always)]
    pub fn compare_exchange_weak(
        &self,
        current: HashBucketEntry,
        new_val: HashBucketEntry,
        success: Ordering,
        failure: Ordering,
    ) -> Result<HashBucketEntry, HashBucketEntry> {
        self.0
            .compare_exchange_weak(current.0, new_val.0, success, failure)
            .map(HashBucketEntry)
            .map_err(HashBucketEntry)
    }
}

impl Default for AtomicHashBucketEntry {
    /// Defaults to [`HashBucketEntry::EMPTY`].
    fn default() -> Self {
        Self::new(HashBucketEntry::EMPTY)
    }
}

impl fmt::Debug for AtomicHashBucketEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Relaxed is fine for debug printing — no synchronization intent.
        let entry = self.load(Ordering::Relaxed);
        write!(f, "Atomic{entry:?}")
    }
}

// ---------------------------------------------------------------------------
// HashBucket — 64-byte cache-line-aligned bucket
// ---------------------------------------------------------------------------

/// Number of data entry slots per bucket.
///
/// FASTER uses 7 entries per bucket: a 64-byte cache line holds 8 × 8-byte
/// slots, and the 8th slot is reserved for the overflow chain pointer.
/// This gives excellent scan locality — the entire bucket fits in a single
/// cache line fetch.
pub const BUCKET_NUM_ENTRIES: usize = 7;

/// A 64-byte (one cache line) hash bucket.
///
/// Each bucket holds [`BUCKET_NUM_ENTRIES`] (7) atomic hash bucket entries
/// plus an overflow pointer that chains to additional buckets when all 7
/// slots are occupied. This is the fundamental building block of FASTER's
/// latch-free hash index.
///
/// # Memory layout
///
/// ```text
///   Byte offset:  0        8       16       24       32       40       48       56       64
///                 ┌────────┬────────┬────────┬────────┬────────┬────────┬────────┬────────┐
///                 │Entry[0]│Entry[1]│Entry[2]│Entry[3]│Entry[4]│Entry[5]│Entry[6]│Overflow│
///                 │  8 B   │  8 B   │  8 B   │  8 B   │  8 B   │  8 B   │  8 B   │  8 B   │
///                 └────────┴────────┴────────┴────────┴────────┴────────┴────────┴────────┘
///                 │◄───────────── 7 × AtomicHashBucketEntry ───────────►│  AtomicLogical  │
///                 │                   (56 bytes)                        │   Address (8B)   │
///                 │◄────────────────── exactly 64 bytes ──────────────────────────────────►│
/// ```
///
/// # Cache-line alignment
///
/// `#[repr(C, align(64))]` ensures every `HashBucket` starts on a 64-byte
/// boundary. This is critical: a bucket scan loads one cache line, and if
/// the bucket straddled two cache lines the penalty would be a second fetch
/// on every probe.
///
/// In arrays (`[HashBucket; N]`), every element is also cache-line aligned
/// because the struct size (64) is a multiple of its alignment (64).
///
/// # Concurrency
///
/// All access to entries and the overflow pointer goes through atomic
/// operations. Multiple threads may scan, insert, and CAS concurrently
/// without any locking. The typical lookup is a linear scan of 7 entries
/// comparing tags — entirely within one cache line.
///
/// # C++ correspondence
///
/// | Rust            | C++ (`hash_bucket.h`)      |
/// |-----------------|----------------------------|
/// | `HashBucket`    | `HashBucket<HID>`          |
/// | `BUCKET_NUM_ENTRIES` | `HashBucket::kNumEntries` (7) |
/// | `overflow_address()` | `overflow_entry` (8th slot) |
#[repr(C, align(64))]
pub struct HashBucket {
    entries: [AtomicHashBucketEntry; BUCKET_NUM_ENTRIES],
    overflow_address: AtomicLogicalAddress,
}

// Compile-time layout assertions.
const _: () = assert!(core::mem::size_of::<HashBucket>() == 64);
const _: () = assert!(core::mem::align_of::<HashBucket>() == 64);

impl HashBucket {
    /// Creates a new empty bucket: all 7 entries are [`HashBucketEntry::EMPTY`]
    /// and the overflow address is [`LogicalAddress::ZERO`].
    pub fn new() -> Self {
        Self {
            entries: [
                AtomicHashBucketEntry::new(HashBucketEntry::EMPTY),
                AtomicHashBucketEntry::new(HashBucketEntry::EMPTY),
                AtomicHashBucketEntry::new(HashBucketEntry::EMPTY),
                AtomicHashBucketEntry::new(HashBucketEntry::EMPTY),
                AtomicHashBucketEntry::new(HashBucketEntry::EMPTY),
                AtomicHashBucketEntry::new(HashBucketEntry::EMPTY),
                AtomicHashBucketEntry::new(HashBucketEntry::EMPTY),
            ],
            overflow_address: AtomicLogicalAddress::new(LogicalAddress::ZERO),
        }
    }

    /// Returns a reference to the entry at the given `index` (0..7).
    ///
    /// # Panics
    ///
    /// Panics if `index >= BUCKET_NUM_ENTRIES` (7).
    #[inline]
    pub fn entry(&self, index: usize) -> &AtomicHashBucketEntry {
        &self.entries[index]
    }

    /// Returns a reference to the overflow address slot.
    ///
    /// The overflow address chains to an additional [`HashBucket`] when all
    /// 7 data slots are full. A value of [`LogicalAddress::ZERO`] means no
    /// overflow bucket exists.
    ///
    /// # Memory ordering
    ///
    /// - Load with `Acquire` to see a fully-initialized overflow bucket.
    /// - Store with `Release` after the overflow bucket has been initialized.
    #[inline]
    pub fn overflow_address(&self) -> &AtomicLogicalAddress {
        &self.overflow_address
    }

    /// Scans all 7 entries for one matching the given `tag`.
    ///
    /// Returns `Some((index, entry))` for the first entry where:
    /// - `entry.tag() == tag`
    /// - `!entry.is_tentative()` (skip entries being concurrently inserted)
    /// - `!entry.is_empty()` (skip free slots)
    ///
    /// Returns `None` if no matching entry is found in this bucket.
    /// The caller must follow the overflow chain to check additional buckets.
    ///
    /// # Memory ordering
    ///
    /// Each entry is loaded with `Acquire` ordering. If we find a match,
    /// the caller can safely dereference the entry's address — the record
    /// content was published with `Release` when the entry was stored.
    #[inline]
    pub fn find_entry(&self, tag: u16) -> Option<(usize, HashBucketEntry)> {
        for i in 0..BUCKET_NUM_ENTRIES {
            let entry = self.entries[i].load(Ordering::Acquire);
            if !entry.is_empty() && !entry.is_tentative() && entry.tag() == tag {
                return Some((i, entry));
            }
        }
        None
    }

    /// Finds the first empty slot in this bucket.
    ///
    /// Returns `Some(index)` for the first slot where `is_empty()` is true,
    /// or `None` if all 7 slots are occupied.
    ///
    /// # Memory ordering
    ///
    /// Each entry is loaded with `Acquire` ordering. Although `Relaxed`
    /// would suffice for the emptiness check alone, using `Acquire` avoids
    /// reloading with a stronger ordering if the caller intends to CAS.
    #[inline]
    pub fn find_empty(&self) -> Option<usize> {
        for i in 0..BUCKET_NUM_ENTRIES {
            let entry = self.entries[i].load(Ordering::Acquire);
            if entry.is_empty() {
                return Some(i);
            }
        }
        None
    }

    /// Attempts to insert an entry with the given `tag` and `address` into
    /// the first empty slot using a compare-and-swap.
    ///
    /// On success, returns `Ok(index)` with the slot index where the entry
    /// was placed. On failure, returns `Err(())` — either because the bucket
    /// is full, or because a concurrent CAS beat us to the empty slot.
    ///
    /// The entry is stored **without** the tentative bit. For the full
    /// two-phase insert protocol (tentative → commit), use [`entry()`] and
    /// [`AtomicHashBucketEntry::compare_exchange()`] directly.
    ///
    /// # Memory ordering
    ///
    /// CAS uses `AcqRel` / `Acquire`: `Release` publishes the new entry
    /// to concurrent readers; `Acquire` on both success and failure ensures
    /// we see the current slot state.
    #[inline]
    #[allow(clippy::result_unit_err)]
    pub fn try_insert(&self, tag: u16, address: LogicalAddress) -> Result<usize, ()> {
        let new_entry = HashBucketEntry::new(tag, address, false);
        for i in 0..BUCKET_NUM_ENTRIES {
            let current = self.entries[i].load(Ordering::Acquire);
            if current.is_empty() {
                match self.entries[i].compare_exchange(
                    HashBucketEntry::EMPTY,
                    new_entry,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(_) => return Ok(i),
                    Err(_) => {
                        // Slot was taken by another thread — try next slot.
                        continue;
                    }
                }
            }
        }
        Err(())
    }

    // -----------------------------------------------------------------------
    // Overflow chain operations
    // -----------------------------------------------------------------------

    /// Returns the overflow bucket, following the overflow pointer.
    ///
    /// If the overflow pointer is [`LogicalAddress::ZERO`], returns `None`.
    /// Otherwise, resolves the address through the pool and returns the bucket.
    ///
    /// # Memory ordering
    ///
    /// The overflow pointer is loaded with `Acquire`. This ensures we see the
    /// fully-initialized overflow bucket that was published with `Release`
    /// when the overflow pointer was CAS'd.
    #[inline]
    pub fn overflow_bucket<'a>(
        &self,
        pool: &'a crate::overflow::OverflowBucketPool,
    ) -> Option<&'a HashBucket> {
        let addr = self.overflow_address.load(Ordering::Acquire);
        if addr == LogicalAddress::ZERO {
            None
        } else {
            Some(pool.get(addr))
        }
    }

    /// Returns the overflow bucket, allocating a new one if none exists.
    ///
    /// This is the core concurrent overflow operation:
    /// 1. Load overflow pointer with `Acquire`.
    /// 2. If non-null, return that bucket (fast path).
    /// 3. If null, allocate a new zeroed bucket from the pool.
    /// 4. CAS the overflow pointer from ZERO to the new bucket's address.
    /// 5. If CAS succeeds, return our new bucket.
    /// 6. If CAS fails (another thread won the race), free our allocation
    ///    and return the winner's bucket.
    ///
    /// # Memory ordering
    ///
    /// - CAS uses `AcqRel`/`Acquire`: `Release` publishes the fully-initialized
    ///   bucket to threads that subsequently load with `Acquire`.
    /// - The losing thread's allocation is freed immediately — no leak.
    ///
    /// # Lock-free guarantee
    ///
    /// This operation is lock-free: exactly one thread's CAS will succeed,
    /// and losers make forward progress (they use the winner's bucket).
    pub fn get_or_create_overflow<'a>(
        &self,
        pool: &'a crate::overflow::OverflowBucketPool,
    ) -> &'a HashBucket {
        // Fast path: overflow already exists.
        let existing = self.overflow_address.load(Ordering::Acquire);
        if existing != LogicalAddress::ZERO {
            return pool.get(existing);
        }

        // Slow path: allocate and race to install.
        let new_addr = pool.allocate();

        // CAS: ZERO → new_addr. Only one thread wins.
        match self.overflow_address.compare_exchange(
            LogicalAddress::ZERO,
            new_addr,
            // Release on success: the zeroed bucket is now visible to
            // any thread that loads this pointer with Acquire.
            Ordering::AcqRel,
            // Acquire on failure: we need to see the winner's bucket.
            Ordering::Acquire,
        ) {
            Ok(_) => {
                // We won — our bucket is installed.
                pool.get(new_addr)
            }
            Err(winner_addr) => {
                // Another thread beat us. Free our allocation, use theirs.
                pool.free(new_addr);
                pool.get(winner_addr)
            }
        }
    }

    /// Walks the bucket chain (primary → overflow → overflow → ...),
    /// calling `f` on each bucket.
    ///
    /// If `f` returns `ControlFlow::Break(value)`, traversal stops and
    /// `Some(value)` is returned. If the chain is exhausted without
    /// breaking, returns `None`.
    ///
    /// # Examples
    ///
    /// ```
    /// use faster_core::hash_bucket::HashBucket;
    /// use faster_core::overflow::OverflowBucketPool;
    /// use core::ops::ControlFlow;
    ///
    /// let bucket = HashBucket::new();
    /// let pool = OverflowBucketPool::new();
    ///
    /// // Count entries across chain (just the primary bucket here).
    /// let mut count = 0u32;
    /// bucket.for_each_bucket(&pool, |_b| -> ControlFlow<()> {
    ///     count += 1;
    ///     ControlFlow::Continue(())
    /// });
    /// assert_eq!(count, 1);
    /// ```
    #[inline]
    pub fn for_each_bucket<T, F>(
        &self,
        pool: &crate::overflow::OverflowBucketPool,
        mut f: F,
    ) -> Option<T>
    where
        F: FnMut(&HashBucket) -> core::ops::ControlFlow<T>,
    {
        // Start with `self` (the primary bucket).
        let mut current: &HashBucket = self;
        loop {
            match f(current) {
                core::ops::ControlFlow::Break(val) => return Some(val),
                core::ops::ControlFlow::Continue(()) => {}
            }
            // Follow overflow pointer.
            let overflow_addr = current.overflow_address.load(Ordering::Acquire);
            if overflow_addr == LogicalAddress::ZERO {
                return None;
            }
            current = pool.get(overflow_addr);
        }
    }

    /// Searches the bucket and all overflow buckets for an entry with
    /// matching `tag`.
    ///
    /// Returns `Some((bucket_addr, slot_index, entry))` where:
    /// - `bucket_addr`: [`LogicalAddress::ZERO`] for the primary bucket,
    ///   or the overflow bucket's address in the pool.
    /// - `slot_index`: The entry's index within its bucket (0..7).
    /// - `entry`: The [`HashBucketEntry`] value found.
    ///
    /// Returns `None` if no matching non-tentative entry is found in the
    /// entire chain.
    ///
    /// # Memory ordering
    ///
    /// Each entry is loaded with `Acquire` (via [`find_entry`](Self::find_entry)).
    /// Overflow pointers are loaded with `Acquire`.
    #[inline]
    pub fn find_entry_in_chain(
        &self,
        tag: u16,
        pool: &crate::overflow::OverflowBucketPool,
    ) -> Option<(LogicalAddress, usize, HashBucketEntry)> {
        // Check primary bucket first.
        if let Some((idx, entry)) = self.find_entry(tag) {
            return Some((LogicalAddress::ZERO, idx, entry));
        }

        // Walk overflow chain.
        let mut overflow_addr = self.overflow_address.load(Ordering::Acquire);
        while overflow_addr != LogicalAddress::ZERO {
            let bucket = pool.get(overflow_addr);
            if let Some((idx, entry)) = bucket.find_entry(tag) {
                return Some((overflow_addr, idx, entry));
            }
            overflow_addr = bucket.overflow_address.load(Ordering::Acquire);
        }
        None
    }

    /// Inserts an entry into the bucket chain, allocating overflow buckets
    /// as needed.
    ///
    /// The algorithm:
    /// 1. Try inserting into the primary bucket (scan for empty slot + CAS).
    /// 2. If full, follow the overflow chain, trying each bucket.
    /// 3. If the entire chain is full, allocate a new overflow bucket at
    ///    the tail and insert there.
    ///
    /// Returns `Ok(())` on success. This operation is lock-free: all
    /// concurrent insertions make forward progress.
    ///
    /// # Memory ordering
    ///
    /// - Entry CAS: `AcqRel`/`Acquire` (via [`try_insert`](Self::try_insert)).
    /// - Overflow CAS: `AcqRel`/`Acquire` (via [`get_or_create_overflow`]).
    ///
    /// # Lock-free correctness argument
    ///
    /// Each thread either successfully CAS's into an empty slot (progress) or
    /// loses the CAS (another thread made progress). In the worst case, a
    /// thread walks the entire chain and extends it, but the chain length is
    /// bounded by the total number of concurrent insert operations.
    #[inline]
    #[allow(clippy::result_unit_err)]
    pub fn insert_in_chain(
        &self,
        tag: u16,
        address: LogicalAddress,
        pool: &crate::overflow::OverflowBucketPool,
    ) -> Result<(), ()> {
        let mut current: &HashBucket = self;

        loop {
            // Try every slot in this bucket.
            if current.try_insert(tag, address).is_ok() {
                return Ok(());
            }

            // Walk to next overflow bucket if it exists.
            let overflow_addr = current.overflow_address.load(Ordering::Acquire);
            if overflow_addr != LogicalAddress::ZERO {
                current = pool.get(overflow_addr);
                continue;
            }

            // No overflow bucket — create one and try inserting there.
            // `get_or_create_overflow` handles the race where two threads
            // both try to create: one wins, the other reuses.
            let new_overflow = current.get_or_create_overflow(pool);
            if new_overflow.try_insert(tag, address).is_ok() {
                return Ok(());
            }
            // Extremely rare: the new overflow was immediately filled by
            // concurrent insertions. Continue iterating from it.
            current = new_overflow;
        }
    }
}

impl Default for HashBucket {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for HashBucket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let overflow = self.overflow_address.load(Ordering::Relaxed);
        write!(f, "HashBucket {{ entries: [")?;
        for i in 0..BUCKET_NUM_ENTRIES {
            if i > 0 {
                write!(f, ", ")?;
            }
            let entry = self.entries[i].load(Ordering::Relaxed);
            write!(f, "{entry:?}")?;
        }
        write!(f, "], overflow: {overflow:?} }}")
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::address::{LogicalAddress, Offset, Page};

    // -- Size and layout assertions --

    #[test]
    fn size_of_hash_bucket_entry() {
        assert_eq!(core::mem::size_of::<HashBucketEntry>(), 8);
    }

    #[test]
    fn size_of_atomic_hash_bucket_entry() {
        assert_eq!(core::mem::size_of::<AtomicHashBucketEntry>(), 8);
    }

    #[test]
    fn alignment_of_hash_bucket_entry() {
        assert_eq!(core::mem::align_of::<HashBucketEntry>(), 8);
    }

    // -- Empty / sentinel --

    #[test]
    fn empty_entry_is_all_zeros() {
        assert_eq!(HashBucketEntry::EMPTY.raw(), 0);
    }

    #[test]
    fn empty_entry_is_empty() {
        assert!(HashBucketEntry::EMPTY.is_empty());
    }

    #[test]
    fn empty_entry_has_zero_address() {
        assert_eq!(HashBucketEntry::EMPTY.address(), LogicalAddress::ZERO);
    }

    #[test]
    fn empty_entry_has_zero_tag() {
        assert_eq!(HashBucketEntry::EMPTY.tag(), 0);
    }

    #[test]
    fn empty_entry_is_not_tentative() {
        assert!(!HashBucketEntry::EMPTY.is_tentative());
    }

    #[test]
    fn default_is_empty() {
        assert_eq!(HashBucketEntry::default(), HashBucketEntry::EMPTY);
    }

    // -- Construction and field extraction --

    #[test]
    fn new_entry_round_trip() {
        let addr = LogicalAddress::new(Page(42), Offset(1024));
        let entry = HashBucketEntry::new(0x1234, addr, false);
        assert_eq!(entry.tag(), 0x1234);
        assert_eq!(entry.address(), addr);
        assert!(!entry.is_tentative());
        assert!(!entry.is_empty());
    }

    #[test]
    fn new_entry_with_tentative() {
        let addr = LogicalAddress::new(Page(1), Offset(100));
        let entry = HashBucketEntry::new(0x3FFF, addr, true);
        assert_eq!(entry.tag(), 0x3FFF);
        assert_eq!(entry.address(), addr);
        assert!(entry.is_tentative());
        assert!(!entry.is_empty());
    }

    #[test]
    fn new_entry_zero_tag_nonzero_address() {
        let addr = LogicalAddress::new(Page(0), Offset(2));
        let entry = HashBucketEntry::new(0, addr, false);
        assert_eq!(entry.tag(), 0);
        assert_eq!(entry.address(), addr);
        assert!(!entry.is_empty()); // address is nonzero
    }

    #[test]
    fn new_entry_nonzero_tag_zero_address() {
        let entry = HashBucketEntry::new(1, LogicalAddress::ZERO, false);
        assert_eq!(entry.tag(), 1);
        assert_eq!(entry.address(), LogicalAddress::ZERO);
        assert!(!entry.is_empty()); // tag makes raw != 0
    }

    // -- Tag boundary values --

    #[test]
    fn tag_zero() {
        let entry = HashBucketEntry::new(0, LogicalAddress::ZERO, false);
        assert_eq!(entry.tag(), 0);
    }

    #[test]
    fn tag_max() {
        let entry = HashBucketEntry::new(MAX_TAG, LogicalAddress::ZERO, false);
        assert_eq!(entry.tag(), MAX_TAG);
        assert_eq!(entry.tag(), 0x3FFF);
    }

    #[test]
    fn tag_one() {
        let entry = HashBucketEntry::new(1, LogicalAddress::ZERO, false);
        assert_eq!(entry.tag(), 1);
    }

    // -- Address boundary values --

    #[test]
    fn address_zero() {
        let entry = HashBucketEntry::new(0x1234, LogicalAddress::ZERO, false);
        assert_eq!(entry.address(), LogicalAddress::ZERO);
    }

    #[test]
    fn address_max() {
        let max_addr = LogicalAddress::MAX;
        let entry = HashBucketEntry::new(0x1234, max_addr, false);
        assert_eq!(entry.address(), max_addr);
    }

    #[test]
    fn address_one() {
        let addr = LogicalAddress::from_raw(1);
        let entry = HashBucketEntry::new(0, addr, false);
        assert_eq!(entry.address(), addr);
    }

    // -- Tentative bit independence --

    #[test]
    fn tentative_does_not_affect_tag() {
        let addr = LogicalAddress::new(Page(10), Offset(500));
        let normal = HashBucketEntry::new(0x2ABC, addr, false);
        let tentative = HashBucketEntry::new(0x2ABC, addr, true);
        assert_eq!(normal.tag(), tentative.tag());
    }

    #[test]
    fn tentative_does_not_affect_address() {
        let addr = LogicalAddress::new(Page(10), Offset(500));
        let normal = HashBucketEntry::new(0x2ABC, addr, false);
        let tentative = HashBucketEntry::new(0x2ABC, addr, true);
        assert_eq!(normal.address(), tentative.address());
    }

    #[test]
    fn with_tentative_sets_bit() {
        let entry = HashBucketEntry::new(0x1234, LogicalAddress::ZERO, false);
        assert!(!entry.is_tentative());
        let tent = entry.with_tentative();
        assert!(tent.is_tentative());
        // Tag and address unchanged
        assert_eq!(tent.tag(), entry.tag());
        assert_eq!(tent.address(), entry.address());
    }

    #[test]
    fn without_tentative_clears_bit() {
        let entry = HashBucketEntry::new(0x1234, LogicalAddress::ZERO, true);
        assert!(entry.is_tentative());
        let cleared = entry.without_tentative();
        assert!(!cleared.is_tentative());
        assert_eq!(cleared.tag(), entry.tag());
        assert_eq!(cleared.address(), entry.address());
    }

    #[test]
    fn with_tentative_is_idempotent() {
        let entry = HashBucketEntry::new(0x1234, LogicalAddress::ZERO, true);
        assert_eq!(entry.with_tentative(), entry);
    }

    #[test]
    fn without_tentative_is_idempotent() {
        let entry = HashBucketEntry::new(0x1234, LogicalAddress::ZERO, false);
        assert_eq!(entry.without_tentative(), entry);
    }

    // -- Bit layout verification against C++ --

    #[test]
    fn bit_layout_address_in_low_48() {
        let addr = LogicalAddress::from_raw(0x0000_DEAD_BEEF_1234);
        let entry = HashBucketEntry::new(0, addr, false);
        assert_eq!(entry.raw(), 0x0000_DEAD_BEEF_1234);
    }

    #[test]
    fn bit_layout_tag_at_bits_48_to_61() {
        // Tag 0x3FFF = all 14 bits set → bits [61:48] all set
        let entry = HashBucketEntry::new(0x3FFF, LogicalAddress::ZERO, false);
        // Expected: bits 48..61 set = 0x3FFF << 48
        assert_eq!(entry.raw(), 0x3FFF_u64 << 48);
    }

    #[test]
    fn bit_layout_tentative_at_bit_63() {
        let entry = HashBucketEntry::new(0, LogicalAddress::ZERO, true);
        assert_eq!(entry.raw(), 1u64 << 63);
    }

    #[test]
    fn bit_layout_reserved_bit_62_not_set_by_construction() {
        // Constructing any entry via `new()` should never set bit 62
        let entry = HashBucketEntry::new(0x3FFF, LogicalAddress::MAX, true);
        assert_eq!(entry.raw() & RESERVED_BIT, 0);
    }

    #[test]
    fn bit_layout_all_fields_combined() {
        let addr = LogicalAddress::from_raw(0x0000_ABCD_1234_5678);
        let entry = HashBucketEntry::new(0x2A5F, addr, true);
        let expected = 0x0000_ABCD_1234_5678_u64 // address
            | (0x2A5F_u64 << 48)                  // tag
            | (1u64 << 63); // tentative
        assert_eq!(entry.raw(), expected);
    }

    // -- from_raw / raw round-trip --

    #[test]
    fn from_raw_round_trip() {
        let raw = 0x8000_1234_DEAD_BEEF_u64;
        let entry = HashBucketEntry::from_raw(raw);
        assert_eq!(entry.raw(), raw);
    }

    // -- Debug / Display --

    #[test]
    fn debug_format() {
        let entry = HashBucketEntry::new(0x1234, LogicalAddress::from_raw(0xFF), false);
        let dbg = format!("{entry:?}");
        assert!(dbg.contains("addr=0x0000000000ff"));
        assert!(dbg.contains("tag=0x1234"));
        assert!(dbg.contains("tent=false"));
    }

    #[test]
    fn display_empty() {
        let s = format!("{}", HashBucketEntry::EMPTY);
        assert_eq!(s, "Entry(empty)");
    }

    #[test]
    fn display_nonempty() {
        let entry = HashBucketEntry::new(0x0ABC, LogicalAddress::from_raw(42), true);
        let s = format!("{entry}");
        assert!(s.contains("0x0abc"));
        assert!(s.contains("tentative"));
    }

    // -- AtomicHashBucketEntry --

    #[test]
    fn atomic_load_store() {
        let atomic = AtomicHashBucketEntry::new(HashBucketEntry::EMPTY);
        let addr = LogicalAddress::new(Page(5), Offset(100));
        let entry = HashBucketEntry::new(0x1234, addr, false);

        // Ordering: Release on store ensures the entry is fully written
        // before any subsequent Acquire load can see it.
        atomic.store(entry, Ordering::Release);
        assert_eq!(atomic.load(Ordering::Acquire), entry);
    }

    #[test]
    fn atomic_compare_exchange_success() {
        let atomic = AtomicHashBucketEntry::new(HashBucketEntry::EMPTY);
        let addr = LogicalAddress::new(Page(1), Offset(42));
        let new_entry = HashBucketEntry::new(0x0ABC, addr, false);

        // Ordering: AcqRel on success ensures we see prior writes and
        // publish our new entry. Acquire on failure to see the actual value.
        let result = atomic.compare_exchange(
            HashBucketEntry::EMPTY,
            new_entry,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        assert_eq!(result, Ok(HashBucketEntry::EMPTY));
        assert_eq!(atomic.load(Ordering::Acquire), new_entry);
    }

    #[test]
    fn atomic_compare_exchange_failure() {
        let addr = LogicalAddress::new(Page(1), Offset(42));
        let existing = HashBucketEntry::new(0x0ABC, addr, false);
        let atomic = AtomicHashBucketEntry::new(existing);

        let wrong_expected = HashBucketEntry::EMPTY;
        let new_entry = HashBucketEntry::new(0x1111, LogicalAddress::from_raw(99), false);

        let result = atomic.compare_exchange(
            wrong_expected,
            new_entry,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        assert_eq!(result, Err(existing));
        // Entry unchanged
        assert_eq!(atomic.load(Ordering::Acquire), existing);
    }

    #[test]
    fn atomic_compare_exchange_weak_success() {
        let atomic = AtomicHashBucketEntry::new(HashBucketEntry::EMPTY);
        let new_entry = HashBucketEntry::new(0x0001, LogicalAddress::from_raw(7), false);

        // Weak CAS may spuriously fail, so retry in a loop
        loop {
            match atomic.compare_exchange_weak(
                HashBucketEntry::EMPTY,
                new_entry,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(_) => continue,
            }
        }
        assert_eq!(atomic.load(Ordering::Acquire), new_entry);
    }

    #[test]
    fn atomic_default_is_empty() {
        let atomic = AtomicHashBucketEntry::default();
        assert!(atomic.load(Ordering::Relaxed).is_empty());
    }

    #[test]
    fn atomic_debug_format() {
        let entry = HashBucketEntry::new(0x0001, LogicalAddress::from_raw(42), false);
        let atomic = AtomicHashBucketEntry::new(entry);
        let dbg = format!("{atomic:?}");
        assert!(dbg.contains("Atomic"));
        assert!(dbg.contains("tag=0x0001"));
    }

    // -- Two-phase insert protocol simulation --

    #[test]
    fn two_phase_insert_protocol() {
        let atomic = AtomicHashBucketEntry::new(HashBucketEntry::EMPTY);
        let addr = LogicalAddress::new(Page(7), Offset(256));

        // Phase 1: CAS empty → tentative entry (reserve slot)
        let tentative = HashBucketEntry::new(0x2222, addr, true);
        let result = atomic.compare_exchange(
            HashBucketEntry::EMPTY,
            tentative,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        assert!(result.is_ok());

        // Verify: entry is visible but tentative
        let loaded = atomic.load(Ordering::Acquire);
        assert!(loaded.is_tentative());
        assert_eq!(loaded.tag(), 0x2222);
        assert_eq!(loaded.address(), addr);

        // Phase 2: (simulated) write record to log at `addr`

        // Phase 3: CAS tentative → committed (clear tentative bit)
        let committed = tentative.without_tentative();
        let result =
            atomic.compare_exchange(tentative, committed, Ordering::AcqRel, Ordering::Acquire);
        assert!(result.is_ok());

        // Verify: entry is committed (not tentative)
        let final_entry = atomic.load(Ordering::Acquire);
        assert!(!final_entry.is_tentative());
        assert_eq!(final_entry.tag(), 0x2222);
        assert_eq!(final_entry.address(), addr);
    }

    // -- Tag matches hash.rs tag extraction --

    #[test]
    fn tag_matches_hash_module_extraction() {
        // The tag in HashBucketEntry occupies the same bit positions as
        // KeyHash::tag() extracts from. Verify they agree.
        use crate::hash::KeyHash;

        // A hash value with a known tag
        let hash_val: u64 = 0x1234_0000_0000_0000;
        let key_hash = KeyHash::new(hash_val);
        let tag = key_hash.tag();

        // Build an entry with the same tag
        let entry = HashBucketEntry::new(tag, LogicalAddress::ZERO, false);
        assert_eq!(entry.tag(), tag);
        assert_eq!(entry.tag(), 0x1234);
    }

    #[test]
    fn tag_consistency_with_hash_module_full_range() {
        use crate::hash::KeyHash;

        // Test several hash values: tag from KeyHash must equal tag in entry
        let test_hashes: &[u64] = &[
            0x0000_0000_0000_0000,
            0x3FFF_0000_0000_0000, // max tag
            0x0001_0000_0000_0000, // min nonzero tag
            0xFFFF_FFFF_FFFF_FFFF, // all bits set
            0x2A5F_DEAD_BEEF_1234, // arbitrary
        ];

        for &h in test_hashes {
            let key_hash = KeyHash::new(h);
            let tag = key_hash.tag();
            let entry = HashBucketEntry::new(tag, LogicalAddress::ZERO, false);
            assert_eq!(
                entry.tag(),
                tag,
                "Tag mismatch for hash 0x{h:016x}: entry.tag()={}, KeyHash.tag()={}",
                entry.tag(),
                tag,
            );
        }
    }

    // -- HashBucket size and alignment --

    #[test]
    fn size_of_hash_bucket() {
        assert_eq!(core::mem::size_of::<HashBucket>(), 64);
    }

    #[test]
    fn alignment_of_hash_bucket() {
        assert_eq!(core::mem::align_of::<HashBucket>(), 64);
    }

    #[test]
    fn bucket_array_elements_are_cache_line_aligned() {
        // In an array [HashBucket; N], every element must start on a 64-byte
        // boundary. This is guaranteed when size == alignment == 64.
        let buckets = [HashBucket::new(), HashBucket::new(), HashBucket::new()];
        for (i, bucket) in buckets.iter().enumerate() {
            let ptr = bucket as *const HashBucket as usize;
            assert_eq!(
                ptr % 64,
                0,
                "HashBucket[{i}] at address 0x{ptr:x} is not cache-line aligned"
            );
        }
    }

    // -- HashBucket::new --

    #[test]
    fn new_bucket_all_entries_empty() {
        let bucket = HashBucket::new();
        for i in 0..BUCKET_NUM_ENTRIES {
            assert!(
                bucket.entry(i).load(Ordering::Relaxed).is_empty(),
                "Entry {i} should be empty in a new bucket"
            );
        }
    }

    #[test]
    fn new_bucket_overflow_is_zero() {
        let bucket = HashBucket::new();
        assert_eq!(
            bucket.overflow_address().load(Ordering::Relaxed),
            LogicalAddress::ZERO
        );
    }

    #[test]
    fn default_bucket_is_new() {
        let bucket = HashBucket::default();
        for i in 0..BUCKET_NUM_ENTRIES {
            assert!(bucket.entry(i).load(Ordering::Relaxed).is_empty());
        }
        assert_eq!(
            bucket.overflow_address().load(Ordering::Relaxed),
            LogicalAddress::ZERO
        );
    }

    // -- entry() access --

    #[test]
    fn entry_returns_correct_slot() {
        let bucket = HashBucket::new();
        let addr = LogicalAddress::new(Page(1), Offset(100));
        // Store a distinct tag in each slot
        for i in 0..BUCKET_NUM_ENTRIES {
            let entry = HashBucketEntry::new(i as u16 + 1, addr, false);
            bucket.entry(i).store(entry, Ordering::Relaxed);
        }
        // Verify each slot has the right tag
        for i in 0..BUCKET_NUM_ENTRIES {
            let loaded = bucket.entry(i).load(Ordering::Relaxed);
            assert_eq!(loaded.tag(), i as u16 + 1);
        }
    }

    #[test]
    #[should_panic]
    fn entry_index_out_of_bounds() {
        let bucket = HashBucket::new();
        let _ = bucket.entry(BUCKET_NUM_ENTRIES); // index 7 → panic
    }

    // -- overflow_address --

    #[test]
    fn overflow_address_store_and_load() {
        let bucket = HashBucket::new();
        let overflow = LogicalAddress::new(Page(99), Offset(0));
        bucket.overflow_address().store(overflow, Ordering::Release);
        assert_eq!(bucket.overflow_address().load(Ordering::Acquire), overflow);
    }

    #[test]
    fn overflow_address_independent_of_entries() {
        let bucket = HashBucket::new();
        let addr = LogicalAddress::new(Page(1), Offset(42));
        let overflow = LogicalAddress::new(Page(100), Offset(0));

        // Fill all entries
        for i in 0..BUCKET_NUM_ENTRIES {
            let entry = HashBucketEntry::new((i as u16) + 1, addr, false);
            bucket.entry(i).store(entry, Ordering::Relaxed);
        }
        // Set overflow
        bucket.overflow_address().store(overflow, Ordering::Release);

        // Verify entries are intact
        for i in 0..BUCKET_NUM_ENTRIES {
            assert_eq!(
                bucket.entry(i).load(Ordering::Relaxed).tag(),
                (i as u16) + 1
            );
        }
        // Verify overflow is intact
        assert_eq!(bucket.overflow_address().load(Ordering::Acquire), overflow);
    }

    // -- find_entry --

    #[test]
    fn find_entry_in_empty_bucket() {
        let bucket = HashBucket::new();
        assert_eq!(bucket.find_entry(0x1234), None);
    }

    #[test]
    fn find_entry_single_match() {
        let bucket = HashBucket::new();
        let addr = LogicalAddress::new(Page(5), Offset(200));
        let entry = HashBucketEntry::new(0x1234, addr, false);
        bucket.entry(3).store(entry, Ordering::Release);

        let result = bucket.find_entry(0x1234);
        assert_eq!(result, Some((3, entry)));
    }

    #[test]
    fn find_entry_first_match_wins() {
        let bucket = HashBucket::new();
        let addr1 = LogicalAddress::new(Page(1), Offset(10));
        let addr2 = LogicalAddress::new(Page(2), Offset(20));
        // Same tag in slots 2 and 5
        bucket.entry(2).store(
            HashBucketEntry::new(0x0ABC, addr1, false),
            Ordering::Release,
        );
        bucket.entry(5).store(
            HashBucketEntry::new(0x0ABC, addr2, false),
            Ordering::Release,
        );

        let (idx, found) = bucket.find_entry(0x0ABC).unwrap();
        assert_eq!(idx, 2);
        assert_eq!(found.address(), addr1);
    }

    #[test]
    fn find_entry_skips_tentative() {
        let bucket = HashBucket::new();
        let addr = LogicalAddress::new(Page(1), Offset(10));
        // Tentative entry in slot 0
        bucket
            .entry(0)
            .store(HashBucketEntry::new(0x1234, addr, true), Ordering::Release);
        // Committed entry with same tag in slot 3
        bucket
            .entry(3)
            .store(HashBucketEntry::new(0x1234, addr, false), Ordering::Release);

        let (idx, _) = bucket.find_entry(0x1234).unwrap();
        assert_eq!(idx, 3);
    }

    #[test]
    fn find_entry_skips_different_tags() {
        let bucket = HashBucket::new();
        let addr = LogicalAddress::new(Page(1), Offset(10));
        bucket
            .entry(0)
            .store(HashBucketEntry::new(0x1111, addr, false), Ordering::Release);
        bucket
            .entry(1)
            .store(HashBucketEntry::new(0x2222, addr, false), Ordering::Release);

        assert_eq!(bucket.find_entry(0x3333), None);
    }

    #[test]
    fn find_entry_all_tentative_returns_none() {
        let bucket = HashBucket::new();
        let addr = LogicalAddress::new(Page(1), Offset(10));
        for i in 0..BUCKET_NUM_ENTRIES {
            bucket
                .entry(i)
                .store(HashBucketEntry::new(0x1234, addr, true), Ordering::Release);
        }
        assert_eq!(bucket.find_entry(0x1234), None);
    }

    // -- find_empty --

    #[test]
    fn find_empty_in_empty_bucket() {
        let bucket = HashBucket::new();
        assert_eq!(bucket.find_empty(), Some(0));
    }

    #[test]
    fn find_empty_first_slot_taken() {
        let bucket = HashBucket::new();
        let addr = LogicalAddress::new(Page(1), Offset(10));
        bucket
            .entry(0)
            .store(HashBucketEntry::new(0x1111, addr, false), Ordering::Release);

        assert_eq!(bucket.find_empty(), Some(1));
    }

    #[test]
    fn find_empty_all_full() {
        let bucket = HashBucket::new();
        let addr = LogicalAddress::new(Page(1), Offset(10));
        for i in 0..BUCKET_NUM_ENTRIES {
            bucket.entry(i).store(
                HashBucketEntry::new((i as u16) + 1, addr, false),
                Ordering::Release,
            );
        }
        assert_eq!(bucket.find_empty(), None);
    }

    #[test]
    fn find_empty_gap_in_middle() {
        let bucket = HashBucket::new();
        let addr = LogicalAddress::new(Page(1), Offset(10));
        // Fill 0, 1, 2; leave 3 empty; fill 4, 5, 6
        for i in [0, 1, 2, 4, 5, 6] {
            bucket.entry(i).store(
                HashBucketEntry::new((i as u16) + 1, addr, false),
                Ordering::Release,
            );
        }
        assert_eq!(bucket.find_empty(), Some(3));
    }

    // -- try_insert --

    #[test]
    fn try_insert_into_empty_bucket() {
        let bucket = HashBucket::new();
        let addr = LogicalAddress::new(Page(1), Offset(42));
        let result = bucket.try_insert(0x1234, addr);
        assert_eq!(result, Ok(0));

        let loaded = bucket.entry(0).load(Ordering::Acquire);
        assert_eq!(loaded.tag(), 0x1234);
        assert_eq!(loaded.address(), addr);
        assert!(!loaded.is_tentative());
    }

    #[test]
    fn try_insert_fills_sequentially() {
        let bucket = HashBucket::new();
        let addr = LogicalAddress::new(Page(1), Offset(10));
        for i in 0..BUCKET_NUM_ENTRIES {
            let tag = (i as u16) + 1;
            assert_eq!(bucket.try_insert(tag, addr), Ok(i));
        }
    }

    #[test]
    fn try_insert_fails_when_full() {
        let bucket = HashBucket::new();
        let addr = LogicalAddress::new(Page(1), Offset(10));
        for i in 0..BUCKET_NUM_ENTRIES {
            bucket.try_insert((i as u16) + 1, addr).unwrap();
        }
        assert_eq!(bucket.try_insert(0x1234, addr), Err(()));
    }

    #[test]
    fn try_insert_then_find_entry() {
        let bucket = HashBucket::new();
        let addr = LogicalAddress::new(Page(7), Offset(256));

        bucket.try_insert(0x1234, addr).unwrap();
        bucket.try_insert(0x2345, addr).unwrap();

        let (_, found) = bucket.find_entry(0x2345).unwrap();
        assert_eq!(found.tag(), 0x2345);
        assert_eq!(found.address(), addr);
    }

    #[test]
    fn try_insert_all_seven_then_find_all() {
        let bucket = HashBucket::new();
        let base_addr = LogicalAddress::new(Page(1), Offset(0));

        // Insert 7 entries with distinct tags
        for i in 0..BUCKET_NUM_ENTRIES {
            let tag = (i as u16) + 1;
            let addr = LogicalAddress::from_raw(base_addr.raw() + i as u64);
            bucket.try_insert(tag, addr).unwrap();
        }

        // Find all 7
        for i in 0..BUCKET_NUM_ENTRIES {
            let tag = (i as u16) + 1;
            let expected_addr = LogicalAddress::from_raw(base_addr.raw() + i as u64);
            let (idx, found) = bucket.find_entry(tag).unwrap();
            assert_eq!(idx, i);
            assert_eq!(found.tag(), tag);
            assert_eq!(found.address(), expected_addr);
        }
    }

    // -- Debug --

    #[test]
    fn bucket_debug_format() {
        let bucket = HashBucket::new();
        let dbg = format!("{bucket:?}");
        assert!(dbg.contains("HashBucket"));
        assert!(dbg.contains("overflow"));
    }

    // -- Overflow chain operations --

    #[test]
    fn overflow_bucket_none_when_empty() {
        let bucket = HashBucket::new();
        let pool = crate::overflow::OverflowBucketPool::new();
        assert!(bucket.overflow_bucket(&pool).is_none());
    }

    #[test]
    fn get_or_create_overflow_creates_new() {
        let bucket = HashBucket::new();
        let pool = crate::overflow::OverflowBucketPool::new();
        let overflow = bucket.get_or_create_overflow(&pool);
        // Overflow should be empty.
        for i in 0..BUCKET_NUM_ENTRIES {
            assert!(overflow.entry(i).load(Ordering::Relaxed).is_empty());
        }
        // Overflow pointer should now be set.
        let addr = bucket.overflow_address().load(Ordering::Acquire);
        assert_ne!(addr, LogicalAddress::ZERO);
    }

    #[test]
    fn get_or_create_overflow_returns_existing() {
        let bucket = HashBucket::new();
        let pool = crate::overflow::OverflowBucketPool::new();
        let o1 = bucket.get_or_create_overflow(&pool) as *const HashBucket;
        let o2 = bucket.get_or_create_overflow(&pool) as *const HashBucket;
        assert_eq!(o1, o2, "Second call should return the same overflow bucket");
    }

    #[test]
    fn get_or_create_overflow_concurrent_race() {
        use std::sync::{Arc, Barrier};
        use std::thread;

        let bucket = Box::leak(Box::new(HashBucket::new()));
        let bucket_ref: &'static HashBucket = bucket;
        let pool = Arc::new(crate::overflow::OverflowBucketPool::new());
        let barrier = Arc::new(Barrier::new(8));

        let handles: Vec<_> = (0..8)
            .map(|_| {
                let pool = Arc::clone(&pool);
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    bucket_ref.get_or_create_overflow(&pool) as *const HashBucket as usize
                })
            })
            .collect();

        let ptrs: Vec<usize> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        // All threads should get the same overflow bucket.
        assert!(
            ptrs.windows(2).all(|w| w[0] == w[1]),
            "All threads must resolve to the same overflow bucket"
        );

        // Only one overflow address should be set.
        let addr = bucket_ref.overflow_address().load(Ordering::Acquire);
        assert_ne!(addr, LogicalAddress::ZERO);
        assert_eq!(pool.get(addr) as *const HashBucket as usize, ptrs[0]);

        // Leak cleanup — we can't un-leak, but this is a test.
    }

    #[test]
    fn for_each_bucket_single() {
        let bucket = HashBucket::new();
        let pool = crate::overflow::OverflowBucketPool::new();
        let mut count = 0u32;
        let result: Option<()> = bucket.for_each_bucket(&pool, |_| {
            count += 1;
            core::ops::ControlFlow::Continue(())
        });
        assert!(result.is_none());
        assert_eq!(count, 1);
    }

    #[test]
    fn for_each_bucket_with_overflow() {
        let bucket = HashBucket::new();
        let pool = crate::overflow::OverflowBucketPool::new();
        // Create two levels of overflow.
        let o1 = bucket.get_or_create_overflow(&pool);
        let _o2 = o1.get_or_create_overflow(&pool);

        let mut count = 0u32;
        bucket.for_each_bucket(&pool, |_| {
            count += 1;
            core::ops::ControlFlow::<()>::Continue(())
        });
        assert_eq!(count, 3);
    }

    #[test]
    fn for_each_bucket_early_break() {
        let bucket = HashBucket::new();
        let pool = crate::overflow::OverflowBucketPool::new();
        let _ = bucket.get_or_create_overflow(&pool);

        let mut visited = 0u32;
        let result = bucket.for_each_bucket(&pool, |_| {
            visited += 1;
            core::ops::ControlFlow::Break(42u32)
        });
        assert_eq!(result, Some(42));
        assert_eq!(visited, 1); // Stopped after first bucket.
    }

    #[test]
    fn find_entry_in_chain_primary() {
        let bucket = HashBucket::new();
        let pool = crate::overflow::OverflowBucketPool::new();
        let addr = LogicalAddress::new(Page(5), Offset(200));
        bucket.try_insert(0x1234, addr).unwrap();

        let found = bucket.find_entry_in_chain(0x1234, &pool);
        assert!(found.is_some());
        let (bucket_addr, idx, entry) = found.unwrap();
        assert_eq!(bucket_addr, LogicalAddress::ZERO); // Primary bucket.
        assert_eq!(idx, 0);
        assert_eq!(entry.tag(), 0x1234);
        assert_eq!(entry.address(), addr);
    }

    #[test]
    fn find_entry_in_chain_overflow() {
        let bucket = HashBucket::new();
        let pool = crate::overflow::OverflowBucketPool::new();
        let addr = LogicalAddress::new(Page(1), Offset(10));

        // Fill primary bucket.
        for i in 0..BUCKET_NUM_ENTRIES {
            bucket.try_insert((i as u16) + 1, addr).unwrap();
        }

        // Insert into overflow.
        let overflow = bucket.get_or_create_overflow(&pool);
        overflow.try_insert(0x00FF, addr).unwrap();

        // Find it via chain search.
        let found = bucket.find_entry_in_chain(0x00FF, &pool);
        assert!(found.is_some());
        let (bucket_addr, idx, entry) = found.unwrap();
        assert_ne!(bucket_addr, LogicalAddress::ZERO); // Found in overflow.
        assert_eq!(idx, 0);
        assert_eq!(entry.tag(), 0x00FF);
    }

    #[test]
    fn find_entry_in_chain_not_found() {
        let bucket = HashBucket::new();
        let pool = crate::overflow::OverflowBucketPool::new();
        bucket
            .try_insert(0x1111, LogicalAddress::from_raw(1))
            .unwrap();
        assert!(bucket.find_entry_in_chain(0x2222, &pool).is_none());
    }

    #[test]
    fn insert_in_chain_primary() {
        let bucket = HashBucket::new();
        let pool = crate::overflow::OverflowBucketPool::new();
        let addr = LogicalAddress::new(Page(1), Offset(42));

        assert!(bucket.insert_in_chain(0x1234, addr, &pool).is_ok());
        let (_, _, entry) = bucket.find_entry_in_chain(0x1234, &pool).unwrap();
        assert_eq!(entry.address(), addr);
    }

    #[test]
    fn insert_in_chain_triggers_overflow() {
        let bucket = HashBucket::new();
        let pool = crate::overflow::OverflowBucketPool::new();
        let addr = LogicalAddress::new(Page(1), Offset(10));

        // Fill primary bucket.
        for i in 0..BUCKET_NUM_ENTRIES {
            bucket.insert_in_chain((i as u16) + 1, addr, &pool).unwrap();
        }
        // This 8th insert must trigger overflow.
        bucket.insert_in_chain(0x00FF, addr, &pool).unwrap();

        // Overflow should exist now.
        assert_ne!(
            bucket.overflow_address().load(Ordering::Acquire),
            LogicalAddress::ZERO
        );

        // All 8 entries must be findable.
        for i in 0..BUCKET_NUM_ENTRIES {
            assert!(
                bucket.find_entry_in_chain((i as u16) + 1, &pool).is_some(),
                "Could not find tag {} in chain",
                (i as u16) + 1
            );
        }
        assert!(bucket.find_entry_in_chain(0x00FF, &pool).is_some());
    }

    #[test]
    fn insert_in_chain_deep_overflow() {
        let bucket = HashBucket::new();
        let pool = crate::overflow::OverflowBucketPool::new();

        // Insert 21 entries (3 buckets × 7 slots).
        for i in 0..21u16 {
            let addr = LogicalAddress::from_raw((i as u64) + 2);
            bucket.insert_in_chain(i + 1, addr, &pool).unwrap();
        }

        // All 21 must be findable.
        for i in 0..21u16 {
            let expected_addr = LogicalAddress::from_raw((i as u64) + 2);
            let found = bucket.find_entry_in_chain(i + 1, &pool);
            assert!(found.is_some(), "Could not find tag {}", i + 1);
            let (_, _, entry) = found.unwrap();
            assert_eq!(entry.tag(), i + 1);
            assert_eq!(entry.address(), expected_addr);
        }
    }

    #[test]
    fn insert_in_chain_concurrent() {
        use std::sync::{Arc, Barrier};
        use std::thread;

        let bucket = Box::leak(Box::new(HashBucket::new()));
        let bucket_ref: &'static HashBucket = bucket;
        let pool = Arc::new(crate::overflow::OverflowBucketPool::new());
        let num_threads = 8;
        let entries_per_thread = 4; // 32 entries total → needs 4+ buckets
        let barrier = Arc::new(Barrier::new(num_threads));

        let handles: Vec<_> = (0..num_threads)
            .map(|t| {
                let pool = Arc::clone(&pool);
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    for i in 0..entries_per_thread {
                        let tag = (t * entries_per_thread + i + 1) as u16;
                        let addr = LogicalAddress::from_raw(tag as u64 + 1);
                        bucket_ref
                            .insert_in_chain(tag, addr, &pool)
                            .expect("insert_in_chain failed");
                    }
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }

        // Verify all entries are findable.
        let total = num_threads * entries_per_thread;
        for i in 0..total {
            let tag = (i + 1) as u16;
            let expected_addr = LogicalAddress::from_raw(tag as u64 + 1);
            let found = bucket_ref.find_entry_in_chain(tag, &pool);
            assert!(found.is_some(), "Missing tag {tag} after concurrent insert");
            let (_, _, entry) = found.unwrap();
            assert_eq!(entry.address(), expected_addr);
        }
    }
}

// ---------------------------------------------------------------------------
// Property tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod proptests {
    use super::*;
    use crate::address::LogicalAddress;
    use proptest::prelude::*;

    /// Strategy for a valid 14-bit tag (0..=0x3FFF).
    fn tag_strategy() -> impl Strategy<Value = u16> {
        0u16..=MAX_TAG
    }

    /// Strategy for a valid 48-bit address.
    fn address_strategy() -> impl Strategy<Value = LogicalAddress> {
        (0u64..=(ADDRESS_MASK)).prop_map(LogicalAddress::from_raw)
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        /// Pack/unpack round-trip: all fields survive encoding into u64 and
        /// decoding back.
        #[test]
        fn round_trip_pack_unpack(
            tag in tag_strategy(),
            addr in address_strategy(),
            tentative: bool,
        ) {
            let entry = HashBucketEntry::new(tag, addr, tentative);
            prop_assert_eq!(entry.tag(), tag);
            prop_assert_eq!(entry.address(), addr);
            prop_assert_eq!(entry.is_tentative(), tentative);
        }

        /// Tentative bit is independent: toggling it does not change tag or address.
        #[test]
        fn tentative_independent_of_other_fields(
            tag in tag_strategy(),
            addr in address_strategy(),
        ) {
            let normal = HashBucketEntry::new(tag, addr, false);
            let tent = HashBucketEntry::new(tag, addr, true);

            prop_assert_eq!(normal.tag(), tent.tag());
            prop_assert_eq!(normal.address(), tent.address());

            // with_tentative / without_tentative
            prop_assert_eq!(normal.with_tentative().tag(), tag);
            prop_assert_eq!(normal.with_tentative().address(), addr);
            prop_assert_eq!(tent.without_tentative().tag(), tag);
            prop_assert_eq!(tent.without_tentative().address(), addr);
        }

        /// Empty check: only all-zeros is empty.
        #[test]
        fn only_zero_is_empty(raw: u64) {
            let entry = HashBucketEntry::from_raw(raw);
            prop_assert_eq!(entry.is_empty(), raw == 0);
        }

        /// from_raw / raw round-trip.
        #[test]
        fn from_raw_raw_identity(raw: u64) {
            prop_assert_eq!(HashBucketEntry::from_raw(raw).raw(), raw);
        }

        /// Tag extraction matches hash.rs tag computation for any hash value.
        #[test]
        fn tag_matches_hash_module(hash_val: u64) {
            use crate::hash::KeyHash;

            let key_hash = KeyHash::new(hash_val);
            let tag = key_hash.tag();
            let entry = HashBucketEntry::new(tag, LogicalAddress::ZERO, false);
            prop_assert_eq!(entry.tag(), tag);
        }

        /// Reserved bit (bit 62) is never set by `new()`.
        #[test]
        fn new_never_sets_reserved_bit(
            tag in tag_strategy(),
            addr in address_strategy(),
            tentative: bool,
        ) {
            let entry = HashBucketEntry::new(tag, addr, tentative);
            prop_assert_eq!(entry.raw() & RESERVED_BIT, 0);
        }

        /// with_tentative / without_tentative round-trip.
        #[test]
        fn tentative_toggle_round_trip(
            tag in tag_strategy(),
            addr in address_strategy(),
        ) {
            let entry = HashBucketEntry::new(tag, addr, false);
            let toggled = entry.with_tentative().without_tentative();
            prop_assert_eq!(toggled, entry);
        }

        /// Insert N entries with distinct tags into a bucket → find them all.
        #[test]
        fn insert_n_then_find_all(
            n in 1usize..=BUCKET_NUM_ENTRIES,
            tags in proptest::collection::vec(1u16..=MAX_TAG, BUCKET_NUM_ENTRIES),
        ) {
            // Deduplicate tags (we need distinct tags for each slot)
            let mut unique_tags: Vec<u16> = Vec::new();
            for &t in &tags {
                if !unique_tags.contains(&t) {
                    unique_tags.push(t);
                }
                if unique_tags.len() == n {
                    break;
                }
            }
            if unique_tags.len() < n {
                // Not enough unique tags generated — skip this case
                return Ok(());
            }

            let bucket = HashBucket::new();
            let addr = LogicalAddress::from_raw(42);

            for (i, &tag) in unique_tags.iter().enumerate() {
                let slot_addr = LogicalAddress::from_raw(addr.raw() + i as u64);
                let result = bucket.try_insert(tag, slot_addr);
                prop_assert!(result.is_ok(), "Insert failed for tag 0x{:04x} at i={}", tag, i);
            }

            // Find each one
            for (i, &tag) in unique_tags.iter().enumerate() {
                let expected_addr = LogicalAddress::from_raw(addr.raw() + i as u64);
                let found = bucket.find_entry(tag);
                prop_assert!(found.is_some(), "Could not find tag 0x{:04x}", tag);
                let (_, entry) = found.unwrap();
                prop_assert_eq!(entry.tag(), tag);
                prop_assert_eq!(entry.address(), expected_addr);
            }
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(16))]

        /// Insert N entries (N > 7) via chain insert → all N are findable.
        /// Lower case count because each case allocates an OverflowBucketPool.
        #[test]
        fn insert_n_chain_then_find_all(
            n in 8usize..=35,
            tags in proptest::collection::vec(1u16..=MAX_TAG, 35),
        ) {
            // Deduplicate tags.
            let mut unique_tags: Vec<u16> = Vec::new();
            for &t in &tags {
                if !unique_tags.contains(&t) {
                    unique_tags.push(t);
                }
                if unique_tags.len() == n {
                    break;
                }
            }
            if unique_tags.len() < n {
                return Ok(());
            }

            let bucket = HashBucket::new();
            let pool = crate::overflow::OverflowBucketPool::new();

            for (i, &tag) in unique_tags.iter().enumerate() {
                let addr = LogicalAddress::from_raw((i as u64) + 2);
                let result = bucket.insert_in_chain(tag, addr, &pool);
                prop_assert!(result.is_ok(), "Chain insert failed for tag 0x{:04x} at i={}", tag, i);
            }

            // Find all N.
            for (i, &tag) in unique_tags.iter().enumerate() {
                let expected_addr = LogicalAddress::from_raw((i as u64) + 2);
                let found = bucket.find_entry_in_chain(tag, &pool);
                prop_assert!(found.is_some(), "Could not find tag 0x{:04x} in chain", tag);
                let (_, _, entry) = found.unwrap();
                prop_assert_eq!(entry.tag(), tag);
                prop_assert_eq!(entry.address(), expected_addr);
            }
        }
    }
}
