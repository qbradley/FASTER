//! Operation status codes for FASTER operations.
//!
//! This module defines [`OperationStatus`] — the outcome of every FASTER
//! key-value operation (Read, Upsert, RMW, Delete). Status codes represent
//! *normal operational outcomes*, not errors. True errors (I/O failure,
//! corruption, invalid state) are represented by [`crate::error::FasterError`].
//!
//! The separation follows Rust idiom: callers match on status for control flow
//! and use `Result` / `?` for error propagation.
//!
//! # Design rationale
//!
//! The C++ FASTER uses a flat `Status` enum (`Ok`, `NotFound`, `Pending`,
//! `Aborted`) plus an internal `OperationStatus` with finer-grained codes.
//! The C# version adds property-style accessors (`Found`, `IsPending`,
//! `InPlaceUpdated`, `Created`, `CopyUpdated`). This Rust implementation
//! unifies both into a single enum with helper predicates.
//!
//! # Examples
//!
//! ```
//! use faster_core::status::OperationStatus;
//!
//! let status = OperationStatus::Ok;
//! assert!(status.is_success());
//! assert!(!status.is_pending());
//!
//! let pending = OperationStatus::Pending;
//! assert!(pending.is_pending());
//! assert!(!pending.is_success());
//! ```

use core::fmt;

/// The outcome of a FASTER key-value operation.
///
/// Every operation on a [`Session`] returns an `OperationStatus` indicating
/// what happened. These are *not* errors — they are control-flow signals that
/// the caller matches on to decide the next step.
///
/// # Variant categories
///
/// | Category | Variants | Meaning |
/// |----------|----------|---------|
/// | Success | `Ok`, `Created`, `InPlaceUpdated`, `CopyUpdated`, `Deleted` | Operation completed |
/// | Deferred | `Pending` | I/O issued; call `complete_pending()` later |
/// | Absent | `NotFound` | Key does not exist |
/// | Cancelled | `Aborted` | Operation was cancelled or could not proceed |
///
/// # Examples
///
/// ```
/// use faster_core::status::OperationStatus;
///
/// fn handle_read(status: OperationStatus) {
///     match status {
///         OperationStatus::Ok => println!("value read successfully"),
///         OperationStatus::Pending => println!("I/O issued, complete later"),
///         OperationStatus::NotFound => println!("key does not exist"),
///         _ => println!("unexpected status: {status}"),
///     }
/// }
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[must_use]
pub enum OperationStatus {
    /// Operation completed successfully.
    ///
    /// For Read operations this means the value was found and copied to the
    /// output buffer. For Upsert/RMW it indicates a generic success when
    /// a more specific status (e.g., `InPlaceUpdated`) is not applicable.
    Ok,

    /// The record resides on disk; asynchronous I/O has been issued.
    ///
    /// The caller **must not** access output buffers until the pending
    /// operation completes. Call [`Session::complete_pending`] to drain
    /// completed I/O operations and invoke their callbacks.
    Pending,

    /// The requested key was not found in the store.
    ///
    /// Returned by Read and Delete when the key is absent. This is a normal
    /// outcome, not an error.
    NotFound,

    /// A new record was created at the log tail.
    ///
    /// Returned by RMW when the key did not previously exist and
    /// `initial_updater` created a fresh record, or by Upsert when inserting
    /// a key for the first time.
    Created,

    /// An existing record was updated in place in the mutable log region.
    ///
    /// Returned by RMW and Upsert when the target record is in the mutable
    /// portion of the hybrid log and was modified without copying.
    InPlaceUpdated,

    /// The record was copied to the log tail and updated there.
    ///
    /// Returned by RMW when the target record is in the read-only region of
    /// the hybrid log. The old record remains (to be reclaimed by compaction)
    /// and a new copy with the update is appended at the tail.
    CopyUpdated,

    /// A tombstone was written for the deleted key.
    ///
    /// Returned by Delete when the key existed and a tombstone record was
    /// successfully appended.
    Deleted,

    /// The operation was aborted and had no effect.
    ///
    /// This can occur when a user callback explicitly cancels an operation,
    /// or when the system determines the operation cannot proceed (e.g.,
    /// store is shutting down).
    Aborted,
}

