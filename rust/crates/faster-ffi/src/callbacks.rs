//! Callback-based Functions implementation for the FFI layer.
//!
//! This module provides [`CallbackFunctions`], which implements the FASTER
//! [`Functions`] trait using C function pointers installed via thread-local
//! storage. When no callbacks are installed, it falls back to the same
//! byte-slice replacement semantics as [`ByteSliceFunctions`].
//!
//! # Architecture
//!
//! Sessions in FASTER are thread-affine, so thread-local storage is the
//! natural scope for per-operation callback state. The `_ex()` FFI functions
//! install callbacks via RAII guards before calling store operations; guards
//! clear the thread-local on drop (even on panic).
//!
//! # Buffer Protocol
//!
//! For `rmw_initial` and `rmw_copy_update`, the value `Vec<u8>` is
//! pre-allocated to `max(input.len(), MIN_CALLBACK_BUF_CAPACITY)` bytes.
//! The callback receives `*mut u8` + `*mut usize` (capacity on entry,
//! actual length on exit). For `rmw_in_place` (atomic), the callback
//! receives the existing value pointer and length, modifies in-place,
//! and returns 0 for success or non-zero for "needs new record".

use std::cell::Cell;

use faster_core::store::{Functions, RmwInPlaceResult};

// ── Minimum buffer capacity ─────────────────────────────────────────

/// Minimum buffer capacity for callback value buffers.
///
/// Prevents tiny allocations when the input is very small.
pub const MIN_CALLBACK_BUF_CAPACITY: usize = 64;

// ── C callback function pointer types ───────────────────────────────

/// RMW initial callback: initialize a new value for a key that doesn't exist.
///
/// `value_ptr` points to a pre-allocated buffer of `*value_len` bytes.
/// The callback writes the initial value and sets `*value_len` to the
/// actual number of bytes written.
///
/// Returns 0 on success, non-zero on failure.
pub type FasterRmwInitialFn = extern "C" fn(
    key_ptr: *const u8,
    key_len: usize,
    input_ptr: *const u8,
    input_len: usize,
    value_ptr: *mut u8,
    value_len: *mut usize,
) -> i32;

/// RMW copy-update callback: create a new value by modifying a copy of the old value.
///
/// `new_value_ptr` points to a pre-allocated buffer of `*new_value_len` bytes.
/// The old value is provided read-only via `old_value_ptr`/`old_value_len`.
///
/// Returns 0 on success, non-zero on failure.
pub type FasterRmwCopyFn = extern "C" fn(
    key_ptr: *const u8,
    key_len: usize,
    input_ptr: *const u8,
    input_len: usize,
    old_value_ptr: *const u8,
    old_value_len: usize,
    new_value_ptr: *mut u8,
    new_value_len: *mut usize,
) -> i32;

/// RMW atomic (in-place) callback: modify the value in the mutable region directly.
///
/// Returns 0 if the update was applied in-place, non-zero if the record
/// needs to be relocated (e.g., variable-length value grew).
pub type FasterRmwAtomicFn = extern "C" fn(
    key_ptr: *const u8,
    key_len: usize,
    input_ptr: *const u8,
    input_len: usize,
    value_ptr: *mut u8,
    value_len: usize,
) -> i32;

/// Upsert put callback: write an input value into the record.
///
/// `value_ptr` is the destination buffer, `value_len` is its capacity.
/// The callback writes the value and sets `*actual_len` to the number
/// of bytes written.
///
/// Returns 0 on success.
pub type FasterUpsertPutFn = extern "C" fn(
    key_ptr: *const u8,
    key_len: usize,
    input_ptr: *const u8,
    input_len: usize,
    value_ptr: *mut u8,
    value_len: usize,
    actual_len: *mut usize,
) -> i32;

/// Upsert atomic (in-place) callback: modify an existing value in-place.
///
/// Returns 0 on success.
pub type FasterUpsertPutAtomicFn = extern "C" fn(
    key_ptr: *const u8,
    key_len: usize,
    input_ptr: *const u8,
    input_len: usize,
    value_ptr: *mut u8,
    value_len: usize,
) -> i32;

/// Read get callback: extract output from a read record.
///
/// `output_ptr` points to a caller-provided buffer of `*output_len` bytes.
/// The callback writes the output and sets `*output_len` to the actual
/// number of bytes written.
///
/// Returns 0 on success, 1 if the buffer was too small.
pub type FasterReadGetFn = extern "C" fn(
    key_ptr: *const u8,
    key_len: usize,
    value_ptr: *const u8,
    value_len: usize,
    output_ptr: *mut u8,
    output_len: *mut usize,
) -> i32;

