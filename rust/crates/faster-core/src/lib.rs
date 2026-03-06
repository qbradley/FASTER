//! # FASTER Core
//!
//! A ground-up Rust implementation of [Microsoft FASTER](https://github.com/microsoft/FASTER)
//! — a high-performance, concurrent, latch-free hash-table-based key-value
//! store designed for larger-than-memory workloads. This crate contains
//! **zero async runtime dependencies**; async adapters live in separate crates.
//!
//! ## Key Concepts
//!
//! | Concept | Description |
//! |---------|-------------|
//! | **Hash Index** | A latch-free concurrent hash table that maps keys to record addresses in the hybrid log. Supports online resize (grow). |
//! | **Hybrid Log** | A circular buffer of pages spanning in-memory and on-disk regions. Hot records stay in memory; cold records are flushed to the storage device. |
//! | **Epoch Protection** | An epoch-based safe memory reclamation scheme that coordinates threads without locks. Each thread "enters" an epoch before touching shared state and "exits" when done. |
//! | **Sessions** | Thread-affine (`!Send`) handles through which all CRUD operations are performed. Sessions track pending I/O and serial numbers for each thread. |
//!
//! ## Feature Summary
//!
//! - **CRUD operations** — [`FasterKv::read`], [`FasterKv::upsert`],
//!   [`FasterKv::rmw`], [`FasterKv::delete`] with pluggable semantics via
//!   the [`Functions`] trait.
//! - **Checkpoint / Recovery** — fold-over and snapshot checkpoints persist
//!   both the hash index and the hybrid log to durable storage. See the
//!   [`checkpoint`] and [`recovery`] modules.
//! - **Hash Table Grow** — online resize doubles the hash bucket count while
//!   concurrent readers and writers continue operating. See [`grow`].
//! - **Pending I/O** — when a record is on disk, the operation returns
//!   [`OperationStatus::Pending`](status::OperationStatus::Pending) and
//!   queues an asynchronous read; callers drain completions via the session.
//! - **Builder Pattern** — [`FasterKvBuilder`] provides fluent, validated
//!   construction of [`FasterKv`] instances.
//!
//! ## Quick Start
//!
//! ```rust
//! use faster_core::{FasterKv, SimpleFunctions, NullDevice};
//! use faster_core::status::OperationStatus;
//!
//! // 1. Create a store (in-memory only via NullDevice)
//! let store: FasterKv<SimpleFunctions<u64, u64>> = FasterKv::new(
//!     Default::default(),
//!     SimpleFunctions::default(),
//!     NullDevice::new(),
//! );
//!
//! // 2. Open a thread-local session
//! let mut session = store.new_session();
//!
//! // 3. Upsert a key-value pair (input = value for SimpleFunctions)
//! let status = store.upsert(&mut session, &42u64, &100u64, ());
//! assert!(status.is_success());
//!
//! // 4. Read it back
//! let mut output: Option<u64> = None;
//! let status = store.read(&mut session, &42u64, &0u64, &mut output, ());
//! assert_eq!(status, OperationStatus::Ok);
//! assert_eq!(output, Some(100));
//!
//! // 5. Read-modify-write (RMW) — adds input to existing value
//! let mut rmw_out: Option<u64> = None;
//! store.rmw(&mut session, &42u64, &10u64, &mut rmw_out, ());
//!
//! // 6. Clean up
//! store.dispose_session(session);
//! ```
//!
//! ## Architecture Overview
//!
//! ```text
//! ┌──────────────────────────────────────────────────────────┐
//! │                      FasterKv<F>                         │
//! │  ┌──────────┐  ┌───────────────┐  ┌──────────────────┐  │
//! │  │ HashIndex │  │ HybridLogAlloc│  │   EpochTable     │  │
//! │  │ (latch-  │  │ (page table,  │  │ (safe memory     │  │
//! │  │  free)   │  │  regions)     │  │  reclamation)    │  │
//! │  └──────────┘  └───────────────┘  └──────────────────┘  │
//! │  ┌──────────┐  ┌───────────────┐  ┌──────────────────┐  │
//! │  │ Flusher  │  │   Evictor     │  │   GrowManager    │  │
//! │  └──────────┘  └───────────────┘  └──────────────────┘  │
//! │         │              │                                 │
//! │         ▼              ▼                                 │
//! │  ┌─────────────────────────┐                            │
//! │  │    Device (I/O backend) │                            │
//! │  └─────────────────────────┘                            │
//! └──────────────────────────────────────────────────────────┘
//!        ▲                ▲                 ▲
//!        │                │                 │
//!   FasterSession   FasterSession     FasterSession
//!   (Thread 1)      (Thread 2)        (Thread N)
//! ```
//!
//! ## Modules
//!
//! - [`store`] — The main user-facing API: [`FasterKv`], [`FasterSession`],
//!   [`FasterKvBuilder`], [`Functions`] trait, and [`SimpleFunctions`].
//! - [`hash`] — Latch-free hash index, bucket layout, overflow chains, and the
//!   [`Hashable`](hash::Hashable) trait.
//! - [`hybrid_log`] — Hybrid log allocator, page table, flush/eviction, and
//!   record read/write accessors.
//! - [`epoch`] — Epoch-based safe memory reclamation ([`EpochTable`](epoch::EpochTable)).
//! - [`checkpoint`] — Checkpoint coordination, metadata, and on-disk writers for
//!   index and log snapshots.
//! - [`recovery`] — Recovery planning and execution: index, log, and session restore.
//! - [`grow`] — Hash table online-resize state machine and bucket splitter.
//! - [`record`] — Record header format, inline variable-length key-value storage.
//! - [`address`] — Logical addressing: [`LogicalAddress`](address::LogicalAddress),
//!   page/offset split.
//! - [`status`] — Operation status codes ([`OperationStatus`](status::OperationStatus)).
//! - [`error`] — Error types ([`FasterError`](error::FasterError)) for exceptional conditions.
//! - [`device`] — Storage [`Device`] trait and built-in implementations
//!   ([`NullDevice`], [`InMemoryDevice`]).
//! - [`sync_file_device`] — File-backed storage device with background I/O thread pool.

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
pub mod checkpoint;
pub mod device;
pub mod epoch;
pub mod error;
pub mod grow;
pub mod hash;
pub use self::hash::bucket as hash_bucket;
pub use self::hash::index as hash_index;
pub use self::hash::overflow;
pub use self::hash::table as hash_table;
pub mod hybrid_log;
pub mod record;
pub mod recovery;
pub mod status;
pub mod store;
pub mod sync_file_device;

pub use device::{
    Device, InMemoryDevice, IoCompletionCallback, IoRequestResult, IoStatus, NullDevice,
    TypedIoContext,
};
pub use metrics::Metrics;
pub use store::{
    FasterKv, FasterKvBuilder, FasterKvConfig, FasterSession, Functions, SessionStats,
    SimpleFunctions,
};
pub use sync_file_device::SyncFileDevice;
