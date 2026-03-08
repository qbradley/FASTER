//! User-defined operation callbacks for a FASTER store.
//!
//! The [`Functions`] trait defines how records are read, written, modified, and
//! deleted. Callers implement this trait to customise the semantics of Read,
//! Upsert, RMW, and Delete operations.
//!
//! Two ready-made implementations are provided:
//!
//! - [`SimpleFunctions`] — basic key-value store with full-value replacement.
//! - [`CounterFunctions`] — atomic increment/decrement (accumulator) pattern.

use core::marker::PhantomData;

use crate::address::LogicalAddress;
use crate::record::{Key, RecordInfo, Value};
use crate::status::OperationStatus;

// ── Operation info structs ──────────────────────────────────────────

/// Context information passed to [`Functions::read`].
///
/// Provides metadata about the record being read, such as its location in the
/// log and the record header. Fields may be extended in the future.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct ReadInfo {
    /// Logical version of the session (0 until checkpoint support lands).
    pub version: u64,
    /// Log address of the record.
    pub address: LogicalAddress,
    /// Header of the record being accessed.
    pub record_info: RecordInfo,
}

impl ReadInfo {
    /// Creates a new `ReadInfo`.
    pub fn new(version: u64, address: LogicalAddress, record_info: RecordInfo) -> Self {
        Self {
            version,
            address,
            record_info,
        }
    }
}

/// Context information passed to [`Functions::upsert`] and
/// [`Functions::upsert_in_place_raw`].
///
/// When upserting a brand-new key the `address` is
/// [`LogicalAddress::INVALID`] and `record_info` is zeroed because the
/// record has not been allocated yet.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct UpsertInfo {
    /// Logical version of the session.
    pub version: u64,
    /// Log address of the existing record (or [`LogicalAddress::INVALID`] for new keys).
    pub address: LogicalAddress,
    /// Header of the existing record (zeroed for new keys).
    pub record_info: RecordInfo,
}

impl UpsertInfo {
    /// Creates a new `UpsertInfo`.
    pub fn new(version: u64, address: LogicalAddress, record_info: RecordInfo) -> Self {
        Self {
            version,
            address,
            record_info,
        }
    }
}

/// Context information passed to [`Functions::rmw_initial`],
/// [`Functions::rmw_in_place`], [`Functions::rmw_in_place_raw`],
/// [`Functions::rmw_copy_update`], [`Functions::rmw_need_initial_update`],
/// and [`Functions::rmw_need_copy_update`].
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct RmwInfo {
    /// Logical version of the session.
    pub version: u64,
    /// Log address of the existing record (or [`LogicalAddress::INVALID`] for initial inserts).
    pub address: LogicalAddress,
    /// Header of the existing record (zeroed for initial inserts).
    pub record_info: RecordInfo,
    /// `true` when this callback is part of a copy-update to a new record.
    pub is_copy_update: bool,
}

impl RmwInfo {
    /// Creates a new `RmwInfo`.
    pub fn new(
        version: u64,
        address: LogicalAddress,
        record_info: RecordInfo,
        is_copy_update: bool,
    ) -> Self {
        Self {
            version,
            address,
            record_info,
            is_copy_update,
        }
    }
}

/// Context information passed to [`Functions::delete`].
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct DeleteInfo {
    /// Logical version of the session.
    pub version: u64,
    /// Log address of the record being deleted.
    pub address: LogicalAddress,
    /// Header of the record being deleted.
    pub record_info: RecordInfo,
}

impl DeleteInfo {
    /// Creates a new `DeleteInfo`.
    pub fn new(version: u64, address: LogicalAddress, record_info: RecordInfo) -> Self {
        Self {
            version,
            address,
            record_info,
        }
    }
}

// ── RmwInPlaceResult ─────────────────────────────────────────────────

/// Result of an in-place RMW update.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RmwInPlaceResult {
    /// Update applied successfully in-place.
    InPlaceOk,
    /// Value needs to be relocated (e.g., variable-length growth).
    NeedsNewRecord,
}

