//! Record header (metadata stored with every record).
//!
//! [`RecordInfo`] is an 8-byte packed header at the start of every record in
//! the hybrid log. It encodes the previous-version chain pointer, checkpoint
//! version, and status flags into a single `u64`.
//!
//! # Bit layout (matches C++ `record.h` exactly)
//!
//! ```text
//!   63    62    61    60     59..48           47..0
//! ┌─────┬─────┬─────┬──────┬──────────────┬───────────────────────────────┐
//! │ Fin │ Tom │ Inv │ Seal │ Version (12) │ Previous Address (48)         │
//! └─────┴─────┴─────┴──────┴──────────────┴───────────────────────────────┘
//! ```
//!
//! - **Previous Address (bits 0..47):** 48-bit [`LogicalAddress`] pointing to
//!   the prior version of this key in the log (version chain).
//! - **Checkpoint Version (bits 48..59):** 12-bit version number identifying
//!   the checkpoint epoch when this record was written (max 4095).
//! - **Sealed (bit 60):** Write-protection flag for RCU transitions. When set,
//!   in-place updates (Upsert/RMW) must fall back to copy-to-tail. Delete
//!   (tombstone placement) is still permitted on sealed records.
//! - **Invalid (bit 61):** Record has been superseded in the mutable region.
//!   Readers skip invalid records.
//! - **Tombstone (bit 62):** Logical deletion marker — key was deleted.
//!   Readers treat this as "not found."
//! - **Final (bit 63):** Used during CPR (Concurrent Prefix Recovery)
//!   coordination. Reserved for read-cache flags in future versions.

use core::fmt;
use core::sync::atomic::{AtomicU64, Ordering};

use crate::address::LogicalAddress;

// ── Bit layout constants (matching C++ record.h) ─────────────────────

/// Mask for the 48-bit previous address field (bits 0..47).
const PREVIOUS_ADDR_MASK: u64 = (1u64 << 48) - 1;

/// Bit position where the 12-bit checkpoint version field starts.
const VERSION_SHIFT: u32 = 48;

/// Mask for the 12-bit checkpoint version field (bits 48..59).
const VERSION_MASK: u64 = 0x0FFF_0000_0000_0000;

/// Maximum checkpoint version value (2^12 − 1 = 4095).
pub(crate) const MAX_VERSION: u16 = (1u16 << 12) - 1;

/// Bit 60: sealed — write-protected during RCU transition.
///
/// When set, in-place Upsert and RMW must copy-to-tail instead.
/// Delete (tombstone) is still permitted on sealed records.
const SEALED_BIT: u64 = 1u64 << 60;

/// Bit 61: record has been invalidated (superseded by a newer record).
const INVALID_BIT: u64 = 1u64 << 61;

/// Bit 62: tombstone — logical deletion marker.
const TOMBSTONE_BIT: u64 = 1u64 << 62;

/// Bit 63: final bit — used during CPR coordination.
const FINAL_BIT: u64 = 1u64 << 63;

// ── RecordInfo ───────────────────────────────────────────────────────

/// Record header — packed into 8 bytes, stored at the start of every record.
///
/// Layout matches C++ `RecordInfo` in `record.h` for behavioral compatibility.
/// See module-level docs for the bit-field diagram.
///
/// # Examples
///
/// ```
/// use faster_core::address::{LogicalAddress, Page, Offset};
/// use faster_core::record::RecordInfo;
///
/// let prev = LogicalAddress::new(Page(10), Offset(256));
/// let info = RecordInfo::new(prev, 7, false, false, false);
///
/// assert_eq!(info.previous_address(), prev);
/// assert_eq!(info.checkpoint_version(), 7);
/// assert!(!info.is_tombstone());
/// assert!(!info.is_invalid());
/// assert!(!info.is_final());
/// ```
#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(transparent)]
pub struct RecordInfo(u64);

// Layout assertions: RecordInfo is the 8-byte header at the start of every log
// record, read via pointer casts in record_ops.rs. Changing size breaks the
// on-disk record format and pointer reinterpretation as AtomicRecordInfo.
const _: () = assert!(core::mem::size_of::<RecordInfo>() == 8);
const _: () = assert!(core::mem::align_of::<RecordInfo>() == 8);

impl RecordInfo {
    /// Creates a new `RecordInfo` with the given field values.
    ///
    /// # Panics (debug only)
    ///
    /// Debug-asserts that `checkpoint_version` fits in 12 bits (≤ 4095).
    ///
    /// # Examples
    ///
    /// ```
    /// use faster_core::address::LogicalAddress;
    /// use faster_core::record::RecordInfo;
    ///
    /// let info = RecordInfo::new(LogicalAddress::INVALID, 0, false, false, false);
    /// assert_eq!(info.previous_address(), LogicalAddress::INVALID);
    /// assert_eq!(info.checkpoint_version(), 0);
    /// ```
    #[inline]
    pub const fn new(
        previous_address: LogicalAddress,
        checkpoint_version: u16,
        invalid: bool,
        tombstone: bool,
        final_bit: bool,
    ) -> Self {
        debug_assert!(
            checkpoint_version <= MAX_VERSION,
            "checkpoint_version exceeds 12-bit max (4095)"
        );

        let mut bits = previous_address.raw() & PREVIOUS_ADDR_MASK;
        bits |= (checkpoint_version as u64) << VERSION_SHIFT;
        if invalid {
            bits |= INVALID_BIT;
        }
        if tombstone {
            bits |= TOMBSTONE_BIT;
        }
        if final_bit {
            bits |= FINAL_BIT;
        }
        Self(bits)
    }

