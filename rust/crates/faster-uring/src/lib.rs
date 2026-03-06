//! # FASTER io_uring
//!
//! Safe Rust wrapper around Linux io_uring for high-performance async I/O.
//!
//! This crate provides [`Ring`], a safe interface to io_uring that encapsulates
//! all unsafe syscall interactions. It is designed for FASTER's page-aligned,
//! direct-I/O workloads where completion-based I/O is essential.
//!
//! # Platform
//!
//! This crate is **Linux-only**. All public types are gated behind
//! `#[cfg(target_os = "linux")]` and will not compile on other platforms.
//!
//! # Buffer Lifetime Safety
//!
//! **Important:** Between submission and completion the kernel holds references
//! to the caller-supplied buffers. The caller **must** ensure that buffers
//! remain valid (not freed, not moved) until the corresponding completion is
//! reaped via [`Ring::reap_completions`]. Violating this invariant is undefined
//! behavior at the kernel level. A future iteration (U2) will add registered
//! buffer support to make this safer.

#![deny(unsafe_op_in_unsafe_fn)]
#![warn(missing_docs)]
#![forbid(clippy::undocumented_unsafe_blocks)]

#[cfg(target_os = "linux")]
mod ring;

#[cfg(target_os = "linux")]
pub use ring::{Ring, UringConfig};
