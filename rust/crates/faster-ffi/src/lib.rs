//! # FASTER FFI
//!
//! C FFI bindings for the FASTER key-value store.
//!
//! Exposes an opaque-handle-based C ABI for embedding FASTER in other
//! languages. Header generation via `cbindgen`.
//!
//! ## Crate Structure
//!
//! - [`handle`] — Opaque handle table (type-safe, thread-safe `u64` → Rust object mapping)
//! - [`error`] — C-compatible `#[repr(C)]` status codes
//! - [`functions`] — `ByteSliceFunctions` — `Vec<u8>` key/value `Functions` impl for FFI
//! - [`session`] — Session wrapper types for the handle table
//!
//! ## FFI Functions
//!
//! ### Store Lifecycle
//! - [`faster_open`] — Create a new FASTER store, returns an opaque handle.
//! - [`faster_close`] — Destroy a store and release all resources.
//!
//! ### Session Lifecycle
//! - [`faster_session_start`] — Begin a new session on a store.
//! - [`faster_session_end`] — End and dispose a session.
//!
//! ### CRUD Operations
//! - [`faster_upsert`] — Insert or update a key-value pair.
//! - [`faster_read`] — Read the value for a key into a caller-provided buffer.
//! - [`faster_delete`] — Delete a key from the store.
//! - [`faster_rmw`] — Read-modify-write (full-value replacement).
//!
//! ### Maintenance
//! - [`faster_complete_pending`] — Drain completed async I/O operations.
//!
//! ## Thread Safety
//!
//! Store handles are thread-safe. Session handles are **not** — they must be
//! used exclusively from the thread that created them. Passing a session handle
//! across threads is undefined behavior.

#![deny(unsafe_op_in_unsafe_fn)]
#![warn(missing_docs)]
#![forbid(clippy::undocumented_unsafe_blocks)]

pub mod error;
pub mod functions;
pub mod handle;
pub mod session;

use std::cell::UnsafeCell;
use std::sync::OnceLock;

use faster_core::status::OperationStatus;
use faster_core::{FasterKv, FasterKvConfig, NullDevice};

use crate::error::FasterStatus;
use crate::functions::ByteSliceFunctions;
use crate::handle::{FasterHandle, HandleTable, INVALID_HANDLE};
use crate::session::SessionCell;

/// The concrete store type used by the FFI layer.
type FfiStore = FasterKv<ByteSliceFunctions>;

/// Global handle table shared by all FFI functions.
fn global_handles() -> &'static HandleTable {
    static HANDLES: OnceLock<HandleTable> = OnceLock::new();
    HANDLES.get_or_init(HandleTable::new)
}

// ── Conversion helper ───────────────────────────────────────────────

/// Map an [`OperationStatus`] from faster-core to a [`FasterStatus`] for C.
fn to_ffi_status(status: OperationStatus) -> FasterStatus {
    match status {
        OperationStatus::Ok => FasterStatus::Ok,
        OperationStatus::NotFound => FasterStatus::NotFound,
        OperationStatus::Pending => FasterStatus::Pending,
        OperationStatus::Created => FasterStatus::Created,
        OperationStatus::InPlaceUpdated => FasterStatus::InPlaceUpdated,
        OperationStatus::CopyUpdated => FasterStatus::CopyUpdated,
        // Deleted and Aborted map to Ok — from the C caller's perspective the
        // operation completed successfully. Aborted is an internal detail.
        OperationStatus::Deleted => FasterStatus::Ok,
        OperationStatus::Aborted => FasterStatus::InternalError,
    }
}

// ── Store lifecycle ─────────────────────────────────────────────────

/// Create a new FASTER key-value store with default configuration.
///
/// Returns an opaque store handle, or [`INVALID_HANDLE`] on failure.
///
/// # Thread Safety
///
/// The returned handle is safe to use from any thread.
#[no_mangle]
pub extern "C" fn faster_open() -> FasterHandle {
    let store = FfiStore::new(
        FasterKvConfig::default(),
        ByteSliceFunctions,
        NullDevice::new(),
    );
    global_handles().insert(store)
}

/// Destroy a FASTER store and release all resources.
///
/// Returns [`FasterStatus::Ok`] on success, [`FasterStatus::InvalidHandle`]
/// if the handle is invalid or already closed.
///
/// # Safety
///
/// All sessions **must** be ended before calling this function. Using a
/// session handle after its store has been closed is undefined behavior.
#[no_mangle]
pub extern "C" fn faster_close(store: FasterHandle) -> FasterStatus {
    match global_handles().remove::<FfiStore>(store) {
        Some(_) => FasterStatus::Ok,
        None => FasterStatus::InvalidHandle,
    }
}

// ── Session lifecycle ───────────────────────────────────────────────

