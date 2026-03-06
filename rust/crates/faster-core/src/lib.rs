//! # FASTER Core
//!
//! The core key-value engine for FASTER — a high-performance, concurrent,
//! durable hash map. This crate contains **zero async runtime dependencies**.
//!
//! All I/O is abstracted behind the completion-callback-based [`Device`] trait
//! defined in the [`device`] module. Async adapters (Tokio, compio, monoio) live
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
//! - [`device`] — Storage device trait for hybrid log backends
//! - [`store`] — Store-level traits: [`Functions`](store::Functions) callbacks

#![deny(unsafe_op_in_unsafe_fn)]
#![warn(missing_docs)]
#![forbid(clippy::undocumented_unsafe_blocks)]

#[macro_use]
mod instrument;

pub mod metrics;
pub(crate) mod sync;

pub mod address;
pub mod allocator;
pub mod buffer_pool;
pub mod device;
pub mod epoch;
pub mod error;
pub mod hash;
pub use self::hash::bucket as hash_bucket;
pub use self::hash::index as hash_index;
pub use self::hash::overflow;
pub use self::hash::table as hash_table;
pub mod record;
pub mod status;
pub mod store;

pub use device::{
    Device, InMemoryDevice, IoCompletionCallback, IoRequestResult, IoStatus, NullDevice,
};
pub use metrics::Metrics;
