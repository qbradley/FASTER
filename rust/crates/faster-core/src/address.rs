//! Logical addressing primitives for the FASTER hybrid log.
//!
//! Every location in the hybrid log is identified by a [`LogicalAddress`]: a 48-bit
//! value encoding a page index (23 bits) and an offset within that page (25 bits).
//! The remaining 16 upper bits of the `u64` are reserved for use by the hash table
//! (tag, tentative bit, etc.).
//!
//! # Layout (matches C++ `address.h` exactly)
//!
//! ```text
//! 63            48 47         25 24          0
//! ┌──────────────┬─────────────┬─────────────┐
//! │  reserved(16) │  page (23)  │ offset (25) │
//! └──────────────┴─────────────┴─────────────┘
//! ```
//!
//! With the default 32 MB page size (`OFFSET_BITS = 25`):
//! - **Offset:** bits 0..24 → 0 to 33,554,431 bytes within a page
//! - **Page:** bits 25..47 → 0 to 8,388,607 pages
//! - **Total addressable:** 8,388,608 × 32 MB = **256 TB**
//!
//! # Types
//!
//! | Type | Description |
//! |------|-------------|
//! | [`LogicalAddress`] | Page + offset packed into a `u64` |
//! | [`AtomicLogicalAddress`] | Thread-safe atomic wrapper for `LogicalAddress` |
//! | [`Page`] | Newtype for a page number (`u32`) |
//! | [`Offset`] | Newtype for an offset within a page (`u32`) |
//!
//! # Examples
//!
//! ```
//! use faster_core::address::{LogicalAddress, Page, Offset};
//!
//! let addr = LogicalAddress::new(Page(42), Offset(1024));
//! assert_eq!(addr.page(), Page(42));
//! assert_eq!(addr.offset(), Offset(1024));
//! assert!(addr.is_valid());
//! ```

use core::fmt;
use core::sync::atomic::{AtomicU64, Ordering};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Constants — match C++ address.h exactly
// ---------------------------------------------------------------------------

/// Number of bits used for the full logical address (page + offset).
/// The upper 16 bits of the `u64` are reserved for the hash table.
pub const ADDRESS_BITS: u32 = 48;

/// Number of bits used for the offset within a page.
#[cfg(feature = "small-pages")]
pub const OFFSET_BITS: u32 = 16; // 64 KB pages for DST

/// Number of bits used for the offset within a page.
/// Default 25 → page size of 2^25 = 32 MB.
#[cfg(not(feature = "small-pages"))]
pub const OFFSET_BITS: u32 = 25;

/// Number of bits used for the page index: `ADDRESS_BITS - OFFSET_BITS`.
pub const PAGE_BITS: u32 = ADDRESS_BITS - OFFSET_BITS; // 23

/// Maximum valid offset value: `(1 << OFFSET_BITS) - 1`.
pub const MAX_OFFSET: u32 = (1u32 << OFFSET_BITS) - 1;

/// Maximum valid page number: `(1 << PAGE_BITS) - 1`.
pub const MAX_PAGE: u32 = ((1u64 << PAGE_BITS) - 1) as u32;

/// Maximum raw address value (48 bits set): `(1 << ADDRESS_BITS) - 1`.
pub const MAX_ADDRESS: u64 = (1u64 << ADDRESS_BITS) - 1;

/// Read-cache bit: the MSB of the 48-bit address (bit 47).
/// When set, the address refers to the read cache rather than the main log.
pub const READ_CACHE_BIT: u64 = 1u64 << (ADDRESS_BITS - 1);

// ---------------------------------------------------------------------------
// Page newtype
// ---------------------------------------------------------------------------

/// A page number in the hybrid log.
///
/// Pages are numbered 0 through [`MAX_PAGE`] (8,388,607 with default settings).
/// Each page is 32 MB (2^[`OFFSET_BITS`] bytes).
///
/// # Examples
///
/// ```
/// use faster_core::address::Page;
///
/// let p = Page(100);
/// assert_eq!(p.0, 100);
/// assert_eq!(format!("{p}"), "Page(100)");
/// ```
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize)]
#[repr(transparent)]
pub struct Page(pub u32);

impl fmt::Debug for Page {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Page({})", self.0)
    }
}

impl fmt::Display for Page {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Page({})", self.0)
    }
}

// ---------------------------------------------------------------------------
// Offset newtype
// ---------------------------------------------------------------------------

