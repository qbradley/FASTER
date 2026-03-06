//! # FASTER io_uring
//!
//! Safe Rust wrapper around Linux io_uring for high-performance async I/O.
//!
//! This crate provides [`Ring`], a safe interface to io_uring that encapsulates
//! all unsafe syscall interactions, and [`BufferPool`], a lock-free pool of
//! sector-aligned buffers that can be registered with the kernel for
//! fixed-buffer I/O.
//!
//! # Platform
//!
//! This crate is **Linux-only**. All public types are gated behind
//! `#[cfg(target_os = "linux")]` and will not compile on other platforms.
//!
//! # Buffer Lifetime Safety
//!
//! Between submission and completion the kernel holds references to
//! caller-supplied buffers. For unregistered buffers the caller must ensure
//! they remain valid until the completion is reaped. For registered buffers
//! (via [`BufferPool`] + [`Ring::register_buffers`]), the pool manages
//! lifetime and alignment; individual [`RegisteredBuffer`] guards enforce
//! exclusive access and return-on-drop semantics.

#![deny(unsafe_op_in_unsafe_fn)]
#![warn(missing_docs)]
#![forbid(clippy::undocumented_unsafe_blocks)]

#[cfg(target_os = "linux")]
mod buffer;
#[cfg(target_os = "linux")]
mod ring;

#[cfg(target_os = "linux")]
pub use buffer::{BufferPool, RegisteredBuffer, SECTOR_ALIGNMENT};
#[cfg(target_os = "linux")]
pub use ring::{Ring, UringConfig};
