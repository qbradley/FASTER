//! # FASTER Tokio Adapter
//!
//! Bridges FASTER's callback-based core API to Tokio's `Future`/`async fn`
//! ecosystem. Provides [`AsyncSession`] wrapping the core `Session` and
//! a Tokio-backed [`Device`] implementation.

#![deny(unsafe_op_in_unsafe_fn)]
#![warn(missing_docs)]
#![forbid(clippy::undocumented_unsafe_blocks)]
