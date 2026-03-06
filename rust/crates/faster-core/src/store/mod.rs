//! The main user-facing API for the FASTER key-value store.
//!
//! This module provides everything needed to create, configure, and operate a
//! FASTER store:
//!
//! - **[`FasterKv`]** — the central store type that owns the hash index, hybrid
//!   log, epoch table, and device. It is `Send + Sync` and designed to be shared
//!   across threads via `Arc`.
//! - **[`FasterKvBuilder`]** — a fluent builder for constructing `FasterKv`
//!   instances with validation (see [`FasterKv::builder`]).
//! - **[`FasterKvConfig`]** — direct configuration struct for advanced users.
//! - **[`FasterSession`]** — a thread-affine (`!Send`) session handle through
//!   which all CRUD operations are performed. Each thread must have its own
//!   session obtained via [`FasterKv::new_session`].
//! - **[`Functions`]** — the trait that defines user-supplied callbacks for
//!   read, upsert, RMW, and delete semantics.
//! - **[`SimpleFunctions`]** / [`CounterFunctions`] — ready-made `Functions`
//!   implementations for common patterns.
//!
//! # Usage Pattern
//!
//! ```text
//! FasterKv::builder()      // configure
//!     .build(fns, device)  // create store
//!     ↓
//! store.new_session()      // per-thread session
//!     ↓
//! store.upsert(session, key, value, ctx)  // CRUD ops
//! store.read(session, key, input, output, ctx)
//! store.rmw(session, key, input, output, ctx)
//! store.delete(session, key, ctx)
//!     ↓
//! store.dispose_session(session)  // clean up
//! ```
//!
//! # Submodules
//!
//! - `functions` — [`Functions`] trait for user-defined operation callbacks,
//!   plus convenience implementations ([`SimpleFunctions`], [`CounterFunctions`]).
//! - `session` — Thread-local session model: [`FasterSession`], [`SessionGuard`],
//!   [`SessionPool`], and pending operation types.
//! - [`kv`] — [`FasterKv`] store and [`FasterKvConfig`] configuration.
//! - [`builder`] — [`FasterKvBuilder`] fluent API with validation.
//! - `operations` — Internal CRUD operation implementations (read, upsert,
//!   RMW, delete) tying the hash index, hybrid log, and session together.

pub mod builder;
mod functions;
pub mod kv;
pub(crate) mod operations;
pub mod pending_io;
mod session;

pub use builder::FasterKvBuilder;
pub use functions::{CounterFunctions, Functions, RmwInPlaceResult, SimpleFunctions};
pub use kv::{FasterKv, FasterKvConfig};
pub use pending_io::{CompletedIo, PendingIoConfig, PendingIoContext, PendingIoError, PendingIoManager};
pub use session::{
    CompletePendingResult, FasterSession, PendingOpType, PendingOperation, SessionGuard,
    SessionPool, SessionStats,
};
