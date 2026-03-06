//! Store-level abstractions for FASTER.
//!
//! This module contains traits and types that define how the FASTER store
//! interacts with user-defined logic for reading, writing, and modifying
//! records.
//!
//! # Modules
//!
//! - [`functions`] — [`Functions`] trait for user-defined operation callbacks,
//!   plus convenience implementations ([`SimpleFunctions`], [`CounterFunctions`]).
//! - [`session`] — Thread-local session model: [`FasterSession`], [`SessionGuard`],
//!   [`SessionPool`], and pending operation types.
//! - [`kv`] — [`FasterKv`] store and [`FasterKvConfig`] configuration.
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
pub use pending_io::{CompletedIo, PendingIoContext, PendingIoError, PendingIoManager};
pub use session::{
    CompletePendingResult, FasterSession, PendingOpType, PendingOperation, SessionGuard,
    SessionPool,
};