/// Read atomic callback: read from a record in-place (same as ReadGetFn
/// but called for records in the mutable region).
pub type FasterReadGetAtomicFn = FasterReadGetFn;

/// Async operation completion callback.
///
/// Called when an asynchronous operation completes.
/// `status` is the FASTER status code, `context` is the user-provided
/// opaque context pointer.
pub type FasterAsyncCallbackFn = extern "C" fn(
    status: u8,
    context: u64,
);

// ── Callback sets (grouped by operation type) ───────────────────────

/// RMW callback set installed per-operation.
#[derive(Clone, Copy)]
pub(crate) struct RmwCallbackSet {
    pub initial: Option<FasterRmwInitialFn>,
    pub copy_update: Option<FasterRmwCopyFn>,
    pub in_place: Option<FasterRmwAtomicFn>,
}

/// Upsert callback set installed per-operation.
#[derive(Clone, Copy)]
#[allow(dead_code)]
pub(crate) struct UpsertCallbackSet {
    pub put: Option<FasterUpsertPutFn>,
    pub put_atomic: Option<FasterUpsertPutAtomicFn>,
}

/// Read callback set installed per-operation.
#[derive(Clone, Copy)]
#[allow(dead_code)]
pub(crate) struct ReadCallbackSet {
    pub get: Option<FasterReadGetFn>,
    pub get_atomic: Option<FasterReadGetAtomicFn>,
}

// ── Thread-local storage ────────────────────────────────────────────

thread_local! {
    pub(crate) static RMW_CALLBACKS: Cell<Option<RmwCallbackSet>> = const { Cell::new(None) };
    pub(crate) static UPSERT_CALLBACKS: Cell<Option<UpsertCallbackSet>> = const { Cell::new(None) };
    pub(crate) static READ_CALLBACKS: Cell<Option<ReadCallbackSet>> = const { Cell::new(None) };
}

// ── RAII guards ─────────────────────────────────────────────────────

/// RAII guard that clears RMW callbacks on drop.
pub(crate) struct RmwCallbackGuard;

impl RmwCallbackGuard {
    pub(crate) fn install(cbs: RmwCallbackSet) -> Self {
        RMW_CALLBACKS.with(|c| c.set(Some(cbs)));
        RmwCallbackGuard
    }
}

impl Drop for RmwCallbackGuard {
    fn drop(&mut self) {
        RMW_CALLBACKS.with(|c| c.set(None));
    }
}

/// RAII guard that clears Upsert callbacks on drop.
pub(crate) struct UpsertCallbackGuard;

impl UpsertCallbackGuard {
    pub(crate) fn install(cbs: UpsertCallbackSet) -> Self {
        UPSERT_CALLBACKS.with(|c| c.set(Some(cbs)));
        UpsertCallbackGuard
    }
}

impl Drop for UpsertCallbackGuard {
    fn drop(&mut self) {
        UPSERT_CALLBACKS.with(|c| c.set(None));
    }
}

/// RAII guard that clears Read callbacks on drop.
pub(crate) struct ReadCallbackGuard;

impl ReadCallbackGuard {
    pub(crate) fn install(cbs: ReadCallbackSet) -> Self {
        READ_CALLBACKS.with(|c| c.set(Some(cbs)));
        ReadCallbackGuard
    }
}

impl Drop for ReadCallbackGuard {
    fn drop(&mut self) {
        READ_CALLBACKS.with(|c| c.set(None));
    }
}

// ── CallbackFunctions ───────────────────────────────────────────────

/// `Functions` implementation that dispatches to C callback function pointers
/// via thread-local storage, with byte-slice replacement as the fallback.
///
/// This type is used as the concrete `Functions` implementation for the FFI
/// layer. When `_ex()` functions install callbacks before an operation, those
/// callbacks are invoked. When standard functions (e.g., `faster_rmw()`) are
/// used, the default byte-slice replacement behavior applies.
#[derive(Debug)]
pub struct CallbackFunctions;

impl Functions for CallbackFunctions {
    type Key = Vec<u8>;
    type Value = Vec<u8>;
    type Input = Vec<u8>;
    type Output = Option<Vec<u8>>;
    type Context = ();