/// An offset within a page.
///
/// Offsets range from 0 to [`MAX_OFFSET`] (33,554,431 with default settings,
/// i.e. a 32 MB page). Represents a byte position within a single page frame.
///
/// # Examples
///
/// ```
/// use faster_core::address::Offset;
///
/// let o = Offset(512);
/// assert_eq!(o.0, 512);
/// assert_eq!(format!("{o}"), "Offset(512)");
/// ```
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize)]
#[repr(transparent)]
pub struct Offset(pub u32);

impl fmt::Debug for Offset {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Offset({})", self.0)
    }
}

impl fmt::Display for Offset {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Offset({})", self.0)
    }
}

// ---------------------------------------------------------------------------
// LogicalAddress
// ---------------------------------------------------------------------------

/// A logical address in the FASTER hybrid log.
///
/// Encodes a [`Page`] number and an [`Offset`] within that page into a single
/// `u64`. The encoding uses 48 bits (25 for offset, 23 for page) and matches
/// the C++ `Address` class in `address.h` exactly.
///
/// # Special values
///
/// | Constant | Value | Meaning |
/// |----------|-------|---------|
/// | [`INVALID`](Self::INVALID) | `1` | Sentinel for "no valid address yet" |
/// | [`ZERO`](Self::ZERO) | `0` | Default / empty hash bucket entry |
/// | [`MAX`](Self::MAX) | `2^48 - 1` | Largest representable address |
///
/// Note: `INVALID` is **1**, not 0. This matches the C++ convention where 0
/// represents an empty/unused hash bucket slot, while 1 means "address field
/// is present but not yet pointing to a valid record."
///
/// # Examples
///
/// ```
/// use faster_core::address::{LogicalAddress, Page, Offset};
///
/// // Construct from page + offset
/// let addr = LogicalAddress::new(Page(10), Offset(256));
/// assert_eq!(addr.page(), Page(10));
/// assert_eq!(addr.offset(), Offset(256));
///
/// // Raw u64 round-trip
/// let raw = addr.raw();
/// assert_eq!(LogicalAddress::from_raw(raw), addr);
///
/// // Special values
/// assert!(!LogicalAddress::INVALID.is_valid());
/// assert!(!LogicalAddress::ZERO.is_valid());
/// ```
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[repr(transparent)]
pub struct LogicalAddress(u64);

// Layout assertions: LogicalAddress is packed into HashBucketEntry (48-bit
// field) and serialized as part of HashBucket's on-disk checkpoint format.
// Changing size breaks bucket serialization and hash index recovery.
const _: () = assert!(core::mem::size_of::<LogicalAddress>() == 8);
const _: () = assert!(core::mem::align_of::<LogicalAddress>() == 8);

/// Mask for the lower 48 address bits.
const ADDRESS_MASK: u64 = MAX_ADDRESS;

/// Mask for the lower 25 offset bits.
const OFFSET_MASK: u64 = (1u64 << OFFSET_BITS) - 1;

impl LogicalAddress {
    /// An invalid address sentinel.
    ///
    /// Set to `1` (not `0`) to distinguish a hash bucket entry that points to an
    /// invalid address from an all-zeros empty entry. Matches C++
    /// `Address::kInvalidAddress`.
    pub const INVALID: Self = Self(1);

    /// The zero address — represents an empty hash bucket slot.
    pub const ZERO: Self = Self(0);

    /// Minimum valid address. Addresses > `INVALID` (i.e. ≥ 2) are valid.
    pub const MIN_VALID: Self = Self(2);

    /// Maximum representable address (all 48 address bits set).
    /// Matches C++ `Address::kMaxAddress`.
    pub const MAX: Self = Self(MAX_ADDRESS);

    /// Creates a new `LogicalAddress` from a page number and offset.
    ///
    /// # Panics (debug only)
    ///
    /// Debug-asserts that `page` ≤ [`MAX_PAGE`] and `offset` ≤ [`MAX_OFFSET`].
    ///
    /// # Examples
    ///
    /// ```
    /// use faster_core::address::{LogicalAddress, Page, Offset};
    ///
    /// let addr = LogicalAddress::new(Page(1), Offset(0));
    /// assert_eq!(addr.page(), Page(1));
    /// assert_eq!(addr.offset(), Offset(0));
    /// ```
    #[inline(always)]
    pub const fn new(page: Page, offset: Offset) -> Self {
        debug_assert!(page.0 <= MAX_PAGE, "page exceeds MAX_PAGE");
        debug_assert!(offset.0 <= MAX_OFFSET, "offset exceeds MAX_OFFSET");
        // Mask inputs to valid ranges for defense-in-depth in release builds.
        let p = (page.0 as u64) & (MAX_PAGE as u64);
        let o = (offset.0 as u64) & (MAX_OFFSET as u64);
        Self((p << OFFSET_BITS) | o)
    }