impl OperationStatus {
    /// Returns `true` if the operation completed successfully.
    ///
    /// Success statuses are: [`Ok`](Self::Ok), [`Created`](Self::Created),
    /// [`InPlaceUpdated`](Self::InPlaceUpdated),
    /// [`CopyUpdated`](Self::CopyUpdated), and [`Deleted`](Self::Deleted).
    ///
    /// # Examples
    ///
    /// ```
    /// use faster_core::status::OperationStatus;
    ///
    /// assert!(OperationStatus::Ok.is_success());
    /// assert!(OperationStatus::Created.is_success());
    /// assert!(OperationStatus::InPlaceUpdated.is_success());
    /// assert!(OperationStatus::CopyUpdated.is_success());
    /// assert!(OperationStatus::Deleted.is_success());
    /// assert!(!OperationStatus::Pending.is_success());
    /// assert!(!OperationStatus::NotFound.is_success());
    /// assert!(!OperationStatus::Aborted.is_success());
    /// ```
    #[inline]
    pub const fn is_success(self) -> bool {
        matches!(
            self,
            Self::Ok | Self::Created | Self::InPlaceUpdated | Self::CopyUpdated | Self::Deleted
        )
    }

    /// Returns `true` if the operation is pending asynchronous I/O completion.
    ///
    /// When this returns `true`, the caller must call `complete_pending()`
    /// before accessing any output values.
    ///
    /// # Examples
    ///
    /// ```
    /// use faster_core::status::OperationStatus;
    ///
    /// assert!(OperationStatus::Pending.is_pending());
    /// assert!(!OperationStatus::Ok.is_pending());
    /// ```
    #[inline]
    pub const fn is_pending(self) -> bool {
        matches!(self, Self::Pending)
    }

    /// Returns `true` if the key was not found in the store.
    ///
    /// # Examples
    ///
    /// ```
    /// use faster_core::status::OperationStatus;
    ///
    /// assert!(OperationStatus::NotFound.is_not_found());
    /// assert!(!OperationStatus::Ok.is_not_found());
    /// ```
    #[inline]
    pub const fn is_not_found(self) -> bool {
        matches!(self, Self::NotFound)
    }

    /// Returns `true` if the operation was aborted.
    ///
    /// # Examples
    ///
    /// ```
    /// use faster_core::status::OperationStatus;
    ///
    /// assert!(OperationStatus::Aborted.is_aborted());
    /// assert!(!OperationStatus::Ok.is_aborted());
    /// ```
    #[inline]
    pub const fn is_aborted(self) -> bool {
        matches!(self, Self::Aborted)
    }

    /// Returns `true` if the operation resulted in a record being updated
    /// or created (i.e., a write occurred).
    ///
    /// This covers [`Created`](Self::Created),
    /// [`InPlaceUpdated`](Self::InPlaceUpdated),
    /// [`CopyUpdated`](Self::CopyUpdated), and [`Deleted`](Self::Deleted).
    /// It does **not** include [`Ok`](Self::Ok) since that variant may
    /// represent a read-only operation.
    ///
    /// # Examples
    ///
    /// ```
    /// use faster_core::status::OperationStatus;
    ///
    /// assert!(OperationStatus::Created.is_modified());
    /// assert!(OperationStatus::Deleted.is_modified());
    /// assert!(!OperationStatus::Ok.is_modified());
    /// assert!(!OperationStatus::NotFound.is_modified());
    /// ```
    #[inline]
    pub const fn is_modified(self) -> bool {
        matches!(
            self,
            Self::Created | Self::InPlaceUpdated | Self::CopyUpdated | Self::Deleted
        )
    }
}

impl fmt::Display for OperationStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ok => write!(f, "Ok"),
            Self::Pending => write!(f, "Pending"),
            Self::NotFound => write!(f, "NotFound"),
            Self::Created => write!(f, "Created"),
            Self::InPlaceUpdated => write!(f, "InPlaceUpdated"),
            Self::CopyUpdated => write!(f, "CopyUpdated"),
            Self::Deleted => write!(f, "Deleted"),
            Self::Aborted => write!(f, "Aborted"),
        }
    }
}

/// The combined result of a FASTER operation: a status code plus an optional
/// output value.
///
/// Operations like Read produce an output value on success, while Upsert/Delete
/// may not. The generic parameter `T` allows callers to specify the output type.
///
/// # Examples
///
/// ```
/// use faster_core::status::{OperationResult, OperationStatus};
///
/// // A successful read with output
/// let result = OperationResult {
///     status: OperationStatus::Ok,
///     output: Some(42u64),
/// };
/// assert!(result.is_success());
/// assert_eq!(result.output, Some(42));
///
/// // A pending operation with no output yet
/// let pending: OperationResult<u64> = OperationResult {
///     status: OperationStatus::Pending,
///     output: None,
/// };
/// assert!(pending.is_pending());
/// assert!(pending.output.is_none());
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationResult<T> {
    /// The status code indicating the outcome of the operation.
    pub status: OperationStatus,
    /// The output value, if the operation produced one.
    ///
    /// Typically `Some(value)` for successful Read operations and `None` for
    /// writes, pending operations, or not-found results.
    pub output: Option<T>,
}

