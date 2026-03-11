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
//! - [`functions`] — `CallbackFunctions` — `Vec<u8>` key/value `Functions` impl for FFI
//! - [`session`] — Session wrapper types for the handle table
//!
//! ## FFI Functions
//!
//! ### Store Lifecycle
//! - [`faster_open`] — Create a new in-memory FASTER store, returns an opaque handle.
//! - [`faster_open_with_path`] — Create a file-backed FASTER store (required for checkpoint/recovery).
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
//! ### Checkpoint / Recovery
//! - [`faster_checkpoint`] — Persist the store to disk, returns a token.
//! - [`faster_recover`] — Restore a store from a checkpoint token.
//!
//! ## Thread Safety
//!
//! Store handles are thread-safe. Session handles are **not** — they must be
//! used exclusively from the thread that created them. Passing a session handle
//! across threads is undefined behavior.

#![deny(unsafe_op_in_unsafe_fn)]
#![warn(missing_docs)]
#![forbid(clippy::undocumented_unsafe_blocks)]

pub mod callbacks;
pub mod error;
pub mod functions;
pub mod handle;
pub mod session;

use std::panic::{self, AssertUnwindSafe};
use std::path::Path;
use std::sync::OnceLock;

use faster_core::checkpoint::{CheckpointToken, CheckpointType};
use faster_core::status::OperationStatus;
use faster_core::{FasterKv, FasterKvConfig, NullDevice, SyncFileDevice};

use crate::callbacks::CallbackFunctions;
use crate::error::FasterStatus;
use crate::handle::{FasterHandle, HandleTable, INVALID_HANDLE};
use crate::session::SessionCell;

/// The concrete store type used by the FFI layer.
type FfiStore = FasterKv<CallbackFunctions>;

/// Global handle table for **store** handles.
///
/// Separate from the session table to avoid nested `RwLock` acquisitions
/// (which can deadlock with writer-preferring pthread rwlocks on Linux).
fn store_handles() -> &'static HandleTable {
    static HANDLES: OnceLock<HandleTable> = OnceLock::new();
    HANDLES.get_or_init(HandleTable::new)
}

/// Global handle table for **session** handles.
fn session_handles() -> &'static HandleTable {
    static HANDLES: OnceLock<HandleTable> = OnceLock::new();
    HANDLES.get_or_init(HandleTable::new)
}

// ── C-compatible checkpoint type ────────────────────────────────────

/// C-compatible checkpoint strategy enum.
///
/// Maps to the internal [`CheckpointType`] used by faster-core.
///
/// # ABI Stability
///
/// This enum is `#[repr(C)]` and its discriminant values are part of the
/// public ABI. New variants may be added, but existing values must never
/// change.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FasterCheckpointType {
    /// Flush all in-memory pages to the main log file.
    FoldOver = 0,
    /// Write a snapshot of the mutable region to a separate file.
    Snapshot = 1,
}

impl FasterCheckpointType {
    fn to_core(self) -> CheckpointType {
        match self {
            Self::FoldOver => CheckpointType::FoldOver,
            Self::Snapshot => CheckpointType::Snapshot,
        }
    }
}

/// Result of a successful checkpoint operation.
///
/// The token is a 128-bit value split into high and low 64-bit halves
/// for C ABI compatibility (C does not have a standard `u128` type).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FasterCheckpointResult {
    /// High 64 bits of the checkpoint token.
    pub token_high: u64,
    /// Low 64 bits of the checkpoint token.
    pub token_low: u64,
    /// Operation status.
    pub status: FasterStatus,
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
        // Revivified is semantically an in-place update (sealed record reused).
        OperationStatus::Revivified => FasterStatus::InPlaceUpdated,
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
/// The store uses an in-memory null device and cannot be checkpointed.
/// Use [`faster_open_with_path`] for checkpoint/recovery support.
///
/// # Thread Safety
///
/// The returned handle is safe to use from any thread.
#[unsafe(no_mangle)]
pub extern "C" fn faster_open() -> FasterHandle {
    panic::catch_unwind(|| {
        let store = FfiStore::new(
            FasterKvConfig::default(),
            CallbackFunctions,
            NullDevice::new(),
        );
        store_handles().insert(store)
    })
    .unwrap_or(INVALID_HANDLE)
}