    /// Creates a `LogicalAddress` from a raw `u64` value.
    ///
    /// The upper 16 bits are masked off so that only the 48-bit address is retained.
    ///
    /// # Examples
    ///
    /// ```
    /// use faster_core::address::LogicalAddress;
    ///
    /// let addr = LogicalAddress::from_raw(0x0000_DEAD_BEEF_1234);
    /// assert_eq!(addr.raw(), 0x0000_DEAD_BEEF_1234);
    /// ```
    #[inline(always)]
    pub const fn from_raw(raw: u64) -> Self {
        Self(raw & ADDRESS_MASK)
    }

    /// Returns the raw `u64` representation of this address.
    #[inline(always)]
    pub const fn raw(self) -> u64 {
        self.0
    }

    /// Extracts the page number from this address.
    ///
    /// # Examples
    ///
    /// ```
    /// use faster_core::address::{LogicalAddress, Page, Offset};
    ///
    /// let addr = LogicalAddress::new(Page(42), Offset(100));
    /// assert_eq!(addr.page(), Page(42));
    /// ```
    #[inline(always)]
    pub const fn page(self) -> Page {
        Page((self.0 >> OFFSET_BITS) as u32)
    }

    /// Extracts the offset within the page from this address.
    ///
    /// # Examples
    ///
    /// ```
    /// use faster_core::address::{LogicalAddress, Page, Offset};
    ///
    /// let addr = LogicalAddress::new(Page(0), Offset(512));
    /// assert_eq!(addr.offset(), Offset(512));
    /// ```
    #[inline(always)]
    pub const fn offset(self) -> Offset {
        Offset((self.0 & OFFSET_MASK) as u32)
    }

    /// Returns `true` if this address is valid (i.e. greater than [`INVALID`](Self::INVALID)).
    ///
    /// Both [`ZERO`](Self::ZERO) and [`INVALID`](Self::INVALID) are considered
    /// not valid — only addresses ≥ [`MIN_VALID`](Self::MIN_VALID) return `true`.
    ///
    /// # Examples
    ///
    /// ```
    /// use faster_core::address::{LogicalAddress, Page, Offset};
    ///
    /// assert!(!LogicalAddress::ZERO.is_valid());
    /// assert!(!LogicalAddress::INVALID.is_valid());
    /// assert!(LogicalAddress::new(Page(0), Offset(2)).is_valid());
    /// ```
    #[inline(always)]
    pub const fn is_valid(self) -> bool {
        self.0 > Self::INVALID.0
    }

    /// Returns `true` if this is the null/zero address.
    #[inline(always)]
    pub const fn is_null(self) -> bool {
        self.0 == 0
    }

    /// Returns `true` if this address refers to the read cache.
    ///
    /// The read-cache bit is bit 47 (the MSB of the 48-bit address space).
    /// Matches C++ `Address::in_readcache()`.
    #[inline(always)]
    pub const fn in_read_cache(self) -> bool {
        (self.0 & READ_CACHE_BIT) != 0
    }
}

impl Default for LogicalAddress {
    /// Defaults to [`LogicalAddress::ZERO`].
    fn default() -> Self {
        Self::ZERO
    }
}

impl fmt::Debug for LogicalAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "LogicalAddress(page={}, offset={}, raw=0x{:012x})",
            self.page().0,
            self.offset().0,
            self.0,
        )
    }
}

impl fmt::Display for LogicalAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.page().0, self.offset().0)
    }
}

impl From<u64> for LogicalAddress {
    /// Converts from raw `u64`, masking the upper 16 bits.
    fn from(raw: u64) -> Self {
        Self::from_raw(raw)
    }
}

impl From<LogicalAddress> for u64 {
    fn from(addr: LogicalAddress) -> Self {
        addr.raw()
    }
}

// ---------------------------------------------------------------------------
// AtomicLogicalAddress
// ---------------------------------------------------------------------------