impl<T> OperationResult<T> {
    /// Creates a new `OperationResult` with the given status and output.
    ///
    /// # Examples
    ///
    /// ```
    /// use faster_core::status::{OperationResult, OperationStatus};
    ///
    /// let result = OperationResult::new(OperationStatus::Ok, Some("hello"));
    /// assert_eq!(result.status, OperationStatus::Ok);
    /// assert_eq!(result.output, Some("hello"));
    /// ```
    #[inline]
    pub const fn new(status: OperationStatus, output: Option<T>) -> Self {
        Self { status, output }
    }

    /// Returns `true` if the operation completed successfully.
    ///
    /// Delegates to [`OperationStatus::is_success`].
    #[inline]
    pub const fn is_success(&self) -> bool {
        self.status.is_success()
    }

    /// Returns `true` if the operation is pending asynchronous I/O.
    ///
    /// Delegates to [`OperationStatus::is_pending`].
    #[inline]
    pub const fn is_pending(&self) -> bool {
        self.status.is_pending()
    }

    /// Returns `true` if the key was not found.
    ///
    /// Delegates to [`OperationStatus::is_not_found`].
    #[inline]
    pub const fn is_not_found(&self) -> bool {
        self.status.is_not_found()
    }

    /// Consumes the result and returns the output value, if any.
    ///
    /// # Examples
    ///
    /// ```
    /// use faster_core::status::{OperationResult, OperationStatus};
    ///
    /// let result = OperationResult::new(OperationStatus::Ok, Some(42));
    /// assert_eq!(result.into_output(), Some(42));
    /// ```
    #[inline]
    pub fn into_output(self) -> Option<T> {
        self.output
    }