/// Create a new FASTER key-value store backed by files in `path`.
///
/// Returns an opaque store handle, or [`INVALID_HANDLE`] on failure
/// (e.g., if the path cannot be created or the pointer is null).
///
/// The store uses a synchronous file device and supports checkpoint/recovery.
///
/// # Safety
///
/// - `path_ptr` must be a valid pointer to `path_len` bytes of UTF-8 data.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn faster_open_with_path(path_ptr: *const u8, path_len: u32) -> FasterHandle {
    if path_len > 0 && path_ptr.is_null() {
        return INVALID_HANDLE;
    }

    let path_bytes = if path_len > 0 {
        // SAFETY: Caller guarantees pointer validity per doc contract.
        unsafe { std::slice::from_raw_parts(path_ptr, path_len as usize) }
    } else {
        return INVALID_HANDLE;
    };

    let path_str = match std::str::from_utf8(path_bytes) {
        Ok(s) => s,
        Err(_) => return INVALID_HANDLE,
    };

    // catch_unwind protects against panics from device/store creation.
    let path_owned = path_str.to_owned();
    panic::catch_unwind(AssertUnwindSafe(|| {
        let device = match SyncFileDevice::new(&path_owned, "log.", 512, 1 << 30, 1) {
            Ok(d) => d,
            Err(_) => return INVALID_HANDLE,
        };

        let store = FfiStore::new(FasterKvConfig::default(), CallbackFunctions, device);
        store_handles().insert(store)
    }))
    .unwrap_or(INVALID_HANDLE)
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
#[unsafe(no_mangle)]
pub extern "C" fn faster_close(store: FasterHandle) -> FasterStatus {
    panic::catch_unwind(AssertUnwindSafe(|| {
        match store_handles().remove::<FfiStore>(store) {
            Some(_) => FasterStatus::Ok,
            None => FasterStatus::InvalidHandle,
        }
    }))
    .unwrap_or(FasterStatus::InternalError)
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
#[unsafe(no_mangle)]
pub extern "C" fn faster_session_start(store: FasterHandle) -> FasterHandle {
    panic::catch_unwind(AssertUnwindSafe(|| {
        let result = store_handles().with::<FfiStore, _>(store, |kv| {
            let session = kv.new_session();
            let cell = SessionCell::new(session);
            session_handles().insert(cell)
        });
        result.unwrap_or(INVALID_HANDLE)
    }))
    .unwrap_or(INVALID_HANDLE)
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
#[unsafe(no_mangle)]
pub extern "C" fn faster_session_end(
    store: FasterHandle,
    session_handle: FasterHandle,
) -> FasterStatus {
    panic::catch_unwind(AssertUnwindSafe(|| {
        // Remove the session from the handle table first.
        let cell = match session_handles().remove::<SessionCell>(session_handle) {
            Some(c) => c,
            None => return FasterStatus::InvalidHandle,
        };

        // Dispose via the store (releases epoch thread resources).
        let session = cell.inner.into_inner();
        let disposed = store_handles().with::<FfiStore, _>(store, |kv| {
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
    }))
    .unwrap_or(FasterStatus::InternalError)
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
    store_handles()
        .with::<FfiStore, _>(store, |kv| {
            session_handles()
                .with::<SessionCell, _>(session_handle, |cell| {
                    // Runtime thread-affinity check (FFI-02).
                    if !cell.check_thread() {
                        return Err(FasterStatus::ThreadMismatch);
                    }
                    // SAFETY: We just verified this is the owning thread, and
                    // the FFI contract forbids concurrent calls on the same session.
                    let session = unsafe { &mut *cell.inner.get() };
                    Ok(f(kv, session))
                })
                .ok_or(FasterStatus::InvalidHandle)?
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
#[unsafe(no_mangle)]
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
        // SAFETY: key_ptr is valid for key_len bytes per FFI caller contract.
        unsafe { std::slice::from_raw_parts(key_ptr, key_len as usize) }.to_vec()
    } else {
        Vec::new()
    };
    let val = if val_len > 0 {
        // SAFETY: Caller guarantees pointer validity per doc contract.
        unsafe { std::slice::from_raw_parts(val_ptr, val_len as usize) }.to_vec()
    } else {
        Vec::new()
    };

    // catch_unwind prevents panics from crossing the FFI boundary.
    panic::catch_unwind(AssertUnwindSafe(|| {
        match with_store_session(store, session, |kv, sess| kv.upsert(sess, &key, &val, ())) {
            Ok(outcome) => to_ffi_status(outcome.status()),
            Err(e) => e,
        }
    }))
    .unwrap_or(FasterStatus::InternalError)
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
#[unsafe(no_mangle)]
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
        // SAFETY: key_ptr is valid for key_len bytes per FFI caller contract.
        unsafe { std::slice::from_raw_parts(key_ptr, key_len as usize) }.to_vec()
    } else {
        Vec::new()
    };

    // catch_unwind prevents panics from crossing the FFI boundary.
    let result = panic::catch_unwind(AssertUnwindSafe(|| {
        with_store_session(store, session, |kv, sess| {
            let mut output: Option<Vec<u8>> = None;
            let status = kv.read(sess, &key, &Vec::new(), &mut output, ());
            (status, output)
        })
    }));

    let inner = match result {
        Ok(inner) => inner,
        Err(_) => return FasterStatus::InternalError,
    };

    let (status, output) = match inner {
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
#[unsafe(no_mangle)]
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
        // SAFETY: key_ptr is valid for key_len bytes per FFI caller contract.
        unsafe { std::slice::from_raw_parts(key_ptr, key_len as usize) }.to_vec()
    } else {
        Vec::new()
    };

    // catch_unwind prevents panics from crossing the FFI boundary.
    panic::catch_unwind(AssertUnwindSafe(|| {
        match with_store_session(store, session, |kv, sess| kv.delete(sess, &key, ())) {
            Ok(outcome) => to_ffi_status(outcome.status()),
            Err(e) => e,
        }
    }))
    .unwrap_or(FasterStatus::InternalError)
}

/// Read-modify-write: atomically read and replace the value for a key.
///
/// For [`CallbackFunctions`], RMW performs full-value replacement (the input
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
#[unsafe(no_mangle)]
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
        // SAFETY: key_ptr is valid for key_len bytes per FFI caller contract.
        unsafe { std::slice::from_raw_parts(key_ptr, key_len as usize) }.to_vec()
    } else {
        Vec::new()
    };
    let input = if input_len > 0 {
        // SAFETY: Caller guarantees pointer validity per doc contract.
        unsafe { std::slice::from_raw_parts(input_ptr, input_len as usize) }.to_vec()
    } else {
        Vec::new()
    };

    // catch_unwind prevents panics from crossing the FFI boundary.
    panic::catch_unwind(AssertUnwindSafe(|| {
        match with_store_session(store, session, |kv, sess| {
            let mut output: Option<Vec<u8>> = None;
            kv.rmw(sess, &key, &input, &mut output, ())
        }) {
            Ok(outcome) => to_ffi_status(outcome.status()),
            Err(e) => e,
        }
    }))
    .unwrap_or(FasterStatus::InternalError)
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
#[unsafe(no_mangle)]
pub unsafe extern "C" fn faster_complete_pending(
    store: FasterHandle,
    session: FasterHandle,
    completed_out: *mut u32,
) -> FasterStatus {
    if completed_out.is_null() {
        return FasterStatus::InvalidArgument;
    }

    // catch_unwind prevents panics from crossing the FFI boundary.
    let result = panic::catch_unwind(AssertUnwindSafe(|| {
        with_store_session(store, session, |kv, sess| {
            let results = kv.complete_pending(sess);
            results.len() as u32
        })
    }));

    match result {
        Ok(Ok(count)) => {
            // SAFETY: completed_out validated non-null above.
            unsafe { completed_out.write(count) };
            FasterStatus::Ok
        }
        Ok(Err(e)) => e,
        Err(_) => FasterStatus::InternalError,
    }
}

// ── Checkpoint / Recovery ───────────────────────────────────────────

/// Take a checkpoint of the current store state.
///
/// Persists the hash index and hybrid log to `checkpoint_dir`. Returns
/// the checkpoint token as a pair of `u64` values (high/low halves of
/// a 128-bit identifier) via the output pointers.
///
/// # Parameters
///
/// - `store` — Store handle from [`faster_open_with_path`].
/// - `checkpoint_dir_ptr` / `checkpoint_dir_len` — UTF-8 directory path
///   where checkpoint files will be written.
/// - `checkpoint_type` — [`FasterCheckpointType::FoldOver`] or
///   [`FasterCheckpointType::Snapshot`].
/// - `token_high_out` / `token_low_out` — On success, written with the
///   high/low 64-bit halves of the checkpoint token.
///
/// # Returns
///
/// [`FasterStatus::Ok`] on success, [`FasterStatus::CheckpointError`] on
/// failure.
///
/// # Safety
///
/// - `checkpoint_dir_ptr` must be a valid pointer to `checkpoint_dir_len`
///   bytes of UTF-8 data.
/// - `token_high_out` and `token_low_out` must be valid, non-null pointers.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn faster_checkpoint(
    store: FasterHandle,
    checkpoint_dir_ptr: *const u8,
    checkpoint_dir_len: u32,
    checkpoint_type: FasterCheckpointType,
    token_high_out: *mut u64,
    token_low_out: *mut u64,
) -> FasterStatus {
    if token_high_out.is_null() || token_low_out.is_null() {
        return FasterStatus::InvalidArgument;
    }
    if checkpoint_dir_len == 0 || checkpoint_dir_ptr.is_null() {
        return FasterStatus::InvalidArgument;
    }

    // SAFETY: Caller guarantees pointer validity per doc contract.
    let dir_bytes =
        unsafe { std::slice::from_raw_parts(checkpoint_dir_ptr, checkpoint_dir_len as usize) };
    let dir_str = match std::str::from_utf8(dir_bytes) {
        Ok(s) => s,
        Err(_) => return FasterStatus::InvalidArgument,
    };

    let dir_owned = dir_str.to_owned();
    let ct = checkpoint_type.to_core();

    // catch_unwind prevents panics from crossing the FFI boundary.
    let result = panic::catch_unwind(AssertUnwindSafe(|| {
        let dir_path = Path::new(&dir_owned);
        store_handles().with::<FfiStore, _>(store, |kv| kv.checkpoint(dir_path, ct))
    }));

    match result {
        Ok(Some(Ok(token))) => {
            let raw = token.as_u128();
            // SAFETY: Pointers validated non-null above.
            unsafe {
                token_high_out.write((raw >> 64) as u64);
                token_low_out.write(raw as u64);
            }
            FasterStatus::Ok
        }
        Ok(Some(Err(_))) => FasterStatus::CheckpointError,
        Ok(None) => FasterStatus::InvalidHandle,
        Err(_) => FasterStatus::InternalError,
    }
}

/// Recover a store from a checkpoint.
///
/// Restores the hash index and log from the checkpoint identified by
/// the given token (high/low 64-bit halves). The store must have been
/// created with [`faster_open_with_path`] pointing to the same directory
/// used during checkpoint.
///
/// All sessions **must** be ended before calling this function.
///
/// # Parameters
///
/// - `store` — Store handle from [`faster_open_with_path`].
/// - `checkpoint_dir_ptr` / `checkpoint_dir_len` — UTF-8 directory path
///   containing checkpoint files.
/// - `token_high` / `token_low` — The checkpoint token to recover. Pass
///   both as `0` to recover the most recent checkpoint.
///
/// # Returns
///
/// [`FasterStatus::Ok`] on success, [`FasterStatus::CheckpointError`] on
/// failure.
///
/// # Safety
///
/// - `checkpoint_dir_ptr` must be a valid pointer to `checkpoint_dir_len`
///   bytes of UTF-8 data.
/// - No sessions may be active on the store when this is called.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn faster_recover(
    store: FasterHandle,
    checkpoint_dir_ptr: *const u8,
    checkpoint_dir_len: u32,
    token_high: u64,
    token_low: u64,
) -> FasterStatus {
    if checkpoint_dir_len == 0 || checkpoint_dir_ptr.is_null() {
        return FasterStatus::InvalidArgument;
    }

    // SAFETY: Caller guarantees pointer validity per doc contract.
    let dir_bytes =
        unsafe { std::slice::from_raw_parts(checkpoint_dir_ptr, checkpoint_dir_len as usize) };
    let dir_str = match std::str::from_utf8(dir_bytes) {
        Ok(s) => s,
        Err(_) => return FasterStatus::InvalidArgument,
    };

    let dir_owned = dir_str.to_owned();
    let token = if token_high == 0 && token_low == 0 {
        None
    } else {
        Some(CheckpointToken::new(
            ((token_high as u128) << 64) | (token_low as u128),
        ))
    };

    // catch_unwind prevents panics from crossing the FFI boundary.
    let result = panic::catch_unwind(AssertUnwindSafe(|| {
        let dir_path = Path::new(&dir_owned);
        store_handles().with_mut::<FfiStore, _>(store, |kv| kv.recover(dir_path, token))
    }));

    match result {
        Ok(Some(Ok(_))) => FasterStatus::Ok,
        Ok(Some(Err(_))) => FasterStatus::CheckpointError,
        Ok(None) => FasterStatus::InvalidHandle,
        Err(_) => FasterStatus::InternalError,
    }
}

// ── Destroy (alias for close) ───────────────────────────────────────

/// Destroy a store handle. This is an alias for [`faster_close`].
///
/// Provided for API symmetry with `faster_open` / `faster_destroy`.
#[unsafe(no_mangle)]
pub extern "C" fn faster_destroy(store: FasterHandle) -> FasterStatus {
    faster_close(store)
}

// ── Session refresh ─────────────────────────────────────────────────

/// Refresh the session epoch and complete any ready pending operations.
///
/// # Parameters
///
/// - `store` — Store handle from [`faster_open`].
/// - `session` — Session handle from [`faster_session_start`].
/// - `completed_out` — If non-null, written with the number of completed ops.
///
/// # Returns
///
/// [`FasterStatus::Ok`] on success.
///
/// # Safety
///
/// - `completed_out`, if non-null, must point to a valid, writable `u32`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn faster_session_refresh(
    store: FasterHandle,
    session: FasterHandle,
    completed_out: *mut u32,
) -> FasterStatus {
    panic::catch_unwind(AssertUnwindSafe(|| {
        match with_store_session(store, session, |kv, sess| kv.refresh(sess)) {
            Ok(result) => {
                if !completed_out.is_null() {
                    // SAFETY: Caller guarantees `completed_out` is valid and writable.
                    unsafe { *completed_out = result.completed };
                }
                FasterStatus::Ok
            }
            Err(status) => status,
        }
    }))
    .unwrap_or(FasterStatus::InternalError)
}