/// Start a new session on the given store.
///
/// Returns an opaque session handle, or [`INVALID_HANDLE`] if the store
/// handle is invalid.
///
/// # Thread Safety
///
/// The returned session handle **must only be used from the calling thread**.
/// Passing it to another thread is undefined behavior.
#[no_mangle]
pub extern "C" fn faster_session_start(store: FasterHandle) -> FasterHandle {
    let result = global_handles().with::<FfiStore, _>(store, |kv| {
        let session = kv.new_session();
        let cell = SessionCell(UnsafeCell::new(session));
        global_handles().insert(cell)
    });
    result.unwrap_or(INVALID_HANDLE)
}

/// End and dispose a session.
///
/// Returns [`FasterStatus::Ok`] on success, [`FasterStatus::InvalidHandle`]
/// if either handle is invalid.
///
/// # Safety
///
/// Must be called from the same thread that created the session. The session
/// handle must not be used after this call.
#[no_mangle]
pub extern "C" fn faster_session_end(
    store: FasterHandle,
    session_handle: FasterHandle,
) -> FasterStatus {
    // Remove the session from the handle table first.
    let cell = match global_handles().remove::<SessionCell>(session_handle) {
        Some(c) => c,
        None => return FasterStatus::InvalidHandle,
    };

    // Dispose via the store (releases epoch thread resources).
    let session = cell.0.into_inner();
    let disposed = global_handles().with::<FfiStore, _>(store, |kv| {
        kv.dispose_session(session);
    });
    match disposed {
        Some(()) => FasterStatus::Ok,
        None => {
            // Store handle is invalid, but we already removed the session.
            // The session will be dropped, which is acceptable.
            FasterStatus::InvalidHandle
        }
    }
}

// ── Helper: run a closure with store + session ──────────────────────

/// Obtain `&FfiStore` and `&mut FfiSession` from their handles, then call `f`.
///
/// Returns the appropriate error status if either handle is invalid.
fn with_store_session<R>(
    store: FasterHandle,
    session_handle: FasterHandle,
    f: impl FnOnce(&FfiStore, &mut crate::session::FfiSession) -> R,
) -> Result<R, FasterStatus> {
    global_handles()
        .with::<FfiStore, _>(store, |kv| {
            global_handles()
                .with::<SessionCell, _>(session_handle, |cell| {
                    // SAFETY: FFI contract requires single-threaded access per session.
                    // No other call can be using this session concurrently.
                    let session = unsafe { &mut *cell.0.get() };
                    f(kv, session)
                })
                .ok_or(FasterStatus::InvalidHandle)
        })
        .ok_or(FasterStatus::InvalidHandle)?
}

// ── CRUD operations ─────────────────────────────────────────────────

/// Insert or update a key-value pair.
///
/// # Parameters
///
/// - `store` — Store handle from [`faster_open`].
/// - `session` — Session handle from [`faster_session_start`].
/// - `key_ptr` / `key_len` — Pointer and length of the key bytes.
/// - `val_ptr` / `val_len` — Pointer and length of the value bytes.
///
/// # Returns
///
/// A [`FasterStatus`] indicating the outcome. On success, one of `Ok`,
/// `Created`, `InPlaceUpdated`, or `CopyUpdated`.
///
/// # Safety
///
/// - `key_ptr` must be valid for reads of `key_len` bytes (or null if `key_len == 0`).
/// - `val_ptr` must be valid for reads of `val_len` bytes (or null if `val_len == 0`).
/// - Must be called from the thread that owns `session`.
#[no_mangle]
pub unsafe extern "C" fn faster_upsert(
    store: FasterHandle,
    session: FasterHandle,
    key_ptr: *const u8,
    key_len: u32,
    val_ptr: *const u8,
    val_len: u32,
) -> FasterStatus {
    // Validate pointers.
    if key_len > 0 && key_ptr.is_null() {
        return FasterStatus::InvalidArgument;
    }
    if val_len > 0 && val_ptr.is_null() {
        return FasterStatus::InvalidArgument;
    }

    // SAFETY: Caller guarantees pointer validity per doc contract.
    let key = if key_len > 0 {
        unsafe { std::slice::from_raw_parts(key_ptr, key_len as usize) }.to_vec()
    } else {
        Vec::new()
    };
    let val = if val_len > 0 {
        unsafe { std::slice::from_raw_parts(val_ptr, val_len as usize) }.to_vec()
    } else {
        Vec::new()
    };

    match with_store_session(store, session, |kv, sess| kv.upsert(sess, &key, &val, ())) {
        Ok(status) => to_ffi_status(status),
        Err(e) => e,
    }
}

