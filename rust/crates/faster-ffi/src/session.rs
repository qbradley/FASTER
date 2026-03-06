//! Session management for the FFI layer.
//!
//! FASTER CRUD operations require a session. Sessions are **thread-affine**
//! (`!Send`) — a session handle **must not** be used from a different thread
//! than the one that created it. Violating this invariant is undefined behavior.
//!
//! The FFI layer stores sessions as `Box<UnsafeCell<FasterSession>>` in the
//! global handle table, wrapped in a `Send + Sync` newtype so they can live
//! in the type-erased `dyn Any + Send + Sync` storage. The `UnsafeCell`
//! interior mutability allows `&`-reference CRUD calls to obtain `&mut Session`.
//!
//! # Safety Contract (for C Callers)
//!
//! 1. Each session handle must only be used from the thread that created it.
//! 2. Sessions must be ended (`faster_session_end`) before the store is closed.
//! 3. No concurrent calls with the same session handle.

use std::cell::UnsafeCell;

use faster_core::FasterSession;

use crate::functions::ByteSliceFunctions;

/// Type alias for our concrete session type.
pub type FfiSession = FasterSession<ByteSliceFunctions>;

/// A `Send + Sync` wrapper around `UnsafeCell<FfiSession>`.
///
/// # Safety
///
/// This is safe **only** because the FFI contract requires that each session
/// handle is used exclusively from a single thread. The `UnsafeCell` is never
/// accessed concurrently — it exists solely to allow `&self`-based handle
/// table lookups to yield `&mut FfiSession`.
pub(crate) struct SessionCell(pub(crate) UnsafeCell<FfiSession>);

// SAFETY: The FFI contract requires single-threaded access per session handle.
// The handle table needs `Send + Sync` for storage in `Box<dyn Any + Send + Sync>`.
// Concurrent access to the same SessionCell is UB that the C caller must avoid.
unsafe impl Send for SessionCell {}
// SAFETY: See above — no concurrent access is permitted by the FFI contract.
unsafe impl Sync for SessionCell {}
