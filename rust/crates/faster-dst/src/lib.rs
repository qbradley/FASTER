//! # Deterministic Simulation Testing (DST) for FASTER
//!
//! FoundationDB-style deterministic simulation testing for the FASTER
//! key-value store. Every execution with the same seed follows the **exact
//! same path** — failures are perfectly reproducible.
//!
//! # Capabilities
//!
//! - **[`DeterministicScheduler`]** — cooperative, single-threaded, seed-controlled
//!   task scheduler that replaces OS thread scheduling with a PRNG-driven
//!   step-function model. Same seed → same task ordering → deterministic execution.
//! - **[`SimulatedDevice`]** — in-memory [`Device`](faster_core::Device) with
//!   configurable fault injection (write errors, partial writes, read errors).
//! - **[`SimulatedStorage`]** — persistent backing store that survives device
//!   drops, enabling crash-recovery testing without the filesystem.
//! - **[`CrashSchedule`]** / **[`CrashRecoveryRunner`]** — 18 crash-point
//!   injection sites across checkpoint, compaction, and recovery state machines.
//!   Uses `crash_point!()` macros in `faster-core` that compile to nothing
//!   without the `simulation` feature (zero-cost).
//! - **[`SeedCampaign`]** — parallel seed-sweep engine that runs thousands of
//!   (scenario, seed) pairs across worker threads, collecting failures with
//!   reproduction commands.
//! - **[`ScenarioTemplate`]** — declarative builder combining workload factory,
//!   fault config, crash schedule, and invariants into a reusable template.
//! - **[`Invariant`]** trait + combinators ([`And`], [`Or`], [`All`], [`Not`])
//!   for composable post-condition verification after recovery.
//! - **[`SimulationTrace`]** — append-only event log for debugging scheduler
//!   decisions and task execution order.
//! - **[`SimulationRuntime`]** — seeded PRNG wrapper for deterministic workload
//!   generation.
//! - **[`SimulationHarness`]** — convenient test harness that ties together a
//!   runtime, storage, and temp checkpoint directory.
//! - **[`SimulatedClock`]** — deterministic time source for tests that need
//!   controlled time progression.
//! - **[`Workload`]** / **[`CrudWorkload`]** — composable workload definitions
//!   with tracked expected state for invariant checking.
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
//! # Campaign Example
//!
//! ```rust,no_run
//! use faster_dst::campaign::SeedCampaign;
//! use faster_dst::scenario::ScenarioTemplate;
//! use faster_dst::workload::CrudWorkload;
//!
//! let template = ScenarioTemplate::builder("basic_recovery")
//!     .workload(|seed| CrudWorkload::new(seed, 50))
//!     .check_committed_recoverable()
//!     .build();
//!
//! let mut campaign = SeedCampaign::new();
//! campaign.add_scenario(template).seed_range(0..100);
//! let report = campaign.run();
//! assert!(report.all_passed());
//! ```
//!
//! # Architecture
//!
//! The simulation abstraction layer in `faster-core/src/sync.rs` provides a
//! three-tier `cfg` cascade (`loom` > `simulation` > `std`) that lets the DST
//! framework swap in deterministic replacements transparently. When the
//! `simulation` feature is disabled, the resulting binary is identical to a
//! standard build — zero runtime cost.
//!
//! See the [Technical Reference](../../.paw/work/deterministic-simulation-testing/Docs.md)
//! for the complete architecture description, API reference, and debugging guide.

pub mod campaign;
pub mod channel;
pub mod clock;
pub mod concurrency;
pub mod cost_model;
pub mod crash;
pub mod device;
pub mod fault;
pub mod harness;
pub mod invariant;
pub mod report;
pub mod runtime;
pub mod scenario;
pub mod scenarios;
pub mod scheduler;
pub mod sim_device_v2;
pub mod sim_store;
pub mod task;
pub mod trace;
pub mod workload;

pub use campaign::SeedCampaign;
pub use channel::SimChannel;
pub use clock::SimulatedClock;
pub use crash::{
    CrashRecoveryResult, CrashRecoveryRunner, CrashScenario, CrashSchedule, CrashScheduleRef,
    CrashTrigger,
};
pub use device::{SimulatedDevice, SimulatedStorage};
pub use sim_device_v2::{SimDeviceV2, SimIoConfig};
pub use fault::{CheckpointPhase, CompactionPhase, CrashPoint, FaultConfig, RecoveryPhase};
pub use harness::SimulationHarness;
pub use invariant::{
    All, AllCommittedRecoverable, And, ConsistentHashIndex, Invariant, MonotonicAddresses,
    NoPhantomReads, Not, Or, ValidPageChecksums,
};
pub use report::{CampaignReport, ScenarioFailure};
pub use runtime::SimulationRuntime;
pub use scenario::ScenarioTemplate;
pub use scheduler::DeterministicScheduler;
pub use sim_store::{CrudOp, CrudResult, SimulatedFasterKv, spawn_crud_worker};
pub use task::{BlockReason, SchedulerResult, SimTask, TaskAction, TaskContext, TaskId};
pub use trace::{IoOp, SimulationTrace, TraceEvent};
pub use workload::{CrudWorkload, Workload};
