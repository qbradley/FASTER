//! # FASTER Device
//!
//! Device trait and built-in I/O implementations for FASTER.
//!
//! The [`Device`] trait is completion-callback-based and runtime-agnostic.
//! Concrete implementations (file I/O, io_uring, null device) are provided
//! here; async adapters wrap these in runtime-specific `Future` types.

#![deny(unsafe_op_in_unsafe_fn)]
#![warn(missing_docs)]
#![forbid(clippy::undocumented_unsafe_blocks)]