/// A thread-safe atomic wrapper around [`LogicalAddress`].
///
/// Used for shared mutable addresses in concurrent data structures
/// (e.g., `HeadAddress`, `TailAddress`, `ReadOnlyAddress` in the hybrid log,
/// and hash bucket entries).
///
/// All methods take an explicit [`Ordering`] parameter — callers choose the
/// memory ordering appropriate for their use case.
///
/// # Examples
///
/// ```
/// use faster_core::address::{LogicalAddress, AtomicLogicalAddress, Page, Offset};
/// use core::sync::atomic::Ordering;
///
/// let atomic = AtomicLogicalAddress::new(LogicalAddress::ZERO);
/// let addr = LogicalAddress::new(Page(5), Offset(100));
///
/// atomic.store(addr, Ordering::Release);
/// assert_eq!(atomic.load(Ordering::Acquire), addr);
/// ```
#[repr(transparent)]
pub struct AtomicLogicalAddress(AtomicU64);

// Layout assertions: AtomicLogicalAddress is the overflow_address field in
// HashBucket (8th slot). Must be exactly 8 bytes so the bucket totals 64 bytes.
// Changing size breaks the on-disk checkpoint format.
const _: () = assert!(core::mem::size_of::<AtomicLogicalAddress>() == 8);
const _: () = assert!(core::mem::align_of::<AtomicLogicalAddress>() == 8);

impl AtomicLogicalAddress {
    /// Creates a new `AtomicLogicalAddress` with the given initial value.
    pub const fn new(addr: LogicalAddress) -> Self {
        Self(AtomicU64::new(addr.0))
    }

    /// Loads the address with the given memory ordering.
    #[inline]
    pub fn load(&self, order: Ordering) -> LogicalAddress {
        LogicalAddress(self.0.load(order))
    }

    /// Stores the address with the given memory ordering.
    #[inline]
    pub fn store(&self, addr: LogicalAddress, order: Ordering) {
        self.0.store(addr.0, order);
    }

    /// Performs an atomic compare-and-exchange.
    ///
    /// If the current value equals `current`, stores `new_val` and returns
    /// `Ok(current)`. Otherwise returns `Err(actual)` where `actual` is the
    /// value that was found.
    ///
    /// # Examples
    ///
    /// ```
    /// use faster_core::address::{LogicalAddress, AtomicLogicalAddress, Page, Offset};
    /// use core::sync::atomic::Ordering;
    ///
    /// let atomic = AtomicLogicalAddress::new(LogicalAddress::ZERO);
    /// let new_addr = LogicalAddress::new(Page(1), Offset(0));
    ///
    /// let result = atomic.compare_exchange(
    ///     LogicalAddress::ZERO,
    ///     new_addr,
    ///     Ordering::AcqRel,
    ///     Ordering::Acquire,
    /// );
    /// assert_eq!(result, Ok(LogicalAddress::ZERO));
    /// assert_eq!(atomic.load(Ordering::Acquire), new_addr);
    /// ```
    #[inline]
    pub fn compare_exchange(
        &self,
        current: LogicalAddress,
        new_val: LogicalAddress,
        success: Ordering,
        failure: Ordering,
    ) -> Result<LogicalAddress, LogicalAddress> {
        self.0
            .compare_exchange(current.0, new_val.0, success, failure)
            .map(LogicalAddress)
            .map_err(LogicalAddress)
    }

    /// Weak compare-and-exchange — may spuriously fail on some platforms,
    /// but can be more efficient in retry loops.
    #[inline]
    pub fn compare_exchange_weak(
        &self,
        current: LogicalAddress,
        new_val: LogicalAddress,
        success: Ordering,
        failure: Ordering,
    ) -> Result<LogicalAddress, LogicalAddress> {
        self.0
            .compare_exchange_weak(current.0, new_val.0, success, failure)
            .map(LogicalAddress)
            .map_err(LogicalAddress)
    }
}

impl Default for AtomicLogicalAddress {
    /// Defaults to [`LogicalAddress::ZERO`].
    fn default() -> Self {
        Self::new(LogicalAddress::ZERO)
    }
}