// ── Wait for all pending ────────────────────────────────────────────

/// Block until all pending operations on this session complete.
///
/// # Parameters
///
/// - `store` — Store handle from [`faster_open`].
/// - `session` — Session handle from [`faster_session_start`].
/// - `completed_out` — If non-null, written with the number of completed ops.
///
/// # Returns
///
/// [`FasterStatus::Ok`] on success.
///
/// # Safety
///
/// - `completed_out`, if non-null, must point to a valid, writable `u32`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn faster_wait_for_all_pending(
    store: FasterHandle,
    session: FasterHandle,
    completed_out: *mut u32,
) -> FasterStatus {
    panic::catch_unwind(AssertUnwindSafe(|| {
        match with_store_session(store, session, |kv, sess| kv.complete_pending_sync(sess)) {
            Ok(results) => {
                if !completed_out.is_null() {
                    // SAFETY: Caller guarantees `completed_out` is valid and writable.
                    unsafe { *completed_out = results.len() as u32 };
                }
                FasterStatus::Ok
            }
            Err(status) => status,
        }
    }))
    .unwrap_or(FasterStatus::InternalError)
}

// ══════════════════════════════════════════════════════════════════════
// Tests
// ══════════════════════════════════════════════════════════════════════

// ══════════════════════════════════════════════════════════════════════
// Extended operations with C callback function pointers
// ══════════════════════════════════════════════════════════════════════

/// Helper: extract a byte slice from an FFI pointer+length pair.
///
/// Returns an empty slice if `len == 0`, regardless of pointer value.
///
/// # Safety
///
/// `ptr` must be valid for reads of `len` bytes when `len > 0`.
unsafe fn extract_bytes(ptr: *const u8, len: u32) -> Vec<u8> {
    if len > 0 {
        // SAFETY: Caller guarantees pointer validity.
        unsafe { std::slice::from_raw_parts(ptr, len as usize) }.to_vec()
    } else {
        Vec::new()
    }
}

/// Read-modify-write with user-supplied C callback function pointers.
///
/// When callback pointers are non-null, they override the default
/// byte-slice replacement semantics for this single RMW operation.
/// Null callback pointers fall back to the standard behavior.
///
/// # Parameters
///
/// - `store` / `session` — Store and session handles.
/// - `key_ptr` / `key_len` — Key bytes.
/// - `input_ptr` / `input_len` — Input bytes (the modification delta).
/// - `initial_cb` — Called to initialize a new value when the key is not found.
/// - `copy_cb` — Called to create a new value by copying and modifying a read-only record.
/// - `atomic_cb` — Called for in-place update in the mutable region.
///
/// # Returns
///
/// A [`FasterStatus`] indicating the outcome.
///
/// # Safety
///
/// Same pointer requirements as [`faster_rmw`], plus callback function
/// pointers (if non-null) must be valid `extern "C"` functions.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn faster_rmw_ex(
    store: FasterHandle,
    session: FasterHandle,
    key_ptr: *const u8,
    key_len: u32,
    input_ptr: *const u8,
    input_len: u32,
    initial_cb: Option<crate::callbacks::FasterRmwInitialFn>,
    copy_cb: Option<crate::callbacks::FasterRmwCopyFn>,
    atomic_cb: Option<crate::callbacks::FasterRmwAtomicFn>,
) -> FasterStatus {
    if key_len > 0 && key_ptr.is_null() {
        return FasterStatus::InvalidArgument;
    }
    if input_len > 0 && input_ptr.is_null() {
        return FasterStatus::InvalidArgument;
    }

    // SAFETY: Caller guarantees pointer validity per doc contract.
    let key = unsafe { extract_bytes(key_ptr, key_len) };
    // SAFETY: Caller guarantees pointer validity per doc contract.
    let input = unsafe { extract_bytes(input_ptr, input_len) };

    panic::catch_unwind(AssertUnwindSafe(|| {
        // Install callbacks for this operation via RAII guard.
        let _guard =
            crate::callbacks::RmwCallbackGuard::install(crate::callbacks::RmwCallbackSet {
                initial: initial_cb,
                copy_update: copy_cb,
                in_place: atomic_cb,
            });

        match with_store_session(store, session, |kv, sess| {
            let mut output: Option<Vec<u8>> = None;
            kv.rmw(sess, &key, &input, &mut output, ())
        }) {
            Ok(outcome) => to_ffi_status(outcome.status()),
            Err(e) => e,
        }
    }))
    .unwrap_or(FasterStatus::InternalError)
}

/// Upsert with user-supplied C callback function pointers.
///
/// When callback pointers are non-null, they override the default
/// byte-slice replacement semantics for this single Upsert operation.
///
/// # Parameters
///
/// - `store` / `session` — Store and session handles.
/// - `key_ptr` / `key_len` — Key bytes.
/// - `input_ptr` / `input_len` — Input bytes (the new value data).
/// - `put_cb` — Called to write a new value.
/// - `put_atomic_cb` — Called for in-place update in the mutable region.
///
/// # Safety
///
/// Same pointer requirements as [`faster_upsert`], plus callback function
/// pointers (if non-null) must be valid `extern "C"` functions.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn faster_upsert_ex(
    store: FasterHandle,
    session: FasterHandle,
    key_ptr: *const u8,
    key_len: u32,
    input_ptr: *const u8,
    input_len: u32,
    put_cb: Option<crate::callbacks::FasterUpsertPutFn>,
    put_atomic_cb: Option<crate::callbacks::FasterUpsertPutAtomicFn>,
) -> FasterStatus {
    if key_len > 0 && key_ptr.is_null() {
        return FasterStatus::InvalidArgument;
    }
    if input_len > 0 && input_ptr.is_null() {
        return FasterStatus::InvalidArgument;
    }

    // SAFETY: Caller guarantees pointer validity per doc contract.
    let key = unsafe { extract_bytes(key_ptr, key_len) };
    // SAFETY: Caller guarantees pointer validity per doc contract.
    let input = unsafe { extract_bytes(input_ptr, input_len) };

    panic::catch_unwind(AssertUnwindSafe(|| {
        let _guard =
            crate::callbacks::UpsertCallbackGuard::install(crate::callbacks::UpsertCallbackSet {
                put: put_cb,
                _put_atomic: put_atomic_cb,
            });

        match with_store_session(store, session, |kv, sess| kv.upsert(sess, &key, &input, ())) {
            Ok(outcome) => to_ffi_status(outcome.status()),
            Err(e) => e,
        }
    }))
    .unwrap_or(FasterStatus::InternalError)
}

