//! Session management for the FFI layer.
//!
//! FASTER CRUD operations require a session. Sessions are **thread-affine**
//! (`!Send`) — a session handle **must not** be used from a different thread
//! than the one that created it.
//!
//! The FFI layer stores sessions as `Box<SessionCell>` in the global handle
//! table. `SessionCell` records the creating thread's ID and performs a
//! runtime check on every access, returning [`FasterStatus::ThreadMismatch`]
//! if called from the wrong thread.
//!
//! # Safety Contract (for C Callers)
//!
//! 1. Each session handle must only be used from the thread that created it.
//! 2. Sessions must be ended (`faster_session_end`) before the store is closed.
//! 3. No concurrent calls with the same session handle.

use std::cell::UnsafeCell;
use std::thread::ThreadId;

use faster_core::FasterSession;

use crate::functions::ByteSliceFunctions;

/// Type alias for our concrete session type.
pub type FfiSession = FasterSession<ByteSliceFunctions>;

/// A `Send + Sync` wrapper around `UnsafeCell<FfiSession>` with runtime
/// thread-affinity enforcement.
///
/// The creating thread's ID is recorded at construction time. Every access
/// through [`check_thread`](SessionCell::check_thread) verifies the caller
/// is on the same thread, converting a hard-to-diagnose UB scenario into a
/// deterministic `ThreadMismatch` error.
pub(crate) struct SessionCell {
    pub(crate) inner: UnsafeCell<FfiSession>,
    owner_thread: ThreadId,
}

impl SessionCell {
    /// Create a new `SessionCell`, recording the current thread as the owner.
    pub(crate) fn new(session: FfiSession) -> Self {
        Self {
            inner: UnsafeCell::new(session),
            owner_thread: std::thread::current().id(),
        }
    }

    /// Returns `true` if the current thread is the one that created this cell.
    #[inline]
    pub(crate) fn check_thread(&self) -> bool {
        std::thread::current().id() == self.owner_thread
    }
}

// SAFETY: The FFI contract requires single-threaded access per session handle.
// The handle table needs `Send + Sync` for storage in `Box<dyn Any + Send + Sync>`.
// Runtime thread-affinity checking in `check_thread()` enforces this at the FFI
// boundary, converting what would be UB into a deterministic error.
unsafe impl Send for SessionCell {}
// SAFETY: See above — no concurrent access is permitted by the FFI contract.
unsafe impl Sync for SessionCell {}
