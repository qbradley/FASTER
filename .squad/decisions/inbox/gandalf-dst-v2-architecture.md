# DST v2.0 Architecture: Deterministic Simulation Testing for Concurrency

**Author:** Gandalf (Lead Architect)  
**Date:** 2026-03-16  
**Status:** Architecture Proposal  
**Requested by:** qbradley  
**Depends on:** [Deadlock Post-Mortem](gandalf-deadlock-postmortem.md), [Loom & DST Gap Analysis](eowyn-loom-dst-gap-analysis.md)

---

## Executive Summary

Two P0 deadlocks escaped 2,100+ tests and only surfaced under sustained 16-thread stress. Our post-mortem concluded that **no existing tool in our portfolio catches time/scale-dependent bugs deterministically**. Loom covers memory ordering; property tests cover state spaces; unit tests cover correctness. None cover pipeline progress under resource saturation.

This document architects DST v2.0 — a FoundationDB-inspired deterministic simulation framework that runs **real `FasterKv` code** with simulated I/O, virtual time, and controlled scheduling. A failing seed becomes a permanent, reproducible regression test. The framework targets CI release-gate integration at <10 minutes for the core suite.

**Key architectural decision:** We extend the existing `faster-dst` crate (which already has SimClock, DeterministicScheduler, SimulatedDevice, SeedCampaign, and 22 crash-recovery scenarios) rather than building from scratch. The existing infrastructure covers crash/recovery; v2.0 adds **live concurrency simulation**.

---

## Table of Contents