/// Read with user-supplied C callback function pointers.
///
/// When callback pointers are non-null, they override the default
/// behavior (copy value → output buffer) for this single Read operation.
///
/// # Parameters
///
/// - `store` / `session` — Store and session handles.
/// - `key_ptr` / `key_len` — Key bytes.
/// - `output_ptr` / `output_buf_len` — Output buffer and its capacity.
/// - `output_len` — Written with the actual output length on success.
/// - `get_cb` — Called to extract output from the read record.
/// - `get_atomic_cb` — Called for reads from the mutable region.
///
/// # Safety
///
/// Same pointer requirements as [`faster_read`], plus callback function
/// pointers (if non-null) must be valid `extern "C"` functions.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn faster_read_ex(
    store: FasterHandle,
    session: FasterHandle,
    key_ptr: *const u8,
    key_len: u32,
    output_ptr: *mut u8,
    output_buf_len: u32,
    output_len: *mut u32,
    get_cb: Option<crate::callbacks::FasterReadGetFn>,
    get_atomic_cb: Option<crate::callbacks::FasterReadGetAtomicFn>,
) -> FasterStatus {
    if key_len > 0 && key_ptr.is_null() {
        return FasterStatus::InvalidArgument;
    }
    if output_buf_len > 0 && output_ptr.is_null() {
        return FasterStatus::InvalidArgument;
    }

    // SAFETY: Caller guarantees pointer validity per doc contract.
    let key = unsafe { extract_bytes(key_ptr, key_len) };

    panic::catch_unwind(AssertUnwindSafe(|| {
        let _guard =
            crate::callbacks::ReadCallbackGuard::install(crate::callbacks::ReadCallbackSet {
                get: get_cb,
                _get_atomic: get_atomic_cb,
            });

        match with_store_session(store, session, |kv, sess| {
            let input = Vec::new();
            let mut output: Option<Vec<u8>> = None;
            let outcome = kv.read(sess, &key, &input, &mut output, ());
            (outcome.status(), output)
        }) {
            Ok((status, output_opt)) => {
                let ffi_status = to_ffi_status(status);
                if let Some(output_data) = output_opt {
                    let copy_len = output_data.len().min(output_buf_len as usize);
                    if copy_len > 0 && !output_ptr.is_null() {
                        // SAFETY: output_ptr valid for output_buf_len bytes per caller contract.
                        unsafe {
                            std::ptr::copy_nonoverlapping(
                                output_data.as_ptr(),
                                output_ptr,
                                copy_len,
                            );
                        }
                    }
                    if !output_len.is_null() {
                        // SAFETY: output_len is valid per caller contract.
                        unsafe { *output_len = output_data.len() as u32 };
                    }
                }
                ffi_status
            }
            Err(e) => e,
        }
    }))
    .unwrap_or(FasterStatus::InternalError)
}

/// Refresh the session's epoch, draining completed operations.
///
/// This is an alias for [`faster_session_refresh`] provided for
/// API symmetry with the C++ `Refresh()` method.
///
/// # Safety
///
/// - `completed_out`, if non-null, must point to a valid, writable `u32`.
/// - Must be called from the thread that owns `session`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn faster_refresh(
    store: FasterHandle,
    session: FasterHandle,
    completed_out: *mut u32,
) -> FasterStatus {
    // SAFETY: Same requirements as faster_session_refresh.
    unsafe { faster_session_refresh(store, session, completed_out) }
}

