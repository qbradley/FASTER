//! Atomic and synchronization primitives.
//!
//! Under `cfg(loom)`, these re-export from the `loom` crate for
//! deterministic concurrency testing. Otherwise, they re-export
//! from `std::sync::atomic`.
//!
//! These re-exports are not yet used by production code (planned for a
//! follow-up refactor). Suppress warnings until then.
#![allow(unused_imports)]

#[cfg(loom)]
pub(crate) use loom::sync::atomic::{AtomicPtr, AtomicU64, AtomicUsize, Ordering, fence};
#[cfg(loom)]
pub(crate) use loom::sync::{Arc, Mutex};
#[cfg(loom)]
pub(crate) use loom::thread;

#[cfg(not(loom))]
pub(crate) use std::sync::atomic::{AtomicPtr, AtomicU64, AtomicUsize, Ordering, fence};
#[cfg(not(loom))]
pub(crate) use std::sync::{Arc, Mutex};
#[cfg(not(loom))]
pub(crate) use std::thread;
