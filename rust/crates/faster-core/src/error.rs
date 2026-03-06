//! Error types for exceptional conditions in FASTER.
//!
//! This module defines [`FasterError`] — the unified error enum for truly
//! exceptional failures (I/O errors, corruption, invalid state). Normal
//! operational outcomes (found, not found, pending) are represented by
//! [`crate::status::OperationStatus`] codes — those are *not* errors.
//!
//! `FasterError` wraps the domain-specific error types
//! ([`CheckpointError`](crate::checkpoint::CheckpointError),
//! [`RecoveryError`](crate::recovery::RecoveryError),
//! [`GrowError`](crate::grow::GrowError)) via [`From`] conversions so that
//! callers at the crate boundary can use a single `Result<T, FasterError>`.
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
//! # Error conversions
//!
//! | Source type | Target variant |
//! |---|---|
//! | [`std::io::Error`] | [`FasterError::Io`] |
//! | [`CheckpointError`](crate::checkpoint::CheckpointError) | [`FasterError::CheckpointError`] |
//! | [`RecoveryError`](crate::recovery::RecoveryError) | [`FasterError::RecoveryError`] |
//! | [`GrowError`](crate::grow::GrowError) | [`FasterError::Grow`] |
//!
//! # Error handling audit
//!
//! The following `unwrap()` / `expect()` / `panic!()` / `unimplemented!()` calls
//! exist in non-test code. Each is classified as **acceptable** (infallible
//! invariant, lock poisoning, etc.) or **should-fix** (future work).
//!
//! ## Should-fix
//!
//! | Location | Kind | Note |
//! |---|---|---|
//! | `store/functions.rs:131` | `unimplemented!()` | Default `upsert_in_place_raw` — runtime panic if trait const not set |
//! | `store/functions.rs:148` | `unimplemented!()` | Default `rmw_in_place_raw` — same issue |
//!
//! ## Acceptable — lock poisoning
//!
//! | Location | Kind | Note |
//! |---|---|---|
//! | `device.rs:312` | `expect` | `InMemoryDevice` RwLock poisoned — unrecoverable |
//! | `device.rs:346` | `expect` | same |
//! | `device.rs:369` | `expect` | same |
//! | `device.rs:377` | `expect` | same |
//! | `device.rs:385` | `expect` | same |
//! | `device.rs:391` | `expect` | same |
//! | `buffer_pool.rs:67` | `expect` | Buffer-pool Mutex poisoned — unrecoverable |
//! | `buffer_pool.rs:73` | `expect` | same |
//! | `buffer_pool.rs:76` | `expect` | same |
//!
//! ## Acceptable — layout / memory safety invariants
//!
//! | Location | Kind | Note |
//! |---|---|---|
//! | `allocator.rs:206` | `expect` | Page-size overflow — programmer error |
//! | `allocator.rs:212` | `expect` | Invalid page layout — programmer error |
//! | `allocator.rs:233` | `expect` | Invalid page layout — programmer error |
//! | `buffer_pool.rs:63` | `expect` | Invalid layout for size/alignment |
//!
//! ## Acceptable — serialization / deserialization invariants
//!
//! | Location | Kind | Note |
//! |---|---|---|
//! | `record/traits.rs:127` | `expect` | Buffer too short for numeric Key deserialize |
//! | `record/traits.rs:154` | `expect` | Buffer too short for numeric Value deserialize |
//! | `record/traits.rs:190` | `expect` | Buffer too short for `Vec<u8>` length prefix |
//! | `record/traits.rs:211` | `expect` | Buffer too short for String length prefix |
//! | `record/traits.rs:230` | `expect` | Invalid UTF-8 in deserialized String |
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

use crate::checkpoint::CheckpointError;
use crate::grow::GrowError;
use crate::recovery::RecoveryError;

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

    /// A hash-table grow (resize) operation failed.
    ///
    /// Wraps a [`GrowError`](crate::grow::GrowError). Automatically
    /// constructed via the `From<GrowError>` implementation.
    ///
    /// # Examples
    ///
    /// ```
    /// use faster_core::error::FasterError;
    /// use faster_core::grow::GrowError;
    ///
    /// let err: FasterError = GrowError::AlreadyInProgress.into();
    /// assert!(matches!(err, FasterError::Grow(_)));
    /// ```
    Grow(GrowError),

    /// A configuration value is invalid.
    ///
    /// Returned when builder-level or runtime configuration validation
    /// detects an invalid parameter (e.g., non-power-of-two page size,
    /// zero-length hash table).
    ///
    /// # Examples
    ///
    /// ```
    /// use faster_core::error::FasterError;
    ///
    /// let err = FasterError::Config("page_size must be a power of two".into());
    /// assert!(err.to_string().contains("Configuration error"));
    /// ```
    Config(String),

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
            Self::Grow(err) => write!(f, "Grow failed: {err}"),
            Self::Config(msg) => write!(f, "Configuration error: {msg}"),
            Self::InternalError(msg) => write!(f, "Internal error: {msg}"),
            Self::SessionError(msg) => write!(f, "Session error: {msg}"),
        }
    }
}

