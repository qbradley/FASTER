//! C-compatible status codes for the FFI layer.
//!
//! Every FFI function returns a [`FasterStatus`] value. Success codes occupy
//! the low range (`0..=99`); error codes start at `100`.
//!
//! These values become a C enum in the `cbindgen`-generated header.

/// C-compatible status codes returned by all FFI functions.
///
/// # ABI Stability
///
/// This enum is `#[repr(C)]` and its discriminant values are part of the
/// public ABI. New variants may be added, but existing values must never
/// change.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FasterStatus {
    /// Operation completed successfully.
    Ok = 0,
    /// Key not found in the store.
    NotFound = 1,
    /// Operation is pending (asynchronous completion required).
    Pending = 2,
    /// A new record was created (upsert semantics).
    Created = 3,
    /// Record was updated in place.
    InPlaceUpdated = 4,
    /// Record was updated via copy-on-write.
    CopyUpdated = 5,

    // ── Error codes (100+) ─────────────────────────────────────────────
    /// The provided handle is invalid or has already been freed.
    InvalidHandle = 100,
    /// One or more arguments are invalid (null pointer, bad length, etc.).
    InvalidArgument = 101,
    /// The caller-provided buffer is too small for the result.
    BufferTooSmall = 102,
    /// An internal error occurred (panic recovery, lock poison, etc.).
    InternalError = 103,
}

impl FasterStatus {
    /// Returns `true` if this status represents a successful outcome
    /// (discriminant < 100).
    #[inline]
    pub fn is_success(self) -> bool {
        (self as u32) < 100
    }

    /// Returns `true` if this status represents an error (discriminant >= 100).
    #[inline]
    pub fn is_error(self) -> bool {
        (self as u32) >= 100
    }
}

impl std::fmt::Display for FasterStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ok => write!(f, "Ok"),
            Self::NotFound => write!(f, "NotFound"),
            Self::Pending => write!(f, "Pending"),
            Self::Created => write!(f, "Created"),
            Self::InPlaceUpdated => write!(f, "InPlaceUpdated"),
            Self::CopyUpdated => write!(f, "CopyUpdated"),
            Self::InvalidHandle => write!(f, "InvalidHandle"),
            Self::InvalidArgument => write!(f, "InvalidArgument"),
            Self::BufferTooSmall => write!(f, "BufferTooSmall"),
            Self::InternalError => write!(f, "InternalError"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repr_c_discriminants() {
        assert_eq!(FasterStatus::Ok as u32, 0);
        assert_eq!(FasterStatus::NotFound as u32, 1);
        assert_eq!(FasterStatus::Pending as u32, 2);
        assert_eq!(FasterStatus::Created as u32, 3);
        assert_eq!(FasterStatus::InPlaceUpdated as u32, 4);
        assert_eq!(FasterStatus::CopyUpdated as u32, 5);
        assert_eq!(FasterStatus::InvalidHandle as u32, 100);
        assert_eq!(FasterStatus::InvalidArgument as u32, 101);
        assert_eq!(FasterStatus::BufferTooSmall as u32, 102);
        assert_eq!(FasterStatus::InternalError as u32, 103);
    }

    #[test]
    fn is_success_and_is_error() {
        assert!(FasterStatus::Ok.is_success());
        assert!(FasterStatus::NotFound.is_success());
        assert!(FasterStatus::Pending.is_success());
        assert!(FasterStatus::Created.is_success());
        assert!(FasterStatus::InPlaceUpdated.is_success());
        assert!(FasterStatus::CopyUpdated.is_success());

        assert!(!FasterStatus::Ok.is_error());

        assert!(FasterStatus::InvalidHandle.is_error());
        assert!(FasterStatus::InvalidArgument.is_error());
        assert!(FasterStatus::BufferTooSmall.is_error());
        assert!(FasterStatus::InternalError.is_error());

        assert!(!FasterStatus::InvalidHandle.is_success());
    }

    #[test]
    fn display_impl() {
        assert_eq!(format!("{}", FasterStatus::Ok), "Ok");
        assert_eq!(format!("{}", FasterStatus::InvalidHandle), "InvalidHandle");
    }

    #[test]
    fn size_is_c_int() {
        // repr(C) enum with values 0..=103 should be 4 bytes (C int).
        assert_eq!(std::mem::size_of::<FasterStatus>(), 4);
    }

    #[test]
    fn clone_and_copy() {
        let s = FasterStatus::Pending;
        let s2 = s;
        #[allow(clippy::clone_on_copy)]
        let s3 = s.clone();
        assert_eq!(s, s2);
        assert_eq!(s, s3);
    }

    #[test]
    fn debug_impl() {
        let dbg = format!("{:?}", FasterStatus::InternalError);
        assert_eq!(dbg, "InternalError");
    }
}