// ── Functions trait ──────────────────────────────────────────────────

/// User-defined operation semantics for a FASTER store.
pub trait Functions: Send + Sync + 'static {
    type Key: Key;
    type Value: Value;
    type Input: Send + Sync + Clone;
    type Output: Send + Sync + Default;
    type Context: Send + Sync;

    fn read(&self, key: &Self::Key, value: &Self::Value, input: &Self::Input, output: &mut Self::Output, info: &ReadInfo);

    /// Read a record's value and produce output.
    fn read(
        &self,
        key: &Self::Key,
        value: &Self::Value,
        input: &Self::Input,
        output: &mut Self::Output,
        info: &ReadInfo,
    );

    fn upsert(&self, key: &Self::Key, value: &mut Self::Value, input: &Self::Input, old_value: Option<&Self::Value>, output: &mut Self::Output, info: &UpsertInfo);

    // ── Upsert ──────────────────────────────────────────────────────

    /// Write a value into a record.
    ///
    /// `old_value` is `Some` for in-place updates in the mutable region,
    /// `None` for new records.
    fn upsert(
        &self,
        key: &Self::Key,
        value: &mut Self::Value,
        input: &Self::Input,
        old_value: Option<&Self::Value>,
        output: &mut Self::Output,
        info: &UpsertInfo,
    );

    // ── Raw in-place support ────────────────────────────────────────────

    /// Whether this implementation supports raw in-place updates.
    ///
    /// When `true`, [`upsert_in_place_raw`](Self::upsert_in_place_raw) and
    /// [`rmw_in_place_raw`](Self::rmw_in_place_raw) bypass the default
    /// deserialize→callback→serialize path. The compiler eliminates
    /// the dead branch at monomorphization time (zero-cost abstraction).
    const SUPPORTS_RAW_IN_PLACE: bool = false;

    /// Raw in-place upsert: directly modify the value bytes in the page.
    ///
    /// # Safety
    ///
    /// `value_ptr` must be valid, aligned, and writable for `value_len` bytes.
    /// The caller must hold epoch protection.
    unsafe fn upsert_in_place_raw(
        &self,
        key: &Self::Key,
        value_ptr: *mut u8,
        value_len: usize,
        input: &Self::Input,
        output: &mut Self::Output,
        info: &UpsertInfo,
    ) {
        let _ = (key, value_ptr, value_len, input, output, info);
        unreachable!("called upsert_in_place_raw but SUPPORTS_RAW_IN_PLACE is false; this is a bug")
    }

    /// Raw in-place RMW: directly modify the value bytes in the page.
    ///
    /// # Safety
    ///
    /// Same requirements as [`upsert_in_place_raw`](Self::upsert_in_place_raw).
    unsafe fn rmw_in_place_raw(
        &self,
        key: &Self::Key,
        value_ptr: *mut u8,
        value_len: usize,
        input: &Self::Input,
        output: &mut Self::Output,
        info: &RmwInfo,
    ) -> RmwInPlaceResult {
        let _ = (key, value_ptr, value_len, input, output, info);
        unreachable!("called rmw_in_place_raw but SUPPORTS_RAW_IN_PLACE is false; this is a bug")
    }

    fn rmw_need_initial_update(&self, _key: &Self::Key, _input: &Self::Input, _info: &RmwInfo) -> bool { true }

    /// Decide whether to create a new record when the key is not found.
    fn rmw_need_initial_update(&self, _key: &Self::Key, _input: &Self::Input, _info: &RmwInfo) -> bool {
        true
    }

    /// Initialize a new record for a key that doesn't exist yet.
    fn rmw_initial(
        &self,
        key: &Self::Key,
        input: &Self::Input,
        value: &mut Self::Value,
        output: &mut Self::Output,
        info: &RmwInfo,
    );

    /// Update a record in-place in the mutable region.
    fn rmw_in_place(
        &self,
        key: &Self::Key,
        input: &Self::Input,
        value: &mut Self::Value,
        output: &mut Self::Output,
        info: &RmwInfo,
    ) -> RmwInPlaceResult;

    /// Decide whether to copy a read-only record before updating.
    fn rmw_need_copy_update(
        &self,
        _key: &Self::Key,
        _input: &Self::Input,
        _old_value: &Self::Value,
        _info: &RmwInfo,
    ) -> bool {
        true
    }

    /// Create a new record by copying and modifying an existing read-only
    /// record.
    fn rmw_copy_update(
        &self,
        key: &Self::Key,
        input: &Self::Input,
        old_value: &Self::Value,
        new_value: &mut Self::Value,
        output: &mut Self::Output,
        info: &RmwInfo,
    );

    /// Called when a pending RMW completes (optional notification).
    fn rmw_completion(
        &self,
        _key: &Self::Key,
        _output: &Self::Output,
        _context: &Self::Context,
        _status: OperationStatus,
    ) {
    }

    // ── Delete ───────────────────────────────────────────────────────

    /// Called when a record is being deleted (optional cleanup).
    fn delete(&self, _key: &Self::Key, _value: &mut Self::Value, _info: &DeleteInfo) {}
}

