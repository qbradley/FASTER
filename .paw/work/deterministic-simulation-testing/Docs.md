# Deterministic Simulation Testing — Technical Reference

## Overview

Deterministic Simulation Testing (DST) is a testing methodology — pioneered by
[FoundationDB](https://apple.github.io/foundationdb/testing.html) — that
replaces real concurrency, I/O, and time with controlled simulations driven by
a single seed. Every execution with the same seed follows the **exact same
path**, making failures perfectly reproducible.

FASTER needs DST because its correctness depends on subtle interactions between
checkpointing, compaction, recovery, and the epoch-protected hybrid log. Bugs
in these paths often surface only under specific crash timings or I/O orderings
that are extremely difficult to trigger with conventional tests. DST lets us
sweep thousands of seeds in seconds and reproduce any failure deterministically.

The framework lives in the `faster-dst` crate and instruments `faster-core` via
a `simulation` feature flag that is **zero-cost when disabled** — production
binaries are byte-identical to builds without the feature.

---

## Architecture

### Simulation Abstraction Layer

**File:** `faster-core/src/sync.rs`

The abstraction layer uses a three-tier `cfg` cascade:

```
#[cfg(loom)]                                  → loom types
#[cfg(all(not(loom), feature = "simulation"))] → simulation types
#[cfg(all(not(loom), not(feature = "simulation")))] → std types
```

All production code imports from `crate::sync` instead of `std::sync` directly.
This single indirection point lets the DST framework swap in deterministic
replacements for atomics, mutexes, threads, time, and channels.

**Precedence:** `loom` > `simulation` > `std`. In practice loom and simulation
are mutually exclusive, but loom wins if both are enabled.

**Zero-cost guarantee:** When the `simulation` feature is disabled, every import
resolves to the standard library type. The compiler sees the same types, the
same layout, and the same code — the resulting binary is identical.

### Deterministic Scheduler

**File:** `faster-dst/src/scheduler.rs`, `faster-dst/src/task.rs`

The `DeterministicScheduler` replaces OS thread scheduling with a cooperative,
single-threaded, seed-controlled task runner.

**Key design decisions:**

1. **Step-function tasks.** Each `SimTask` is a `FnMut(&mut TaskContext) ->
   TaskAction` called repeatedly on the **same OS thread**. There are no async
   runtimes or real threads involved.

2. **PRNG-based selection.** The scheduler uses `SmallRng` seeded from the
   campaign seed. At each step it picks a random ready task. Same seed → same
   task ordering → deterministic execution.

3. **TaskAction protocol:**
   - `Complete` — task is finished, remove from the run queue.
   - `Yield` — voluntarily give up the CPU, remain ready.
   - `Park(BlockReason)` — block until an external event (channel message, I/O
     completion, or time advancement) unparks the task.

4. **Time advancement.** `SimulatedClock` is manually advanced by the
   scheduler. There is no wall-clock dependency — all time queries return
   deterministic values.

5. **Deadlock detection.** If every task is parked and no external event can
   unpark any of them, the scheduler returns `SchedulerResult::Deadlock` instead
   of hanging.

### Crash-Point Injection

**Files:** `faster-core/src/sim_hooks.rs`, `faster-dst/src/crash.rs`,
`faster-dst/src/fault.rs`

The crash injection system lets scenarios simulate process crashes at precise
points inside `faster-core`'s state machines.

**How it works:**

1. **`crash_point!("label")` macro** — placed at 18 strategic locations inside
   `faster-core`. Without the `simulation` feature it compiles to nothing
   (zero-cost). With the feature enabled, it checks a thread-local crash oracle.

2. **Thread-local crash oracle** — `CrashSchedule` is installed as a
   thread-local `Rc<RefCell<CrashSchedule>>` via `install_schedule()`. The
   `crash_point!` macro calls `should_crash(label)` which consults the schedule
   to decide whether to fire.

3. **`SimulatedCrash` sentinel** — when a crash point fires, it calls
   `panic_any(SimulatedCrash { point: label })`. The campaign engine wraps
   scenario execution in `catch_unwind` to capture the panic without aborting
   the process.

4. **`CrashTrigger` controls:**
   - `OnVisit(n)` — crash on the Nth visit (1-based).
   - `Always` — crash every time.
   - `Never` — disabled (for tracing/documentation).

**18 instrumented crash-point sites:**

| Category | Phases |
|----------|--------|
| **Checkpoint** (6) | PrepareToInProgress, IndexWritten, InProgressToWaitFlush, WaitFlushToWaitCompletion, MetadataPersisted, Completed |
| **Compaction** (6) | ScanComplete, CopyComplete, PointerSwingComplete, EpochDrained, BeginAddressAdvanced, TruncationComplete |
| **Recovery** (6) | DiscoveryComplete, PlanSelected, ValidationComplete, IndexCrcVerified, IndexBucketsLoaded, LogValidated |

### Page CRC-32C Checksums

**File:** `faster-core/src/hybrid_log/page.rs`

Every page written to disk includes an 8-byte `PageTrailer` in its last bytes:

```
┌──────────────────────────────────────────────────┬──────────┐
│                 page data                         │ trailer  │
│                                                  │ 8 bytes  │
└──────────────────────────────────────────────────┴──────────┘
                                                    ┌────┬────┐
                                                    │ vb │crc │
                                                    │ u32│u32 │
                                                    └────┴────┘
```

- `valid_bytes` (u32, LE) — number of record bytes the CRC covers.
- `crc32` (u32, LE) — CRC-32C of `page[0..valid_bytes]`.

**Flush path:** CRC is computed before every page write. On recovery, the CRC is
recomputed and compared. Mismatches indicate torn writes.

**Recovery validation** is controlled by `ChecksumValidationPolicy`:
- `Reject` (default) — return an error and halt.
- `Warn` — log and continue with best-effort recovery.
- `Repair` — fall back to last good checkpoint (future work).

**Format version:** `FORMAT_VERSION_CURRENT = 3`. Version 2 checkpoints (without
CRC trailers) are supported for backward compatibility.

### Campaign Engine

**Files:** `faster-dst/src/campaign.rs`, `faster-dst/src/scenario.rs`,
`faster-dst/src/report.rs`

The campaign engine sweeps thousands of (scenario, seed) pairs in parallel.

**Components:**

1. **`ScenarioTemplate`** — a declarative specification:
   - Workload factory: `Fn(seed) -> CrudWorkload`
   - Fault configuration: `FaultConfig`
   - Crash schedule factory: `Option<Fn(seed) -> CrashSchedule>`
   - Invariant factories: `Vec<Fn(&CrudWorkload) -> Box<dyn Invariant>>`

2. **`SeedCampaign`** — the execution engine:
   - Accepts multiple `ScenarioTemplate` instances.
   - Spreads (template × seed) pairs across worker threads via
     `std::thread::scope`.
   - Each pair runs in isolation with its own temp directory.
   - Failures are collected, not propagated — the campaign always runs to
     completion.

3. **`CampaignReport`** — structured results:
   - `total`, `passed`, `failed` counts.
   - `elapsed` wall-clock duration.
   - `failures: Vec<ScenarioFailure>` with seed, scenario name, and error.
   - `display_human()` — terminal-friendly summary.
   - `display_json()` — machine-readable JSON.
   - `reproduction_command()` — cargo command to re-run a single failing seed.

---

## API Reference

### DeterministicScheduler

```rust
use faster_dst::DeterministicScheduler;

let mut sched = DeterministicScheduler::new(42); // seed
sched.spawn("worker-1", |ctx| {
    // do work...
    TaskAction::Complete
});
let result = sched.run_until_complete();
assert_eq!(result, SchedulerResult::AllComplete);
```

### SimTask / TaskAction / TaskContext

| Type | Purpose |
|------|---------|
| `SimTask` | A cooperative task — a step-function closure run repeatedly |
| `TaskId` | Unique identifier assigned by the scheduler |
| `TaskAction::Complete` | Task is finished |
| `TaskAction::Yield` | Give up the CPU, stay ready |
| `TaskAction::Park(BlockReason)` | Block until unparked |
| `TaskContext` | Per-step context with task ID and scheduler handle |
| `BlockReason` | Why a task parked: `Channel`, `IoCompletion`, `Time` |
| `SchedulerResult` | Outcome: `AllComplete` or `Deadlock` |

### ScenarioTemplate / ScenarioTemplateBuilder

```rust
use faster_dst::scenario::ScenarioTemplate;
use faster_dst::workload::CrudWorkload;
use faster_dst::fault::FaultConfig;
use faster_dst::crash::{CrashSchedule, CrashTrigger};
use faster_dst::fault::CrashPoint;

let template = ScenarioTemplate::builder("my_scenario")
    .workload(|seed| CrudWorkload::new(seed, 100))
    .fault_config(FaultConfig::none().with_partial_writes(0.05))
    .crash_schedule(|seed| {
        let mut s = CrashSchedule::new();
        let phase = (seed % 6) as usize;
        s.add(CrashPoint::ALL_CHECKPOINT[phase], CrashTrigger::Always);
        s
    })
    .check_committed_recoverable()  // convenience: adds AllCommittedRecoverable
    .build();
```

### SeedCampaign / CampaignReport

```rust
use faster_dst::campaign::SeedCampaign;

let mut campaign = SeedCampaign::new();
campaign
    .add_scenario(template)
    .seed_range(0..1000)
    .thread_count(8);

let report = campaign.run();

if report.all_passed() {
    println!("{}", report.display_human());
} else {
    eprintln!("{}", report.display_human());
    // JSON for CI integration:
    eprintln!("{}", report.display_json());
}
```

### CrashSchedule / CrashTrigger / CrashRecoveryRunner

```rust
use faster_dst::crash::{CrashSchedule, CrashTrigger, CrashRecoveryRunner, CrashScenario};
use faster_dst::fault::CrashPoint;
use faster_dst::workload::CrudWorkload;

let mut schedule = CrashSchedule::new();
schedule.add(
    CrashPoint::Checkpoint(faster_dst::CheckpointPhase::MetadataPersisted),
    CrashTrigger::OnVisit(1),
);

let scenario = CrashScenario {
    workload: CrudWorkload::new(42, 50),
    crash_schedule: schedule,
    invariants: vec![Box::new(faster_dst::AllCommittedRecoverable::from_pairs(&[]))],
};
```

### FaultConfig

```rust
use faster_dst::FaultConfig;

let config = FaultConfig::none()
    .with_write_errors(0.01)     // 1% write failures
    .with_read_errors(0.005)     // 0.5% read failures
    .with_partial_writes(0.02)   // 2% torn writes
    .with_write_limit(5000);     // fail all writes after 5000
```

### Invariant Trait + Combinators

```rust
use faster_dst::invariant::*;
use faster_dst::workload::{CrudWorkload, Workload};

// Standard invariants
let workload = CrudWorkload::new(42, 50);
let inv1 = AllCommittedRecoverable::new(workload.expected_state());
let inv2 = NoPhantomReads::new(vec![9999, 9998]);
let inv3 = MonotonicAddresses::new(50);
let inv4 = ConsistentHashIndex::from_range(0..50);
let inv5 = ValidPageChecksums::new();

// Combinators
let combined = And::new(inv1, inv2);    // both must pass
// Or::new(a, b)                        // at least one passes
// Not::new(inv)                        // inverts result
// All::new(vec![...])                  // all in vec pass
```

| Invariant | Checks |
|-----------|--------|
| `AllCommittedRecoverable` | Every committed key→value pair is present after recovery |
| `NoPhantomReads` | Specified keys do NOT exist (uncommitted writes lost) |
| `MonotonicAddresses` | All keys in `[0, N)` are readable without panics |
| `ConsistentHashIndex` | Double-read consistency (detects stale index entries) |
| `ValidPageChecksums` | Marker — CRC validation passed during recovery |

### SimulationTrace / TraceEvent

```rust
use faster_dst::trace::{SimulationTrace, TraceEvent};

let trace = SimulationTrace::new();
// The scheduler automatically appends events:
// TraceEvent::Spawn, Select, Yield, Park, Unpark, Complete,
// TimeAdvance, IoStart, IoComplete, ChannelSend, ChannelRecv
```

---

## Writing Scenarios

### Step-by-Step Guide

#### 1. Define the workload factory

The workload factory receives a seed and returns a `CrudWorkload`. The seed
controls the PRNG inside the workload, making record selection deterministic.

```rust
.workload(|seed| CrudWorkload::new(seed, 100))
```

The second argument is the number of records. `CrudWorkload::new` generates
deterministic `(key, value)` pairs and tracks the expected state.

#### 2. Configure fault injection (optional)

```rust
.fault_config(
    FaultConfig::none()
        .with_partial_writes(0.05)
        .with_write_errors(0.01)
)
```

Fault rates are probabilities in `[0.0, 1.0]`. Set to `0.0` (default) for
no faults.

#### 3. Set up crash schedule (optional)

```rust
.crash_schedule(|seed| {
    let mut s = CrashSchedule::new();
    let phase_idx = (seed % 6) as usize;
    let point = CrashPoint::ALL_CHECKPOINT[phase_idx];
    s.add(point, CrashTrigger::Always);
    s
})
```

The crash schedule factory also receives the seed, so different seeds crash at
different points. The common pattern is `seed % N` to cycle through phases.

#### 4. Choose invariants

```rust
// Convenience method:
.check_committed_recoverable()

// Or manually:
.invariant(|wl| Box::new(AllCommittedRecoverable::new(wl.expected_state())))
.invariant(|_wl| Box::new(ValidPageChecksums::new()))
```

Invariant factories receive a reference to the workload so they can access
`expected_state()` for data-dependent checks.

#### 5. Build the ScenarioTemplate

```rust
let template = ScenarioTemplate::builder("my_scenario")
    .workload(|seed| CrudWorkload::new(seed, 100))
    .crash_schedule(|seed| { /* ... */ })
    .check_committed_recoverable()
    .build();
```

#### 6. Run with SeedCampaign

```rust
let mut campaign = SeedCampaign::new();
campaign
    .add_scenario(template)
    .seed_range(0..1000)
    .thread_count(8);

let report = campaign.run();
assert!(report.all_passed(), "{}", report.display_human());
```

### Complete Example

```rust
use faster_dst::campaign::SeedCampaign;
use faster_dst::crash::{CrashSchedule, CrashTrigger};
use faster_dst::fault::{CrashPoint, FaultConfig};
use faster_dst::invariant::{AllCommittedRecoverable, ValidPageChecksums};
use faster_dst::scenario::ScenarioTemplate;
use faster_dst::workload::{CrudWorkload, Workload};

let template = ScenarioTemplate::builder("checkpoint_with_torn_writes")
    .workload(|seed| CrudWorkload::new(seed, 50))
    .fault_config(FaultConfig::none().with_partial_writes(0.1))
    .crash_schedule(|seed| {
        let mut schedule = CrashSchedule::new();
        let phase_idx = (seed % 6) as usize;
        schedule.add(
            CrashPoint::ALL_CHECKPOINT[phase_idx],
            CrashTrigger::Always,
        );
        schedule
    })
    .invariant(|wl| Box::new(AllCommittedRecoverable::new(wl.expected_state())))
    .invariant(|_wl| Box::new(ValidPageChecksums::new()))
    .build();

let mut campaign = SeedCampaign::new();
campaign
    .add_scenario(template)
    .seed_range(0..500)
    .thread_count(4);

let report = campaign.run();
println!("{}", report.display_human());
assert!(report.all_passed());
```

---

## CI Integration

### Recommended Test Tiers

| Tier | Purpose | Time Budget | What to Run |
|------|---------|-------------|-------------|
| **Tier 1** (precheckin) | Smoke tests | < 60 s | Unit tests, small seed ranges (0..10) |
| **Tier 2** (CI) | Standard coverage | < 5 min | All 5 standard scenarios, 100 seeds each |
| **Tier 3** (nightly) | Deep exploration | < 30 min | 10,000 seeds × 5 templates |

### Commands

```bash
# All DST tests (Tier 1 + 2)
cargo nextest run -p faster-dst

# Include Tier 3 (long-running) tests
cargo nextest run -p faster-dst --run-ignored

# Campaign tests only
cargo nextest run -p faster-dst --test campaign_tests

# Full workspace check (includes DST)
cd rust && ./scripts/precheckin
```

### Standard Scenario Templates

| Template | Module | Records | Crash Points | Invariant |
|----------|--------|---------|--------------|-----------|
| `crud_stress` | `scenarios::crud_stress` | 200 | None | AllCommittedRecoverable |
| `checkpoint_crash` | `scenarios::checkpoint_crash` | 50 | 6 checkpoint phases | AllCommittedRecoverable |
| `compaction_crash` | `scenarios::compaction_crash` | 100 | 6 compaction phases | AllCommittedRecoverable |
| `recovery_stress` | `scenarios::recovery_stress` | 50 | 6 recovery phases | AllCommittedRecoverable |
| `torn_write` | `scenarios::torn_write` | 50 | 6 checkpoint phases | AllCommittedRecoverable + ValidPageChecksums |

---

## Debugging Failures

### Seed Reproduction

When a campaign reports a failure, the output includes a reproduction command:

```
FAIL: checkpoint_crash seed=42 — key 5: expected 50, got NotFound
  Reproduce: cargo test -p faster-dst --test campaign_tests -- checkpoint_crash --seed=42
```

To reproduce:
1. Copy the reproduction command from the report.
2. Run it. The seed guarantees the same crash point fires at the same time.
3. Add `RUST_LOG=trace` for detailed output.

### Trace Logging

`SimulationTrace` records every scheduler decision as a `TraceEvent`:

| Event | Description |
|-------|-------------|
| `Spawn` | New task added to run queue |
| `Select` | Scheduler picked a task to run |
| `Yield` | Task voluntarily yielded |
| `Park` | Task blocked (with `BlockReason`) |
| `Unpark` | Task unblocked by external event |
| `Complete` | Task finished |
| `TimeAdvance` | Simulated clock moved forward |
| `IoStart` | I/O operation began |
| `IoComplete` | I/O operation finished |
| `ChannelSend` | Message sent on `SimChannel` |
| `ChannelRecv` | Message received from `SimChannel` |

To use traces for debugging:
1. Create a `SimulationTrace::new()` and pass it to the scheduler.
2. After the run, iterate over events to see the exact execution order.
3. Compare traces from a passing seed and a failing seed to find divergence.

### Common Failure Patterns

**CRC mismatch → torn write during checkpoint.**
The `crash_point!` fired between writing page data and flushing metadata. On
recovery, the CRC recomputation found a partial page. Fix: ensure the checkpoint
state machine handles partial writes gracefully.

**Recovery failure → crash during state machine transition.**
A crash point fired between two recovery phases (e.g., after loading the index
but before validating the log). The recovery code path did not expect this
intermediate state. Fix: make each recovery phase idempotent.

**Invariant violation → consistency bug in compaction or epoch management.**
`AllCommittedRecoverable` found a missing or wrong value. This usually indicates
that a compaction or epoch drain discarded data it should have preserved. Debug
by narrowing the crash point and reducing the workload size to find the minimal
reproducing case.

**Deadlock → tasks mutually waiting.**
The scheduler returns `SchedulerResult::Deadlock` when all tasks are parked with
no pending unpark events. This indicates a logic bug in the task coordination
(e.g., two tasks each waiting for the other's channel message).