    // ── Read ────────────────────────────────────────────────────────

    fn read(
        &self,
        key: &Vec<u8>,
        value: &Vec<u8>,
        _input: &Vec<u8>,
        output: &mut Option<Vec<u8>>,
    ) {
        let cbs = READ_CALLBACKS.with(|c| c.get());
        if let Some(cbs) = cbs {
            if let Some(get_fn) = cbs.get {
                let mut buf = vec![0u8; value.len().max(64)];
                let mut out_len = buf.len();
                let rc = get_fn(
                    key.as_ptr(),
                    key.len(),
                    value.as_ptr(),
                    value.len(),
                    buf.as_mut_ptr(),
                    &mut out_len,
                );
                if rc == 0 {
                    buf.truncate(out_len);
                    *output = Some(buf);
                }
                return;
            }
        }
        // Fallback: copy value → output
        *output = Some(value.clone());
    }

    // ── Upsert ──────────────────────────────────────────────────────

    fn upsert(
        &self,
        key: &Vec<u8>,
        value: &mut Vec<u8>,
        input: &Vec<u8>,
        _old_value: Option<&Vec<u8>>,
        _output: &mut Option<Vec<u8>>,
    ) {
        let cbs = UPSERT_CALLBACKS.with(|c| c.get());
        if let Some(cbs) = cbs {
            if let Some(put_fn) = cbs.put {
                let cap = input.len().max(MIN_CALLBACK_BUF_CAPACITY);
                value.resize(cap, 0);
                let mut actual_len = cap;
                let rc = put_fn(
                    key.as_ptr(),
                    key.len(),
                    input.as_ptr(),
                    input.len(),
                    value.as_mut_ptr(),
                    cap,
                    &mut actual_len,
                );
                if rc == 0 {
                    value.truncate(actual_len);
                }
                return;
            }
        }
        // Fallback: value = input (full replacement)
        value.clear();
        value.extend_from_slice(input);
    }

    // ── RMW ─────────────────────────────────────────────────────────

    fn rmw_initial(
        &self,
        key: &Vec<u8>,
        input: &Vec<u8>,
        value: &mut Vec<u8>,
        _output: &mut Option<Vec<u8>>,
    ) {
        let cbs = RMW_CALLBACKS.with(|c| c.get());
        if let Some(cbs) = cbs {
            if let Some(initial_fn) = cbs.initial {
                let cap = input.len().max(MIN_CALLBACK_BUF_CAPACITY);
                value.resize(cap, 0);
                let mut value_len = cap;
                let _rc = initial_fn(
                    key.as_ptr(),
                    key.len(),
                    input.as_ptr(),
                    input.len(),
                    value.as_mut_ptr(),
                    &mut value_len,
                );
                value.truncate(value_len);
                return;
            }
        }
        // Fallback: value = input
        value.clear();
        value.extend_from_slice(input);
    }

    fn rmw_in_place(
        &self,
        key: &Vec<u8>,
        input: &Vec<u8>,
        value: &mut Vec<u8>,
        _output: &mut Option<Vec<u8>>,
    ) -> RmwInPlaceResult {
        let cbs = RMW_CALLBACKS.with(|c| c.get());
        if let Some(cbs) = cbs {
            if let Some(atomic_fn) = cbs.in_place {
                let rc = atomic_fn(
                    key.as_ptr(),
                    key.len(),
                    input.as_ptr(),
                    input.len(),
                    value.as_mut_ptr(),
                    value.len(),
                );
                return if rc == 0 {
                    RmwInPlaceResult::InPlaceOk
                } else {
                    RmwInPlaceResult::NeedsNewRecord
                };
            }
        }
        // Fallback: full replacement (always needs new record if lengths differ)
        if value.len() == input.len() {
            value.copy_from_slice(input);
            RmwInPlaceResult::InPlaceOk
        } else {
            value.clear();
            value.extend_from_slice(input);
            RmwInPlaceResult::NeedsNewRecord
        }
    }