// ── SimpleFunctions ─────────────────────────────────────────────────

#[derive(Debug)]
pub struct SimpleFunctions<K, V> {
    _phantom: PhantomData<(K, V)>,
}

impl<K, V> SimpleFunctions<K, V> {
    pub fn new() -> Self { Self { _phantom: PhantomData } }
}

impl<K, V> Default for SimpleFunctions<K, V> {
    fn default() -> Self { Self::new() }
}

impl<K, V> Functions for SimpleFunctions<K, V>
where K: Key, V: Value + Copy,
{
    type Key = K;
    type Value = V;
    type Input = V;
    type Output = Option<V>;
    type Context = ();

    fn read(
        &self,
        _key: &Self::Key,
        value: &Self::Value,
        _input: &Self::Input,
        output: &mut Self::Output,
        _info: &ReadInfo,
    ) {
        *output = Some(*value);
    }

    fn upsert(
        &self,
        _key: &Self::Key,
        value: &mut Self::Value,
        input: &Self::Input,
        _old_value: Option<&Self::Value>,
        _output: &mut Self::Output,
        _info: &UpsertInfo,
    ) {
        *value = *input;
    }

    const SUPPORTS_RAW_IN_PLACE: bool = true;

    unsafe fn upsert_in_place_raw(
        &self,
        _key: &Self::Key,
        value_ptr: *mut u8,
        value_len: usize,
        input: &Self::Input,
        _output: &mut Self::Output,
        _info: &UpsertInfo,
    ) {
        debug_assert_eq!(value_len, core::mem::size_of::<V>());
        unsafe { core::ptr::write(value_ptr as *mut V, *input) };
    }

    unsafe fn rmw_in_place_raw(
        &self,
        _key: &Self::Key,
        value_ptr: *mut u8,
        value_len: usize,
        input: &Self::Input,
        _output: &mut Self::Output,
        _info: &RmwInfo,
    ) -> RmwInPlaceResult {
        debug_assert_eq!(value_len, core::mem::size_of::<V>());
        unsafe { core::ptr::write(value_ptr as *mut V, *input) };
        RmwInPlaceResult::InPlaceOk
    }

    fn rmw_initial(
        &self,
        _key: &Self::Key,
        input: &Self::Input,
        value: &mut Self::Value,
        _output: &mut Self::Output,
        _info: &RmwInfo,
    ) {
        *value = *input;
    }

    fn rmw_in_place(
        &self,
        _key: &Self::Key,
        input: &Self::Input,
        value: &mut Self::Value,
        _output: &mut Self::Output,
        _info: &RmwInfo,
    ) -> RmwInPlaceResult {
        *value = *input;
        RmwInPlaceResult::InPlaceOk
    }

    fn rmw_copy_update(
        &self,
        _key: &Self::Key,
        input: &Self::Input,
        _old_value: &Self::Value,
        new_value: &mut Self::Value,
        _output: &mut Self::Output,
        _info: &RmwInfo,
    ) {
        *new_value = *input;
    }
}

