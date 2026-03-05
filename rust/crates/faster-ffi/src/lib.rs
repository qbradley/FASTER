//! # FASTER FFI
//!
//! C FFI bindings for the FASTER key-value store.
//!
//! Exposes an opaque-handle-based C ABI (~15 functions) for embedding FASTER
//! in other languages. Header generation via `cbindgen`.

#![deny(unsafe_op_in_unsafe_fn)]
#![warn(missing_docs)]
#![forbid(clippy::undocumented_unsafe_blocks)]