impl fmt::Debug for AtomicLogicalAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let addr = self.load(Ordering::Relaxed);
        write!(f, "AtomicLogicalAddress({addr:?})")
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use core::sync::atomic::Ordering;

    // -- Size / layout assertions --

    #[test]
    fn size_of_logical_address() {
        assert_eq!(core::mem::size_of::<LogicalAddress>(), 8);
    }

    #[test]
    fn size_of_page() {
        assert_eq!(core::mem::size_of::<Page>(), 4);
    }

    #[test]
    fn size_of_offset() {
        assert_eq!(core::mem::size_of::<Offset>(), 4);
    }

    #[test]
    fn size_of_atomic_logical_address() {
        assert_eq!(core::mem::size_of::<AtomicLogicalAddress>(), 8);
    }

    // -- Constants --

    #[test]
    fn constants_match_cpp() {
        // C++: kInvalidAddress = 1
        assert_eq!(LogicalAddress::INVALID.raw(), 1);
        // C++: kMaxAddress = (1 << 48) - 1
        assert_eq!(LogicalAddress::MAX.raw(), (1u64 << 48) - 1);
        // Zero is the default (empty hash bucket entry)
        assert_eq!(LogicalAddress::ZERO.raw(), 0);
        // Bit constants
        assert_eq!(ADDRESS_BITS, 48);
        #[cfg(not(feature = "small-pages"))]
        {
            assert_eq!(OFFSET_BITS, 25);
            assert_eq!(PAGE_BITS, 23);
            assert_eq!(MAX_OFFSET, (1u32 << 25) - 1);
            assert_eq!(MAX_PAGE, (1u32 << 23) - 1);
        }
        #[cfg(feature = "small-pages")]
        {
            assert_eq!(OFFSET_BITS, 16);
            assert_eq!(PAGE_BITS, 32);
            assert_eq!(MAX_OFFSET, (1u32 << 16) - 1);
        }
    }

    // -- Page/Offset extraction --

    #[test]
    fn new_then_page_offset() {
        let addr = LogicalAddress::new(Page(42), Offset(1024));
        assert_eq!(addr.page(), Page(42));
        assert_eq!(addr.offset(), Offset(1024));
    }

    #[test]
    fn page_zero_offset_zero() {
        let addr = LogicalAddress::new(Page(0), Offset(0));
        assert_eq!(addr.page(), Page(0));
        assert_eq!(addr.offset(), Offset(0));
        assert_eq!(addr.raw(), 0);
    }

    #[test]
    fn max_page_max_offset() {
        let addr = LogicalAddress::new(Page(MAX_PAGE), Offset(MAX_OFFSET));
        assert_eq!(addr.page(), Page(MAX_PAGE));
        assert_eq!(addr.offset(), Offset(MAX_OFFSET));
        assert_eq!(addr.raw(), MAX_ADDRESS);
    }

    #[test]
    fn page_only() {
        let addr = LogicalAddress::new(Page(1), Offset(0));
        assert_eq!(addr.raw(), 1u64 << OFFSET_BITS);
        assert_eq!(addr.page(), Page(1));
        assert_eq!(addr.offset(), Offset(0));
    }

    #[test]
    fn offset_only() {
        let addr = LogicalAddress::new(Page(0), Offset(1));
        assert_eq!(addr.raw(), 1);
        assert_eq!(addr.page(), Page(0));
        assert_eq!(addr.offset(), Offset(1));
    }

    // -- Validity --

    #[test]
    fn validity() {
        assert!(!LogicalAddress::ZERO.is_valid());
        assert!(!LogicalAddress::INVALID.is_valid());
        assert!(LogicalAddress::MIN_VALID.is_valid());
        assert!(LogicalAddress::new(Page(0), Offset(2)).is_valid());
        assert!(LogicalAddress::MAX.is_valid());
    }

    #[test]
    fn is_null() {
        assert!(LogicalAddress::ZERO.is_null());
        assert!(!LogicalAddress::INVALID.is_null());
        assert!(!LogicalAddress::new(Page(1), Offset(0)).is_null());
    }

    // -- Read cache bit --

    #[test]
    fn read_cache_bit() {
        // Bit 47 set → in read cache
        let rc_addr = LogicalAddress::from_raw(READ_CACHE_BIT | 100);
        assert!(rc_addr.in_read_cache());

        // Normal address → not in read cache
        let normal = LogicalAddress::new(Page(5), Offset(100));
        assert!(!normal.in_read_cache());
    }

    // -- from_raw / raw round-trip --

    #[test]
    fn from_raw_masks_upper_bits() {
        let with_upper = 0xFFFF_0000_0000_0042u64;
        let addr = LogicalAddress::from_raw(with_upper);
        // Upper 16 bits stripped
        assert_eq!(addr.raw(), 0x0000_0000_0000_0042);
    }

    #[test]
    fn from_u64_trait() {
        let addr: LogicalAddress = 0x42u64.into();
        assert_eq!(addr.raw(), 0x42);
        let raw: u64 = addr.into();
        assert_eq!(raw, 0x42);
    }

    // -- Ordering --

    #[test]
    fn ordering_is_by_raw_value() {
        let a = LogicalAddress::new(Page(0), Offset(100));
        let b = LogicalAddress::new(Page(1), Offset(0));
        assert!(a < b, "page 0 address should be less than page 1 address");

        let c = LogicalAddress::new(Page(1), Offset(50));
        let d = LogicalAddress::new(Page(1), Offset(100));
        assert!(c < d, "same page, smaller offset should be less");
    }

    // -- Display / Debug --

    #[test]
    fn display_format() {
        let addr = LogicalAddress::new(Page(10), Offset(256));
        assert_eq!(format!("{addr}"), "10:256");
    }

    #[test]
    fn debug_format() {
        let addr = LogicalAddress::new(Page(1), Offset(0));
        let dbg = format!("{addr:?}");
        assert!(dbg.contains("LogicalAddress"));
        assert!(dbg.contains("page=1"));
        assert!(dbg.contains("offset=0"));
    }

    // -- Default --

    #[test]
    fn default_is_zero() {
        assert_eq!(LogicalAddress::default(), LogicalAddress::ZERO);
    }

    // -- AtomicLogicalAddress --

    #[test]
    fn atomic_load_store() {
        let atomic = AtomicLogicalAddress::new(LogicalAddress::ZERO);
        let addr = LogicalAddress::new(Page(7), Offset(512));
        atomic.store(addr, Ordering::SeqCst);
        assert_eq!(atomic.load(Ordering::SeqCst), addr);
    }

    #[test]
    fn atomic_compare_exchange_success() {
        let atomic = AtomicLogicalAddress::new(LogicalAddress::ZERO);
        let new_addr = LogicalAddress::new(Page(1), Offset(0));

        let result = atomic.compare_exchange(
            LogicalAddress::ZERO,
            new_addr,
            Ordering::AcqRel,
            Ordering::Acquire,
        );

        assert_eq!(result, Ok(LogicalAddress::ZERO));
        assert_eq!(atomic.load(Ordering::Acquire), new_addr);
    }

    #[test]
    fn atomic_compare_exchange_failure() {
        let addr_a = LogicalAddress::new(Page(1), Offset(0));
        let addr_b = LogicalAddress::new(Page(2), Offset(0));
        let atomic = AtomicLogicalAddress::new(addr_a);

        // Try to CAS from ZERO → addr_b, but actual is addr_a
        let result = atomic.compare_exchange(
            LogicalAddress::ZERO,
            addr_b,
            Ordering::AcqRel,
            Ordering::Acquire,
        );

        assert_eq!(result, Err(addr_a));
        // Value unchanged
        assert_eq!(atomic.load(Ordering::Acquire), addr_a);
    }

    #[test]
    fn atomic_compare_exchange_weak_success() {
        let atomic = AtomicLogicalAddress::new(LogicalAddress::ZERO);
        let new_addr = LogicalAddress::new(Page(3), Offset(64));

        // Weak CAS may fail spuriously, so retry in a loop
        loop {
            match atomic.compare_exchange_weak(
                LogicalAddress::ZERO,
                new_addr,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(prev) => {
                    assert_eq!(prev, LogicalAddress::ZERO);
                    break;
                }
                Err(_) => continue,
            }
        }
        assert_eq!(atomic.load(Ordering::Acquire), new_addr);
    }

    #[test]
    fn atomic_default_is_zero() {
        let atomic = AtomicLogicalAddress::default();
        assert_eq!(atomic.load(Ordering::Relaxed), LogicalAddress::ZERO);
    }

    #[test]
    fn atomic_debug() {
        let atomic = AtomicLogicalAddress::new(LogicalAddress::INVALID);
        let dbg = format!("{atomic:?}");
        assert!(dbg.contains("AtomicLogicalAddress"));
    }

    // -- Page / Offset newtypes --

    #[test]
    fn page_traits() {
        let p = Page(100);
        let p2 = p; // Copy
        assert_eq!(p, p2); // PartialEq
        assert!(Page(1) < Page(2)); // Ord

        let mut pages = vec![Page(3), Page(1), Page(2)];
        pages.sort();
        assert_eq!(pages, vec![Page(1), Page(2), Page(3)]);

        // Hash
        use std::collections::HashSet;
        let mut set = HashSet::new();
        set.insert(Page(42));
        assert!(set.contains(&Page(42)));
    }

    #[test]
    fn offset_traits() {
        let o = Offset(512);
        let o2 = o; // Copy
        assert_eq!(o, o2);
        assert!(Offset(0) < Offset(1));

        // Default
        assert_eq!(Offset::default(), Offset(0));
        assert_eq!(Page::default(), Page(0));
    }

    #[test]
    fn page_display_debug() {
        assert_eq!(format!("{}", Page(5)), "Page(5)");
        assert_eq!(format!("{:?}", Page(5)), "Page(5)");
    }

    #[test]
    fn offset_display_debug() {
        assert_eq!(format!("{}", Offset(42)), "Offset(42)");
        assert_eq!(format!("{:?}", Offset(42)), "Offset(42)");
    }
}