    /// Creates a `RecordInfo` from a raw `u64` bit pattern.
    ///
    /// No validation is performed — the caller is responsible for ensuring
    /// the bit pattern is valid.
    ///
    /// # Examples
    ///
    /// ```
    /// use faster_core::record::RecordInfo;
    ///
    /// let info = RecordInfo::from_raw(0);
    /// assert!(info.is_null());
    /// ```
    #[inline(always)]
    pub const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    /// Returns the raw `u64` representation of this record header.
    #[inline(always)]
    pub const fn raw(self) -> u64 {
        self.0
    }

    /// Returns `true` if all bits are zero (null/empty record).
    #[inline(always)]
    pub const fn is_null(self) -> bool {
        self.0 == 0
    }

    /// Extracts the 48-bit previous address (version chain pointer).
    ///
    /// # Examples
    ///
    /// ```
    /// use faster_core::address::{LogicalAddress, Page, Offset};
    /// use faster_core::record::RecordInfo;
    ///
    /// let addr = LogicalAddress::new(Page(5), Offset(100));
    /// let info = RecordInfo::new(addr, 0, false, false, false);
    /// assert_eq!(info.previous_address(), addr);
    /// ```
    #[inline(always)]
    pub const fn previous_address(self) -> LogicalAddress {
        LogicalAddress::from_raw(self.0 & PREVIOUS_ADDR_MASK)
    }

    /// Extracts the 12-bit checkpoint version (0..=4095).
    ///
    /// # Examples
    ///
    /// ```
    /// use faster_core::address::LogicalAddress;
    /// use faster_core::record::RecordInfo;
    ///
    /// let info = RecordInfo::new(LogicalAddress::ZERO, 42, false, false, false);
    /// assert_eq!(info.checkpoint_version(), 42);
    /// ```
    #[inline(always)]
    pub const fn checkpoint_version(self) -> u16 {
        ((self.0 & VERSION_MASK) >> VERSION_SHIFT) as u16
    }

    /// Returns `true` if the invalid bit (bit 61) is set.
    ///
    /// An invalid record has been superseded by a newer record at the log
    /// tail. Readers skip invalid records during chain traversal.
    #[inline(always)]
    pub const fn is_invalid(self) -> bool {
        self.0 & INVALID_BIT != 0
    }

    /// Returns `true` if the tombstone bit (bit 62) is set.
    ///
    /// A tombstone marks a logically deleted key. Readers treat this as
    /// "not found."
    #[inline(always)]
    pub const fn is_tombstone(self) -> bool {
        self.0 & TOMBSTONE_BIT != 0
    }

    /// Returns `true` if the final bit (bit 63) is set.
    ///
    /// Used during CPR (Concurrent Prefix Recovery) coordination.
    #[inline(always)]
    pub const fn is_final(self) -> bool {
        self.0 & FINAL_BIT != 0
    }

    /// Returns a copy with the invalid bit set.
    #[inline]
    pub const fn with_invalid(self) -> Self {
        Self(self.0 | INVALID_BIT)
    }

    /// Returns a copy with the tombstone bit set.
    #[inline]
    pub const fn with_tombstone(self) -> Self {
        Self(self.0 | TOMBSTONE_BIT)
    }

    /// Returns a copy with the final bit set.
    #[inline]
    pub const fn with_final(self) -> Self {
        Self(self.0 | FINAL_BIT)
    }

    /// Returns `true` if the sealed bit (bit 60) is set.
    ///
    /// A sealed record is write-protected: in-place Upsert and RMW
    /// must fall back to copy-to-tail. Delete is still permitted.
    #[inline(always)]
    pub const fn is_sealed(self) -> bool {
        self.0 & SEALED_BIT != 0
    }

    /// Returns a copy with the sealed bit set.
    #[inline]
    pub const fn with_sealed(self) -> Self {
        Self(self.0 | SEALED_BIT)
    }

    /// Returns a copy with the sealed bit cleared.
    ///
    /// Used during **revivification**: when an in-place update is performed
    /// on a previously sealed record, the sealed bit must be cleared to
    /// allow future in-place updates without copy-to-tail overhead.
    #[inline]
    pub const fn with_sealed_cleared(self) -> Self {
        Self(self.0 & !SEALED_BIT)
    }

    /// Returns a copy with the previous address replaced.
    #[inline]
    pub const fn with_previous_address(self, addr: LogicalAddress) -> Self {
        Self((self.0 & !PREVIOUS_ADDR_MASK) | (addr.raw() & PREVIOUS_ADDR_MASK))
    }