// ── CounterFunctions ────────────────────────────────────────────────

/// Counter functions for atomic increment/decrement patterns.
///
/// - `Key` = `K` (any key type).
/// - `Value` = `i64` (the counter).
/// - `Input` = `i64` (the delta to add).
/// - `Output` = `i64` (the counter value after the operation).
/// - `Context` = `()`.
///
/// # Examples
///
/// ```
/// use faster_core::store::{CounterFunctions, Functions, RmwInfo, RmwInPlaceResult};
/// use faster_core::address::LogicalAddress;
/// use faster_core::record::RecordInfo;
///
/// let f = CounterFunctions::<u64>::new();
/// let info = RmwInfo::new(0, LogicalAddress::INVALID, RecordInfo::from_raw(0), false);
///
/// // Initial: counter starts at the delta value.
/// let mut value: i64 = 0;
/// let mut output: i64 = 0;
/// f.rmw_initial(&42u64, &5i64, &mut value, &mut output, &info);
/// assert_eq!(value, 5);
///
/// // In-place: accumulate.
/// let result = f.rmw_in_place(&42u64, &3i64, &mut value, &mut output, &info);
/// assert_eq!(value, 8);
/// assert_eq!(output, 8);
/// assert_eq!(result, RmwInPlaceResult::InPlaceOk);
/// ```
#[derive(Debug)]
pub struct CounterFunctions<K> {
    _phantom: PhantomData<K>,
}

impl<K> CounterFunctions<K> {
    pub fn new() -> Self { Self { _phantom: PhantomData } }
}

impl<K> Default for CounterFunctions<K> {
    fn default() -> Self { Self::new() }
}