    fn rmw_copy_update(
        &self,
        key: &Vec<u8>,
        input: &Vec<u8>,
        old_value: &Vec<u8>,
        new_value: &mut Vec<u8>,
        _output: &mut Option<Vec<u8>>,
    ) {
        let cbs = RMW_CALLBACKS.with(|c| c.get());
        if let Some(cbs) = cbs {
            if let Some(copy_fn) = cbs.copy_update {
                let cap = old_value.len().max(input.len()).max(MIN_CALLBACK_BUF_CAPACITY);
                new_value.resize(cap, 0);
                let mut new_len = cap;
                let _rc = copy_fn(
                    key.as_ptr(),
                    key.len(),
                    input.as_ptr(),
                    input.len(),
                    old_value.as_ptr(),
                    old_value.len(),
                    new_value.as_mut_ptr(),
                    &mut new_len,
                );
                new_value.truncate(new_len);
                return;
            }
        }
        // Fallback: new_value = input
        new_value.clear();
        new_value.extend_from_slice(input);
    }

    fn delete(&self, _key: &Vec<u8>, _value: &mut Vec<u8>) {}
}

// ── Unit tests ──────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── RMW callback tests ──────────────────────────────────────────

    /// Sum-store pattern: value is a little-endian u64 counter,
    /// input is the delta to add.
    extern "C" fn sum_initial(
        _key_ptr: *const u8, _key_len: usize,
        input_ptr: *const u8, input_len: usize,
        value_ptr: *mut u8, value_len: *mut usize,
    ) -> i32 {
        assert_eq!(input_len, 8);
        // SAFETY: test callback, pointers valid
        unsafe {
            let delta = u64::from_le_bytes(std::slice::from_raw_parts(input_ptr, 8).try_into().unwrap());
            let bytes = delta.to_le_bytes();
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), value_ptr, 8);
            *value_len = 8;
        }
        0
    }

    extern "C" fn sum_atomic(
        _key_ptr: *const u8, _key_len: usize,
        input_ptr: *const u8, input_len: usize,
        value_ptr: *mut u8, value_len: usize,
    ) -> i32 {
        assert_eq!(input_len, 8);
        assert_eq!(value_len, 8);
        // SAFETY: test callback, pointers valid
        unsafe {
            let delta = u64::from_le_bytes(std::slice::from_raw_parts(input_ptr, 8).try_into().unwrap());
            let current = u64::from_le_bytes(std::slice::from_raw_parts(value_ptr, 8).try_into().unwrap());
            let new_val = current + delta;
            std::ptr::copy_nonoverlapping(new_val.to_le_bytes().as_ptr(), value_ptr, 8);
        }
        0
    }

    extern "C" fn sum_copy(
        _key_ptr: *const u8, _key_len: usize,
        input_ptr: *const u8, input_len: usize,
        old_ptr: *const u8, old_len: usize,
        new_ptr: *mut u8, new_len: *mut usize,
    ) -> i32 {
        assert_eq!(input_len, 8);
        assert_eq!(old_len, 8);
        // SAFETY: test callback, pointers valid
        unsafe {
            let delta = u64::from_le_bytes(std::slice::from_raw_parts(input_ptr, 8).try_into().unwrap());
            let old_val = u64::from_le_bytes(std::slice::from_raw_parts(old_ptr, 8).try_into().unwrap());
            let result = old_val + delta;
            std::ptr::copy_nonoverlapping(result.to_le_bytes().as_ptr(), new_ptr, 8);
            *new_len = 8;
        }
        0
    }

    #[test]
    fn rmw_callbacks_sum_store() {
        let f = CallbackFunctions;
        let key = vec![1u8, 2, 3];
        let input = 42u64.to_le_bytes().to_vec();

        // Install callbacks
        let _guard = RmwCallbackGuard::install(RmwCallbackSet {
            initial: Some(sum_initial),
            copy_update: Some(sum_copy),
            in_place: Some(sum_atomic),
        });

        // Test initial
        let mut value = Vec::new();
        let mut output = None;
        f.rmw_initial(&key, &input, &mut value, &mut output);
        assert_eq!(u64::from_le_bytes(value[..8].try_into().unwrap()), 42);

        // Test in-place (add 10)
        let input2 = 10u64.to_le_bytes().to_vec();
        let result = f.rmw_in_place(&key, &input2, &mut value, &mut output);
        assert_eq!(result, RmwInPlaceResult::InPlaceOk);
        assert_eq!(u64::from_le_bytes(value[..8].try_into().unwrap()), 52);

        // Test copy-update (add 8)
        let input3 = 8u64.to_le_bytes().to_vec();
        let old_value = value.clone();
        let mut new_value = Vec::new();
        f.rmw_copy_update(&key, &input3, &old_value, &mut new_value, &mut output);
        assert_eq!(u64::from_le_bytes(new_value[..8].try_into().unwrap()), 60);
    }

    #[test]
    fn rmw_fallback_without_callbacks() {
        let f = CallbackFunctions;
        let key = vec![1u8];
        let input = vec![10u8, 20];

        // No callbacks installed — should do byte-slice replacement
        let mut value = Vec::new();
        let mut output = None;
        f.rmw_initial(&key, &input, &mut value, &mut output);
        assert_eq!(value, vec![10, 20]);

        let input2 = vec![30u8, 40];
        let result = f.rmw_in_place(&key, &input2, &mut value, &mut output);
        assert_eq!(result, RmwInPlaceResult::InPlaceOk);
        assert_eq!(value, vec![30, 40]);

        // Different length → NeedsNewRecord
        let input3 = vec![50u8, 60, 70];
        let result = f.rmw_in_place(&key, &input3, &mut value, &mut output);
        assert_eq!(result, RmwInPlaceResult::NeedsNewRecord);
    }

    #[test]
    fn guard_cleanup_on_drop() {
        {
            let _guard = RmwCallbackGuard::install(RmwCallbackSet {
                initial: Some(sum_initial),
                copy_update: None,
                in_place: None,
            });
            assert!(RMW_CALLBACKS.with(|c| c.get()).is_some());
        }
        // After guard drops, should be None
        assert!(RMW_CALLBACKS.with(|c| c.get()).is_none());
    }

    // ── Read callback tests ─────────────────────────────────────────

    extern "C" fn read_double(
        _key_ptr: *const u8, _key_len: usize,
        value_ptr: *const u8, value_len: usize,
        output_ptr: *mut u8, output_len: *mut usize,
    ) -> i32 {
        // SAFETY: test callback, pointers valid
        unsafe {
            let needed = value_len * 2;
            if needed > *output_len {
                *output_len = needed;
                return 1;
            }
            std::ptr::copy_nonoverlapping(value_ptr, output_ptr, value_len);
            std::ptr::copy_nonoverlapping(value_ptr, output_ptr.add(value_len), value_len);
            *output_len = needed;
        }
        0
    }

    #[test]
    fn read_callback_custom() {
        let f = CallbackFunctions;
        let key = vec![1u8];
        let value = vec![0xAA, 0xBB];
        let input = Vec::new();

        let _guard = ReadCallbackGuard::install(ReadCallbackSet {
            get: Some(read_double),
            get_atomic: Some(read_double),
        });

        let mut output = None;
        f.read(&key, &value, &input, &mut output);
        assert_eq!(output, Some(vec![0xAA, 0xBB, 0xAA, 0xBB]));
    }

    #[test]
    fn read_fallback_copies_value() {
        let f = CallbackFunctions;
        let key = vec![1u8];
        let value = vec![0xDE, 0xAD];
        let input = Vec::new();
        let mut output = None;
        f.read(&key, &value, &input, &mut output);
        assert_eq!(output, Some(vec![0xDE, 0xAD]));
    }

    // ── Upsert callback tests ───────────────────────────────────────

    extern "C" fn upsert_uppercase(
        _key_ptr: *const u8, _key_len: usize,
        input_ptr: *const u8, input_len: usize,
        value_ptr: *mut u8, _value_len: usize,
        actual_len: *mut usize,
    ) -> i32 {
        // SAFETY: test callback, pointers valid
        unsafe {
            let input = std::slice::from_raw_parts(input_ptr, input_len);
            for (i, &b) in input.iter().enumerate() {
                *value_ptr.add(i) = b.to_ascii_uppercase();
            }
            *actual_len = input_len;
        }
        0
    }

    #[test]
    fn upsert_callback_custom() {
        let f = CallbackFunctions;
        let key = vec![1u8];
        let input = b"hello".to_vec();

        let _guard = UpsertCallbackGuard::install(UpsertCallbackSet {
            put: Some(upsert_uppercase),
            put_atomic: None,
        });

        let mut value = Vec::new();
        let mut output = None;
        f.upsert(&key, &mut value, &input, None, &mut output);
        assert_eq!(value, b"HELLO");
    }

    #[test]
    fn upsert_fallback_replaces_value() {
        let f = CallbackFunctions;
        let key = vec![1u8];
        let input = vec![42u8, 43];
        let mut value = vec![1u8, 2, 3];
        let mut output = None;
        f.upsert(&key, &mut value, &input, None, &mut output);
        assert_eq!(value, vec![42, 43]);
    }
}
