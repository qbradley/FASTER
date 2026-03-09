//! # Deterministic Simulation Testing (DST) for FASTER
//!
//! This crate provides infrastructure for testing FASTER under deterministic,
//! reproducible conditions. Key capabilities:
//!
//! - **[`SimulatedDevice`]** — an in-memory [`Device`](faster_core::Device) with
//!   configurable fault injection (write errors, partial writes, read errors).
//! - **[`SimulatedStorage`]** — persistent backing store that survives device
//!   drops, enabling crash-recovery testing without the filesystem.
//! - **[`SimulationRuntime`]** — seeded PRNG wrapper for deterministic workload
//!   generation.
//! - **[`SimulationHarness`]** — convenient test harness that ties together a
//!   runtime, storage, and temp checkpoint directory.
//! - **[`SimulatedClock`]** — deterministic time source for tests that need
//!   controlled time progression.
//! - **[`Workload`]** / **[`Invariant`]** traits — composable abstractions for
//!   defining test workloads and post-condition checks.
//!
//! # Quick Example
//!
//! ```rust
//! use faster_core::checkpoint::CheckpointType;
//! use faster_dst::harness::SimulationHarness;
//!
//! let harness = SimulationHarness::new(42);
//!
//! // Phase 1: write records and checkpoint (uses file-backed device)
//! let token = {
//!     let store = harness.create_file_store();
//!     let mut s = store.new_session();
//!     for i in 0..50u64 {
//!         let _ = store.upsert(&mut s, &i, &(i * 10), ());
//!     }
//!     let tok = store
//!         .checkpoint(harness.checkpoint_dir(), CheckpointType::FoldOver)
//!         .unwrap();
//!     store.dispose_session(s);
//!     tok
//! }; // store dropped — simulates crash
//!
//! // Phase 2: recover and verify
//! let mut store = harness.create_file_store();
//! store.recover(harness.checkpoint_dir(), Some(token)).unwrap();
//! let mut s = store.new_session();
//! for i in 0..50u64 {
//!     let val: Option<u64> = store.read_simple(&mut s, &i);
//!     assert_eq!(val, Some(i * 10));
//! }
//! store.dispose_session(s);
//! ```
//!
//! # Future Work (requires `faster-core` changes)
//!
//! The following capabilities would improve DST coverage but require hooks
//! inside `faster-core`:
//!
//! - **Deterministic thread scheduling** — inject a custom scheduler into the
//!   epoch system so that thread interleavings are controlled by the seed.
//!   Today, multi-threaded tests are non-deterministic; `loom` covers some of
//!   this but at a different abstraction level.
//! - **Crash points inside the checkpoint state machine** — allow the
//!   [`CheckpointOrchestrator`](faster_core::checkpoint::CheckpointOrchestrator)
//!   to accept an injectable callback between phases (Prepare → InProgress →
//!   WaitFlush → …) so DST can simulate crashes mid-checkpoint.
//! - **Recovery crash injection** — similar hooks inside
//!   [`RecoveryManager`](faster_core::recovery::RecoveryManager) to crash
//!   mid-recovery and verify re-recovery succeeds.
//! - **Page-level CRC validation** — add checksums to hybrid log pages so the
//!   recovery path can detect torn pages written by [`SimulatedDevice`]'s
//!   partial-write fault injection.

pub mod channel;
pub mod clock;
pub mod device;
pub mod fault;
pub mod harness;
pub mod invariant;
pub mod runtime;
pub mod scheduler;
pub mod sim_store;
pub mod task;
pub mod trace;
pub mod workload;

pub use channel::SimChannel;
pub use clock::SimulatedClock;
pub use device::{SimulatedDevice, SimulatedStorage};
pub use fault::{CrashPoint, FaultConfig};
pub use harness::SimulationHarness;
pub use invariant::{AllCommittedRecoverable, Invariant};
pub use runtime::SimulationRuntime;
pub use scheduler::DeterministicScheduler;
pub use sim_store::{CrudOp, CrudResult, SimulatedFasterKv, spawn_crud_worker};
pub use task::{BlockReason, SchedulerResult, SimTask, TaskAction, TaskContext, TaskId};
pub use trace::{IoOp, SimulationTrace, TraceEvent};
pub use workload::{CrudWorkload, Workload};