/// Read the value for a key into a caller-provided buffer.
///
/// # Parameters
///
/// - `store` — Store handle from [`faster_open`].
/// - `session` — Session handle from [`faster_session_start`].
/// - `key_ptr` / `key_len` — Pointer and length of the key bytes.
/// - `val_buf` / `val_buf_len` — Caller-provided output buffer and its capacity.
/// - `val_out_len` — On success, written with the actual value length (even if
///   the buffer was too small, so the caller can retry with a larger buffer).
///
/// # Returns
///
/// - [`FasterStatus::Ok`] — Value copied into `val_buf`.
/// - [`FasterStatus::BufferTooSmall`] — Value exists but `val_buf_len` is too
///   small. `*val_out_len` is set to the required size.
/// - [`FasterStatus::NotFound`] — Key does not exist.
/// - [`FasterStatus::Pending`] — Async I/O issued; call [`faster_complete_pending`].
///
/// # Safety
///
/// - `key_ptr` must be valid for reads of `key_len` bytes (or null if `key_len == 0`).
/// - `val_buf` must be valid for writes of `val_buf_len` bytes (or null if `val_buf_len == 0`).
/// - `val_out_len` must be a valid, non-null pointer to a `u32`.
/// - Must be called from the thread that owns `session`.
#[no_mangle]
pub unsafe extern "C" fn faster_read(
    store: FasterHandle,
    session: FasterHandle,
    key_ptr: *const u8,
    key_len: u32,
    val_buf: *mut u8,
    val_buf_len: u32,
    val_out_len: *mut u32,
) -> FasterStatus {
    // Validate required pointer.
    if val_out_len.is_null() {
        return FasterStatus::InvalidArgument;
    }
    if key_len > 0 && key_ptr.is_null() {
        return FasterStatus::InvalidArgument;
    }
    if val_buf_len > 0 && val_buf.is_null() {
        return FasterStatus::InvalidArgument;
    }

    // SAFETY: Caller guarantees pointer validity per doc contract.
    let key = if key_len > 0 {
        unsafe { std::slice::from_raw_parts(key_ptr, key_len as usize) }.to_vec()
    } else {
        Vec::new()
    };

    let result = with_store_session(store, session, |kv, sess| {
        let mut output: Option<Vec<u8>> = None;
        let status = kv.read(sess, &key, &Vec::new(), &mut output, ());
        (status, output)
    });

    let (status, output) = match result {
        Ok(pair) => pair,
        Err(e) => return e,
    };

    match status {
        s if s.is_not_found() => {
            // SAFETY: val_out_len validated non-null above.
            unsafe { val_out_len.write(0) };
            FasterStatus::NotFound
        }
        s if s.is_pending() => {
            // SAFETY: val_out_len validated non-null above.
            unsafe { val_out_len.write(0) };
            FasterStatus::Pending
        }
        s if s.is_success() => {
            if let Some(ref data) = output {
                let data_len = data.len() as u32;
                // SAFETY: val_out_len validated non-null above.
                unsafe { val_out_len.write(data_len) };

                if data_len > val_buf_len {
                    return FasterStatus::BufferTooSmall;
                }

                if !data.is_empty() {
                    // SAFETY: val_buf validated for val_buf_len bytes above,
                    // and data_len <= val_buf_len.
                    unsafe {
                        std::ptr::copy_nonoverlapping(data.as_ptr(), val_buf, data.len());
                    }
                }
                FasterStatus::Ok
            } else {
                // Success but no output — treat as not-found.
                // SAFETY: val_out_len validated non-null above.
                unsafe { val_out_len.write(0) };
                FasterStatus::NotFound
            }
        }
        _ => {
            // SAFETY: val_out_len validated non-null above.
            unsafe { val_out_len.write(0) };
            FasterStatus::InternalError
        }
    }
}

/// Delete a key from the store.
///
/// # Parameters
///
/// - `store` — Store handle from [`faster_open`].
/// - `session` — Session handle from [`faster_session_start`].
/// - `key_ptr` / `key_len` — Pointer and length of the key bytes.
///
/// # Returns
///
/// [`FasterStatus::Ok`] if the key was deleted, [`FasterStatus::NotFound`]
/// if the key did not exist.
///
/// # Safety
///
/// - `key_ptr` must be valid for reads of `key_len` bytes (or null if `key_len == 0`).
/// - Must be called from the thread that owns `session`.
#[no_mangle]
pub unsafe extern "C" fn faster_delete(
    store: FasterHandle,
    session: FasterHandle,
    key_ptr: *const u8,
    key_len: u32,
) -> FasterStatus {
    if key_len > 0 && key_ptr.is_null() {
        return FasterStatus::InvalidArgument;
    }

    // SAFETY: Caller guarantees pointer validity per doc contract.
    let key = if key_len > 0 {
        unsafe { std::slice::from_raw_parts(key_ptr, key_len as usize) }.to_vec()
    } else {
        Vec::new()
    };

    match with_store_session(store, session, |kv, sess| kv.delete(sess, &key, ())) {
        Ok(status) => to_ffi_status(status),
        Err(e) => e,
    }
}