impl std::error::Error for FasterError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            Self::Grow(err) => Some(err),
            _ => None,
        }
    }
}

impl From<std::io::Error> for FasterError {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err)
    }
}

impl From<CheckpointError> for FasterError {
    fn from(err: CheckpointError) -> Self {
        Self::CheckpointError(err.to_string())
    }
}

impl From<RecoveryError> for FasterError {
    fn from(err: RecoveryError) -> Self {
        Self::RecoveryError(err.to_string())
    }
}

impl From<GrowError> for FasterError {
    fn from(err: GrowError) -> Self {
        Self::Grow(err)
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
    fn display_grow_error() {
        let err = FasterError::Grow(GrowError::AlreadyInProgress);
        let msg = err.to_string();
        assert!(msg.starts_with("Grow failed:"), "got: {msg}");
        assert!(msg.contains("already in progress"), "got: {msg}");
    }

    #[test]
    fn display_config_error() {
        let err = FasterError::Config("page size must be power of two".into());
        assert_eq!(
            err.to_string(),
            "Configuration error: page size must be power of two"
        );
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

    // ── std::error::Error — source chain ────────────────────────────

    #[test]
    fn error_source_is_io_for_io_variant() {
        let io = std::io::Error::other("disk failure");
        let err = FasterError::Io(io);
        let source = std::error::Error::source(&err);
        assert!(source.is_some());
        assert!(source.unwrap().to_string().contains("disk failure"));
    }

    #[test]
    fn error_source_is_grow_for_grow_variant() {
        let err = FasterError::Grow(GrowError::AlreadyInProgress);
        let source = std::error::Error::source(&err);
        assert!(source.is_some(), "Grow variant should expose source");
        assert!(
            source.unwrap().to_string().contains("already in progress"),
            "source should be the inner GrowError"
        );
    }

    #[test]
    fn error_source_is_none_for_string_variants() {
        let variants: Vec<FasterError> = vec![
            FasterError::CheckpointError("x".into()),
            FasterError::RecoveryError("x".into()),
            FasterError::InvalidOperation("x".into()),
            FasterError::Config("x".into()),
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

    // ── From<CheckpointError> ───────────────────────────────────────

    #[test]
    fn from_checkpoint_error_already_in_progress() {
        let ckpt = CheckpointError::AlreadyInProgress;
        let err: FasterError = ckpt.into();
        assert!(matches!(err, FasterError::CheckpointError(_)));
        assert!(
            err.to_string().contains("already in progress"),
            "got: {}",
            err
        );
    }

    #[test]
    fn from_checkpoint_error_io() {
        let io = std::io::Error::new(std::io::ErrorKind::BrokenPipe, "pipe broken");
        let ckpt = CheckpointError::IoError(io);
        let err: FasterError = ckpt.into();
        assert!(matches!(err, FasterError::CheckpointError(_)));
        assert!(err.to_string().contains("pipe broken"), "got: {}", err);
    }

    // ── From<RecoveryError> ─────────────────────────────────────────

    #[test]
    fn from_recovery_error_no_checkpoints() {
        let rec = RecoveryError::NoCheckpointsFound;
        let err: FasterError = rec.into();
        assert!(matches!(err, FasterError::RecoveryError(_)));
        assert!(
            err.to_string().contains("no completed checkpoints"),
            "got: {}",
            err
        );
    }

    #[test]
    fn from_recovery_error_corrupt_metadata() {
        let rec = RecoveryError::CorruptMetadata("bad CRC".into());
        let err: FasterError = rec.into();
        assert!(matches!(err, FasterError::RecoveryError(_)));
        assert!(err.to_string().contains("bad CRC"), "got: {}", err);
    }

    // ── From<GrowError> ─────────────────────────────────────────────

    #[test]
    fn from_grow_error_already_in_progress() {
        let grow = GrowError::AlreadyInProgress;
        let err: FasterError = grow.into();
        assert!(matches!(err, FasterError::Grow(_)));
        assert!(
            err.to_string().contains("already in progress"),
            "got: {}",
            err
        );
    }

    #[test]
    fn from_grow_error_invalid_size() {
        let grow = GrowError::InvalidSize("must be power of two".into());
        let err: FasterError = grow.into();
        assert!(matches!(err, FasterError::Grow(GrowError::InvalidSize(_))));
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

    #[test]
    fn question_mark_propagation_from_grow() {
        fn try_grow() -> Result<()> {
            let grow_err: std::result::Result<(), GrowError> =
                Err(GrowError::AlreadyInProgress);
            grow_err?;
            Ok(())
        }

        let result = try_grow();
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), FasterError::Grow(_)));
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

    #[test]
    fn error_debug_grow_variant() {
        let err = FasterError::Grow(GrowError::AlreadyInProgress);
        let dbg = format!("{err:?}");
        assert!(dbg.contains("Grow"));
        assert!(dbg.contains("AlreadyInProgress"));
    }
}
