//! Error types for exceptional conditions in FASTER.
//!
//! This module defines [`FasterError`] — the error enum for truly exceptional
//! failures (I/O errors, corruption, invalid state). Normal operational
//! outcomes (found, not found, pending) are represented by
//! [`crate::status::OperationStatus`] codes — those are *not* errors.
//!
//! The separation follows Rust idiom: `Result<T, FasterError>` carries
//! exceptional failures that propagate with `?`, while
//! [`OperationStatus`](crate::status::OperationStatus) carries control-flow
//! signals that callers match on.
//!
//! # Convenience alias
//!
//! The [`Result<T>`] type alias wraps `std::result::Result<T, FasterError>`
//! for ergonomic use throughout the crate.
//!
//! # Examples
//!
//! ```
//! use faster_core::error::{FasterError, Result};
//!
//! fn open_device(path: &str) -> Result<()> {
//!     if path.is_empty() {
//!         return Err(FasterError::InvalidOperation(
//!             "device path must not be empty".into(),
//!         ));
//!     }
//!     Ok(())
//! }
//!
//! let err = open_device("").unwrap_err();
//! assert!(matches!(err, FasterError::InvalidOperation(_)));
//! println!("{err}"); // "Invalid operation: device path must not be empty"
//! ```

use core::fmt;

/// The error type for exceptional FASTER failures.
///
/// `FasterError` represents conditions that prevent an operation from
/// completing at all — as opposed to
/// [`OperationStatus`](crate::status::OperationStatus) which represents
/// normal outcomes (including "not found" and "pending").
///
/// All variants carry enough context to produce actionable diagnostic
/// messages via their [`Display`] implementation.
///
/// # Examples
///
/// ```
/// use faster_core::error::FasterError;
///
/// // I/O errors convert automatically via From
/// let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file gone");
/// let faster_err: FasterError = io_err.into();
/// assert!(matches!(faster_err, FasterError::Io(_)));
///
/// // String-based variants for structured diagnostics
/// let err = FasterError::CheckpointError("token abc123 not found on disk".into());
/// assert!(err.to_string().contains("abc123"));
/// ```
#[derive(Debug)]
pub enum FasterError {
    /// An I/O error occurred on the underlying storage device.
    ///
    /// Wraps a [`std::io::Error`]. Automatically constructed via the
    /// `From<std::io::Error>` implementation, enabling `?` propagation
    /// from any I/O call.
    ///
    /// # Examples
    ///
    /// ```
    /// use faster_core::error::FasterError;
    ///
    /// let io_err = std::io::Error::new(std::io::ErrorKind::BrokenPipe, "disk offline");
    /// let err = FasterError::from(io_err);
    /// assert!(err.to_string().contains("disk offline"));
    /// ```
    Io(std::io::Error),

    /// A checkpoint operation failed.
    ///
    /// The string describes *why* the checkpoint failed (e.g., "unable to
    /// flush index pages", "checkpoint token already exists").
    ///
    /// # Examples
    ///
    /// ```
    /// use faster_core::error::FasterError;
    ///
    /// let err = FasterError::CheckpointError("flush timed out after 30s".into());
    /// assert!(err.to_string().contains("Checkpoint"));
    /// ```
    CheckpointError(String),

    /// A recovery operation failed.
    ///
    /// The string describes the recovery failure (e.g., "log metadata
    /// corrupted", "checkpoint token not found").
    ///
    /// # Examples
    ///
    /// ```
    /// use faster_core::error::FasterError;
    ///
    /// let err = FasterError::RecoveryError("log metadata corrupted at page 42".into());
    /// assert!(err.to_string().contains("Recovery"));
    /// ```
    RecoveryError(String),

    /// The caller attempted an invalid operation.
    ///
    /// Indicates API misuse: calling an operation on a closed store,
    /// using an expired session, passing invalid arguments, etc.
    ///
    /// # Examples
    ///
    /// ```
    /// use faster_core::error::FasterError;
    ///
    /// let err = FasterError::InvalidOperation("store is closed".into());
    /// assert!(err.to_string().contains("Invalid operation"));
    /// ```
    InvalidOperation(String),

    /// An internal invariant was violated.
    ///
    /// This indicates a bug in the FASTER implementation itself — not
    /// user error. If you encounter this in production, please file a bug.
    ///
    /// # Examples
    ///
    /// ```
    /// use faster_core::error::FasterError;
    ///
    /// let err = FasterError::InternalError("hash bucket chain cycle detected".into());
    /// assert!(err.to_string().contains("Internal error"));
    /// ```
    InternalError(String),

    /// A session lifecycle error occurred.
    ///
    /// Covers session creation failures, using a session after it has been
    /// disposed, or exceeding the maximum number of concurrent sessions.
    ///
    /// # Examples
    ///
    /// ```
    /// use faster_core::error::FasterError;
    ///
    /// let err = FasterError::SessionError("max 128 concurrent sessions exceeded".into());
    /// assert!(err.to_string().contains("Session error"));
    /// ```
    SessionError(String),
}