/// Create a fresh session (session continuation stub).
///
/// In the C++ FASTER API, `ContinueSession` resumes a session from a
/// checkpoint token. This Rust implementation does not yet support true
/// session continuation, so this creates a fresh session and writes
/// serial number 0 to `serial_out`.
///
/// # Parameters
///
/// - `store` — Store handle from [`faster_open`].
/// - `serial_out` — If non-null, written with the serial number (always 0).
///
/// # Returns
///
/// A new session handle, or [`INVALID_HANDLE`] on failure.
///
/// # Safety
///
/// - `serial_out`, if non-null, must point to a valid, writable `u64`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn faster_continue_session(
    store: FasterHandle,
    serial_out: *mut u64,
) -> FasterHandle {
    panic::catch_unwind(AssertUnwindSafe(|| {
        let result = store_handles().with::<FfiStore, _>(store, |kv| {
            let session = kv.new_session();
            let cell = SessionCell::new(session);
            session_handles().insert(cell)
        });
        let handle = match result {
            Some(h) => h,
            None => return INVALID_HANDLE,
        };

        if !serial_out.is_null() {
            // SAFETY: Caller guarantees serial_out is valid and writable.
            unsafe { *serial_out = 0 };
        }

        handle
    }))
    .unwrap_or(INVALID_HANDLE)
}

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
        assert_eq!(faster_session_end(store, 9999), FasterStatus::InvalidHandle);
        faster_close(store);
    }

    #[test]
    fn session_double_end() {
        let store = faster_open();
        let sess = faster_session_start(store);
        assert_eq!(faster_session_end(store, sess), FasterStatus::Ok);
        assert_eq!(faster_session_end(store, sess), FasterStatus::InvalidHandle);
        faster_close(store);
    }

    // ── Upsert ─────────────────────────────────────────────────────

    #[test]
    fn upsert_basic() {
        let store = faster_open();
        let sess = faster_session_start(store);

        let key = b"key1";
        let val = b"value1";
        // SAFETY: Test passes valid stack-allocated pointers with matching lengths.
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

        // SAFETY: Testing null-key error path; function validates before dereferencing.
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

        // SAFETY: Testing null-value error path; function validates before dereferencing.
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

        // SAFETY: Testing invalid-store error path; function validates handle before use.
        let status = unsafe { faster_upsert(9999, sess, b"k".as_ptr(), 1, b"v".as_ptr(), 1) };
        assert_eq!(status, FasterStatus::InvalidHandle);

        faster_session_end(store, sess);
        faster_close(store);
    }

    #[test]
    fn upsert_invalid_session() {
        let store = faster_open();

        // SAFETY: Testing invalid-session error path; function validates handle before use.
        let status = unsafe { faster_upsert(store, 9999, b"k".as_ptr(), 1, b"v".as_ptr(), 1) };
        assert_eq!(status, FasterStatus::InvalidHandle);

        faster_close(store);
    }

    #[test]
    fn upsert_empty_key_and_value() {
        let store = faster_open();
        let sess = faster_session_start(store);

        // Zero-length key and value with null pointers should work.
        // SAFETY: Zero-length slices with null pointers are valid; function skips dereferencing.
        let status =
            unsafe { faster_upsert(store, sess, std::ptr::null(), 0, std::ptr::null(), 0) };
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
        // SAFETY: Test passes valid stack-allocated pointers with matching lengths.
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
        // SAFETY: All pointers are valid stack references; buf is large enough for val_buf_len.
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
        // SAFETY: All pointers are valid stack references; buf is large enough for val_buf_len.
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
        // SAFETY: Test passes valid stack-allocated pointers with matching lengths.
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
        // SAFETY: All pointers are valid stack references; buf is intentionally small to test error path.
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
        // SAFETY: big_buf is heap-allocated with sufficient capacity; all pointers valid.
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
        // SAFETY: Testing null val_out_len error path; function validates before dereferencing.
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
        // SAFETY: Testing invalid-store error path; function validates handle before use.
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
        // SAFETY: Testing invalid-session error path; function validates handle before use.
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
        // SAFETY: Test passes valid stack-allocated pointers with matching lengths.
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

        // SAFETY: Test passes valid stack-allocated pointer with matching length.
        let status = unsafe { faster_delete(store, sess, key.as_ptr(), key.len() as u32) };
        assert!(status.is_success(), "delete returned {status:?}");

        // Verify key is gone.
        let mut buf = [0u8; 64];
        let mut out_len: u32 = 0;
        // SAFETY: All pointers are valid stack references; buf is large enough for val_buf_len.
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
        // SAFETY: Test passes valid stack-allocated pointer with matching length.
        let status = unsafe { faster_delete(store, sess, key.as_ptr(), key.len() as u32) };
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
        // SAFETY: Testing null-key error path; function validates before dereferencing.
        let status = unsafe { faster_delete(store, sess, std::ptr::null(), 10) };
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
        // SAFETY: Test passes valid stack-allocated pointers with matching lengths.
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
        // SAFETY: Test passes valid stack-allocated pointers with matching lengths.
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
        // SAFETY: All pointers are valid stack references; buf is large enough for val_buf_len.
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
        // SAFETY: Testing null-key error path; function validates before dereferencing.
        let s1 = unsafe { faster_rmw(store, sess, std::ptr::null(), 10, b"v".as_ptr(), 1) };
        assert_eq!(s1, FasterStatus::InvalidArgument);

        // Null input with nonzero length.
        // SAFETY: Testing null-input error path; function validates before dereferencing.
        let s2 = unsafe { faster_rmw(store, sess, b"k".as_ptr(), 1, std::ptr::null(), 10) };
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
        // SAFETY: All pointers are valid stack references; store and session are valid handles.
        let status = unsafe { faster_complete_pending(store, sess, &mut completed) };
        assert_eq!(status, FasterStatus::Ok);
        assert_eq!(completed, 0);

        faster_session_end(store, sess);
        faster_close(store);
    }

    #[test]
    fn complete_pending_null_out() {
        let store = faster_open();
        let sess = faster_session_start(store);

        // SAFETY: Testing null completed_out error path; function validates before dereferencing.
        let status = unsafe { faster_complete_pending(store, sess, std::ptr::null_mut()) };
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
            // SAFETY: key and val are valid heap-allocated Strings; pointers and lengths match.
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
            // SAFETY: key is a valid String; buf and out_len are valid stack references.
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
            // SAFETY: key and new_val are valid heap-allocated Strings; pointers and lengths match.
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
            // SAFETY: key is a valid String; buf and out_len are valid stack references.
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
            // SAFETY: key is a valid String; pointer and length match.
            let status = unsafe { faster_delete(store, sess, key.as_ptr(), key.len() as u32) };
            assert!(status.is_success(), "delete {i} returned {status:?}");
        }

        // 6. Verify odd keys are gone, even keys still there.
        for i in 0u32..10 {
            let key = format!("key-{i}");
            let mut buf = [0u8; 64];
            let mut out_len: u32 = 0;
            // SAFETY: key is a valid String; buf and out_len are valid stack references.
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
        // SAFETY: All pointers are valid stack references; store and session are valid handles.
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
        // SAFETY: Test passes valid stack-allocated pointers with matching lengths.
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
        // SAFETY: All pointers are valid stack references; buf is large enough for val_buf_len.
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

        // SAFETY: Test passes valid stack-allocated pointers with matching lengths.
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
        // SAFETY: All pointers are valid stack references; buf is large enough for val_buf_len.
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
        // SAFETY: Key pointer is valid; null value pointer is safe with zero length.
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
        // SAFETY: All pointers are valid stack references; buf is large enough for val_buf_len.
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

    // ── F3: Multi-threaded sessions ────────────────────────────────

    #[test]
    fn multi_threaded_sessions() {
        let store = faster_open();
        let num_threads = 4;
        let ops_per_thread = 50;

        let threads: Vec<_> = (0..num_threads)
            .map(|t| {
                std::thread::spawn(move || {
                    // Each thread creates its own session (sessions are per-thread).
                    let sess = faster_session_start(store);
                    assert_ne!(sess, INVALID_HANDLE);

                    for i in 0..ops_per_thread {
                        let key = format!("t{t}-k{i}");
                        let val = format!("t{t}-v{i}");
                        // SAFETY: key and val are valid heap Strings; pointers and lengths match.
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
                        assert!(status.is_success(), "thread {t} upsert {i}: {status:?}");
                    }

                    // Read back all keys written by this thread.
                    for i in 0..ops_per_thread {
                        let key = format!("t{t}-k{i}");
                        let expected = format!("t{t}-v{i}");
                        let mut buf = [0u8; 128];
                        let mut out_len: u32 = 0;
                        // SAFETY: All pointers are valid stack/heap refs.
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
                        assert_eq!(status, FasterStatus::Ok, "thread {t} read {i}");
                        assert_eq!(
                            &buf[..out_len as usize],
                            expected.as_bytes(),
                            "thread {t} value mismatch at {i}"
                        );
                    }

                    // Complete pending.
                    let mut completed: u32 = 0;
                    // SAFETY: Valid stack pointer.
                    unsafe {
                        faster_complete_pending(store, sess, &mut completed);
                    }

                    faster_session_end(store, sess);
                })
            })
            .collect();

        for t in threads {
            t.join().expect("thread panicked");
        }
        faster_close(store);
    }

    #[test]
    fn session_from_wrong_store() {
        let store1 = faster_open();
        let store2 = faster_open();

        let sess1 = faster_session_start(store1);
        assert_ne!(sess1, INVALID_HANDLE);

        // Use session from store1 with store2 — the CRUD call should still work
        // because the session handle is valid in the global table, but ending
        // the session on the wrong store should dispose it on a foreign store.
        // The real guarantee is that session_start with wrong store returns error.
        assert_eq!(faster_session_start(INVALID_HANDLE), INVALID_HANDLE);
        assert_eq!(faster_session_start(9999), INVALID_HANDLE);

        // Ending a session handle that doesn't exist is InvalidHandle.
        assert_eq!(
            faster_session_end(store2, 9999),
            FasterStatus::InvalidHandle
        );

        // Clean up correctly.
        faster_session_end(store1, sess1);
        faster_close(store1);
        faster_close(store2);
    }

    #[test]
    fn session_end_wrong_store_handle() {
        let store1 = faster_open();
        let store2 = faster_open();

        let sess = faster_session_start(store1);
        assert_ne!(sess, INVALID_HANDLE);

        // Ending the session with the wrong store handle: the session is removed
        // from the handle table (so it won't leak), but dispose_session is called
        // on the wrong store. This is a programming error but should not crash.
        let status = faster_session_end(store2, sess);
        // The session handle is consumed regardless.
        assert_eq!(
            faster_session_end(store1, sess),
            FasterStatus::InvalidHandle
        );
        // store2 didn't own the session, but we expect Ok since the dispose is
        // a best-effort operation (the session object is still dropped correctly).
        assert!(
            status == FasterStatus::Ok || status == FasterStatus::InvalidHandle,
            "unexpected: {status:?}"
        );

        faster_close(store1);
        faster_close(store2);
    }

    // ── F5: Checkpoint / Recovery ──────────────────────────────────

    /// Helper: create a file-backed store in a temp directory.
    fn open_file_backed_store(dir: &std::path::Path) -> FasterHandle {
        let path = dir.to_str().unwrap();
        // SAFETY: path is a valid UTF-8 string from a TempDir.
        unsafe { faster_open_with_path(path.as_ptr(), path.len() as u32) }
    }

    #[test]
    fn open_with_path_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let store = open_file_backed_store(tmp.path());
        assert_ne!(store, INVALID_HANDLE);

        let sess = faster_session_start(store);
        assert_ne!(sess, INVALID_HANDLE);

        let key = b"pathkey";
        let val = b"pathval";
        // SAFETY: Valid stack pointers.
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
        assert!(status.is_success());

        faster_session_end(store, sess);
        faster_close(store);
    }

    #[test]
    fn open_with_path_null_ptr() {
        // SAFETY: Testing null path error path.
        let h = unsafe { faster_open_with_path(std::ptr::null(), 10) };
        assert_eq!(h, INVALID_HANDLE);
    }

    #[test]
    fn open_with_path_zero_len() {
        // SAFETY: Testing zero-length path error path.
        let h = unsafe { faster_open_with_path(b"/tmp".as_ptr(), 0) };
        assert_eq!(h, INVALID_HANDLE);
    }

    #[test]
    fn checkpoint_invalid_args() {
        let store = faster_open();

        let dir = b"/tmp/faster_test_checkpoint_invalid";
        let mut high: u64 = 0;
        let mut low: u64 = 0;

        // Null token_high_out.
        // SAFETY: Testing null pointer error path.
        let s = unsafe {
            faster_checkpoint(
                store,
                dir.as_ptr(),
                dir.len() as u32,
                FasterCheckpointType::FoldOver,
                std::ptr::null_mut(),
                &mut low,
            )
        };
        assert_eq!(s, FasterStatus::InvalidArgument);

        // Null token_low_out.
        // SAFETY: Testing null pointer error path.
        let s = unsafe {
            faster_checkpoint(
                store,
                dir.as_ptr(),
                dir.len() as u32,
                FasterCheckpointType::FoldOver,
                &mut high,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(s, FasterStatus::InvalidArgument);

        // Null dir ptr.
        // SAFETY: Testing null pointer error path.
        let s = unsafe {
            faster_checkpoint(
                store,
                std::ptr::null(),
                10,
                FasterCheckpointType::FoldOver,
                &mut high,
                &mut low,
            )
        };
        assert_eq!(s, FasterStatus::InvalidArgument);

        // Invalid store.
        // SAFETY: Testing invalid handle error path.
        let s = unsafe {
            faster_checkpoint(
                9999,
                dir.as_ptr(),
                dir.len() as u32,
                FasterCheckpointType::FoldOver,
                &mut high,
                &mut low,
            )
        };
        assert_eq!(s, FasterStatus::InvalidHandle);

        faster_close(store);
    }

    #[test]
    fn recover_invalid_args() {
        let store = faster_open();

        // Null dir ptr.
        // SAFETY: Testing null pointer error path.
        let s = unsafe { faster_recover(store, std::ptr::null(), 10, 0, 0) };
        assert_eq!(s, FasterStatus::InvalidArgument);

        // Zero-length dir.
        // SAFETY: Testing zero-length error path.
        let s = unsafe { faster_recover(store, b"/tmp".as_ptr(), 0, 0, 0) };
        assert_eq!(s, FasterStatus::InvalidArgument);

        // Invalid store.
        // SAFETY: Testing invalid handle error path.
        let s = unsafe { faster_recover(9999, b"/tmp".as_ptr(), 4, 0, 0) };
        assert_eq!(s, FasterStatus::InvalidHandle);

        faster_close(store);
    }

    #[test]
    fn checkpoint_fold_over_and_recover() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("store");

        // 1. Open file-backed store, write data, checkpoint.
        let store = open_file_backed_store(&data_dir);
        assert_ne!(store, INVALID_HANDLE);

        let sess = faster_session_start(store);
        assert_ne!(sess, INVALID_HANDLE);

        for i in 0u32..20 {
            let key = format!("ckpt-key-{i}");
            let val = format!("ckpt-val-{i}");
            // SAFETY: Valid heap String pointers.
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
        }

        let mut token_high: u64 = 0;
        let mut token_low: u64 = 0;
        let ckpt_path = data_dir.to_str().unwrap();
        // SAFETY: Valid pointers from TempDir and stack vars.
        let status = unsafe {
            faster_checkpoint(
                store,
                ckpt_path.as_ptr(),
                ckpt_path.len() as u32,
                FasterCheckpointType::FoldOver,
                &mut token_high,
                &mut token_low,
            )
        };
        assert_eq!(status, FasterStatus::Ok, "checkpoint failed");
        assert!(
            token_high != 0 || token_low != 0,
            "token should be non-zero"
        );

        // 2. End session, close store.
        faster_session_end(store, sess);
        faster_close(store);

        // 3. Open a new store, recover from checkpoint, verify data.
        let store2 = open_file_backed_store(&data_dir);
        assert_ne!(store2, INVALID_HANDLE);

        // SAFETY: Valid pointers.
        let recover_status = unsafe {
            faster_recover(
                store2,
                ckpt_path.as_ptr(),
                ckpt_path.len() as u32,
                token_high,
                token_low,
            )
        };
        assert_eq!(recover_status, FasterStatus::Ok, "recover failed");

        let sess2 = faster_session_start(store2);
        assert_ne!(sess2, INVALID_HANDLE);

        for i in 0u32..20 {
            let key = format!("ckpt-key-{i}");
            let expected = format!("ckpt-val-{i}");
            let mut buf = [0u8; 128];
            let mut out_len: u32 = 0;
            // SAFETY: Valid pointers.
            let status = unsafe {
                faster_read(
                    store2,
                    sess2,
                    key.as_ptr(),
                    key.len() as u32,
                    buf.as_mut_ptr(),
                    buf.len() as u32,
                    &mut out_len,
                )
            };
            assert_eq!(status, FasterStatus::Ok, "read key {i} after recover");
            assert_eq!(
                &buf[..out_len as usize],
                expected.as_bytes(),
                "value mismatch for key {i}"
            );
        }

        faster_session_end(store2, sess2);
        faster_close(store2);
    }

    #[test]
    fn checkpoint_snapshot_succeeds() {
        // Snapshot checkpoint creates metadata but the orchestrator doesn't yet
        // write snapshot data files, so we only test the checkpoint side here.
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("store");

        let store = open_file_backed_store(&data_dir);
        let sess = faster_session_start(store);

        for i in 0u32..10 {
            let key = format!("snap-{i}");
            let val = format!("snapval-{i}");
            // SAFETY: Valid heap String pointers.
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
        }

        let mut token_high: u64 = 0;
        let mut token_low: u64 = 0;
        let ckpt_path = data_dir.to_str().unwrap();
        // SAFETY: Valid pointers.
        let status = unsafe {
            faster_checkpoint(
                store,
                ckpt_path.as_ptr(),
                ckpt_path.len() as u32,
                FasterCheckpointType::Snapshot,
                &mut token_high,
                &mut token_low,
            )
        };
        assert_eq!(status, FasterStatus::Ok, "snapshot checkpoint failed");
        assert!(
            token_high != 0 || token_low != 0,
            "snapshot token should be non-zero"
        );

        faster_session_end(store, sess);
        faster_close(store);
    }

    #[test]
    fn checkpoint_type_enum_values() {
        assert_eq!(FasterCheckpointType::FoldOver as u32, 0);
        assert_eq!(FasterCheckpointType::Snapshot as u32, 1);
    }

    // ── Destroy tests ──────────────────────────────────────────────

    #[test]
    fn destroy_is_close_alias() {
        let h = faster_open();
        assert_ne!(h, INVALID_HANDLE);
        assert_eq!(faster_destroy(h), FasterStatus::Ok);
        assert_eq!(faster_destroy(h), FasterStatus::InvalidHandle);
    }

    // ── Session refresh tests ──────────────────────────────────────

    #[test]
    fn session_refresh_basic() {
        let store = faster_open();
        let sess = faster_session_start(store);
        let mut completed: u32 = 99;
        // SAFETY: `store` and `sess` are valid handles; `completed` is a valid mutable reference.
        // SAFETY: test-controlled pointers are valid.
        let status = unsafe { faster_session_refresh(store, sess, &mut completed) };
        assert_eq!(status, FasterStatus::Ok);
        assert_eq!(completed, 0);
        faster_session_end(store, sess);
        faster_close(store);
    }

    #[test]
    fn session_refresh_null_out() {
        let store = faster_open();
        let sess = faster_session_start(store);
        // SAFETY: `store` and `sess` are valid handles; null output pointer is explicitly supported.
        // SAFETY: null out-pointer is explicitly handled by the function.
        let status = unsafe { faster_session_refresh(store, sess, std::ptr::null_mut()) };
        assert_eq!(status, FasterStatus::Ok);
        faster_session_end(store, sess);
        faster_close(store);
    }

    #[test]
    fn session_refresh_invalid_handles() {
        let mut completed: u32 = 0;
        assert_eq!(
            // SAFETY: testing with invalid handles — the function is designed to return InvalidHandle.
            // SAFETY: test-controlled pointers are valid.
            unsafe { faster_session_refresh(999, 999, &mut completed) },
            FasterStatus::InvalidHandle,
        );
    }

    // ── Wait for all pending tests ─────────────────────────────────

    #[test]
    fn wait_for_all_pending_basic() {
        let store = faster_open();
        let sess = faster_session_start(store);
        let mut completed: u32 = 99;
        // SAFETY: `store` and `sess` are valid handles; `completed` is a valid mutable reference.
        // SAFETY: test-controlled pointers are valid.
        let status = unsafe { faster_wait_for_all_pending(store, sess, &mut completed) };
        assert_eq!(status, FasterStatus::Ok);
        assert_eq!(completed, 0);
        faster_session_end(store, sess);
        faster_close(store);
    }

    #[test]
    fn wait_for_all_pending_null_out() {
        let store = faster_open();
        let sess = faster_session_start(store);
        // SAFETY: `store` and `sess` are valid handles; null output pointer is explicitly supported.
        // SAFETY: null out-pointer is explicitly handled by the function.
        let status = unsafe { faster_wait_for_all_pending(store, sess, std::ptr::null_mut()) };
        assert_eq!(status, FasterStatus::Ok);
        faster_session_end(store, sess);
        faster_close(store);
    }

    // ── Thread-affinity enforcement tests ──────────────────────────

    #[test]
    fn thread_mismatch_upsert() {
        let store = faster_open();
        let sess = faster_session_start(store);
        let status = std::thread::spawn(move || {
            let key = b"k";
            let val = b"v";
            // SAFETY: `store` and `sess` are valid handles; pointers reference stack-allocated slices.
            unsafe {
                faster_upsert(
                    store,
                    sess,
                    key.as_ptr(),
                    key.len() as u32,
                    val.as_ptr(),
                    val.len() as u32,
                )
            }
        })
        .join()
        .unwrap();
        assert_eq!(status, FasterStatus::ThreadMismatch);
        faster_session_end(store, sess);
        faster_close(store);
    }

    #[test]
    fn thread_mismatch_read() {
        let store = faster_open();
        let sess = faster_session_start(store);
        let status = std::thread::spawn(move || {
            let key = b"k";
            let mut buf = [0u8; 64];
            let mut out_len: u32 = 0;
            // SAFETY: `store` and `sess` are valid handles; all pointers reference stack allocations.
            unsafe {
                faster_read(
                    store,
                    sess,
                    key.as_ptr(),
                    key.len() as u32,
                    buf.as_mut_ptr(),
                    buf.len() as u32,
                    &mut out_len,
                )
            }
        })
        .join()
        .unwrap();
        assert_eq!(status, FasterStatus::ThreadMismatch);
        faster_session_end(store, sess);
        faster_close(store);
    }

    #[test]
    fn thread_mismatch_delete() {
        let store = faster_open();
        let sess = faster_session_start(store);
        let status = std::thread::spawn(move || {
            let key = b"k";
            // SAFETY: `store` and `sess` are valid handles; key pointer references a stack-allocated slice.
            unsafe { faster_delete(store, sess, key.as_ptr(), key.len() as u32) }
        })
        .join()
        .unwrap();
        assert_eq!(status, FasterStatus::ThreadMismatch);
        faster_session_end(store, sess);
        faster_close(store);
    }

    #[test]
    fn thread_mismatch_complete_pending() {
        let store = faster_open();
        let sess = faster_session_start(store);
        let status = std::thread::spawn(move || {
            let mut completed: u32 = 0;
            // SAFETY: `store` and `sess` are valid handles; `completed` is a valid mutable reference.
            unsafe { faster_complete_pending(store, sess, &mut completed) }
        })
        .join()
        .unwrap();
        assert_eq!(status, FasterStatus::ThreadMismatch);
        faster_session_end(store, sess);
        faster_close(store);
    }

    #[test]
    fn thread_mismatch_refresh() {
        let store = faster_open();
        let sess = faster_session_start(store);
        let status = std::thread::spawn(move || {
            let mut completed: u32 = 0;
            // SAFETY: `store` and `sess` are valid handles; `completed` is a valid mutable reference.
            unsafe { faster_session_refresh(store, sess, &mut completed) }
        })
        .join()
        .unwrap();
        assert_eq!(status, FasterStatus::ThreadMismatch);
        faster_session_end(store, sess);
        faster_close(store);
    }

    #[test]
    fn same_thread_succeeds() {
        let store = faster_open();
        let sess = faster_session_start(store);
        let key = b"st";
        let val = b"ok";
        // SAFETY: `store` and `sess` are valid handles; all pointers reference stack allocations.
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
        assert!(status.is_success(), "expected success, got {status}");
        assert_ne!(status, FasterStatus::ThreadMismatch);
        faster_session_end(store, sess);
        faster_close(store);
    }

    #[test]
    fn thread_mismatch_status_code() {
        assert_eq!(FasterStatus::ThreadMismatch as u32, 105);
        assert!(FasterStatus::ThreadMismatch.is_error());
    }

    // ── Extended operation tests ────────────────────────────────────

    /// Sum-store RMW callback: initial sets value = input
    extern "C" fn test_sum_initial(
        _key_ptr: *const u8,
        _key_len: usize,
        input_ptr: *const u8,
        input_len: usize,
        value_ptr: *mut u8,
        value_len: *mut usize,
    ) -> i32 {
        // SAFETY: FFI call with valid store/session handles obtained from test setup.
        unsafe {
            std::ptr::copy_nonoverlapping(input_ptr, value_ptr, input_len);
            *value_len = input_len;
        }
        0
    }

    /// Sum-store RMW callback: atomic adds input (u64) to value (u64)
    extern "C" fn test_sum_atomic(
        _key_ptr: *const u8,
        _key_len: usize,
        input_ptr: *const u8,
        input_len: usize,
        value_ptr: *mut u8,
        value_len: usize,
    ) -> i32 {
        assert_eq!(input_len, 8);
        assert_eq!(value_len, 8);
        // SAFETY: FFI call with valid store/session handles obtained from test setup.
        unsafe {
            let delta =
                u64::from_le_bytes(std::slice::from_raw_parts(input_ptr, 8).try_into().unwrap());
            let current =
                u64::from_le_bytes(std::slice::from_raw_parts(value_ptr, 8).try_into().unwrap());
            let result = current + delta;
            std::ptr::copy_nonoverlapping(result.to_le_bytes().as_ptr(), value_ptr, 8);
        }
        0
    }

    /// Sum-store RMW callback: copy-update adds input to old value
    extern "C" fn test_sum_copy(
        _key_ptr: *const u8,
        _key_len: usize,
        input_ptr: *const u8,
        input_len: usize,
        old_ptr: *const u8,
        old_len: usize,
        new_ptr: *mut u8,
        new_len: *mut usize,
    ) -> i32 {
        assert_eq!(input_len, 8);
        assert_eq!(old_len, 8);
        // SAFETY: FFI call with valid store/session handles obtained from test setup.
        unsafe {
            let delta =
                u64::from_le_bytes(std::slice::from_raw_parts(input_ptr, 8).try_into().unwrap());
            let old_val =
                u64::from_le_bytes(std::slice::from_raw_parts(old_ptr, 8).try_into().unwrap());
            let result = old_val + delta;
            std::ptr::copy_nonoverlapping(result.to_le_bytes().as_ptr(), new_ptr, 8);
            *new_len = 8;
        }
        0
    }

    #[test]
    fn rmw_ex_sum_store_pattern() {
        let store = faster_open();
        let session = faster_session_start(store);

        let key = 42u64.to_le_bytes();
        let input = 10u64.to_le_bytes();

        // First RMW creates the record via initial callback
        // SAFETY: FFI call with valid store/session handles obtained from test setup.
        let status = unsafe {
            faster_rmw_ex(
                store,
                session,
                key.as_ptr(),
                key.len() as u32,
                input.as_ptr(),
                input.len() as u32,
                Some(test_sum_initial),
                Some(test_sum_copy),
                Some(test_sum_atomic),
            )
        };
        assert!(status.is_success());

        // Second RMW adds to existing via atomic callback
        let input2 = 5u64.to_le_bytes();
        // SAFETY: FFI call with valid store/session handles obtained from test setup.
        let status = unsafe {
            faster_rmw_ex(
                store,
                session,
                key.as_ptr(),
                key.len() as u32,
                input2.as_ptr(),
                input2.len() as u32,
                Some(test_sum_initial),
                Some(test_sum_copy),
                Some(test_sum_atomic),
            )
        };
        assert!(status.is_success());

        // Read back — should be 15
        let mut buf = [0u8; 8];
        let mut out_len: u32 = 0;
        // SAFETY: FFI call with valid store/session handles obtained from test setup.
        let status = unsafe {
            faster_read(
                store,
                session,
                key.as_ptr(),
                key.len() as u32,
                buf.as_mut_ptr(),
                buf.len() as u32,
                &mut out_len,
            )
        };
        assert!(status.is_success());
        assert_eq!(out_len, 8);
        assert_eq!(u64::from_le_bytes(buf), 15);

        faster_session_end(store, session);
        faster_close(store);
    }

    #[test]
    fn rmw_ex_null_callbacks_fallback() {
        let store = faster_open();
        let session = faster_session_start(store);

        let key = b"nulltest";
        let input = b"value1";

        // All-null callbacks → byte-slice replacement fallback
        // SAFETY: FFI call with valid store/session handles obtained from test setup.
        let status = unsafe {
            faster_rmw_ex(
                store,
                session,
                key.as_ptr(),
                key.len() as u32,
                input.as_ptr(),
                input.len() as u32,
                None,
                None,
                None,
            )
        };
        assert!(status.is_success());

        let mut buf = [0u8; 32];
        let mut out_len: u32 = 0;
        // SAFETY: FFI call with valid store/session handles obtained from test setup.
        let status = unsafe {
            faster_read(
                store,
                session,
                key.as_ptr(),
                key.len() as u32,
                buf.as_mut_ptr(),
                buf.len() as u32,
                &mut out_len,
            )
        };
        assert!(status.is_success());
        assert_eq!(&buf[..out_len as usize], b"value1");

        faster_session_end(store, session);
        faster_close(store);
    }

    #[test]
    fn upsert_ex_custom_put() {
        extern "C" fn uppercase_put(
            _key_ptr: *const u8,
            _key_len: usize,
            input_ptr: *const u8,
            input_len: usize,
            value_ptr: *mut u8,
            _value_len: usize,
            actual_len: *mut usize,
        ) -> i32 {
            // SAFETY: FFI call with valid store/session handles obtained from test setup.
            unsafe {
                let input = std::slice::from_raw_parts(input_ptr, input_len);
                for (i, &b) in input.iter().enumerate() {
                    *value_ptr.add(i) = b.to_ascii_uppercase();
                }
                *actual_len = input_len;
            }
            0
        }

        let store = faster_open();
        let session = faster_session_start(store);

        let key = b"upsert_key";
        let input = b"hello";

        // SAFETY: FFI call with valid store/session handles obtained from test setup.
        let status = unsafe {
            faster_upsert_ex(
                store,
                session,
                key.as_ptr(),
                key.len() as u32,
                input.as_ptr(),
                input.len() as u32,
                Some(uppercase_put),
                None,
            )
        };
        assert!(status.is_success());

        let mut buf = [0u8; 32];
        let mut out_len: u32 = 0;
        // SAFETY: FFI call with valid store/session handles obtained from test setup.
        let status = unsafe {
            faster_read(
                store,
                session,
                key.as_ptr(),
                key.len() as u32,
                buf.as_mut_ptr(),
                buf.len() as u32,
                &mut out_len,
            )
        };
        assert!(status.is_success());
        assert_eq!(&buf[..out_len as usize], b"HELLO");

        faster_session_end(store, session);
        faster_close(store);
    }

    #[test]
    fn upsert_ex_null_callbacks_fallback() {
        let store = faster_open();
        let session = faster_session_start(store);

        let key = b"upsert_null";
        let input = b"plain_value";

        // SAFETY: FFI call with valid store/session handles obtained from test setup.
        let status = unsafe {
            faster_upsert_ex(
                store,
                session,
                key.as_ptr(),
                key.len() as u32,
                input.as_ptr(),
                input.len() as u32,
                None,
                None,
            )
        };
        assert!(status.is_success());

        let mut buf = [0u8; 32];
        let mut out_len: u32 = 0;
        // SAFETY: FFI call with valid store/session handles obtained from test setup.
        let status = unsafe {
            faster_read(
                store,
                session,
                key.as_ptr(),
                key.len() as u32,
                buf.as_mut_ptr(),
                buf.len() as u32,
                &mut out_len,
            )
        };
        assert!(status.is_success());
        assert_eq!(&buf[..out_len as usize], b"plain_value");

        faster_session_end(store, session);
        faster_close(store);
    }

    #[test]
    fn read_ex_custom_result() {
        extern "C" fn prefix_read(
            _key_ptr: *const u8,
            _key_len: usize,
            value_ptr: *const u8,
            value_len: usize,
            output_ptr: *mut u8,
            output_len: *mut usize,
        ) -> i32 {
            // SAFETY: FFI call with valid store/session handles obtained from test setup.
            unsafe {
                let prefix = b"READ:";
                let total = prefix.len() + value_len;
                if total > *output_len {
                    *output_len = total;
                    return 1;
                }
                std::ptr::copy_nonoverlapping(prefix.as_ptr(), output_ptr, prefix.len());
                std::ptr::copy_nonoverlapping(value_ptr, output_ptr.add(prefix.len()), value_len);
                *output_len = total;
            }
            0
        }

        let store = faster_open();
        let session = faster_session_start(store);

        // Insert a value first
        let key = b"read_key";
        let value = b"data";
        // SAFETY: FFI call with valid store/session handles obtained from test setup.
        let status = unsafe {
            faster_upsert(
                store,
                session,
                key.as_ptr(),
                key.len() as u32,
                value.as_ptr(),
                value.len() as u32,
            )
        };
        assert!(status.is_success());

        // Read with custom callback
        let mut buf = [0u8; 32];
        let mut out_len: u32 = 0;
        // SAFETY: FFI call with valid store/session handles obtained from test setup.
        let status = unsafe {
            faster_read_ex(
                store,
                session,
                key.as_ptr(),
                key.len() as u32,
                buf.as_mut_ptr(),
                buf.len() as u32,
                &mut out_len,
                Some(prefix_read),
                Some(prefix_read),
            )
        };
        assert!(status.is_success());
        assert_eq!(&buf[..out_len as usize], b"READ:data");

        faster_session_end(store, session);
        faster_close(store);
    }

    #[test]
    fn read_ex_null_callbacks_fallback() {
        let store = faster_open();
        let session = faster_session_start(store);

        let key = b"read_null";
        let value = b"original";
        // SAFETY: FFI call with valid store/session handles obtained from test setup.
        let status = unsafe {
            faster_upsert(
                store,
                session,
                key.as_ptr(),
                key.len() as u32,
                value.as_ptr(),
                value.len() as u32,
            )
        };
        assert!(status.is_success());

        let mut buf = [0u8; 32];
        let mut out_len: u32 = 0;
        // SAFETY: FFI call with valid store/session handles obtained from test setup.
        let status = unsafe {
            faster_read_ex(
                store,
                session,
                key.as_ptr(),
                key.len() as u32,
                buf.as_mut_ptr(),
                buf.len() as u32,
                &mut out_len,
                None,
                None,
            )
        };
        assert!(status.is_success());
        assert_eq!(&buf[..out_len as usize], b"original");

        faster_session_end(store, session);
        faster_close(store);
    }

    #[test]
    fn refresh_basic() {
        let store = faster_open();
        let session = faster_session_start(store);

        let mut completed: u32 = 0;
        // SAFETY: FFI call with valid store/session handles obtained from test setup.
        let status = unsafe { faster_refresh(store, session, &mut completed) };
        assert_eq!(status, FasterStatus::Ok);

        faster_session_end(store, session);
        faster_close(store);
    }

    #[test]
    fn refresh_null_completed_out() {
        let store = faster_open();
        let session = faster_session_start(store);

        // SAFETY: FFI call with valid store/session handles obtained from test setup.
        let status = unsafe { faster_refresh(store, session, std::ptr::null_mut()) };
        assert_eq!(status, FasterStatus::Ok);

        faster_session_end(store, session);
        faster_close(store);
    }

    #[test]
    fn continue_session_basic() {
        let store = faster_open();
        let mut serial: u64 = 999;
        // SAFETY: FFI call with valid store/session handles obtained from test setup.
        let session = unsafe { faster_continue_session(store, &mut serial) };
        assert_ne!(session, INVALID_HANDLE);
        assert_eq!(serial, 0);

        // The continued session should be usable
        let key = b"cont_key";
        let value = b"cont_val";
        // SAFETY: FFI call with valid store/session handles obtained from test setup.
        let status = unsafe {
            faster_upsert(
                store,
                session,
                key.as_ptr(),
                key.len() as u32,
                value.as_ptr(),
                value.len() as u32,
            )
        };
        assert!(status.is_success());

        faster_session_end(store, session);
        faster_close(store);
    }

    #[test]
    fn continue_session_null_serial() {
        let store = faster_open();
        // SAFETY: FFI call with valid store/session handles obtained from test setup.
        let session = unsafe { faster_continue_session(store, std::ptr::null_mut()) };
        assert_ne!(session, INVALID_HANDLE);

        faster_session_end(store, session);
        faster_close(store);
    }

    #[test]
    fn continue_session_invalid_store() {
        // SAFETY: FFI call with valid store/session handles obtained from test setup.
        let session = unsafe { faster_continue_session(INVALID_HANDLE, std::ptr::null_mut()) };
        assert_eq!(session, INVALID_HANDLE);
    }

    #[test]
    fn async_callback_fn_type_compatible() {
        // Verify the type can hold a valid extern "C" function pointer
        extern "C" fn dummy_async_cb(_status: u8, _context: u64) {}
        let _cb: crate::callbacks::FasterAsyncCallbackFn = dummy_async_cb;
        let _opt: Option<crate::callbacks::FasterAsyncCallbackFn> = Some(dummy_async_cb);
        // Option<extern "C" fn> should be the same size as a function pointer (niche optimization)
        assert_eq!(
            std::mem::size_of::<Option<crate::callbacks::FasterAsyncCallbackFn>>(),
            std::mem::size_of::<usize>(),
        );
    }
}
