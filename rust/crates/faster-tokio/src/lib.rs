//! # FASTER Tokio Adapter
//!
//! Bridges FASTER's callback-based core API to Tokio's `Future`/`async fn`
//! ecosystem. Provides [`AsyncFasterKv`] for managed store lifecycle with
//! background maintenance, [`AsyncSession`] for async CRUD, and a
//! Tokio-backed [`Device`] implementation.
//!
//! The [`bridge`] module is the foundation: it converts FASTER's completion
//! callbacks into standard Rust [`Future`]s using only `std` types — no
//! async runtime dependency.

#![deny(unsafe_op_in_unsafe_fn)]
#![warn(missing_docs)]
#![forbid(clippy::undocumented_unsafe_blocks)]

pub mod bridge;
pub mod device;
pub mod kv;
pub mod session;

pub use device::TokioFileDevice;
pub use kv::AsyncFasterKv;
pub use session::AsyncSession;
