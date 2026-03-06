//! Atomic and synchronization primitives.
//!
//! Under `cfg(loom)`, these re-export from the `loom` crate for
//! deterministic concurrency testing. Otherwise, they re-export
//! from `std::sync::atomic`.
//!
//! These re-exports are not yet used by production code (planned for a
//! follow-up refactor). Suppress warnings until then.
//!
//! # ARM Memory Ordering Audit (H2)
//!
//! All `Ordering::Relaxed` uses in this crate fall into one category:
//! **advisory counters** (metrics, `live_entry_count`, `high_water`,
//! `allocator.count`). These are never read to make synchronization
//! decisions — they provide approximate statistics for diagnostics and
//! display. On ARM (aarch64), `Relaxed` atomic loads/stores guarantee
//! individual atomicity, which is sufficient for this purpose.
//!
//! All synchronization-critical paths use `Acquire`, `Release`, `AcqRel`,
//! or `SeqCst` as appropriate:
//! - **DrainList** CAS push/drain: `AcqRel`/`Acquire`
//! - **Page state transitions**: `AcqRel` via `compare_exchange`
//! - **Epoch table**: `Acquire`/`Release` on entry/exit boundaries
//! - **Hash index**: `AcqRel` CAS, `Acquire` loads
//! - **Allocator tail**: `AcqRel` CAS, `Acquire` loads
//!
//! No `#[cfg(target_arch = "aarch64")]` fences are needed — the existing
//! ordering annotations are sufficient for ARM weak-memory correctness.
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