/// Read-modify-write: atomically read and replace the value for a key.
///
/// For [`ByteSliceFunctions`], RMW performs full-value replacement (the input
/// becomes the new value). If the key does not exist, it is created.
///
/// # Parameters
///
/// - `store` — Store handle from [`faster_open`].
/// - `session` — Session handle from [`faster_session_start`].
/// - `key_ptr` / `key_len` — Pointer and length of the key bytes.
/// - `input_ptr` / `input_len` — Pointer and length of the new value bytes.
///
/// # Returns
///
/// A [`FasterStatus`] indicating the outcome.
///
/// # Safety
///
/// - `key_ptr` must be valid for reads of `key_len` bytes (or null if `key_len == 0`).
/// - `input_ptr` must be valid for reads of `input_len` bytes (or null if `input_len == 0`).
/// - Must be called from the thread that owns `session`.
#[no_mangle]
pub unsafe extern "C" fn faster_rmw(
    store: FasterHandle,
    session: FasterHandle,
    key_ptr: *const u8,
    key_len: u32,
    input_ptr: *const u8,
    input_len: u32,
) -> FasterStatus {
    if key_len > 0 && key_ptr.is_null() {
        return FasterStatus::InvalidArgument;
    }
    if input_len > 0 && input_ptr.is_null() {
        return FasterStatus::InvalidArgument;
    }

    // SAFETY: Caller guarantees pointer validity per doc contract.
    let key = if key_len > 0 {
        unsafe { std::slice::from_raw_parts(key_ptr, key_len as usize) }.to_vec()
    } else {
        Vec::new()
    };
    let input = if input_len > 0 {
        unsafe { std::slice::from_raw_parts(input_ptr, input_len as usize) }.to_vec()
    } else {
        Vec::new()
    };

    match with_store_session(store, session, |kv, sess| {
        let mut output: Option<Vec<u8>> = None;
        kv.rmw(sess, &key, &input, &mut output, ())
    }) {
        Ok(status) => to_ffi_status(status),
        Err(e) => e,
    }
}

/// Drain completed asynchronous I/O operations for a session.
///
/// Call this after receiving [`FasterStatus::Pending`] from a CRUD operation.
/// Returns the number of completed operations via `completed_out`.
///
/// # Safety
///
/// - `completed_out` must be a valid, non-null pointer to a `u32`.
/// - Must be called from the thread that owns `session`.
#[no_mangle]
pub unsafe extern "C" fn faster_complete_pending(
    store: FasterHandle,
    session: FasterHandle,
    completed_out: *mut u32,
) -> FasterStatus {
    if completed_out.is_null() {
        return FasterStatus::InvalidArgument;
    }

    match with_store_session(store, session, |kv, sess| {
        let results = kv.complete_pending(sess);
        results.len() as u32
    }) {
        Ok(count) => {
            // SAFETY: completed_out validated non-null above.
            unsafe { completed_out.write(count) };
            FasterStatus::Ok
        }
        Err(e) => e,
    }
}