// ---------------------------------------------------------------------------
// Property-based tests (proptest)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod proptests {
    use super::*;
    use proptest::prelude::*;

    prop_compose! {
        /// Generates a valid page number in [0, MAX_PAGE].
        fn arb_page()(p in 0..=MAX_PAGE) -> Page {
            Page(p)
        }
    }

    prop_compose! {
        /// Generates a valid offset in [0, MAX_OFFSET].
        fn arb_offset()(o in 0..=MAX_OFFSET) -> Offset {
            Offset(o)
        }
    }

    proptest! {
        /// Round-trip: `new(page, offset)` → `page()` / `offset()` → `new` is identity.
        #[test]
        fn round_trip_page_offset(page in 0..=MAX_PAGE, offset in 0..=MAX_OFFSET) {
            let addr = LogicalAddress::new(Page(page), Offset(offset));
            prop_assert_eq!(addr.page(), Page(page));
            prop_assert_eq!(addr.offset(), Offset(offset));

            // Reconstruct and compare
            let addr2 = LogicalAddress::new(addr.page(), addr.offset());
            prop_assert_eq!(addr, addr2);
        }

        /// Raw round-trip: `from_raw(addr.raw()) == addr` for any valid address.
        #[test]
        fn round_trip_raw(page in 0..=MAX_PAGE, offset in 0..=MAX_OFFSET) {
            let addr = LogicalAddress::new(Page(page), Offset(offset));
            let raw = addr.raw();
            prop_assert_eq!(LogicalAddress::from_raw(raw), addr);
        }

        /// `from_raw` strips upper 16 bits.
        #[test]
        fn from_raw_strips_upper_bits(raw in any::<u64>()) {
            let addr = LogicalAddress::from_raw(raw);
            prop_assert_eq!(addr.raw(), raw & MAX_ADDRESS);
        }

        /// Ordering consistency: for any two addresses, their Ord agrees with
        /// the underlying `u64` ordering.
        #[test]
        fn ordering_consistent_with_raw(
            p1 in 0..=MAX_PAGE, o1 in 0..=MAX_OFFSET,
            p2 in 0..=MAX_PAGE, o2 in 0..=MAX_OFFSET,
        ) {
            let a = LogicalAddress::new(Page(p1), Offset(o1));
            let b = LogicalAddress::new(Page(p2), Offset(o2));
            prop_assert_eq!(a.cmp(&b), a.raw().cmp(&b.raw()));
        }

        /// Validity: only addresses > 1 (INVALID) are valid.
        #[test]
        fn validity_property(page in 0..=MAX_PAGE, offset in 0..=MAX_OFFSET) {
            let addr = LogicalAddress::new(Page(page), Offset(offset));
            prop_assert_eq!(addr.is_valid(), addr.raw() > 1);
        }

        /// Nullness: only the zero address is null.
        #[test]
        fn null_property(page in 0..=MAX_PAGE, offset in 0..=MAX_OFFSET) {
            let addr = LogicalAddress::new(Page(page), Offset(offset));
            prop_assert_eq!(addr.is_null(), addr.raw() == 0);
        }
    }
}