impl<K> Functions for CounterFunctions<K>
where K: Key,
{
    type Key = K;
    type Value = i64;
    type Input = i64;
    type Output = i64;
    type Context = ();

    fn read(
        &self,
        _key: &Self::Key,
        value: &Self::Value,
        _input: &Self::Input,
        output: &mut Self::Output,
        _info: &ReadInfo,
    ) {
        *output = *value;
    }

    fn upsert(
        &self,
        _key: &Self::Key,
        value: &mut Self::Value,
        input: &Self::Input,
        _old_value: Option<&Self::Value>,
        _output: &mut Self::Output,
        _info: &UpsertInfo,
    ) {
        *value = *input;
    }

    fn rmw_initial(
        &self,
        _key: &Self::Key,
        input: &Self::Input,
        value: &mut Self::Value,
        _output: &mut Self::Output,
        _info: &RmwInfo,
    ) {
        *value = *input;
    }

    fn rmw_in_place(
        &self,
        _key: &Self::Key,
        input: &Self::Input,
        value: &mut Self::Value,
        output: &mut Self::Output,
        _info: &RmwInfo,
    ) -> RmwInPlaceResult {
        *value += *input;
        *output = *value;
        RmwInPlaceResult::InPlaceOk
    }

    fn rmw_copy_update(
        &self,
        _key: &Self::Key,
        input: &Self::Input,
        old_value: &Self::Value,
        new_value: &mut Self::Value,
        output: &mut Self::Output,
        _info: &RmwInfo,
    ) {
        *new_value = *old_value + *input;
        *output = *new_value;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::address::{LogicalAddress, Offset, Page};
    use crate::record::RecordInfo;

    /// Convenience: a dummy ReadInfo for unit tests.
    fn dummy_read_info() -> ReadInfo {
        ReadInfo::new(0, LogicalAddress::INVALID, RecordInfo::from_raw(0))
    }

    /// Convenience: a dummy UpsertInfo for unit tests.
    fn dummy_upsert_info() -> UpsertInfo {
        UpsertInfo::new(0, LogicalAddress::INVALID, RecordInfo::from_raw(0))
    }

    /// Convenience: a dummy RmwInfo for unit tests.
    fn dummy_rmw_info() -> RmwInfo {
        RmwInfo::new(0, LogicalAddress::INVALID, RecordInfo::from_raw(0), false)
    }

    fn dummy_read_info() -> ReadInfo { ReadInfo::new(0, LogicalAddress::INVALID, RecordInfo::default()) }
    fn dummy_upsert_info() -> UpsertInfo { UpsertInfo::new(0, LogicalAddress::INVALID, RecordInfo::default()) }
    fn dummy_rmw_info() -> RmwInfo { RmwInfo::new(0, LogicalAddress::INVALID, RecordInfo::default(), false) }

    #[test]
    fn simple_functions_read_round_trip() {
        let f = SimpleFunctions::<u64, u64>::new();
        let mut output: Option<u64> = None;

        f.read(&key, &value, &input, &mut output, &dummy_read_info());
        assert_eq!(output, Some(42));
    }

    #[test]
    fn simple_functions_upsert() {
        let f = SimpleFunctions::<u64, u64>::new();
        let mut output: Option<u64> = None;
        let mut value = 0u64;
        f.upsert(&key, &mut value, &100u64, None, &mut output, &dummy_upsert_info());
        assert_eq!(value, 100);

        // Upsert with old value (in-place update).
        let old = 100u64;
        f.upsert(&key, &mut value, &200u64, Some(&old), &mut output, &dummy_upsert_info());
        assert_eq!(value, 200);
    }

    #[test]
    fn simple_functions_rmw_lifecycle() {
        let f = SimpleFunctions::<u64, u64>::new();
        let mut output: Option<u64> = None;

        // Phase 1: initial — key doesn't exist.
        assert!(f.rmw_need_initial_update(&key, &10u64, &dummy_rmw_info()));
        let mut value = 0u64;
        f.rmw_initial(&key, &10u64, &mut value, &mut output, &dummy_rmw_info());
        assert_eq!(value, 10);

        // Phase 2: in-place update — key exists in mutable region.
        let result = f.rmw_in_place(&key, &20u64, &mut value, &mut output, &dummy_rmw_info());
        assert_eq!(result, RmwInPlaceResult::InPlaceOk);
        assert_eq!(value, 20);
        let old_value = 20u64;
        assert!(f.rmw_need_copy_update(&key, &30u64, &old_value, &dummy_rmw_info()));
        let mut new_value = 0u64;
        f.rmw_copy_update(&key, &30u64, &old_value, &mut new_value, &mut output, &dummy_rmw_info());
        assert_eq!(new_value, 30);
    }

    #[test]
    fn counter_functions_accumulate() {
        let f = CounterFunctions::<u64>::new();
        let mut output = 0i64;
        let mut value = 0i64;
        f.rmw_initial(&key, &1i64, &mut value, &mut output, &dummy_rmw_info());
        assert_eq!(value, 1);
        for _ in 0..9 {
            let result = f.rmw_in_place(&key, &1i64, &mut value, &mut output, &dummy_rmw_info());
            assert_eq!(result, RmwInPlaceResult::InPlaceOk);
        }
        assert_eq!(value, 10);
    }

    #[test]
    fn counter_functions_initial_then_update() {
        let f = CounterFunctions::<u64>::new();
        let mut output = 0i64;
        let mut value = 0i64;
        f.rmw_initial(&key, &100i64, &mut value, &mut output, &dummy_rmw_info());
        assert_eq!(value, 100);

        // In-place: add 50.
        let result = f.rmw_in_place(&key, &50i64, &mut value, &mut output, &dummy_rmw_info());
        assert_eq!(result, RmwInPlaceResult::InPlaceOk);
        assert_eq!(value, 150);
        let mut new_value = 0i64;
        f.rmw_copy_update(&key, &25i64, &value, &mut new_value, &mut output, &dummy_rmw_info());
        assert_eq!(new_value, 175);
    }

    #[test]
    fn functions_trait_is_object_safe() {
        fn use_functions<F: Functions>(f: &F, key: &F::Key, val: &F::Value, input: &F::Input) {
            let mut output = F::Output::default();
            f.read(key, val, input, &mut output, &dummy_read_info());
        }
        let f = SimpleFunctions::<u64, u64>::new();
        use_functions(&f, &1u64, &42u64, &0u64);
    }

    #[test]
    fn simple_functions_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<SimpleFunctions<u64, u64>>();
    }

    #[test]
    fn counter_functions_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<CounterFunctions<u64>>();
    }

    #[test]
    fn rmw_in_place_result_traits() {
        let a = RmwInPlaceResult::NeedsNewRecord;
        let b = a;
        assert_eq!(a, b);
        assert_ne!(RmwInPlaceResult::InPlaceOk, RmwInPlaceResult::NeedsNewRecord);
    }

    #[test]
    fn simple_functions_default() {
        let f = SimpleFunctions::<u64, u64>::default();
        let mut output: Option<u64> = None;
        f.read(&1u64, &42u64, &0u64, &mut output, &dummy_read_info());
        assert_eq!(output, Some(42));
    }

    #[test]
    fn counter_functions_default() {
        let f = CounterFunctions::<u64>::default();
        let mut value = 0i64;
        let mut output = 0i64;
        f.rmw_initial(&1u64, &5i64, &mut value, &mut output, &dummy_rmw_info());
        assert_eq!(value, 5);
    }

    #[test]
    fn counter_functions_read() {
        let f = CounterFunctions::<u64>::new();
        let mut output = 0i64;
        f.read(&1u64, &42i64, &0i64, &mut output, &dummy_read_info());
        assert_eq!(output, 42);
    }

    #[test]
    fn counter_functions_upsert() {
        let f = CounterFunctions::<u64>::new();
        let mut value = 0i64;
        let mut output = 0i64;
        f.upsert(&1u64, &mut value, &99i64, None, &mut output, &dummy_upsert_info());
        assert_eq!(value, 99);
    }

    #[test]
    fn simple_functions_supports_raw_in_place() {
        const { assert!(SimpleFunctions::<u64, u64>::SUPPORTS_RAW_IN_PLACE) };
    }

    #[test]
    fn counter_functions_does_not_support_raw_in_place() {
        const { assert!(!CounterFunctions::<u64>::SUPPORTS_RAW_IN_PLACE) };
    }

    #[test]
    fn upsert_in_place_raw_round_trip_u64() {
        let f = SimpleFunctions::<u64, u64>::new();
        let mut buf = 0u64.to_le_bytes();
        let value_ptr = buf.as_mut_ptr();
        let value_len = core::mem::size_of::<u64>();
        let mut output: Option<u64> = None;
        // SAFETY: `value_ptr` and `value_len` refer to a valid, aligned `u64` buffer on the stack.
        unsafe {
            f.upsert_in_place_raw(&1u64, value_ptr, value_len, &42u64, &mut output, &dummy_upsert_info());
        }
        assert_eq!(u64::from_le_bytes(buf), 42);
    }

    #[test]
    fn rmw_in_place_raw_round_trip_u64() {
        let f = SimpleFunctions::<u64, u64>::new();
        let mut buf = 0u64.to_le_bytes();
        let value_ptr = buf.as_mut_ptr();
        let value_len = core::mem::size_of::<u64>();
        let mut output: Option<u64> = None;
        // SAFETY: `value_ptr` and `value_len` refer to a valid, aligned `u64` buffer on the stack.
        let result =
            unsafe { f.rmw_in_place_raw(&1u64, value_ptr, value_len, &99u64, &mut output, &dummy_rmw_info()) };
        assert_eq!(result, RmwInPlaceResult::InPlaceOk);
        assert_eq!(u64::from_le_bytes(buf), 99);
    }

    #[test]
    fn upsert_in_place_raw_round_trip_u32() {
        let f = SimpleFunctions::<u64, u32>::new();
        let mut buf = 0u32.to_le_bytes();
        let value_ptr = buf.as_mut_ptr();
        let value_len = core::mem::size_of::<u32>();
        let mut output: Option<u32> = None;
        // SAFETY: `value_ptr` and `value_len` refer to a valid, aligned `u32` buffer on the stack.
        unsafe {
            f.upsert_in_place_raw(&1u64, value_ptr, value_len, &12345u32, &mut output, &dummy_upsert_info());
        }
        assert_eq!(u32::from_le_bytes(buf), 12345);
    }

    #[test]
    fn raw_path_successive_overwrites() {
        let f = SimpleFunctions::<u64, u64>::new();
        let mut buf = [0u8; 8];
        let ptr = buf.as_mut_ptr();
        let mut output: Option<u64> = None;
        for expected in [1u64, 100, u64::MAX, 0, 42] {
            // SAFETY: `ptr` points to an 8-byte stack buffer valid for the lifetime of this loop.
            unsafe {
                f.upsert_in_place_raw(&0u64, ptr, 8, &expected, &mut output, &dummy_upsert_info());
            }
            assert_eq!(u64::from_le_bytes(buf), expected);
        }
    }

    #[test]
    fn non_raw_functions_still_work() {
        let f = CounterFunctions::<u64>::new();
        const { assert!(!CounterFunctions::<u64>::SUPPORTS_RAW_IN_PLACE) };
        let mut value = 10i64;
        let mut output = 0i64;
        let result = f.rmw_in_place(&1u64, &5i64, &mut value, &mut output, &dummy_rmw_info());
        assert_eq!(result, RmwInPlaceResult::InPlaceOk);
        assert_eq!(value, 15);
    }

    // ── Info struct tests ───────────────────────────────────────────

    #[test]
    fn read_info_fields() {
        let addr = LogicalAddress::new(Page(3), Offset(128));
        let ri = RecordInfo::from_raw(0xDEAD);
        let info = ReadInfo::new(42, addr, ri);
        assert_eq!(info.version, 42);
        assert_eq!(info.address, addr);
        assert_eq!(info.record_info.raw(), 0xDEAD);
    }

    #[test]
    fn upsert_info_fields() {
        let info = UpsertInfo::new(7, LogicalAddress::INVALID, RecordInfo::from_raw(0));
        assert_eq!(info.version, 7);
        assert_eq!(info.address, LogicalAddress::INVALID);
    }

    #[test]
    fn rmw_info_copy_update_flag() {
        let info_no = RmwInfo::new(0, LogicalAddress::INVALID, RecordInfo::from_raw(0), false);
        assert!(!info_no.is_copy_update);

        let info_yes = RmwInfo::new(0, LogicalAddress::INVALID, RecordInfo::from_raw(0), true);
        assert!(info_yes.is_copy_update);
    }

    #[test]
    fn delete_info_fields() {
        let addr = LogicalAddress::new(Page(1), Offset(64));
        let ri = RecordInfo::from_raw(0xBEEF);
        let info = DeleteInfo::new(99, addr, ri);
        assert_eq!(info.version, 99);
        assert_eq!(info.address, addr);
        assert_eq!(info.record_info.raw(), 0xBEEF);
    }

    #[test]
    fn info_structs_are_copy_clone_debug() {
        let ri = ReadInfo::new(0, LogicalAddress::INVALID, RecordInfo::from_raw(0));
        let ri2 = ri; // Copy
        let ri3 = ri.clone(); // Clone
        let _ = format!("{:?}", ri2); // Debug
        let _ = ri3;
    }
}