    /// Transforms the output value using the given closure, leaving the
    /// status unchanged.
    ///
    /// # Examples
    ///
    /// ```
    /// use faster_core::status::{OperationResult, OperationStatus};
    ///
    /// let result = OperationResult::new(OperationStatus::Ok, Some(21));
    /// let doubled = result.map(|v| v * 2);
    /// assert_eq!(doubled.output, Some(42));
    /// assert_eq!(doubled.status, OperationStatus::Ok);
    /// ```
    #[inline]
    pub fn map<U, F: FnOnce(T) -> U>(self, f: F) -> OperationResult<U> {
        OperationResult {
            status: self.status,
            output: self.output.map(f),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── OperationStatus trait impls ──────────────────────────────────

    #[test]
    fn status_is_copy() {
        let a = OperationStatus::Ok;
        let b = a; // Copy
        assert_eq!(a, b);
    }

    #[test]
    fn status_debug_and_display() {
        // Debug
        assert_eq!(format!("{:?}", OperationStatus::Ok), "Ok");
        assert_eq!(format!("{:?}", OperationStatus::Pending), "Pending");
        assert_eq!(format!("{:?}", OperationStatus::CopyUpdated), "CopyUpdated");

        // Display
        assert_eq!(format!("{}", OperationStatus::Ok), "Ok");
        assert_eq!(format!("{}", OperationStatus::Pending), "Pending");
        assert_eq!(format!("{}", OperationStatus::NotFound), "NotFound");
        assert_eq!(format!("{}", OperationStatus::Created), "Created");
        assert_eq!(
            format!("{}", OperationStatus::InPlaceUpdated),
            "InPlaceUpdated"
        );
        assert_eq!(format!("{}", OperationStatus::CopyUpdated), "CopyUpdated");
        assert_eq!(format!("{}", OperationStatus::Deleted), "Deleted");
        assert_eq!(format!("{}", OperationStatus::Aborted), "Aborted");
    }

    #[test]
    fn status_equality_and_hash() {
        use std::collections::HashSet;

        assert_eq!(OperationStatus::Ok, OperationStatus::Ok);
        assert_ne!(OperationStatus::Ok, OperationStatus::Pending);

        let mut set = HashSet::new();
        set.insert(OperationStatus::Ok);
        set.insert(OperationStatus::Pending);
        set.insert(OperationStatus::Ok); // duplicate
        assert_eq!(set.len(), 2);
    }

    #[test]
    fn status_clone() {
        let a = OperationStatus::CopyUpdated;
        #[allow(clippy::clone_on_copy)]
        let b = a.clone();
        assert_eq!(a, b);
    }

    // ── is_success ──────────────────────────────────────────────────

    #[test]
    fn is_success_true_for_success_variants() {
        assert!(OperationStatus::Ok.is_success());
        assert!(OperationStatus::Created.is_success());
        assert!(OperationStatus::InPlaceUpdated.is_success());
        assert!(OperationStatus::CopyUpdated.is_success());
        assert!(OperationStatus::Deleted.is_success());
    }

    #[test]
    fn is_success_false_for_non_success_variants() {
        assert!(!OperationStatus::Pending.is_success());
        assert!(!OperationStatus::NotFound.is_success());
        assert!(!OperationStatus::Aborted.is_success());
    }

    // ── is_pending ──────────────────────────────────────────────────

    #[test]
    fn is_pending_only_for_pending() {
        assert!(OperationStatus::Pending.is_pending());
        for status in all_statuses() {
            if status != OperationStatus::Pending {
                assert!(!status.is_pending(), "{status} should not be pending");
            }
        }
    }

    // ── is_not_found ────────────────────────────────────────────────

    #[test]
    fn is_not_found_only_for_not_found() {
        assert!(OperationStatus::NotFound.is_not_found());
        for status in all_statuses() {
            if status != OperationStatus::NotFound {
                assert!(!status.is_not_found(), "{status} should not be not_found");
            }
        }
    }

    // ── is_aborted ──────────────────────────────────────────────────

    #[test]
    fn is_aborted_only_for_aborted() {
        assert!(OperationStatus::Aborted.is_aborted());
        for status in all_statuses() {
            if status != OperationStatus::Aborted {
                assert!(!status.is_aborted(), "{status} should not be aborted");
            }
        }
    }

    // ── is_modified ─────────────────────────────────────────────────

    #[test]
    fn is_modified_for_write_statuses() {
        assert!(OperationStatus::Created.is_modified());
        assert!(OperationStatus::InPlaceUpdated.is_modified());
        assert!(OperationStatus::CopyUpdated.is_modified());
        assert!(OperationStatus::Deleted.is_modified());
    }

    #[test]
    fn is_modified_false_for_non_write_statuses() {
        assert!(!OperationStatus::Ok.is_modified());
        assert!(!OperationStatus::Pending.is_modified());
        assert!(!OperationStatus::NotFound.is_modified());
        assert!(!OperationStatus::Aborted.is_modified());
    }

    // ── Send + Sync ─────────────────────────────────────────────────

    #[test]
    fn status_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<OperationStatus>();
    }

    // ── OperationResult ─────────────────────────────────────────────

    #[test]
    fn operation_result_new() {
        let r = OperationResult::new(OperationStatus::Ok, Some(42u32));
        assert_eq!(r.status, OperationStatus::Ok);
        assert_eq!(r.output, Some(42));
    }

    #[test]
    fn operation_result_delegates_helpers() {
        let ok = OperationResult::new(OperationStatus::Ok, Some(1));
        assert!(ok.is_success());
        assert!(!ok.is_pending());
        assert!(!ok.is_not_found());

        let pending: OperationResult<i32> = OperationResult::new(OperationStatus::Pending, None);
        assert!(pending.is_pending());
        assert!(!pending.is_success());

        let nf: OperationResult<i32> = OperationResult::new(OperationStatus::NotFound, None);
        assert!(nf.is_not_found());
    }

    #[test]
    fn operation_result_into_output() {
        let r = OperationResult::new(OperationStatus::Ok, Some("hello"));
        assert_eq!(r.into_output(), Some("hello"));

        let empty: OperationResult<&str> = OperationResult::new(OperationStatus::NotFound, None);
        assert_eq!(empty.into_output(), None);
    }

    #[test]
    fn operation_result_map() {
        let r = OperationResult::new(OperationStatus::Created, Some(10));
        let mapped = r.map(|v| v * 3);
        assert_eq!(mapped.status, OperationStatus::Created);
        assert_eq!(mapped.output, Some(30));

        let empty: OperationResult<i32> = OperationResult::new(OperationStatus::Pending, None);
        let mapped_empty = empty.map(|v| v + 1);
        assert_eq!(mapped_empty.output, None);
        assert_eq!(mapped_empty.status, OperationStatus::Pending);
    }

    #[test]
    fn operation_result_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<OperationResult<u64>>();
        assert_send_sync::<OperationResult<String>>();
    }

    #[test]
    fn operation_result_debug_and_eq() {
        let a = OperationResult::new(OperationStatus::Ok, Some(1));
        let b = OperationResult::new(OperationStatus::Ok, Some(1));
        let c = OperationResult::new(OperationStatus::Ok, Some(2));
        assert_eq!(a, b);
        assert_ne!(a, c);

        let dbg = format!("{:?}", a);
        assert!(dbg.contains("Ok"));
        assert!(dbg.contains("Some(1)"));
    }

    // ── helpers ─────────────────────────────────────────────────────

    fn all_statuses() -> Vec<OperationStatus> {
        vec![
            OperationStatus::Ok,
            OperationStatus::Pending,
            OperationStatus::NotFound,
            OperationStatus::Created,
            OperationStatus::InPlaceUpdated,
            OperationStatus::CopyUpdated,
            OperationStatus::Deleted,
            OperationStatus::Aborted,
        ]
    }
}