    /// Returns a copy with the checkpoint version replaced.
    ///
    /// # Panics (debug only)
    ///
    /// Debug-asserts that `version` ≤ 4095.
    #[inline]
    pub const fn with_checkpoint_version(self, version: u16) -> Self {
        debug_assert!(version <= MAX_VERSION, "version exceeds 12-bit max");
        Self((self.0 & !VERSION_MASK) | ((version as u64) << VERSION_SHIFT))
    }
}

impl Default for RecordInfo {
    /// Defaults to a null record (all bits zero).
    fn default() -> Self {
        Self(0)
    }
}

impl fmt::Debug for RecordInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RecordInfo")
            .field("previous_address", &self.previous_address())
            .field("checkpoint_version", &self.checkpoint_version())
            .field("sealed", &self.is_sealed())
            .field("invalid", &self.is_invalid())
            .field("tombstone", &self.is_tombstone())
            .field("final", &self.is_final())
            .field("raw", &format_args!("0x{:016x}", self.0))
            .finish()
    }
}

impl fmt::Display for RecordInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "RecordInfo(prev={}, v={}{}{}{}{})",
            self.previous_address(),
            self.checkpoint_version(),
            if self.is_sealed() { " S" } else { "" },
            if self.is_invalid() { " I" } else { "" },
            if self.is_tombstone() { " T" } else { "" },
            if self.is_final() { " F" } else { "" },
        )
    }
}

// ── AtomicRecordInfo ─────────────────────────────────────────────────

/// Thread-safe atomic wrapper around [`RecordInfo`].
///
/// Used when records in the mutable log region need concurrent updates
/// (e.g., setting the invalid or tombstone bits via `fetch_or`). All
/// methods take an explicit [`Ordering`] parameter — callers choose the
/// memory ordering appropriate for their use case.
///
/// # Examples
///
/// ```
/// use core::sync::atomic::Ordering;
/// use faster_core::address::LogicalAddress;
/// use faster_core::record::{RecordInfo, AtomicRecordInfo};
///
/// let info = RecordInfo::new(LogicalAddress::ZERO, 1, false, false, false);
/// let atomic = AtomicRecordInfo::new(info);
///
/// // Atomically mark as tombstone
/// atomic.set_tombstone(Ordering::Release);
/// let loaded = atomic.load(Ordering::Acquire);
/// assert!(loaded.is_tombstone());
/// assert_eq!(loaded.checkpoint_version(), 1);
/// ```
#[repr(transparent)]
pub struct AtomicRecordInfo(AtomicU64);

// Layout assertions: AtomicRecordInfo is created by pointer-casting a raw
// `*const u8` in record_ops.rs (lines ~110, ~299). Must match RecordInfo in
// size so the cast is valid. Changing layout breaks CAS on record headers.
const _: () = assert!(core::mem::size_of::<AtomicRecordInfo>() == 8);
const _: () = assert!(core::mem::align_of::<AtomicRecordInfo>() == 8);

impl AtomicRecordInfo {
    /// Creates a new `AtomicRecordInfo` with the given initial value.
    pub const fn new(info: RecordInfo) -> Self {
        Self(AtomicU64::new(info.0))
    }

    /// Loads the record info with the given ordering.
    #[inline]
    pub fn load(&self, order: Ordering) -> RecordInfo {
        RecordInfo(self.0.load(order))
    }

    /// Stores the record info with the given ordering.
    #[inline]
    pub fn store(&self, info: RecordInfo, order: Ordering) {
        self.0.store(info.0, order);
    }

    /// Atomically sets the invalid bit. Returns the *previous* value.
    #[inline]
    pub fn set_invalid(&self, order: Ordering) -> RecordInfo {
        RecordInfo(self.0.fetch_or(INVALID_BIT, order))
    }

    /// Atomically sets the tombstone bit. Returns the *previous* value.
    #[inline]
    pub fn set_tombstone(&self, order: Ordering) -> RecordInfo {
        RecordInfo(self.0.fetch_or(TOMBSTONE_BIT, order))
    }

    /// Atomically sets the final bit. Returns the *previous* value.
    #[inline]
    pub fn set_final(&self, order: Ordering) -> RecordInfo {
        RecordInfo(self.0.fetch_or(FINAL_BIT, order))
    }

    /// Atomically sets the sealed bit (bit 60) with `Release` ordering.
    ///
    /// Uses `Release` so that all prior writes to the record's value
    /// are visible to threads that later observe the sealed flag.
    /// Returns the *previous* value.
    #[inline]
    pub fn seal(&self) -> RecordInfo {
        RecordInfo(self.0.fetch_or(SEALED_BIT, Ordering::Release))
    }

    /// Loads the record info with `Acquire` ordering and checks the
    /// sealed bit.
    ///
    /// Uses `Acquire` to pair with the `Release` in [`seal()`](Self::seal),
    /// ensuring all record writes before sealing are visible.
    #[inline]
    pub fn is_sealed(&self) -> bool {
        self.load(Ordering::Acquire).is_sealed()
    }

    /// Attempts to seal the record via compare-and-swap.
    ///
    /// Returns `Ok(previous)` if the CAS succeeded (sealed bit was not
    /// already set), or `Err(actual)` if it failed. Uses `AcqRel`
    /// ordering on success and `Acquire` on failure.
    #[inline]
    pub fn try_seal(&self) -> Result<RecordInfo, RecordInfo> {
        let current = self.load(Ordering::Acquire);
        if current.is_sealed() {
            return Err(current);
        }
        let desired = current.with_sealed();
        self.compare_exchange(current, desired, Ordering::AcqRel, Ordering::Acquire)
    }