// ══════════════════════════════════════════════════════════════════════
// Tests
// ══════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    // ── Store lifecycle ────────────────────────────────────────────

    #[test]
    fn open_close_roundtrip() {
        let h = faster_open();
        assert_ne!(h, INVALID_HANDLE);
        assert_eq!(faster_close(h), FasterStatus::Ok);
    }

    #[test]
    fn close_invalid_handle() {
        assert_eq!(faster_close(INVALID_HANDLE), FasterStatus::InvalidHandle);
        assert_eq!(faster_close(9999), FasterStatus::InvalidHandle);
    }

    #[test]
    fn double_close() {
        let h = faster_open();
        assert_eq!(faster_close(h), FasterStatus::Ok);
        assert_eq!(faster_close(h), FasterStatus::InvalidHandle);
    }

    // ── Session lifecycle ──────────────────────────────────────────

    #[test]
    fn session_start_end() {
        let store = faster_open();
        let sess = faster_session_start(store);
        assert_ne!(sess, INVALID_HANDLE);
        assert_eq!(faster_session_end(store, sess), FasterStatus::Ok);
        faster_close(store);
    }

    #[test]
    fn session_start_invalid_store() {
        assert_eq!(faster_session_start(INVALID_HANDLE), INVALID_HANDLE);
        assert_eq!(faster_session_start(9999), INVALID_HANDLE);
    }

    #[test]
    fn session_end_invalid_handles() {
        let store = faster_open();
        assert_eq!(
            faster_session_end(store, INVALID_HANDLE),
            FasterStatus::InvalidHandle
        );
        assert_eq!(
            faster_session_end(store, 9999),
            FasterStatus::InvalidHandle
        );
        faster_close(store);
    }

    #[test]
    fn session_double_end() {
        let store = faster_open();
        let sess = faster_session_start(store);
        assert_eq!(faster_session_end(store, sess), FasterStatus::Ok);
        assert_eq!(
            faster_session_end(store, sess),
            FasterStatus::InvalidHandle
        );
        faster_close(store);
    }

    // ── Upsert ─────────────────────────────────────────────────────

    #[test]
    fn upsert_basic() {
        let store = faster_open();
        let sess = faster_session_start(store);

        let key = b"key1";
        let val = b"value1";
        let status = unsafe {
            faster_upsert(
                store,
                sess,
                key.as_ptr(),
                key.len() as u32,
                val.as_ptr(),
                val.len() as u32,
            )
        };
        assert!(status.is_success(), "upsert returned {status:?}");

        faster_session_end(store, sess);
        faster_close(store);
    }

    #[test]
    fn upsert_null_key_nonzero_len() {
        let store = faster_open();
        let sess = faster_session_start(store);

        let status = unsafe {
            faster_upsert(
                store,
                sess,
                std::ptr::null(),
                10, // non-zero len with null ptr
                b"v".as_ptr(),
                1,
            )
        };
        assert_eq!(status, FasterStatus::InvalidArgument);

        faster_session_end(store, sess);
        faster_close(store);
    }

    #[test]
    fn upsert_null_val_nonzero_len() {
        let store = faster_open();
        let sess = faster_session_start(store);

        let status = unsafe {
            faster_upsert(
                store,
                sess,
                b"k".as_ptr(),
                1,
                std::ptr::null(),
                10, // non-zero len with null ptr
            )
        };
        assert_eq!(status, FasterStatus::InvalidArgument);

        faster_session_end(store, sess);
        faster_close(store);
    }

    #[test]
    fn upsert_invalid_store() {
        let store = faster_open();
        let sess = faster_session_start(store);

        let status =
            unsafe { faster_upsert(9999, sess, b"k".as_ptr(), 1, b"v".as_ptr(), 1) };
        assert_eq!(status, FasterStatus::InvalidHandle);

        faster_session_end(store, sess);
        faster_close(store);
    }

    #[test]
    fn upsert_invalid_session() {
        let store = faster_open();

        let status =
            unsafe { faster_upsert(store, 9999, b"k".as_ptr(), 1, b"v".as_ptr(), 1) };
        assert_eq!(status, FasterStatus::InvalidHandle);

        faster_close(store);
    }

    #[test]
    fn upsert_empty_key_and_value() {
        let store = faster_open();
        let sess = faster_session_start(store);

        // Zero-length key and value with null pointers should work.
        let status = unsafe {
            faster_upsert(store, sess, std::ptr::null(), 0, std::ptr::null(), 0)
        };
        assert!(status.is_success(), "empty upsert returned {status:?}");

        faster_session_end(store, sess);
        faster_close(store);
    }

    // ── Read ───────────────────────────────────────────────────────

    #[test]
    fn read_after_upsert() {
        let store = faster_open();
        let sess = faster_session_start(store);

        let key = b"hello";
        let val = b"world";
        unsafe {
            faster_upsert(
                store,
                sess,
                key.as_ptr(),
                key.len() as u32,
                val.as_ptr(),
                val.len() as u32,
            );
        }

        let mut buf = [0u8; 64];
        let mut out_len: u32 = 0;
        let status = unsafe {
            faster_read(
                store,
                sess,
                key.as_ptr(),
                key.len() as u32,
                buf.as_mut_ptr(),
                buf.len() as u32,
                &mut out_len,
            )
        };
        assert_eq!(status, FasterStatus::Ok);
        assert_eq!(out_len, 5);
        assert_eq!(&buf[..out_len as usize], b"world");

        faster_session_end(store, sess);
        faster_close(store);
    }

    #[test]
    fn read_not_found() {
        let store = faster_open();
        let sess = faster_session_start(store);

        let key = b"nonexistent";
        let mut buf = [0u8; 64];
        let mut out_len: u32 = 0;
        let status = unsafe {
            faster_read(
                store,
                sess,
                key.as_ptr(),
                key.len() as u32,
                buf.as_mut_ptr(),
                buf.len() as u32,
                &mut out_len,
            )
        };
        assert_eq!(status, FasterStatus::NotFound);
        assert_eq!(out_len, 0);

        faster_session_end(store, sess);
        faster_close(store);
    }

    #[test]
    fn read_buffer_too_small() {
        let store = faster_open();
        let sess = faster_session_start(store);

        let key = b"key";
        let val = b"a long value that won't fit";
        unsafe {
            faster_upsert(
                store,
                sess,
                key.as_ptr(),
                key.len() as u32,
                val.as_ptr(),
                val.len() as u32,
            );
        }

        // Provide a buffer that's too small.
        let mut buf = [0u8; 4];
        let mut out_len: u32 = 0;
        let status = unsafe {
            faster_read(
                store,
                sess,
                key.as_ptr(),
                key.len() as u32,
                buf.as_mut_ptr(),
                buf.len() as u32,
                &mut out_len,
            )
        };
        assert_eq!(status, FasterStatus::BufferTooSmall);
        // out_len should tell us the actual size needed.
        assert_eq!(out_len, val.len() as u32);

        // Now retry with a sufficiently large buffer.
        let mut big_buf = vec![0u8; out_len as usize];
        let mut out_len2: u32 = 0;
        let status2 = unsafe {
            faster_read(
                store,
                sess,
                key.as_ptr(),
                key.len() as u32,
                big_buf.as_mut_ptr(),
                big_buf.len() as u32,
                &mut out_len2,
            )
        };
        assert_eq!(status2, FasterStatus::Ok);
        assert_eq!(out_len2, val.len() as u32);
        assert_eq!(&big_buf[..out_len2 as usize], val);

        faster_session_end(store, sess);
        faster_close(store);
    }

    #[test]
    fn read_null_out_len() {
        let store = faster_open();
        let sess = faster_session_start(store);

        let mut buf = [0u8; 64];
        let status = unsafe {
            faster_read(
                store,
                sess,
                b"k".as_ptr(),
                1,
                buf.as_mut_ptr(),
                buf.len() as u32,
                std::ptr::null_mut(), // null val_out_len
            )
        };
        assert_eq!(status, FasterStatus::InvalidArgument);

        faster_session_end(store, sess);
        faster_close(store);
    }

    #[test]
    fn read_invalid_handles() {
        let store = faster_open();
        let sess = faster_session_start(store);

        let mut buf = [0u8; 64];
        let mut out_len: u32 = 0;

        // Invalid store.
        let s1 = unsafe {
            faster_read(
                9999,
                sess,
                b"k".as_ptr(),
                1,
                buf.as_mut_ptr(),
                64,
                &mut out_len,
            )
        };
        assert_eq!(s1, FasterStatus::InvalidHandle);

        // Invalid session.
        let s2 = unsafe {
            faster_read(
                store,
                9999,
                b"k".as_ptr(),
                1,
                buf.as_mut_ptr(),
                64,
                &mut out_len,
            )
        };
        assert_eq!(s2, FasterStatus::InvalidHandle);

        faster_session_end(store, sess);
        faster_close(store);
    }

    // ── Delete ─────────────────────────────────────────────────────

    #[test]
    fn delete_after_upsert() {
        let store = faster_open();
        let sess = faster_session_start(store);

        let key = b"to_delete";
        let val = b"temporary";
        unsafe {
            faster_upsert(
                store,
                sess,
                key.as_ptr(),
                key.len() as u32,
                val.as_ptr(),
                val.len() as u32,
            );
        }

        let status = unsafe {
            faster_delete(store, sess, key.as_ptr(), key.len() as u32)
        };
        assert!(status.is_success(), "delete returned {status:?}");

        // Verify key is gone.
        let mut buf = [0u8; 64];
        let mut out_len: u32 = 0;
        let read_status = unsafe {
            faster_read(
                store,
                sess,
                key.as_ptr(),
                key.len() as u32,
                buf.as_mut_ptr(),
                buf.len() as u32,
                &mut out_len,
            )
        };
        assert_eq!(read_status, FasterStatus::NotFound);

        faster_session_end(store, sess);
        faster_close(store);
    }

    #[test]
    fn delete_nonexistent_key() {
        let store = faster_open();
        let sess = faster_session_start(store);

        let key = b"never_inserted";
        let status = unsafe {
            faster_delete(store, sess, key.as_ptr(), key.len() as u32)
        };
        // Deleting a non-existent key returns NotFound.
        assert_eq!(status, FasterStatus::NotFound);

        faster_session_end(store, sess);
        faster_close(store);
    }

    #[test]
    fn delete_invalid_args() {
        let store = faster_open();
        let sess = faster_session_start(store);

        // Null key with nonzero length.
        let status =
            unsafe { faster_delete(store, sess, std::ptr::null(), 10) };
        assert_eq!(status, FasterStatus::InvalidArgument);

        faster_session_end(store, sess);
        faster_close(store);
    }

    // ── RMW ────────────────────────────────────────────────────────

    #[test]
    fn rmw_creates_then_replaces() {
        let store = faster_open();
        let sess = faster_session_start(store);

        let key = b"rmw_key";

        // RMW on non-existent key should create it.
        let val1 = b"first";
        let status = unsafe {
            faster_rmw(
                store,
                sess,
                key.as_ptr(),
                key.len() as u32,
                val1.as_ptr(),
                val1.len() as u32,
            )
        };
        assert!(status.is_success(), "rmw create returned {status:?}");

        // RMW again should replace.
        let val2 = b"second";
        let status2 = unsafe {
            faster_rmw(
                store,
                sess,
                key.as_ptr(),
                key.len() as u32,
                val2.as_ptr(),
                val2.len() as u32,
            )
        };
        assert!(status2.is_success(), "rmw replace returned {status2:?}");

        // Read should return latest.
        let mut buf = [0u8; 64];
        let mut out_len: u32 = 0;
        let rs = unsafe {
            faster_read(
                store,
                sess,
                key.as_ptr(),
                key.len() as u32,
                buf.as_mut_ptr(),
                buf.len() as u32,
                &mut out_len,
            )
        };
        assert_eq!(rs, FasterStatus::Ok);
        assert_eq!(&buf[..out_len as usize], b"second");

        faster_session_end(store, sess);
        faster_close(store);
    }

    #[test]
    fn rmw_invalid_args() {
        let store = faster_open();
        let sess = faster_session_start(store);

        // Null key with nonzero length.
        let s1 = unsafe {
            faster_rmw(store, sess, std::ptr::null(), 10, b"v".as_ptr(), 1)
        };
        assert_eq!(s1, FasterStatus::InvalidArgument);

        // Null input with nonzero length.
        let s2 = unsafe {
            faster_rmw(store, sess, b"k".as_ptr(), 1, std::ptr::null(), 10)
        };
        assert_eq!(s2, FasterStatus::InvalidArgument);

        faster_session_end(store, sess);
        faster_close(store);
    }

    // ── Complete pending ────────────────────────────────────────────

    #[test]
    fn complete_pending_empty() {
        let store = faster_open();
        let sess = faster_session_start(store);

        let mut completed: u32 = 0;
        let status = unsafe {
            faster_complete_pending(store, sess, &mut completed)
        };
        assert_eq!(status, FasterStatus::Ok);
        assert_eq!(completed, 0);

        faster_session_end(store, sess);
        faster_close(store);
    }

    #[test]
    fn complete_pending_null_out() {
        let store = faster_open();
        let sess = faster_session_start(store);

        let status = unsafe {
            faster_complete_pending(store, sess, std::ptr::null_mut())
        };
        assert_eq!(status, FasterStatus::InvalidArgument);

        faster_session_end(store, sess);
        faster_close(store);
    }

    // ── Integration: full CRUD round-trip ───────────────────────────

    #[test]
    fn full_crud_roundtrip() {
        let store = faster_open();
        let sess = faster_session_start(store);

        // 1. Upsert 10 key-value pairs.
        for i in 0u32..10 {
            let key = format!("key-{i}");
            let val = format!("val-{i}");
            let status = unsafe {
                faster_upsert(
                    store,
                    sess,
                    key.as_ptr(),
                    key.len() as u32,
                    val.as_ptr(),
                    val.len() as u32,
                )
            };
            assert!(status.is_success(), "upsert {i} returned {status:?}");
        }

        // 2. Read all back and verify.
        for i in 0u32..10 {
            let key = format!("key-{i}");
            let expected = format!("val-{i}");
            let mut buf = [0u8; 64];
            let mut out_len: u32 = 0;
            let status = unsafe {
                faster_read(
                    store,
                    sess,
                    key.as_ptr(),
                    key.len() as u32,
                    buf.as_mut_ptr(),
                    buf.len() as u32,
                    &mut out_len,
                )
            };
            assert_eq!(status, FasterStatus::Ok, "read {i}");
            assert_eq!(
                &buf[..out_len as usize],
                expected.as_bytes(),
                "value mismatch for key-{i}"
            );
        }

        // 3. RMW to update even keys.
        for i in (0u32..10).step_by(2) {
            let key = format!("key-{i}");
            let new_val = format!("updated-{i}");
            let status = unsafe {
                faster_rmw(
                    store,
                    sess,
                    key.as_ptr(),
                    key.len() as u32,
                    new_val.as_ptr(),
                    new_val.len() as u32,
                )
            };
            assert!(status.is_success(), "rmw {i} returned {status:?}");
        }

        // 4. Verify even keys were updated, odd keys unchanged.
        for i in 0u32..10 {
            let key = format!("key-{i}");
            let expected = if i % 2 == 0 {
                format!("updated-{i}")
            } else {
                format!("val-{i}")
            };
            let mut buf = [0u8; 64];
            let mut out_len: u32 = 0;
            unsafe {
                faster_read(
                    store,
                    sess,
                    key.as_ptr(),
                    key.len() as u32,
                    buf.as_mut_ptr(),
                    buf.len() as u32,
                    &mut out_len,
                );
            }
            assert_eq!(
                &buf[..out_len as usize],
                expected.as_bytes(),
                "mismatch for key-{i}"
            );
        }

        // 5. Delete odd keys.
        for i in (1u32..10).step_by(2) {
            let key = format!("key-{i}");
            let status = unsafe {
                faster_delete(store, sess, key.as_ptr(), key.len() as u32)
            };
            assert!(status.is_success(), "delete {i} returned {status:?}");
        }

        // 6. Verify odd keys are gone, even keys still there.
        for i in 0u32..10 {
            let key = format!("key-{i}");
            let mut buf = [0u8; 64];
            let mut out_len: u32 = 0;
            let status = unsafe {
                faster_read(
                    store,
                    sess,
                    key.as_ptr(),
                    key.len() as u32,
                    buf.as_mut_ptr(),
                    buf.len() as u32,
                    &mut out_len,
                )
            };
            if i % 2 == 0 {
                assert_eq!(status, FasterStatus::Ok, "even key-{i} should exist");
            } else {
                assert_eq!(
                    status,
                    FasterStatus::NotFound,
                    "odd key-{i} should be deleted"
                );
            }
        }

        // 7. Complete pending (should be 0 for in-memory store).
        let mut completed: u32 = 0;
        unsafe {
            faster_complete_pending(store, sess, &mut completed);
        }
        assert_eq!(completed, 0);

        // Cleanup.
        faster_session_end(store, sess);
        faster_close(store);
    }

    // ── Multiple sessions ──────────────────────────────────────────

    #[test]
    fn multiple_sessions_isolated() {
        let store = faster_open();
        let sess1 = faster_session_start(store);
        let sess2 = faster_session_start(store);
        assert_ne!(sess1, sess2);

        // Write via session 1.
        let key = b"shared_key";
        let val = b"from_s1";
        unsafe {
            faster_upsert(
                store,
                sess1,
                key.as_ptr(),
                key.len() as u32,
                val.as_ptr(),
                val.len() as u32,
            );
        }

        // Read via session 2.
        let mut buf = [0u8; 64];
        let mut out_len: u32 = 0;
        let status = unsafe {
            faster_read(
                store,
                sess2,
                key.as_ptr(),
                key.len() as u32,
                buf.as_mut_ptr(),
                buf.len() as u32,
                &mut out_len,
            )
        };
        assert_eq!(status, FasterStatus::Ok);
        assert_eq!(&buf[..out_len as usize], b"from_s1");

        faster_session_end(store, sess1);
        faster_session_end(store, sess2);
        faster_close(store);
    }

    // ── Edge cases ─────────────────────────────────────────────────

    #[test]
    fn upsert_overwrite() {
        let store = faster_open();
        let sess = faster_session_start(store);

        let key = b"overwrite";
        let val1 = b"first";
        let val2 = b"second_longer";

        unsafe {
            faster_upsert(
                store,
                sess,
                key.as_ptr(),
                key.len() as u32,
                val1.as_ptr(),
                val1.len() as u32,
            );
            faster_upsert(
                store,
                sess,
                key.as_ptr(),
                key.len() as u32,
                val2.as_ptr(),
                val2.len() as u32,
            );
        }

        let mut buf = [0u8; 64];
        let mut out_len: u32 = 0;
        let status = unsafe {
            faster_read(
                store,
                sess,
                key.as_ptr(),
                key.len() as u32,
                buf.as_mut_ptr(),
                buf.len() as u32,
                &mut out_len,
            )
        };
        assert_eq!(status, FasterStatus::Ok);
        assert_eq!(&buf[..out_len as usize], b"second_longer");

        faster_session_end(store, sess);
        faster_close(store);
    }

    #[test]
    fn read_zero_length_value() {
        let store = faster_open();
        let sess = faster_session_start(store);

        let key = b"empty_val";
        // Upsert with empty value.
        unsafe {
            faster_upsert(
                store,
                sess,
                key.as_ptr(),
                key.len() as u32,
                std::ptr::null(),
                0,
            );
        }

        let mut buf = [0u8; 64];
        let mut out_len: u32 = 0;
        let status = unsafe {
            faster_read(
                store,
                sess,
                key.as_ptr(),
                key.len() as u32,
                buf.as_mut_ptr(),
                buf.len() as u32,
                &mut out_len,
            )
        };
        assert_eq!(status, FasterStatus::Ok);
        assert_eq!(out_len, 0);

        faster_session_end(store, sess);
        faster_close(store);
    }
}
