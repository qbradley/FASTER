//! Synchronization, threading, time, and channel abstractions.
//!
//! This module provides cfg-gated re-exports that enable transparent swapping
//! of standard library types with alternatives for testing frameworks:
//!
//! - **`loom`**: Deterministic concurrency testing (model-checks all interleavings)
//! - **`simulation`**: Cooperative single-threaded DST framework (seed-controlled scheduling)
//! - **std**: Production builds
//!
//! **cfg precedence**: `loom` > `simulation` > `std`
//! (loom and simulation are mutually exclusive in practice, but loom takes priority if both enabled)
//!
//! ## Atomic Ordering Audit (ARM)
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

// ============================================================
// Tier 1: loom — deterministic concurrency model checking
// ============================================================
#[cfg(loom)]
pub(crate) use loom::sync::atomic::{
    AtomicBool, AtomicPtr, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering, fence,
};
#[cfg(loom)]
pub(crate) use loom::sync::{Arc, Mutex};
// loom 0.7 does not provide RwLock — fall back to std.
// loom's thread module doesn't provide `sleep` — we shim it here.
#[cfg(loom)]
pub(crate) mod thread {
    pub use loom::thread::*;

    /// Under loom, `sleep` just yields — loom doesn't model real time.
    pub fn sleep(_duration: std::time::Duration) {
        loom::thread::yield_now();
    }
}
#[cfg(loom)]
pub(crate) use std::sync::{RwLock, RwLockReadGuard};

// ============================================================
// Tier 2: simulation — cooperative DST framework
// (Phase 1: re-exports std; Phase 2 swaps to SimClock/SimChannel/scheduler)
// ============================================================
#[cfg(all(not(loom), feature = "simulation"))]
pub(crate) use std::sync::atomic::{
    AtomicBool, AtomicPtr, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering, fence,
};
#[cfg(all(not(loom), feature = "simulation"))]
pub(crate) use std::sync::{Arc, Mutex, RwLock, RwLockReadGuard};

// Simulation tier thread module: wraps yield_now / sleep to record
// scheduling events in sim_hooks before delegating to the real OS.
#[cfg(all(not(loom), feature = "simulation"))]
pub(crate) mod thread {
    pub use std::thread::*;

    /// Yield the current thread, recording the event for the DST scheduler.
    pub fn yield_now() {
        crate::sim_hooks::on_yield();
        std::thread::yield_now();
    }

    /// Sleep for `duration`, recording the event for the DST scheduler.
    pub fn sleep(duration: std::time::Duration) {
        crate::sim_hooks::on_sleep(duration);
        std::thread::sleep(duration);
    }
}

// ============================================================
// Tier 3: std — production builds
// ============================================================
#[cfg(all(not(loom), not(feature = "simulation")))]
pub(crate) use std::sync::atomic::{
    AtomicBool, AtomicPtr, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering, fence,
};
#[cfg(all(not(loom), not(feature = "simulation")))]
pub(crate) use std::sync::{Arc, Mutex, RwLock, RwLockReadGuard};
#[cfg(all(not(loom), not(feature = "simulation")))]
pub(crate) use std::thread;

// ============================================================
// Time abstractions (not affected by loom)
// Simulation branch uses SimInstant for deterministic virtual time
// ============================================================
#[cfg(not(feature = "simulation"))]
pub(crate) use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[cfg(feature = "simulation")]
pub(crate) use std::time::{Duration, SystemTime, UNIX_EPOCH};
#[cfg(feature = "simulation")]
pub(crate) use crate::sim_time::SimInstant as Instant;

// ============================================================
// Channel abstractions (not affected by loom for our purposes)
// Phase 2 will swap simulation branch to SimChannel
// ============================================================
pub(crate) mod channel {
    #[cfg(not(feature = "simulation"))]
    pub(crate) use std::sync::mpsc::{Receiver, Sender, channel};

    #[cfg(feature = "simulation")]
    pub(crate) use std::sync::mpsc::{Receiver, Sender, channel};
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_types_available() {
        let _bool = AtomicBool::new(false);
        let _u32 = AtomicU32::new(0);
        let _u64 = AtomicU64::new(0);
        let _usize = AtomicUsize::new(0);
        let _u8 = AtomicU8::new(0);
        let _ptr = AtomicPtr::new(std::ptr::null_mut::<u8>());
        let _ = Ordering::Relaxed;
        fence(Ordering::SeqCst);
    }

    #[test]
    fn sync_primitives_available() {
        let arc = Arc::new(42);
        let _mutex = Mutex::new(0);
        let _rwlock = RwLock::new(0);
        assert_eq!(*arc, 42);
    }

    #[test]
    fn time_types_available() {
        let now = Instant::now();
        let _dur = Duration::from_millis(1);
        let _sys = SystemTime::now();
        let _epoch = UNIX_EPOCH;
        assert!(now.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn channel_available() {
        let (tx, rx) = channel::channel::<i32>();
        tx.send(42).unwrap();
        assert_eq!(rx.recv().unwrap(), 42);
    }
}