    /// Attempts to **revivify** a sealed record via compare-and-swap.
    ///
    /// Atomically clears the sealed bit if the current header matches
    /// `expected`. This is the CAS primitive for revivification: only one
    /// thread can win the race to unseal a record for in-place update.
    ///
    /// Returns `Ok(previous)` on success, `Err(actual)` on failure.
    /// Uses `AcqRel` on success, `Acquire` on failure.
    #[inline]
    pub fn try_revivify(&self, expected: RecordInfo) -> Result<RecordInfo, RecordInfo> {
        debug_assert!(
            expected.is_sealed(),
            "try_revivify called with unsealed RecordInfo"
        );
        let desired = expected.with_sealed_cleared();
        self.compare_exchange(expected, desired, Ordering::AcqRel, Ordering::Acquire)
    }

    /// Compare-and-swap. Returns `Ok(current)` on success, `Err(actual)` on
    /// failure.
    #[inline]
    pub fn compare_exchange(
        &self,
        current: RecordInfo,
        new: RecordInfo,
        success: Ordering,
        failure: Ordering,
    ) -> Result<RecordInfo, RecordInfo> {
        self.0
            .compare_exchange(current.0, new.0, success, failure)
            .map(RecordInfo)
            .map_err(RecordInfo)
    }

    /// Weak compare-and-swap (may spuriously fail).
    #[inline]
    pub fn compare_exchange_weak(
        &self,
        current: RecordInfo,
        new: RecordInfo,
        success: Ordering,
        failure: Ordering,
    ) -> Result<RecordInfo, RecordInfo> {
        self.0
            .compare_exchange_weak(current.0, new.0, success, failure)
            .map(RecordInfo)
            .map_err(RecordInfo)
    }
}

impl fmt::Debug for AtomicRecordInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "AtomicRecordInfo({:?})", self.load(Ordering::Relaxed))
    }
}

// ── Tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::address::{Offset, Page};

    // ── Size / layout ───────────────────────────────────────────────

    #[test]
    fn record_info_is_exactly_8_bytes() {
        assert_eq!(core::mem::size_of::<RecordInfo>(), 8);
    }

    #[test]
    fn atomic_record_info_is_exactly_8_bytes() {
        assert_eq!(core::mem::size_of::<AtomicRecordInfo>(), 8);
    }

    // ── Constructor / field extraction ───────────────────────────────

    #[test]
    fn new_with_all_false_flags() {
        let addr = LogicalAddress::new(Page(100), Offset(512));
        let info = RecordInfo::new(addr, 42, false, false, false);
        assert_eq!(info.previous_address(), addr);
        assert_eq!(info.checkpoint_version(), 42);
        assert!(!info.is_invalid());
        assert!(!info.is_tombstone());
        assert!(!info.is_final());
    }

    #[test]
    fn new_with_all_true_flags() {
        let addr = LogicalAddress::new(Page(0), Offset(0));
        let info = RecordInfo::new(addr, 4095, true, true, true);
        assert_eq!(info.previous_address(), addr);
        assert_eq!(info.checkpoint_version(), 4095);
        assert!(info.is_invalid());
        assert!(info.is_tombstone());
        assert!(info.is_final());
    }

    #[test]
    fn previous_address_preserves_48_bits() {
        let addr = LogicalAddress::MAX;
        let info = RecordInfo::new(addr, 0, false, false, false);
        assert_eq!(info.previous_address(), addr);
    }

    #[test]
    fn checkpoint_version_max() {
        let info = RecordInfo::new(LogicalAddress::ZERO, MAX_VERSION, false, false, false);
        assert_eq!(info.checkpoint_version(), MAX_VERSION);
    }

    #[test]
    fn individual_flag_bits() {
        let base = LogicalAddress::ZERO;

        let inv = RecordInfo::new(base, 0, true, false, false);
        assert!(inv.is_invalid());
        assert!(!inv.is_tombstone());
        assert!(!inv.is_final());

        let tomb = RecordInfo::new(base, 0, false, true, false);
        assert!(!tomb.is_invalid());
        assert!(tomb.is_tombstone());
        assert!(!tomb.is_final());

        let fin = RecordInfo::new(base, 0, false, false, true);
        assert!(!fin.is_invalid());
        assert!(!fin.is_tombstone());
        assert!(fin.is_final());
    }

    // ── Bit layout verification (C++ compatibility) ─────────────────

    #[test]
    fn bit_layout_matches_cpp() {
        // C++ bitfield order (LSB to MSB on little-endian):
        //   previous_address_ : 48  (bits 0..47)
        //   checkpoint_version: 13  (bits 48..60)
        //   invalid           :  1  (bit 61)
        //   tombstone         :  1  (bit 62)
        //   final             :  1  (bit 63)

        // previous_address only → should be in lower 48 bits
        let info = RecordInfo::new(
            LogicalAddress::from_raw(0xABCD_1234_5678),
            0,
            false,
            false,
            false,
        );
        assert_eq!(info.raw(), 0xABCD_1234_5678);

        // checkpoint_version = 1 → bit 48
        let info = RecordInfo::new(LogicalAddress::ZERO, 1, false, false, false);
        assert_eq!(info.raw(), 1u64 << 48);

        // checkpoint_version = 0x0FFF (max) → bits 48..59
        let info = RecordInfo::new(LogicalAddress::ZERO, 0x0FFF, false, false, false);
        assert_eq!(info.raw(), 0x0FFF_u64 << 48);

        // invalid bit → bit 61
        let info = RecordInfo::new(LogicalAddress::ZERO, 0, true, false, false);
        assert_eq!(info.raw(), 1u64 << 61);

        // tombstone bit → bit 62
        let info = RecordInfo::new(LogicalAddress::ZERO, 0, false, true, false);
        assert_eq!(info.raw(), 1u64 << 62);

        // final bit → bit 63
        let info = RecordInfo::new(LogicalAddress::ZERO, 0, false, false, true);
        assert_eq!(info.raw(), 1u64 << 63);
    }

    #[test]
    fn all_fields_packed_correctly() {
        let addr = LogicalAddress::from_raw(0x0000_DEAD_BEEF_1234);
        let info = RecordInfo::new(addr, 0x0ABC, true, true, true);

        let raw = info.raw();
        // Previous address: lower 48 bits
        assert_eq!(raw & PREVIOUS_ADDR_MASK, 0x0000_DEAD_BEEF_1234);
        // Version: bits 48..59
        assert_eq!((raw >> 48) & 0x0FFF, 0x0ABC);
        // Invalid: bit 61
        assert_ne!(raw & (1u64 << 61), 0);
        // Tombstone: bit 62
        assert_ne!(raw & (1u64 << 62), 0);
        // Final: bit 63
        assert_ne!(raw & (1u64 << 63), 0);
    }

    // ── from_raw / raw round-trip ───────────────────────────────────

    #[test]
    fn from_raw_round_trip() {
        let original = 0xF123_4567_89AB_CDEFu64;
        let info = RecordInfo::from_raw(original);
        assert_eq!(info.raw(), original);
    }

    #[test]
    fn is_null() {
        assert!(RecordInfo::from_raw(0).is_null());
        assert!(!RecordInfo::from_raw(1).is_null());
        assert!(RecordInfo::default().is_null());
    }

    // ── Builder-style methods ───────────────────────────────────────

    #[test]
    fn with_invalid() {
        let info = RecordInfo::new(LogicalAddress::ZERO, 5, false, false, false);
        let invalid = info.with_invalid();
        assert!(invalid.is_invalid());
        assert_eq!(invalid.checkpoint_version(), 5);
        assert!(!invalid.is_tombstone());
    }

    #[test]
    fn with_tombstone() {
        let info = RecordInfo::new(LogicalAddress::ZERO, 5, false, false, false);
        let tomb = info.with_tombstone();
        assert!(tomb.is_tombstone());
        assert!(!tomb.is_invalid());
    }

    #[test]
    fn with_final() {
        let info = RecordInfo::new(LogicalAddress::ZERO, 5, false, false, false);
        let fin = info.with_final();
        assert!(fin.is_final());
        assert!(!fin.is_invalid());
    }

    #[test]
    fn with_previous_address() {
        let info = RecordInfo::new(LogicalAddress::ZERO, 5, true, true, true);
        let addr = LogicalAddress::new(Page(99), Offset(42));
        let updated = info.with_previous_address(addr);
        assert_eq!(updated.previous_address(), addr);
        assert_eq!(updated.checkpoint_version(), 5);
        assert!(updated.is_invalid());
        assert!(updated.is_tombstone());
        assert!(updated.is_final());
    }

    #[test]
    fn with_checkpoint_version() {
        let addr = LogicalAddress::new(Page(1), Offset(2));
        let info = RecordInfo::new(addr, 0, true, false, true);
        let updated = info.with_checkpoint_version(100);
        assert_eq!(updated.checkpoint_version(), 100);
        assert_eq!(updated.previous_address(), addr);
        assert!(updated.is_invalid());
        assert!(updated.is_final());
    }

    // ── Debug / Display ─────────────────────────────────────────────

    #[test]
    fn debug_format() {
        let info = RecordInfo::new(LogicalAddress::ZERO, 1, true, false, false);
        let dbg = format!("{info:?}");
        assert!(dbg.contains("RecordInfo"));
        assert!(dbg.contains("invalid: true"));
        assert!(dbg.contains("checkpoint_version: 1"));
    }

    #[test]
    fn display_format() {
        let info = RecordInfo::new(LogicalAddress::ZERO, 7, false, true, false);
        let s = format!("{info}");
        assert!(s.contains("v=7"));
        assert!(s.contains(" T"));
    }

    // ── Send / Sync ─────────────────────────────────────────────────

    #[test]
    fn record_info_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<RecordInfo>();
    }

    #[test]
    fn atomic_record_info_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<AtomicRecordInfo>();
    }

    // ── AtomicRecordInfo operations ─────────────────────────────────

    #[test]
    fn atomic_load_store() {
        let info = RecordInfo::new(LogicalAddress::ZERO, 10, false, false, false);
        let atomic = AtomicRecordInfo::new(info);

        let loaded = atomic.load(Ordering::SeqCst);
        assert_eq!(loaded, info);

        let new_info = RecordInfo::new(LogicalAddress::ZERO, 20, true, false, false);
        atomic.store(new_info, Ordering::SeqCst);
        assert_eq!(atomic.load(Ordering::SeqCst), new_info);
    }

    #[test]
    fn atomic_set_invalid() {
        let info = RecordInfo::new(LogicalAddress::ZERO, 5, false, false, false);
        let atomic = AtomicRecordInfo::new(info);

        let prev = atomic.set_invalid(Ordering::SeqCst);
        assert!(!prev.is_invalid()); // was not set before
        assert!(atomic.load(Ordering::SeqCst).is_invalid()); // now set
        assert_eq!(atomic.load(Ordering::SeqCst).checkpoint_version(), 5);
    }

    #[test]
    fn atomic_set_tombstone() {
        let info = RecordInfo::new(LogicalAddress::ZERO, 5, false, false, false);
        let atomic = AtomicRecordInfo::new(info);

        let prev = atomic.set_tombstone(Ordering::SeqCst);
        assert!(!prev.is_tombstone());
        assert!(atomic.load(Ordering::SeqCst).is_tombstone());
    }

    #[test]
    fn atomic_set_final() {
        let info = RecordInfo::new(LogicalAddress::ZERO, 5, false, false, false);
        let atomic = AtomicRecordInfo::new(info);

        let prev = atomic.set_final(Ordering::SeqCst);
        assert!(!prev.is_final());
        assert!(atomic.load(Ordering::SeqCst).is_final());
    }

    #[test]
    fn atomic_compare_exchange_success() {
        let info = RecordInfo::new(LogicalAddress::ZERO, 1, false, false, false);
        let atomic = AtomicRecordInfo::new(info);

        let new_info = info.with_tombstone();
        let result = atomic.compare_exchange(info, new_info, Ordering::SeqCst, Ordering::SeqCst);
        assert!(result.is_ok());
        assert_eq!(atomic.load(Ordering::SeqCst), new_info);
    }

    #[test]
    fn atomic_compare_exchange_failure() {
        let info = RecordInfo::new(LogicalAddress::ZERO, 1, false, false, false);
        let atomic = AtomicRecordInfo::new(info);

        let wrong = RecordInfo::new(LogicalAddress::ZERO, 99, false, false, false);
        let new_info = info.with_tombstone();
        let result = atomic.compare_exchange(wrong, new_info, Ordering::SeqCst, Ordering::SeqCst);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), info); // actual value
    }

    #[test]
    fn atomic_debug() {
        let info = RecordInfo::new(LogicalAddress::ZERO, 1, false, false, false);
        let atomic = AtomicRecordInfo::new(info);
        let dbg = format!("{atomic:?}");
        assert!(dbg.contains("AtomicRecordInfo"));
    }

    // ── Sealed bit tests ────────────────────────────────────────────

    #[test]
    fn sealed_bit_position() {
        // Sealed bit is bit 60.
        let info = RecordInfo::from_raw(1u64 << 60);
        assert!(info.is_sealed());
        assert!(!info.is_invalid());
        assert!(!info.is_tombstone());
        assert!(!info.is_final());
        assert_eq!(info.checkpoint_version(), 0);
    }

    #[test]
    fn new_never_sealed() {
        // Records created via new() are never sealed.
        let info = RecordInfo::new(LogicalAddress::ZERO, 100, true, true, true);
        assert!(!info.is_sealed());
    }

    #[test]
    fn with_sealed() {
        let info = RecordInfo::new(LogicalAddress::ZERO, 42, false, false, false);
        let sealed = info.with_sealed();
        assert!(sealed.is_sealed());
        assert_eq!(sealed.checkpoint_version(), 42);
        assert!(!sealed.is_invalid());
    }

    #[test]
    fn sealed_preserves_all_other_fields() {
        let addr = LogicalAddress::new(Page(77), Offset(999));
        let info = RecordInfo::new(addr, 3000, true, true, true);
        let sealed = info.with_sealed();
        assert!(sealed.is_sealed());
        assert_eq!(sealed.previous_address(), addr);
        assert_eq!(sealed.checkpoint_version(), 3000);
        assert!(sealed.is_invalid());
        assert!(sealed.is_tombstone());
        assert!(sealed.is_final());
    }

    #[test]
    fn sealed_idempotent() {
        let info = RecordInfo::new(LogicalAddress::ZERO, 1, false, false, false).with_sealed();
        let double_sealed = info.with_sealed();
        assert_eq!(info.raw(), double_sealed.raw());
    }

    #[test]
    fn sealed_in_debug_output() {
        let info = RecordInfo::new(LogicalAddress::ZERO, 1, false, false, false).with_sealed();
        let dbg = format!("{info:?}");
        assert!(dbg.contains("sealed: true"));
    }

    #[test]
    fn sealed_in_display_output() {
        let info = RecordInfo::new(LogicalAddress::ZERO, 1, false, false, false).with_sealed();
        let s = format!("{info}");
        assert!(s.contains(" S"));
    }

    #[test]
    fn atomic_seal() {
        let info = RecordInfo::new(LogicalAddress::ZERO, 5, false, false, false);
        let atomic = AtomicRecordInfo::new(info);

        let prev = atomic.seal();
        assert!(!prev.is_sealed());
        assert!(atomic.is_sealed());
        assert_eq!(atomic.load(Ordering::Relaxed).checkpoint_version(), 5);
    }

    #[test]
    fn atomic_seal_idempotent() {
        let info = RecordInfo::new(LogicalAddress::ZERO, 5, false, false, false);
        let atomic = AtomicRecordInfo::new(info);
        atomic.seal();
        let prev = atomic.seal();
        assert!(prev.is_sealed());
        assert!(atomic.is_sealed());
    }

    #[test]
    fn atomic_try_seal_success() {
        let info = RecordInfo::new(LogicalAddress::ZERO, 5, false, false, false);
        let atomic = AtomicRecordInfo::new(info);
        let result = atomic.try_seal();
        assert!(result.is_ok());
        assert!(!result.unwrap().is_sealed()); // previous value wasn't sealed
        assert!(atomic.is_sealed());
    }

    #[test]
    fn atomic_try_seal_already_sealed() {
        let info = RecordInfo::new(LogicalAddress::ZERO, 5, false, false, false);
        let atomic = AtomicRecordInfo::new(info);
        atomic.seal();
        let result = atomic.try_seal();
        assert!(result.is_err());
    }

    #[test]
    fn atomic_try_seal_concurrent_race() {
        // Simulate a CAS race: another thread seals between our load and CAS.
        let info = RecordInfo::new(LogicalAddress::ZERO, 5, false, false, false);
        let atomic = AtomicRecordInfo::new(info);

        // Load the current value (not sealed).
        let current = atomic.load(Ordering::Acquire);
        assert!(!current.is_sealed());

        // "Another thread" seals it.
        atomic.seal();

        // Our CAS should fail.
        let desired = current.with_sealed();
        let result = atomic.compare_exchange(current, desired, Ordering::AcqRel, Ordering::Acquire);
        assert!(result.is_err());
        let actual = result.unwrap_err();
        assert!(actual.is_sealed());
    }

    #[test]
    fn sealed_raw_round_trip() {
        let addr = LogicalAddress::new(Page(42), Offset(256));
        let info = RecordInfo::new(addr, 999, true, false, true).with_sealed();
        let raw = info.raw();
        let restored = RecordInfo::from_raw(raw);
        assert!(restored.is_sealed());
        assert_eq!(restored.previous_address(), addr);
        assert_eq!(restored.checkpoint_version(), 999);
        assert!(restored.is_invalid());
        assert!(!restored.is_tombstone());
        assert!(restored.is_final());
    }

    // ── Property tests ──────────────────────────────────────────────

    mod proptests {
        use super::*;
        use proptest::prelude::*;

        prop_compose! {
            fn arb_logical_address()(raw in 0u64..=(1u64 << 48) - 1) -> LogicalAddress {
                LogicalAddress::from_raw(raw)
            }
        }

        proptest! {
            #[test]
            fn record_info_round_trip(
                addr in arb_logical_address(),
                version in 0u16..=MAX_VERSION,
                sealed in any::<bool>(),
                invalid in any::<bool>(),
                tombstone in any::<bool>(),
                final_bit in any::<bool>(),
            ) {
                let mut info = RecordInfo::new(addr, version, invalid, tombstone, final_bit);
                if sealed { info = info.with_sealed(); }
                prop_assert_eq!(info.previous_address(), addr);
                prop_assert_eq!(info.checkpoint_version(), version);
                prop_assert_eq!(info.is_sealed(), sealed);
                prop_assert_eq!(info.is_invalid(), invalid);
                prop_assert_eq!(info.is_tombstone(), tombstone);
                prop_assert_eq!(info.is_final(), final_bit);
            }

            #[test]
            fn raw_round_trip(raw in any::<u64>()) {
                let info = RecordInfo::from_raw(raw);
                prop_assert_eq!(info.raw(), raw);
            }

            #[test]
            fn with_previous_address_preserves_flags(
                addr1 in arb_logical_address(),
                addr2 in arb_logical_address(),
                version in 0u16..=MAX_VERSION,
                invalid in any::<bool>(),
                tombstone in any::<bool>(),
                final_bit in any::<bool>(),
            ) {
                let info = RecordInfo::new(addr1, version, invalid, tombstone, final_bit);
                let updated = info.with_previous_address(addr2);
                prop_assert_eq!(updated.previous_address(), addr2);
                prop_assert_eq!(updated.checkpoint_version(), version);
                prop_assert_eq!(updated.is_invalid(), invalid);
                prop_assert_eq!(updated.is_tombstone(), tombstone);
                prop_assert_eq!(updated.is_final(), final_bit);
            }

            #[test]
            fn with_version_preserves_other_fields(
                addr in arb_logical_address(),
                v1 in 0u16..=MAX_VERSION,
                v2 in 0u16..=MAX_VERSION,
                invalid in any::<bool>(),
                tombstone in any::<bool>(),
                final_bit in any::<bool>(),
            ) {
                let info = RecordInfo::new(addr, v1, invalid, tombstone, final_bit);
                let updated = info.with_checkpoint_version(v2);
                prop_assert_eq!(updated.previous_address(), addr);
                prop_assert_eq!(updated.checkpoint_version(), v2);
                prop_assert_eq!(updated.is_invalid(), invalid);
                prop_assert_eq!(updated.is_tombstone(), tombstone);
                prop_assert_eq!(updated.is_final(), final_bit);
            }

            #[test]
            fn seal_preserves_other_fields(
                addr in arb_logical_address(),
                version in 0u16..=MAX_VERSION,
                invalid in any::<bool>(),
                tombstone in any::<bool>(),
                final_bit in any::<bool>(),
            ) {
                let info = RecordInfo::new(addr, version, invalid, tombstone, final_bit);
                let sealed = info.with_sealed();
                prop_assert!(sealed.is_sealed());
                prop_assert_eq!(sealed.previous_address(), addr);
                prop_assert_eq!(sealed.checkpoint_version(), version);
                prop_assert_eq!(sealed.is_invalid(), invalid);
                prop_assert_eq!(sealed.is_tombstone(), tombstone);
                prop_assert_eq!(sealed.is_final(), final_bit);
            }

            #[test]
            fn with_sealed_cleared_preserves_other_fields(
                addr in arb_logical_address(),
                version in 0u16..=MAX_VERSION,
                invalid in any::<bool>(),
                tombstone in any::<bool>(),
                final_bit in any::<bool>(),
            ) {
                let info = RecordInfo::new(addr, version, invalid, tombstone, final_bit).with_sealed();
                let unsealed = info.with_sealed_cleared();
                prop_assert!(!unsealed.is_sealed());
                prop_assert_eq!(unsealed.previous_address(), addr);
                prop_assert_eq!(unsealed.checkpoint_version(), version);
                prop_assert_eq!(unsealed.is_invalid(), invalid);
                prop_assert_eq!(unsealed.is_tombstone(), tombstone);
                prop_assert_eq!(unsealed.is_final(), final_bit);
            }
        }
    }

    #[test]
    fn with_sealed_cleared_basic() {
        let addr = LogicalAddress::new(Page(5), Offset(128));
        let info = RecordInfo::new(addr, 42, false, false, false).with_sealed();
        assert!(info.is_sealed());
        let unsealed = info.with_sealed_cleared();
        assert!(!unsealed.is_sealed());
        assert_eq!(unsealed.previous_address(), addr);
        assert_eq!(unsealed.checkpoint_version(), 42);
    }

    #[test]
    fn with_sealed_cleared_already_unsealed() {
        let info = RecordInfo::new(LogicalAddress::ZERO, 0, false, false, false);
        let still_unsealed = info.with_sealed_cleared();
        assert_eq!(info.raw(), still_unsealed.raw());
    }

    #[test]
    fn with_sealed_cleared_round_trip() {
        let addr = LogicalAddress::new(Page(100), Offset(512));
        let info = RecordInfo::new(addr, 999, true, true, true);
        let sealed = info.with_sealed();
        let unsealed = sealed.with_sealed_cleared();
        assert_eq!(unsealed.raw(), info.raw());
    }

    #[test]
    fn atomic_try_revivify_success() {
        let info = RecordInfo::new(LogicalAddress::ZERO, 7, false, false, false).with_sealed();
        let atomic = AtomicRecordInfo::new(info);
        assert!(atomic.try_revivify(info).is_ok());
        let current = atomic.load(Ordering::Acquire);
        assert!(!current.is_sealed());
        assert_eq!(current.checkpoint_version(), 7);
    }

    #[test]
    fn atomic_try_revivify_already_unsealed() {
        let info = RecordInfo::new(LogicalAddress::ZERO, 0, false, false, false);
        let sealed = info.with_sealed();
        let atomic = AtomicRecordInfo::new(info);
        assert!(atomic.try_revivify(sealed).is_err());
    }

    #[test]
    fn atomic_try_revivify_concurrent_race() {
        let info = RecordInfo::new(LogicalAddress::ZERO, 3, false, false, false).with_sealed();
        let atomic = AtomicRecordInfo::new(info);
        assert!(atomic.try_revivify(info).is_ok());
        assert!(atomic.try_revivify(info).is_err());
        let current = atomic.load(Ordering::Acquire);
        assert!(!current.is_sealed());
    }

    #[test]
    fn atomic_try_revivify_preserves_all_fields() {
        let addr = LogicalAddress::new(Page(42), Offset(256));
        let info = RecordInfo::new(addr, 999, true, false, true).with_sealed();
        let atomic = AtomicRecordInfo::new(info);
        assert!(atomic.try_revivify(info).is_ok());
        let current = atomic.load(Ordering::Acquire);
        assert!(!current.is_sealed());
        assert_eq!(current.previous_address(), addr);
        assert_eq!(current.checkpoint_version(), 999);
        assert!(current.is_invalid());
        assert!(!current.is_tombstone());
        assert!(current.is_final());
    }
}