impl fmt::Display for FasterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(err) => write!(f, "I/O error: {err}"),
            Self::CheckpointError(msg) => write!(f, "Checkpoint failed: {msg}"),
            Self::RecoveryError(msg) => write!(f, "Recovery failed: {msg}"),
            Self::InvalidOperation(msg) => write!(f, "Invalid operation: {msg}"),
            Self::InternalError(msg) => write!(f, "Internal error: {msg}"),
            Self::SessionError(msg) => write!(f, "Session error: {msg}"),
        }
    }
}

impl std::error::Error for FasterError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            _ => None,
        }
    }
}

impl From<std::io::Error> for FasterError {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err)
    }
}

/// A convenience alias for `std::result::Result<T, FasterError>`.
///
/// Used throughout the FASTER crate for operations that may fail with
/// an exceptional error.
///
/// # Examples
///
/// ```
/// use faster_core::error::{FasterError, Result};
///
/// fn do_work() -> Result<u64> {
///     Ok(42)
/// }
///
/// fn propagate_io() -> Result<()> {
///     let _file = std::fs::File::open("/nonexistent")?;
///     Ok(())
/// }
///
/// assert!(do_work().is_ok());
/// assert!(propagate_io().is_err());
/// ```
pub type Result<T> = std::result::Result<T, FasterError>;

#[cfg(test)]
mod tests {
    use super::*;

    // ── Display ─────────────────────────────────────────────────────

    #[test]
    fn display_io_error() {
        let io = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "access denied");
        let err = FasterError::Io(io);
        let msg = err.to_string();
        assert!(msg.starts_with("I/O error:"), "got: {msg}");
        assert!(msg.contains("access denied"), "got: {msg}");
    }

    #[test]
    fn display_checkpoint_error() {
        let err = FasterError::CheckpointError("token not found".into());
        assert_eq!(err.to_string(), "Checkpoint failed: token not found");
    }

    #[test]
    fn display_recovery_error() {
        let err = FasterError::RecoveryError("corrupted metadata".into());
        assert_eq!(err.to_string(), "Recovery failed: corrupted metadata");
    }

    #[test]
    fn display_invalid_operation() {
        let err = FasterError::InvalidOperation("store is closed".into());
        assert_eq!(err.to_string(), "Invalid operation: store is closed");
    }

    #[test]
    fn display_internal_error() {
        let err = FasterError::InternalError("invariant violated".into());
        assert_eq!(err.to_string(), "Internal error: invariant violated");
    }

    #[test]
    fn display_session_error() {
        let err = FasterError::SessionError("session disposed".into());
        assert_eq!(err.to_string(), "Session error: session disposed");
    }

    // ── std::error::Error ───────────────────────────────────────────

    #[test]
    fn error_source_is_io_for_io_variant() {
        let io = std::io::Error::new(std::io::ErrorKind::Other, "disk failure");
        let err = FasterError::Io(io);
        let source = std::error::Error::source(&err);
        assert!(source.is_some());
        assert!(source.unwrap().to_string().contains("disk failure"));
    }

    #[test]
    fn error_source_is_none_for_non_io_variants() {
        let variants: Vec<FasterError> = vec![
            FasterError::CheckpointError("x".into()),
            FasterError::RecoveryError("x".into()),
            FasterError::InvalidOperation("x".into()),
            FasterError::InternalError("x".into()),
            FasterError::SessionError("x".into()),
        ];
        for err in &variants {
            assert!(
                std::error::Error::source(err).is_none(),
                "expected no source for {err}"
            );
        }
    }

    // ── From<std::io::Error> ────────────────────────────────────────

    #[test]
    fn from_io_error() {
        let io = std::io::Error::new(std::io::ErrorKind::NotFound, "no such file");
        let err: FasterError = io.into();
        assert!(matches!(err, FasterError::Io(_)));
        assert!(err.to_string().contains("no such file"));
    }

    // ── ? operator propagation ──────────────────────────────────────

    #[test]
    fn question_mark_propagation() {
        fn inner() -> Result<()> {
            let _ = std::fs::File::open("/this/path/does/not/exist")?;
            Ok(())
        }

        let result = inner();
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, FasterError::Io(_)));
    }

    // ── Result type alias ───────────────────────────────────────────

    #[test]
    fn result_alias_ok() {
        fn work() -> Result<u64> {
            Ok(42)
        }
        assert_eq!(work().unwrap(), 42);
    }

    #[test]
    fn result_alias_err() {
        fn fail() -> Result<()> {
            Err(FasterError::InvalidOperation("nope".into()))
        }
        assert!(fail().is_err());
    }

    // ── Send + Sync ─────────────────────────────────────────────────

    #[test]
    fn error_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<FasterError>();
    }

    // ── Debug ───────────────────────────────────────────────────────

    #[test]
    fn error_debug_format() {
        let err = FasterError::InternalError("oops".into());
        let dbg = format!("{err:?}");
        assert!(dbg.contains("InternalError"));
        assert!(dbg.contains("oops"));
    }
}
