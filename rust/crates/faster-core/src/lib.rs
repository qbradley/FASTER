//! # FASTER Core
//!
//! The core key-value engine for FASTER — a high-performance, concurrent,
//! durable hash map. This crate contains **zero async runtime dependencies**.
//!
//! All I/O is abstracted behind the completion-callback-based `Device` trait
//! defined in [`faster-device`]. Async adapters (Tokio, compio, monoio) live
//! in separate crates and provide `Future`/`async fn` wrappers.
//!
//! ## Modules
//!
//! - [`epoch`] — Epoch-based safe memory reclamation
//! - [`allocator`] — Arena allocator, aligned buffers, overflow bucket pool
//! - [`hash`] — Hash index, bucket layout, overflow chains
//! - [`record`] — Record format, inline variable-length key-value storage
//! - [`address`] — Logical addressing: `LogicalAddress`, page/offset split
//! - [`status`] — Operation status codes (bitflag-based)
//! - [`error`] — Error types for exceptional conditions

#![deny(unsafe_op_in_unsafe_fn)]
#![warn(missing_docs)]
#![forbid(clippy::undocumented_unsafe_blocks)]

pub mod address;
pub mod allocator;
pub mod epoch;
pub mod error;
pub mod hash;
pub mod record;
pub mod status;