1. [Requirements & Constraints](#1-requirements--constraints)
2. [Architecture Decisions](#2-architecture-decisions)
3. [What to Simulate vs What Stays Real](#3-what-to-simulate-vs-what-stays-real)
4. [Core Design: The Simulation Model](#4-core-design-the-simulation-model)
5. [Release Gate Integration](#5-release-gate-integration)
6. [Phased Implementation Roadmap](#6-phased-implementation-roadmap)
7. [Comparison with Alternatives](#7-comparison-with-alternatives)
8. [Risk Analysis](#8-risk-analysis)
9. [Appendix: FoundationDB Design Principles Applied](#appendix-foundationdb-design-principles-applied)

---

## 1. Requirements & Constraints

### 1.1 Hard Requirements

| Requirement | Target | Rationale |
|-------------|--------|-----------|
| **Determinism** | Same seed → identical execution → identical outcome | Reproducibility is the entire point. A flaky DST is worse than no DST — it wastes engineering time. |
| **No false positives** | A failure MUST indicate a real bug | qbradley's directive: "must NOT fail intermittently or for non-bug reasons." Infrastructure flakes destroy trust. |
| **Bug class coverage** | Must catch pipeline progress failures (Bug 1), scale-dependent starvation (Bug 2), and future variants | These are the classes our existing tools cannot reach. |
| **Release gate fitness** | Core suite <10 min, extended suite <60 min | Must run on every PR or nightly. Longer is acceptable for extended exploration. |
| **Resource bounded** | 4-8 cores, 16-32 GB RAM (standard CI runner) | Cannot require special hardware. Must be runnable on developer laptops too. |
| **Real code execution** | Simulated I/O and time, but real `FasterKv` logic | Models diverge from reality. FoundationDB's key insight: run the actual code. |

### 1.2 Soft Requirements

| Requirement | Target | Rationale |
|-------------|--------|-----------|
| **Developer ergonomics** | Reproduce failure in <30 seconds from seed | Fast iteration is critical. Developer gets seed from CI, runs locally, debugs immediately. |
| **Incremental adoption** | Must not break existing 22 DST scenarios or 1,700+ unit tests | Build on what works. |
| **Zero production cost** | `simulation` feature compiles away completely | Already achieved with `crash_point!` macro pattern and `#[cfg(feature = "simulation")]`. |

### 1.3 Non-Requirements (Explicit Exclusions)

| Not Required | Why |
|-------------|-----|
| Network simulation | FASTER is not distributed. No network I/O to simulate. |
| Full memory simulation | Page frames are small in tests. Track growth patterns, not individual allocations. |
| Async runtime replacement | FASTER deliberately avoids async runtimes (`std::thread` + channels). We simulate concurrency at the task level, not the runtime level. |

---

## 2. Architecture Decisions

### AD-1: Extend `faster-dst`, Don't Create a New Crate

**Decision:** DST v2.0 lives in the existing `faster-dst` crate as new modules alongside the crash-recovery infrastructure.

**Rationale:**
- `faster-dst` already has: `SimulatedClock` (clock.rs), `DeterministicScheduler` (scheduler.rs), `SimTask`/`TaskAction`/`BlockReason` (task.rs), `SeedCampaign` (campaign.rs), `SimulatedDevice` (device.rs), `Workload` trait (workload.rs), `Invariant` trait + 5 implementations (invariant.rs), 22 scenarios.
- Creating a separate crate would duplicate the campaign runner, invariant checkers, device wrappers, and test harness.
- The `simulation` feature flag on `faster-core` already gates crash-point hooks — we'll extend it to gate concurrency hooks using the same pattern.
- Scenario families are a natural extension: `scenarios/crash/` (existing) + `scenarios/concurrency/` (new).

**Trade-off:** This means DST v2.0 and crash-recovery DST share a dependency surface. If v2.0 changes break crash scenarios, we have a regression. Mitigated by: existing campaign tests run both families.

---

### AD-2: Run Real `FasterKv` Code with Simulated I/O and Time

**Decision:** The simulator runs our actual `FasterKv`, `HybridLogAllocator`, `PageFlusher`, `PageEvictor`, epoch system, and hash index — all real production code. Only I/O (Device), time (clock), and thread scheduling are simulated.

**Rationale:**
- FoundationDB's core principle: "Run real code, not models." Models diverge from reality; real code catches real bugs.
- Bug 1 lives in the interaction between `flush_sealed_pages()` → `Device::write_async()` → `IoRequestResult::QueueFull` → page state machine. If we model any of these, we might model the bug away.
- Bug 2 lives in `InMemoryDevice::truncate_until()` under `RwLock::write()`. The real `RwLock` and real `fill(0)` cost is what causes the bug.
- The `Device` trait (`faster-core/src/device.rs:229-302`) is already the abstraction boundary. `FasterKv` takes `Box<dyn Device>` — we just provide a smarter simulated device.

**Trade-off:** Running real code means we can't speed-skip expensive operations. Bug 2's `fill(0)` on a 9GB Vec takes seconds with real code. Solution: in simulation, we bound data sizes but configure resource pressure to trigger the same conditions at smaller scale (see Section 4.3).

---

### AD-3: Two-Phase Thread Model — Controlled Multi-Thread (Phase 1) → Green Thread (Phase 2)

**Decision:** Phase 1 uses real OS threads with simulated I/O completion timing. Phase 2 replaces OS threads with `DeterministicScheduler` cooperative tasks for full determinism.

#### Phase 1: Controlled Multi-Thread (MVP)

```
┌─────────────────────────────────────────────────┐
│                  Test Harness                     │
│  ┌──────────┐  ┌──────────┐  ┌──────────┐       │
│  │ Writer 1 │  │ Writer 2 │  │ Writer N │  (OS) │
│  └────┬─────┘  └────┬─────┘  └────┬─────┘       │
│       │              │              │              │
│       ▼              ▼              ▼              │
│  ┌────────────────────────────────────────┐       │
│  │           FasterKv (REAL CODE)          │       │
│  │  HybridLog · Epoch · HashIndex · Flush │       │
│  └────────────────┬───────────────────────┘       │
│                   │                                │
│                   ▼                                │
│  ┌────────────────────────────────────────┐       │
│  │      SimDevice v2 (ASYNC QUEUE)        │       │
│  │  queue_depth · latency · QueueFull     │       │
│  │  completion_queue → scheduler drains   │       │
│  └────────────────┬───────────────────────┘       │
│                   │                                │
│                   ▼                                │
│  ┌─────────┐  ┌──────────┐                        │
│  │SimClock │  │Completion│ (single scheduler      │
│  │(virtual)│  │Scheduler │  thread drains I/O     │
│  └─────────┘  └──────────┘  callbacks at sim-time)│
└─────────────────────────────────────────────────┘
```

**How it works:**
- Writer threads are real OS threads calling real `FasterKv::upsert()`.
- `SimDevice v2` has a bounded completion queue. `write_async()` returns `QueueFull` when queue is full (deterministic based on queue depth config). Successfully submitted I/O is enqueued with a simulated completion time.
- A dedicated **completion scheduler thread** drains the queue, advancing SimClock, and firing I/O callbacks at the correct virtual time.
- `maintenance()` calls `device.poll_completions()`, which drains ready completions from the queue.
- Timeouts in `Drop` and checkpoint orchestrator use SimClock instead of `Instant::now()`.

**Determinism level:** I/O timing is deterministic (seeded). Thread interleaving is NOT deterministic (OS scheduler). However, I/O timing and back-pressure control is sufficient to **reliably trigger Bug 1-class failures** — the bug manifests whenever QueueFull occurs, regardless of thread interleaving order.

**Catches:** Pipeline progress failures (Bug 1), I/O back-pressure cascades, timeout-dependent bugs. Does NOT catch: fine-grained thread interleaving bugs (that's loom's job).

#### Phase 2: Green Thread Simulation (Full Determinism)

```
┌───────────────────────────────────────────────────┐
│              DeterministicScheduler                 │
│   (single OS thread, all tasks cooperative)         │
│                                                     │
│  ┌──────┐ ┌──────┐ ┌──────┐ ┌─────────────┐      │
│  │Task 1│ │Task 2│ │Task N│ │Maintenance  │      │
│  │Writer│ │Writer│ │Writer│ │Task         │      │
│  └──┬───┘ └──┬───┘ └──┬───┘ └──────┬──────┘      │
│     │        │        │             │              │
│     ▼        ▼        ▼             ▼              │
│  ┌────────────────────────────────────────┐        │
│  │         FasterKv (REAL CODE)            │        │
│  │   Session per task · Epoch per task     │        │
│  └────────────────┬───────────────────────┘        │
│                   │                                 │
│                   ▼                                 │
│  ┌────────────────────────────────────────┐        │
│  │    SimDevice v2 (SCHEDULER-INTEGRATED) │        │
│  │  Completion → schedule callback event  │        │
│  │  QueueFull → park task                 │        │
│  └────────────────────────────────────────┘        │
│                                                     │
│  SimClock: scheduler advances after each step       │
│  PRNG: seed controls task selection + I/O timing    │
│  Result: FULLY DETERMINISTIC                        │
└───────────────────────────────────────────────────┘
```

**How it works:**
- Each "writer thread" is a `SimTask` in the `DeterministicScheduler`.
- Tasks call real `FasterKv` operations synchronously. After each operation, they yield (`TaskAction::Yield`) to let other tasks run.
- `SimDevice v2` enqueues I/O completions as scheduler events at `SimClock::now() + latency`. When a task calls `poll_completions()`, the scheduler fires ready events.
- The scheduler's seeded PRNG picks which ready task runs next → same seed = same interleaving = fully deterministic.
- Each task has its own `FasterSession` stored in task-local storage (`TaskContext::set_local`).

**Determinism level:** FULL. Same seed → same task interleaving → same I/O timing → same execution → same outcome. A failing seed is a permanent regression test.

**Catches:** Everything Phase 1 catches, PLUS: fine-grained scheduling-dependent bugs, epoch interaction timing, maintenance vs writer interleaving.

**Rationale for two phases:**
1. Phase 1 is implementable in ~4 weeks and catches the bug classes we need most urgently (pipeline progress).
2. Phase 2 requires adapting FasterKv's session/epoch model for green threads (~4-6 additional weeks) but delivers FoundationDB-grade determinism.
3. Phase 1 is independently valuable even if Phase 2 is never built.

---

### AD-4: SimDevice v2 — Async Completion Queue with Pressure Control

**Decision:** Replace the current synchronous `SimulatedDevice` (all I/O returns `CompletedSync`) with an async model where I/O completions are deferred and dispatched by the scheduler.

**Current state** (from `faster-dst/src/device.rs:200-358`):
```
SimulatedDevice.write_async() → immediately calls callback → returns CompletedSync
```
This hides all I/O timing. Bug 1 requires QueueFull; Bug 2 requires lock contention during truncate_until. Neither can manifest with synchronous completion.

**New design:**
```rust
pub struct SimDeviceV2 {
    // Configuration (immutable after construction)
    queue_depth: usize,
    base_latency: Duration,          // Simulated I/O time
    latency_jitter: Duration,        // Random ± jitter
    fault_config: FaultConfig,       // Existing fault injection

    // State (mutable, protected)
    pending_queue: Mutex<VecDeque<PendingIo>>,  // In-flight I/O
    storage: Arc<SimulatedStorage>,              // Existing backing store
    clock: Arc<SimulatedClock>,                  // Virtual time source
    rng: Mutex<ChaChaRng>,                       // Deterministic PRNG

    // Metrics
    total_submitted: AtomicU64,
    total_completed: AtomicU64,
    total_queue_full: AtomicU64,
}

struct PendingIo {
    complete_at: u64,                // SimClock nanos when callback should fire
    callback: IoCompletionCallback,  // The callback function pointer
    context: *mut u8,                // Callback context
    status: IoStatus,                // Success or injected error
    bytes: u32,                      // Transfer size
}
```

**Behavior:**
1. `write_async()`: If `pending_queue.len() >= queue_depth` → return `QueueFull`. Otherwise, compute `complete_at = clock.now() + base_latency ± jitter(rng)`, enqueue `PendingIo`, return `Submitted` (NOT `CompletedSync`).
2. `poll_completions()`: Drain all `PendingIo` where `complete_at <= clock.now()`. Fire callbacks. Return count.
3. `truncate_until()`: In Phase 2, yield to scheduler with `BlockReason::Time(O(n) estimate)`. In Phase 1, execute real truncate (bounded by test data size).

**Why `ChaChaRng`:** Cryptographic quality isn't needed, but `ChaChaRng` is seedable, portable, and produces identical sequences across platforms. `SmallRng` is not guaranteed cross-platform reproducible.

---

### AD-5: Virtual Time via SimClock — Already Exists, Needs Wiring

**Decision:** Wire the existing `SimulatedClock` (`faster-dst/src/clock.rs`) into `faster-core` through the `simulation` feature flag in `sync.rs`.

**Current state** (`sync.rs:82-88`):
```rust
// Phase 1 (current): all tiers use std time
pub(crate) use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
```

**Target state:**
```rust
#[cfg(all(not(loom), feature = "simulation"))]
pub(crate) mod time {
    pub use std::time::Duration;
    // Instant and SystemTime replaced by SimClock-backed versions
    pub use crate::sim_hooks::SimInstant as Instant;
    pub use crate::sim_hooks::SimSystemTime as SystemTime;
}

#[cfg(all(not(loom), not(feature = "simulation")))]
pub(crate) use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
```

**Time advancement model:**
- In Phase 1 (controlled multi-thread): The completion scheduler thread advances SimClock as it processes I/O completions. Time advances in I/O-latency-sized steps.
- In Phase 2 (green thread): The `DeterministicScheduler` advances SimClock between task steps. When all tasks are blocked on I/O, time jumps to the next completion event (no wasted cycles).

**How virtual time handles "O(n) takes O(n) time":**
- SimDevice's `truncate_until()` doesn't actually zero memory. Instead, it computes the virtual duration: `duration = data_size * ns_per_byte` and tells the scheduler "this task is blocked for `duration`." Other tasks run while this one sleeps.
- This catches Bug 2 without needing 9GB of real memory: even with 1MB of test data, if `ns_per_byte` is configured to model production scale, the virtual lock hold time triggers starvation in simulation.

---

### AD-6: Buggify — Chaos Injection at Semantic Points

**Decision:** Extend the existing `crash_point!` pattern with a new `buggify!` macro for delay/failure injection at runtime.

**Design:**
```rust
// In faster-core/src/sim_hooks.rs (alongside crash_point!)
macro_rules! buggify {
    ($label:expr) => {
        #[cfg(feature = "simulation")]
        {
            $crate::sim_hooks::maybe_inject_delay($label);
        }
    };
    ($label:expr, fail) => {
        #[cfg(feature = "simulation")]
        {
            if $crate::sim_hooks::should_inject_failure($label) {
                return Err($crate::sim_hooks::injected_error($label));
            }
        }
    };
}
```

**Placement (key semantic points):**
```rust
// In flush.rs — before each page flush
buggify!("pre_flush_page");

// In operations.rs — allocator retry loop
buggify!("allocator_retry_yield");

// In device.rs — before write_async callback
buggify!("pre_io_callback");

// In kv.rs — maintenance entry
buggify!("maintenance_entry");

// In page.rs — state transition CAS
buggify!("page_state_transition");
```

**Injection controlled by seed:** The thread-local buggify oracle (installed by test harness) uses the seeded PRNG to decide injection probability. Same seed → same injection decisions → deterministic chaos.

**Zero-cost without `simulation` feature:** Compiles to nothing, identical to `crash_point!`.

---

## 3. What to Simulate vs What Stays Real

This is the most critical architectural decision. The principle: **simulate the minimum necessary for determinism; keep everything else real to maximize bug-catching power.**

| Component | Real or Simulated | Rationale |
|-----------|------------------|-----------|
| **Hash index** | ✅ **Real** | Lock-free atomics. CAS operations work correctly in single-threaded simulation (always succeed without contention). In multi-task simulation, scheduler interleaving creates interesting orderings. No benefit to modeling. |
| **Epoch system** | ✅ **Real** (with adaptation) | The epoch table is an array of `AtomicU64` — works in simulation. `EpochThread`/`EpochGuard` are `!Send` (thread-affine), which works for Phase 1 (real threads). Phase 2 needs a small adaptation: map task IDs to epoch entry indices via `TaskContext::set_local`. |
| **HybridLog page state machine** | ✅ **Real** | This IS the component where both bugs live. The `PageState` transitions (Free → Open → Sealed → Flushing → Flushed → Evicted) must be real code to catch real bugs. |
| **Flush/evict pipeline** | ✅ **Real** | `PageFlusher`, `PageEvictor`, `maintenance()` — all real. The pipeline coordination is exactly what Bug 1 tests. |
| **Device I/O** | 🔶 **Simulated** | `SimDeviceV2` replaces `InMemoryDevice`. Async completion queue with configurable latency, queue depth, and fault injection. This is the primary lever for triggering timing-dependent bugs. |
| **Time** | 🔶 **Simulated** | `SimulatedClock` replaces `Instant::now()` and `SystemTime::now()` under `simulation` feature. Virtual time advances per scheduler (no wall-clock dependency). |
| **Thread scheduling** | Phase 1: ✅ **Real** (OS) / Phase 2: 🔶 **Simulated** (green threads) | Phase 1 loses thread-level determinism but gains I/O-level determinism. Phase 2 achieves full determinism. |
| **Memory allocation** | ✅ **Real** | Page frames use `std::alloc` with sector alignment. Real allocation catches OOM, alignment bugs. Test data sizes are bounded (config-controlled). |
| **Locks (RwLock, Mutex)** | Phase 1: ✅ **Real** / Phase 2: 🔶 **Instrumented** | Phase 1: real locks, real contention (non-deterministic but catches starvation at test scale). Phase 2: scheduler-aware locks that yield on contention, enabling deterministic lock ordering. |
| **Random number generation** | 🔶 **Simulated** | All randomness from `ChaChaRng` seeded by test seed. No `thread_rng()` or entropy sources. |

### Why Keep the Epoch System Real?

The epoch system (`epoch/table.rs`, `epoch/guard.rs`) is tempting to simulate because it's a concurrency primitive. But:
1. It's **lock-free** — only uses `AtomicU64` loads/stores with `Acquire`/`Release` ordering.
2. Loom already validates its memory ordering correctness (4 loom tests).
3. The bugs we're targeting are NOT epoch bugs — they're pipeline/I/O bugs.
4. Simulating epochs would require reimplementing the safe-epoch computation, which could mask real epoch bugs.

The only adaptation needed: in Phase 2 (green threads), `EpochThread::new()` must accept a task-provided entry index instead of discovering it from the OS thread. This is a ~10-line change gated behind `#[cfg(feature = "simulation")]`.

### Why Not Simulate the Hash Index?

The hash index is ~3,000 lines of lock-free concurrent code. Simulating it would mean:
- Building a simplified model that might not have the same bugs
- Missing prefetch timing interactions (P-05, P-06)
- Missing hash table growth race conditions

By keeping it real, every DST scenario implicitly tests the hash index under concurrent load. Free coverage.

---

## 4. Core Design: The Simulation Model

### 4.1 Concurrency Scenario Structure

```rust
// New: faster-dst/src/scenarios/concurrency/mod.rs
pub struct ConcurrencyScenario {
    pub name: &'static str,
    pub description: &'static str,
    pub config: ConcurrencyConfig,
    pub workload: Box<dyn Fn(u64) -> Box<dyn ConcurrencyWorkload>>,
    pub invariants: Vec<Box<dyn ConcurrencyInvariant>>,
}

pub struct ConcurrencyConfig {
    pub num_writers: usize,           // 2-64
    pub num_readers: usize,           // 0-64
    pub buffer_pages: usize,          // 4-256
    pub device_queue_depth: usize,    // 2-1024
    pub io_latency: Duration,         // 1ms-100ms (virtual)
    pub io_latency_jitter: Duration,  // 0-50ms
    pub lossy_mode: bool,             // Enables eviction with truncate
    pub sim_duration: Duration,       // 1s-300s (virtual)
    pub ops_per_writer: usize,        // 1K-10M
    pub key_distribution: KeyDist,    // Linear, Zipf, Uniform
    pub buggify_probability: f64,     // 0.0-0.1
    pub mutable_fraction: f64,        // 0.1-0.9
}

pub enum KeyDist {
    Linear,                           // k = writer_id * stride + seq (max alloc pressure)
    Zipf { theta: f64 },             // Hot-key skew (read-heavy, less alloc pressure)
    Uniform { range: u64 },          // Random keys in range (balanced)
}
```

### 4.2 Concurrency Invariants (New)

Existing invariants (`AllCommittedRecoverable`, `NoPhantomReads`, etc.) check **correctness after recovery**. Concurrency invariants check **liveness during execution**:

```rust
pub trait ConcurrencyInvariant {
    fn check_progress(&self, stats: &ExecutionStats) -> Result<(), String>;
}

pub struct ExecutionStats {
    pub total_ops: u64,
    pub ops_per_second_timeline: Vec<(Duration, f64)>,  // (sim_time, ops/s)
    pub min_throughput_1s_window: f64,
    pub max_stall_duration: Duration,     // Longest gap between ops
    pub maintenance_latencies: Vec<Duration>,
    pub queue_full_count: u64,
    pub total_sim_time: Duration,
}
```

**Built-in concurrency invariants:**

| Invariant | What It Checks | Catches |
|-----------|---------------|---------|
| `ThroughputFloor(min_ops_per_sec)` | ops/s never drops below threshold in any 1s window | Bug 1 (throughput cliff from 200K to 14 ops/s) |
| `NoStall(max_duration)` | No gap > max_duration between successful operations | Bug 1 (permanent stall), Bug 2 (multi-second stalls) |
| `BoundedMaintenanceLatency(max)` | Every `maintenance()` call completes within max virtual time | Bug 2 (O(n) truncate_until under lock) |
| `ForwardProgress(min_ops_per_epoch)` | At least N operations complete per epoch bump | Detects any form of liveness failure |
| `AllWritersComplete` | Every writer task finishes within sim_duration | Catches permanent deadlocks |

### 4.3 Scale Simulation Without Real Scale

The key insight for Bug 2: we don't need 9GB of real data to detect the bug. We need the **virtual cost** of `truncate_until` to scale with data written.

**Approach: Virtual cost annotations**

```rust
// In sim_hooks.rs
#[cfg(feature = "simulation")]
pub fn annotate_cost(label: &str, data_size: usize) {
    // Tell the scheduler: "this operation costs O(data_size) virtual time"
    // Scheduler models the cost without actually doing the work
    let ns_per_byte = VIRTUAL_COST_TABLE.get(label).unwrap_or(&1);
    let virtual_duration = Duration::from_nanos((data_size as u64) * ns_per_byte);
    // In Phase 1: sleep for a scaled-down real duration
    // In Phase 2: park task for virtual_duration
    sim_sleep(virtual_duration);
}
```

**Placement in `InMemoryDevice::truncate_until()`:**
```rust
fn truncate_until(&self, offset: u64) {
    #[cfg(feature = "simulation")]
    sim_hooks::annotate_cost("truncate_until", offset as usize);

    #[cfg(not(feature = "simulation"))]
    {
        // Production: no-op (already fixed per Bug 2 post-mortem)
    }
}
```

This means: even with 1MB of test data, if the virtual cost model says "truncate_until at offset 1MB costs 100ms," the simulator detects that a 100ms write-lock hold starves other threads. The **pattern** is caught at small scale.

### 4.4 Watchdog: Deadlock Detection

```rust
pub struct Watchdog {
    max_stall: Duration,       // Max time between any two ops
    max_total: Duration,       // Max total simulation time
    check_interval: Duration,  // How often to check
}
```

**Phase 1 (real threads):** A dedicated watchdog thread monitors a shared `AtomicU64` that writers increment on each operation. If no increment for `max_stall` real time, the watchdog captures thread backtraces and fails with diagnostic output.

**Phase 2 (green threads):** The scheduler checks after each step: if no task has made progress for `max_stall` virtual time and all tasks are either blocked or yielding, declare deadlock. The scheduler can print the exact task states, block reasons, and event queue.

### 4.5 Seed Strategy & Event Trace

**Every simulation run produces:**
```
SimulationTrace {
    seed: u64,
    config: ConcurrencyConfig,
    events: Vec<TraceEvent>,
    // TraceEvent = (sim_time, task_id, event_type, detail)
    // event_type ∈ {IoSubmit, IoComplete, QueueFull, PageTransition,
    //               MaintenanceStart, MaintenanceEnd, AllocRetry,
    //               EpochBump, BuggifyInject, ...}
    outcome: SimulationOutcome,  // Passed | Failed(assertion, trace_at)
}
```

**Failure output format:**
```
SIMULATION FAILED
  Seed:       0xDEADBEEF42
  Scenario:   multi_writer_saturation
  Config:     {writers: 8, buffer: 16, queue_depth: 4, latency: 10ms}
  Assertion:  ThroughputFloor(1000): min_throughput=0 at T=4.2s
  
  Event trace (last 20 events before failure):
    T=4.100s [writer-3]  upsert(key=30042) → OK
    T=4.105s [maint]     flush_sealed(page=7) → QueueFull
    T=4.105s [maint]     poll_completions() → 0
    T=4.110s [writer-1]  upsert(key=10041) → BufferFull (retry 1/32)
    T=4.115s [writer-2]  upsert(key=20041) → BufferFull (retry 1/32)
    ...
    T=4.200s [watchdog]  STALL DETECTED: no ops for 100ms

  Reproduce: cargo test --features simulation dst_concurrency -- --seed 0xDEADBEEF42
```

---

## 5. Release Gate Integration

### 5.1 CI Pipeline Integration

```
PR Gate (every PR, <10 min):
├── cargo fmt --check
├── cargo clippy -- -D warnings
├── cargo nextest run              (unit + integration, ~90s)
├── cargo test --features simulation dst_concurrency
│   ├── 10 fixed regression seeds  (known-good, ~2 min)
│   └── 20 random exploration seeds (~5 min)
└── Total: ~8 min

Nightly Extended (daily, <60 min):
├── All PR gate checks
├── cargo test --features simulation dst_concurrency_extended
│   ├── 10 fixed regression seeds
│   └── 200 random exploration seeds (~30 min)
├── cargo test --features simulation dst_crash_recovery
│   └── 3009 scenarios (existing, ~220s)
├── 16-thread stress test (5 min wall-clock)
└── Total: ~45 min

Weekly Exploration (weekend, <8 hr):
├── 10,000 random seeds across all concurrency scenarios
├── Extended sim_duration (300s virtual per seed)
├── Results → seed database for regression suite curation
└── Any failure → auto-file GitHub issue with seed + trace
```

### 5.2 Seed Management Strategy

**Three seed classes:**

| Class | Purpose | Storage | Lifecycle |
|-------|---------|---------|-----------|
| **Regression seeds** | Seeds that found real bugs — permanent regression tests | `faster-dst/src/scenarios/concurrency/regression_seeds.rs` (checked into source) | Never removed. If code change breaks a regression seed, it's a real regression. |
| **Exploration seeds** | Random seeds for discovering new bugs | Generated from system entropy at CI time | Ephemeral. A failing exploration seed is promoted to regression seed. |
| **Canary seeds** | Fixed seeds chosen to exercise specific code paths (QueueFull, lossy eviction, etc.) | Checked into source alongside scenario definitions | Maintained by scenario authors. |

**Seed rot mitigation:**

Code changes can alter execution paths, causing the same seed to exercise different code. This is inevitable and acceptable IF:
1. Regression seeds still PASS (the bug they found is still fixed).
2. If a regression seed fails after a code change, it means the change reintroduced the bug.
3. If a regression seed passes but now exercises different code, that's fine — the seed's value was in finding the original bug and preventing its regression.

**Handling seed rot:**
- Regression seeds include the commit SHA they were captured at.
- If a regression seed stops triggering its original code path (detectable via trace diff), flag for review but don't remove.
- Exploration seeds regenerate every run — they're immune to rot by design.

### 5.3 Failure-to-Fix Pipeline

```
1. DST finds failure → log seed + trace + assertion
2. CI reports: "DST failure: seed=0xABC, scenario=X, assertion=Y"
3. Developer runs: `cargo test --features simulation dst_seed -- --seed 0xABC`
4. Failure reproduces locally in <30s (deterministic)
5. Developer debugs with trace output (every I/O event, page transition, etc.)
6. Developer fixes bug
7. Developer verifies: seed 0xABC now passes
8. Developer adds seed to regression_seeds.rs
9. PR includes: code fix + regression seed
```

**Time from detection to reproduction: <30 seconds.** This is DST's killer feature vs stress testing (where reproduction can take hours of trial-and-error).

---

## 6. Phased Implementation Roadmap

### Phase 1: Async SimDevice + Concurrency Scenarios (4-5 weeks)

**Goal:** Catch Bug 1-class failures (pipeline progress under I/O back-pressure) with controlled I/O timing. Not fully deterministic (real OS threads), but I/O timing is seeded and reproducible.

| Week | Deliverable | Owner | Validation |
|------|-------------|-------|------------|
| 1 | `SimDeviceV2` with async completion queue, queue depth, latency | Éowyn | Unit tests: QueueFull at configured depth, completions fire after simulated latency |
| 1 | Wire `SimulatedClock` into `sync.rs` under `simulation` feature | Sam | Existing DST scenarios still pass; `Instant::now()` returns SimClock time |
| 2 | `ConcurrencyInvariant` trait + `ThroughputFloor`, `NoStall`, `AllWritersComplete` | Éowyn | Unit tests for each invariant |
| 2 | `ConcurrencyConfig` + `ConcurrencyScenario` harness | Éowyn | Harness runs a trivial 2-writer scenario end-to-end |
| 3 | 5 core scenarios: `multi_writer_saturation`, `lossy_eviction`, `mixed_rw`, `small_buffer_pressure`, `queue_full_cascade` | Éowyn + Boromir | Each scenario catches at least one known bug pattern |
| 3 | Watchdog thread for real-thread mode | Sam | Detects intentionally-injected deadlock within 5s |
| 4 | `buggify!` macro + 10 injection points | Sam | Controlled injection at known sensitive points |
| 4 | `SeedCampaign` extension for concurrency scenarios | Éowyn | 100-seed campaign completes in <5 min |
| 5 | CI integration (PR gate: 30 seeds, nightly: 200 seeds) | Boromir | Green CI run, failure report format validated |

**Success criteria:** Running the `multi_writer_saturation` scenario with `queue_depth=4` on the **pre-fix** code (before Bug 1 fix) produces a consistent failure for any seed that triggers QueueFull. Running on post-fix code passes for all seeds.

**Estimated CI time:** ~5 min for 30 seeds (PR gate), ~15 min for 200 seeds (nightly).

### Phase 2: Green Thread Adapter + Full Determinism (4-6 weeks)

**Goal:** Replace OS threads with `DeterministicScheduler` tasks. Same seed = same execution = same outcome. Fully deterministic.

| Week | Deliverable | Owner | Validation |
|------|-------------|-------|------------|
| 6 | `SimThread` adapter: maps `std::thread::spawn` to `DeterministicScheduler::spawn` under `simulation` feature | Sam | Single-thread regression: all existing tests pass under simulation mode |
| 7 | `SimRwLock` / `SimMutex`: scheduler-aware locks that `Park` the current task on contention | Sam | Lock contention yields to scheduler; deadlock detected as "all tasks parked" |
| 8 | Epoch system adaptation: task-local entry index, `SimEpochThread` | Sam + Gandalf (review) | Epoch safety invariants hold under green-thread scheduling |
| 9 | Session adaptation: `FasterSession` usable across task yields (task-local storage) | Sam | Multi-writer concurrency scenario runs fully deterministic |
| 10 | Full determinism validation: same seed on Linux x86_64 and macOS aarch64 produces identical trace | Éowyn | Cross-platform seed reproducibility confirmed |
| 11 | Migrate Phase 1 scenarios to green-thread mode | Éowyn | All scenarios pass; at least 5 regression seeds from Phase 1 still trigger same code paths |

**Success criteria:** Running seed X on any platform produces byte-identical `SimulationTrace`. Bug 1 and Bug 2 are both caught deterministically. Developer can single-step through the scheduler to understand exactly how a deadlock forms.

**Hard technical risk:** `FasterKv` internal spawns (grow state machine, checkpoint state machine) use `std::thread::spawn` inside core logic. These must be intercepted and routed to the scheduler. See [Section 8: Risk Analysis](#8-risk-analysis).

### Phase 3: Buggify, Extended Scenarios, Exploration (2-3 weeks)

**Goal:** Maximize coverage through chaos injection and scenario diversity.

| Week | Deliverable | Owner |
|------|-------------|-------|
| 12 | 30 additional injection points for `buggify!` across flush, evict, checkpoint, grow | Éowyn |
| 12 | 15 additional concurrency scenarios (grow-under-load, checkpoint-under-pressure, compaction-with-writers, etc.) | Éowyn + Boromir |
| 13 | Weekly exploration campaign (10,000 seeds, 8hr budget) | CI automation |
| 13 | Seed database + auto-issue-filing for failures | Boromir |
| 14 | Documentation: DST developer guide, scenario authoring guide, seed management | Gandalf |

**Success criteria:** Weekly exploration campaign runs unattended. Any failure auto-files a GitHub issue with reproduction instructions. Regression seed suite grows monotonically.

### Existing Infrastructure Reused

| Component | Location | Reuse in v2.0 |
|-----------|----------|---------------|
| `SimulatedClock` | `faster-dst/src/clock.rs` | ✅ Direct reuse — already has `advance()`, `now()`, `system_time_now()` |
| `DeterministicScheduler` | `faster-dst/src/scheduler.rs` | ✅ Direct reuse — already has `spawn()`, `run_until_complete()`, seeded PRNG task selection, `advance_time_if_needed()` |
| `SimTask` / `TaskAction` / `BlockReason` | `faster-dst/src/task.rs` | ✅ Direct reuse — `BlockReason::IoCompletion` and `BlockReason::Time` are exactly what we need |
| `SeedCampaign` | `faster-dst/src/campaign.rs` | ✅ Extend — add concurrency scenario family alongside crash family |
| `SimulatedStorage` | `faster-dst/src/device.rs` | ✅ Direct reuse — `Arc<RwLock<Vec<u8>>>` backing store with `snapshot()`/`restore()` |
| `SimulationHarness` | `faster-dst/src/harness.rs` | ✅ Extend — add `create_concurrency_harness()` method |
| `SimulationTrace` | `faster-dst/src/trace.rs` | ✅ Extend — add concurrency event types |
| `Workload` trait | `faster-dst/src/workload.rs` | ✅ Extend — add `ConcurrencyWorkload` variant |
| `Invariant` trait | `faster-dst/src/invariant.rs` | ✅ Model — `ConcurrencyInvariant` follows same pattern |
| `crash_point!` macro | `faster-core/src/sim_hooks.rs` | ✅ Pattern reuse — `buggify!` follows identical `#[cfg(feature = "simulation")]` gating |
| `sync.rs` cfg cascade | `faster-core/src/sync.rs` | ✅ Extend — wire SimClock into simulation tier |

**Net new code estimated:** ~3,000-4,000 lines for Phase 1, ~2,000-3,000 lines for Phase 2, ~1,000 lines for Phase 3.

---

## 7. Comparison with Alternatives

### Why Not Just Run Stress Tests Longer?

| Dimension | Stress Tests | DST v2.0 |
|-----------|-------------|-----------|
| **Determinism** | ❌ Non-deterministic. Same test, different hardware = different result. | ✅ Same seed = same execution. |
| **Reproduction** | ❌ "Run for 5 minutes and hope it fails again." Hours to reproduce intermittent failure. | ✅ `--seed 0xABC` reproduces in <30s. |
| **CI fitness** | ⚠️ 5-minute stress tests are acceptable nightly; 60-minute runs are not PR-gate material. | ✅ <10 min for core suite, every PR. |
| **Coverage** | ⚠️ Only finds bugs that manifest on THIS hardware in THIS time window. | ✅ Explores timing space via seed variation. Different seeds exercise different timing combinations. |
| **Resource cost** | ❌ 16 cores, 32GB RAM, 5 minutes wall-clock per run. Expensive at CI scale. | ✅ Single-threaded (Phase 2), <1 GB RAM, <10 min. |
| **False positives** | ⚠️ Hardware contention, CI load, thermal throttling can cause spurious timeouts. | ✅ Virtual time eliminates all environment sensitivity. |
| **Debugging** | ❌ Add printf, rerun, hope it reproduces. | ✅ Event trace shows exact sequence of operations. |

**Verdict:** Stress tests are complementary (they catch bugs DST can't model, like thermal issues). But they cannot be the primary release gate because they're non-deterministic and resource-heavy.

### Why Not Expand Loom?

| Dimension | Loom | DST v2.0 |
|-----------|------|-----------|
| **Bug 1 (pipeline progress)** | ❌ State space explosion. Would need to model 10+ components with 7+ threads. Infeasible. | ✅ Runs real code; QueueFull triggers naturally. |
| **Bug 2 (scale starvation)** | ❌ Fundamentally impossible. Loom has no concept of time or operation duration. | ✅ Virtual time models O(n) lock hold. |
| **State space** | ❌ Exponential in threads × operations. Max practical: 3 threads, 4-8 ops each. | ✅ Bounded by sim_duration × ops_per_second. Linear in scenario configuration. |
| **What loom IS good for** | ✅ Memory ordering bugs (Acquire/Release/SeqCst correctness), ABA races, CAS protocols. | ❌ Not designed for atomic-level verification. |

**Verdict:** Loom and DST are complementary, not competing. Loom validates primitive correctness; DST validates system-level liveness. Both stay in the test portfolio.

### Why Not Use Miri?

| Dimension | Miri | DST v2.0 |
|-----------|------|-----------|
| **Bug detection** | UB (undefined behavior), Stacked Borrows violations, uninitialized memory. | Pipeline progress, timing, back-pressure, starvation. |
| **Concurrency** | ⚠️ Basic data race detection. Cannot model scheduling strategies or I/O timing. | ✅ Full concurrency simulation with controlled scheduling. |
| **Performance** | ❌ 10-100x slower than native. Cannot run at scale. | ✅ Simulation is virtual — no real I/O, no real waiting. |

**Verdict:** Miri catches a completely different bug class (memory safety). Not applicable for our target bugs.

### What Does DST Give Us That Nothing Else Can?

**Unique capability matrix:**

| Capability | Only DST? | Why? |
|-----------|-----------|------|
| **Deterministic reproduction of timing bugs** | ✅ Yes | Stress tests find timing bugs but can't reproduce them. Loom can't model timing. DST seeds make timing bugs reproducible. |
| **Virtual time progression** | ✅ Yes | No other tool in our portfolio has virtual time. This is what enables scale simulation without real scale. |
| **I/O back-pressure control** | ✅ Yes | We can precisely control when QueueFull occurs, how long I/O takes, and what order completions fire. No randomness from hardware. |
| **Progress assertions under load** | ⚠️ Shared with stress tests | But DST adds: deterministic reproduction, bounded time, virtual time. |
| **Buggify chaos injection** | ✅ Yes (deterministic chaos) | Stress tests have random chaos (OS scheduling). DST has CONTROLLED chaos (seeded injection). |

---

## 8. Risk Analysis

### Risk 1: FasterKv Internal Thread Spawns (Phase 2 — HIGH)

**Problem:** `grow/state_machine.rs` and `checkpoint/state_machine.rs` call `std::thread::spawn()` for background work. In Phase 2 (green threads), these must be intercepted.

**Mitigation:**
- Extend `sync.rs` to gate `thread::spawn`:
  ```rust
  #[cfg(feature = "simulation")]
  pub fn spawn<F, T>(f: F) -> JoinHandle<T>
  where F: FnOnce() -> T + Send + 'static, T: Send + 'static
  {
      // Route to DeterministicScheduler::spawn()
      // Return a SimJoinHandle that parks the calling task until the spawned task completes
  }
  ```
- Audit: grep for `std::thread::spawn` outside `sync.rs` — must all go through the gated module.
- Risk level: **Manageable.** The spawn calls are in well-isolated state machine modules.

### Risk 2: `!Send`/`!Sync` Session Types (Phase 2 — MEDIUM)

**Problem:** `FasterSession` and `EpochGuard` are `!Send` (thread-affine). In green-thread mode, they must be usable across yield points.

**Mitigation:**
- In simulation mode, sessions don't need `Send` — they're never actually moved between OS threads. They're task-local state within a single OS thread.
- `TaskContext::set_local::<FasterSession>()` stores the session per task. Each task step retrieves it.
- `EpochGuard` is stack-scoped (RAII) — each task step creates and drops its own guard. No cross-yield holding.
- Risk level: **Low-Medium.** Requires careful audit of guard lifetimes but no fundamental redesign.

### Risk 3: Cross-Platform Seed Reproducibility (Phase 2 — MEDIUM)

**Problem:** Floating-point arithmetic, struct padding, HashMap ordering may differ across platforms, causing the same seed to produce different traces.

**Mitigation:**
- Use `ChaChaRng` (platform-independent output).
- Avoid `HashMap` in scheduler (use `BTreeMap` or `Vec` with deterministic ordering).
- No floating-point in the scheduler hot path.
- Validate: run same seed on x86_64 Linux and aarch64 macOS, compare traces.
- Risk level: **Low** if we're disciplined about avoiding non-deterministic containers.

### Risk 4: Phase 1 Non-Determinism May Cause Flaky CI (Phase 1 — MEDIUM)

**Problem:** Phase 1 uses real OS threads. Thread scheduling is non-deterministic. A seed might pass on one run and fail on another due to different thread interleavings.

**Mitigation:**
- Phase 1 assertions must be **robust to scheduling variation**. Instead of "exact trace match," assert "throughput never drops below X" — this is a property that holds regardless of scheduling IF the code is correct.
- If a Phase 1 seed is flaky (passes sometimes, fails sometimes), it means the bug is scheduling-dependent. Promote to Phase 2 investigation (which has deterministic scheduling).
- For the PR gate, only use Phase 1 for invariants that are **scheduling-independent** (e.g., "no permanent deadlock" — if the code deadlocks, it deadlocks regardless of scheduling).
- Risk level: **Acceptable for Phase 1.** Phase 2 eliminates this risk entirely.

### Risk 5: Performance of Green Thread Simulation (Phase 2 — LOW)

**Problem:** Running 16 "virtual writers" in a single OS thread via cooperative scheduling may be slower than real 16-thread execution.

**Mitigation:**
- DST doesn't need to be fast — it needs to be **deterministic**. Virtual time means we skip all real I/O waiting.
- A typical scenario: 16 writers × 10K ops each = 160K ops. At ~10μs per op (in-memory, no real I/O), that's ~1.6 seconds wall-clock.
- The scheduler overhead (task switch = function call) adds ~100ns per switch. 160K switches = 16ms. Negligible.
- Risk level: **Very low.** Single-threaded simulation of in-memory operations is fast.

---

## Appendix: FoundationDB Design Principles Applied

| FDB Principle | Application in FASTER DST v2.0 |
|--------------|-------------------------------|
| **Run real code, not models** | Real `FasterKv`, `HybridLog`, epoch system, hash index. Only Device and time are simulated. |
| **Control all non-determinism** | Seeded PRNG for scheduler (Phase 2), I/O timing, buggify injection. `SimClock` for time. No `thread_rng()`. |
| **Buggify everything** | `buggify!` macro at flush, evict, allocate, maintenance, page transition points. Zero-cost without `simulation` feature. |
| **Seeds are golden** | Failing seed → regression_seeds.rs → permanent test. Never delete a regression seed. |
| **Fast iteration** | `--seed 0xABC` reproduces in <30s. Event trace shows exact failure sequence. No "run for 5 minutes and hope." |

### What FoundationDB Does Differently (And Why)

FDB's entire codebase is written against a simulation layer from day one — they use `Future`/`Promise` for all I/O, which the simulator intercepts. FASTER was not designed this way. Our adaptation:

1. FDB: all I/O through `Future` → We: all I/O through `Device` trait (same abstraction boundary, different mechanism).
2. FDB: single-threaded actor model → We: Phase 1 uses real threads (pragmatic); Phase 2 uses cooperative tasks (deterministic).
3. FDB: simulates network partitions → We: don't need network simulation (not distributed). Simulate I/O queue pressure instead.
4. FDB: built simulation from day one → We: retrofit simulation onto existing code. Harder, but `#[cfg(feature = "simulation")]` and the `Device` trait make it tractable.

The `Device` trait is our saving grace. It's the single abstraction boundary through which all I/O flows. By providing a smarter `SimDeviceV2`, we intercept the I/O path without modifying any core logic. The `sync.rs` cfg cascade gives us the time and threading interception points. The `crash_point!` / `buggify!` pattern gives us zero-cost chaos injection.

**FASTER is in a better position than most systems for DST retrofitting** because it was designed with trait-based I/O abstraction and compile-time feature gating from the start.

---

*— Gandalf, Lead Architect*
*"A wizard is never late. Nor is he early. He arrives precisely when he means to — and so should your I/O callbacks."*
