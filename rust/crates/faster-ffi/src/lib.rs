//! # FASTER FFI
//!
//! C FFI bindings for the FASTER key-value store.
//!
//! Exposes an opaque-handle-based C ABI (~15 functions) for embedding FASTER
//! in other languages. Header generation via `cbindgen`.
//!
//! ## Crate Structure
//!
//! - [`handle`] — Opaque handle table (type-safe, thread-safe `u64` → Rust object mapping)
//! - [`error`] — C-compatible `#[repr(C)]` status codes
//!
//! Future waves will add CRUD functions, session management, and more.

#![deny(unsafe_op_in_unsafe_fn)]
#![warn(missing_docs)]
#![forbid(clippy::undocumented_unsafe_blocks)]

pub mod error;
pub mod handle;
