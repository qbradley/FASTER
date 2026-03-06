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
//! - `operations` — Internal CRUD operation implementations (read, upsert,
//!   RMW, delete) tying the hash index, hybrid log, and session together.

mod functions;
#[allow(dead_code)] // Consumed by FasterKv (item 4).
pub(crate) mod operations;
mod session;

pub use functions::{CounterFunctions, Functions, RmwInPlaceResult, SimpleFunctions};
pub use session::{FasterSession, PendingOpType, PendingOperation, SessionGuard, SessionPool};
