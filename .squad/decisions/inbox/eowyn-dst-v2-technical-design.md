# DST v2.0 Core Technical Design

**Author:** Éowyn (Deterministic Simulation Testing Expert)  
**Date:** 2026-03-17  
**Status:** Technical Design — Implementation-Ready  
**Requested by:** qbradley  
**Depends on:**  
- [Architecture Decisions](gandalf-dst-v2-architecture.md) — 3-phase plan, real code through Device trait  
- [Internals Inventory](sam-dst-internals-inventory.md) — state machines, locks, I/O paths  

---

## Scope

This document provides Rust API sketches and implementation details for the five core DST v2.0 subsystems. It assumes the architecture decisions in Gandalf's doc are accepted and builds directly on the existing `faster-dst` infrastructure (SimulatedClock, DeterministicScheduler, SimTask, SimulatedDevice, SeedCampaign).

**Out of scope:** Buggify macro design, scenario authoring guide, Phase 3 exploration campaigns. Those are covered in the architecture doc.

---

## 1. SimClock (Virtual Time)

### 1.1 Design Goals

- Replace `Instant::now()` and `SystemTime::now()` under `#[cfg(feature = "simulation")]`
- Event-driven advancement: only the scheduler advances time, never wall-clock
- Model O(n) operations as sim-time cost without performing real work
- Thread-safe (used from multiple simulated tasks on one OS thread)

### 1.2 Struct Definition

The existing `SimulatedClock` in `faster-dst/src/clock.rs` already provides the core. We extend it with a `charge()` method for virtual cost modeling and a global accessor for use from `sync.rs`.

```rust
// faster-dst/src/clock.rs (extend existing)

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Deterministic virtual clock. Time advances only via explicit calls.
/// All methods are wait-free (AtomicU64 operations).
pub struct SimClock {
    /// Current monotonic time in nanoseconds.
    nanos: AtomicU64,
    /// Wall-clock epoch offset (default: 2025-01-01T00:00:00Z).
    epoch_offset_nanos: AtomicU64,
}

impl SimClock {
    pub fn new() -> Self {
        Self {
            nanos: AtomicU64::new(0),
            // 2025-01-01T00:00:00Z = 1_735_689_600 seconds
            epoch_offset_nanos: AtomicU64::new(1_735_689_600_000_000_000),
        }
    }

    /// Current monotonic time.
    pub fn now(&self) -> Duration {
        Duration::from_nanos(self.nanos.load(Ordering::Relaxed))
    }

    /// Current monotonic time as raw nanoseconds (avoids Duration construction in hot paths).
    pub fn now_nanos(&self) -> u64 {
        self.nanos.load(Ordering::Relaxed)
    }

    /// Advance time forward by `delta`. Panics if delta is zero (likely a bug).
    pub fn advance(&self, delta: Duration) {
        debug_assert!(!delta.is_zero(), "SimClock::advance(0) is likely a bug");
        self.nanos.fetch_add(delta.as_nanos() as u64, Ordering::Relaxed);
    }

    /// Set clock to exact point. Used by scheduler for time-jump to next event.
    pub fn set(&self, time: Duration) {
        self.nanos.store(time.as_nanos() as u64, Ordering::Relaxed);
    }

    /// Charge virtual time for an O(n) operation without performing real work.
    ///
    /// Example: `sim_clock.charge(Duration::from_nanos(bytes as u64 * COST_PER_BYTE))`
    ///
    /// This advances the clock, making the operation "take time" in simulation.
    /// The scheduler sees other tasks' deadlines become reachable during this cost.
    pub fn charge(&self, cost: Duration) {
        if !cost.is_zero() {
            self.nanos.fetch_add(cost.as_nanos() as u64, Ordering::Relaxed);
        }
    }

    /// Wall-clock time (monotonic + epoch offset). For SystemTime replacement.
    pub fn system_time_nanos(&self) -> u64 {
        self.epoch_offset_nanos.load(Ordering::Relaxed)
            + self.nanos.load(Ordering::Relaxed)
    }
}
```

### 1.3 Virtual Cost Constants

```rust
// faster-dst/src/clock.rs

/// Virtual cost model for O(n) operations.
/// Calibrated to approximate production hardware (NVMe SSD, DDR5).
pub mod cost {
    use std::time::Duration;

    /// Cost per byte for memory zeroing (fill(0)).
    /// Basis: ~10 GB/s memset throughput → ~0.1 ns/byte.
    /// Inflated 10× to make starvation detectable at small test scale.
    pub const MEMZERO_NS_PER_BYTE: u64 = 1;

    /// Cost per byte for device truncation modeling.
    /// Models the old O(n) truncate_until behavior that caused Bug 2.
    pub const TRUNCATE_NS_PER_BYTE: u64 = 1;

    /// Cost per hash bucket for invalidate_entries_in_range().
    /// Basis: ~100ns per bucket (cache miss + CAS).
    pub const HASH_INVALIDATE_NS_PER_BUCKET: u64 = 100;

    /// Cost per epoch table entry for compute_safe_epoch() scan.
    pub const EPOCH_SCAN_NS_PER_ENTRY: u64 = 5;

    /// Helper: compute charge duration for a byte-proportional operation.
    pub fn bytes_cost(bytes: u64, ns_per_byte: u64) -> Duration {
        Duration::from_nanos(bytes.saturating_mul(ns_per_byte))
    }
}
```

### 1.4 SimInstant and SimSystemTime (sync.rs Wrappers)

These are thin wrappers that satisfy the API contract of `std::time::Instant` and `std::time::SystemTime` while routing to the global `SimClock`.

```rust
// faster-core/src/sim_time.rs (new file, only compiled under simulation)

use std::time::Duration;

/// Thread-local reference to the active SimClock.
/// Set by the test harness before any FasterKv operations.
std::thread_local! {
    static SIM_CLOCK: std::cell::RefCell<Option<std::sync::Arc<crate::SimClock>>> =
        std::cell::RefCell::new(None);
}

/// Install a SimClock for the current thread. Called by test harness.
pub fn install_sim_clock(clock: std::sync::Arc<crate::SimClock>) {
    SIM_CLOCK.with(|c| *c.borrow_mut() = Some(clock));
}

fn with_clock<R>(f: impl FnOnce(&crate::SimClock) -> R) -> R {
    SIM_CLOCK.with(|c| {
        let borrow = c.borrow();
        let clock = borrow.as_ref().expect("SimClock not installed; call install_sim_clock()");
        f(clock)
    })
}

/// Drop-in replacement for `std::time::Instant`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct SimInstant {
    nanos: u64,
}

impl SimInstant {
    pub fn now() -> Self {
        Self { nanos: with_clock(|c| c.now_nanos()) }
    }

    pub fn elapsed(&self) -> Duration {
        let current = with_clock(|c| c.now_nanos());
        Duration::from_nanos(current.saturating_sub(self.nanos))
    }

    pub fn duration_since(&self, earlier: SimInstant) -> Duration {
        Duration::from_nanos(self.nanos.saturating_sub(earlier.nanos))
    }

    pub fn checked_duration_since(&self, earlier: SimInstant) -> Option<Duration> {
        self.nanos.checked_sub(earlier.nanos).map(Duration::from_nanos)
    }

    pub fn checked_add(&self, duration: Duration) -> Option<Self> {
        self.nanos
            .checked_add(duration.as_nanos() as u64)
            .map(|n| Self { nanos: n })
    }

    pub fn checked_sub(&self, duration: Duration) -> Option<Self> {
        self.nanos
            .checked_sub(duration.as_nanos() as u64)
            .map(|n| Self { nanos: n })
    }
}

impl std::ops::Add<Duration> for SimInstant {
    type Output = Self;
    fn add(self, rhs: Duration) -> Self {
        Self { nanos: self.nanos + rhs.as_nanos() as u64 }
    }
}

impl std::ops::Sub<Duration> for SimInstant {
    type Output = Self;
    fn sub(self, rhs: Duration) -> Self {
        Self { nanos: self.nanos.saturating_sub(rhs.as_nanos() as u64) }
    }
}

impl std::ops::Sub for SimInstant {
    type Output = Duration;
    fn sub(self, rhs: Self) -> Duration {
        Duration::from_nanos(self.nanos.saturating_sub(rhs.nanos))
    }
}
```

### 1.5 How Time Flows

```
Scheduler loop:
  1. Pick next ready task (PRNG-selected)
  2. Run one step of that task
  3. Task may call SimClock::charge() for O(n) work
  4. Task returns Yield / Park(IoCompletion) / Park(Time(deadline)) / Complete
  5. If no ready tasks:
     a. Find min deadline across all blocked tasks and pending I/O events
     b. Jump clock to that deadline: clock.set(min_deadline)
     c. Unpark all tasks/events whose deadline <= now
  6. If no ready tasks AND no pending deadlines → deadlock
```

**Key property:** Wall-clock never passes. Virtual time only advances when the scheduler decides. A 300-second simulated scenario takes ~2 seconds wall-clock.

---

## 2. DstScheduler (Deterministic Thread Scheduling)

### 2.1 Design Goals

- Single OS thread runs all simulated tasks (cooperative multitasking)
- Seed-controlled task selection: same seed → same interleaving → same outcome
- Priority queue: `(wake_time, tiebreak)` determines next task
- Model `thread::spawn` as task spawn, `yield_now` as reschedule point
- Detect deadlock (all blocked, no events) and starvation (task not run for >N sim-time)

### 2.2 Struct Definition

The existing `DeterministicScheduler` uses `Vec<TaskId>` ready queue with random selection. We replace it with a `BTreeMap`-based priority queue for deterministic ordering by wake time + tiebreak, and add I/O event tracking.

```rust
// faster-dst/src/scheduler_v2.rs

use std::collections::{BTreeMap, HashMap};
use std::time::Duration;
use rand::SeedableRng;
use rand_chacha::ChaChaRng;  // Cross-platform reproducible (replaces SmallRng)

/// Unique task identifier.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TaskId(pub u64);

/// Why a task is parked.
#[derive(Clone, Debug)]
pub enum BlockReason {
    /// Waiting for I/O completion (keyed by request ID).
    IoCompletion(u64),
    /// Sleeping until absolute sim-time deadline.
    Time(Duration),
    /// Waiting to acquire a SimMutex/SimRwLock.
    Lock(LockId),
    /// Waiting for a channel message.
    Channel(ChannelId),
}

/// Composite key for the priority queue. Guarantees deterministic ordering.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct WakeKey {
    /// Sim-time when this task becomes eligible to run.
    wake_time_nanos: u64,
    /// Tiebreaker derived from seed: ensures deterministic pick among
    /// tasks waking at the same time. Computed as `hash(seed, task_id, step)`.
    tiebreak: u64,
}

pub struct DstScheduler {
    // --- Determinism ---
    rng: ChaChaRng,
    seed: u64,

    // --- Task state ---
    tasks: HashMap<TaskId, DstTask>,
    /// Ready tasks ordered by (wake_time, tiebreak). The front is next to run.
    ready_queue: BTreeMap<WakeKey, TaskId>,
    /// Blocked tasks with their block reason.
    blocked: HashMap<TaskId, BlockReason>,

    // --- I/O event queue ---
    /// Pending I/O completions: (completion_time_nanos, request_id, callback_info).
    io_events: BTreeMap<IoEventKey, IoEvent>,

    // --- Time ---
    clock: Arc<SimClock>,

    // --- Diagnostics ---
    /// Last step time per task (for starvation detection).
    last_run: HashMap<TaskId, u64>,
    /// Starvation threshold in sim-nanoseconds.
    starvation_threshold_nanos: u64,
    step_count: u64,
    next_task_id: u64,
    next_io_request_id: u64,
}
```

### 2.3 Core API

```rust
impl DstScheduler {
    /// Create a new scheduler with the given seed and clock.
    pub fn new(seed: u64, clock: Arc<SimClock>) -> Self {
        Self {
            rng: ChaChaRng::seed_from_u64(seed),
            seed,
            tasks: HashMap::new(),
            ready_queue: BTreeMap::new(),
            blocked: HashMap::new(),
            io_events: BTreeMap::new(),
            clock,
            last_run: HashMap::new(),
            starvation_threshold_nanos: Duration::from_secs(30).as_nanos() as u64,
            step_count: 0,
            next_task_id: 0,
            next_io_request_id: 0,
        }
    }

    /// Spawn a new task. Returns its TaskId. Task is immediately ready.
    pub fn spawn<F>(&mut self, name: impl Into<String>, step_fn: F) -> TaskId
    where
        F: FnMut(&mut TaskContext) -> TaskAction + 'static,
    {
        let id = TaskId(self.next_task_id);
        self.next_task_id += 1;
        let task = DstTask {
            id,
            name: name.into(),
            step_fn: Box::new(step_fn),
            context: TaskContext::new(id),
        };
        let key = self.make_wake_key(0); // wake_time = 0 = ready now
        self.tasks.insert(id, task);
        self.ready_queue.insert(key, id);
        self.last_run.insert(id, 0);
        id
    }

    /// Run the simulation until all tasks complete or deadlock is detected.
    pub fn run_to_completion(&mut self) -> SchedulerOutcome {
        loop {
            // 1. Check for starvation
            self.check_starvation();

            // 2. Drain I/O events whose completion time <= now
            self.drain_ready_io_events();

            // 3. Pick and run next ready task
            if let Some((_key, task_id)) = self.ready_queue.pop_first() {
                self.run_one_step(task_id);
                self.step_count += 1;
            } else if !self.io_events.is_empty() || self.has_time_blocked_tasks() {
                // 4. No ready tasks but pending events — jump time forward
                self.advance_to_next_event();
            } else if self.blocked.is_empty() {
                // 5. All tasks complete
                return SchedulerOutcome::AllComplete {
                    steps: self.step_count,
                    sim_time: self.clock.now(),
                };
            } else {
                // 6. Deadlock: all tasks blocked, no pending events
                return SchedulerOutcome::Deadlock {
                    blocked_tasks: self.blocked.iter()
                        .map(|(id, reason)| (*id, reason.clone()))
                        .collect(),
                    steps: self.step_count,
                    sim_time: self.clock.now(),
                };
            }
        }
    }

    /// Schedule an I/O completion event at a future sim-time.
    /// Returns a request ID that the blocked task waits on.
    pub fn schedule_io_completion(
        &mut self,
        complete_at: Duration,
        callback: IoCompletionCallback,
        context: *mut u8,
        status: IoStatus,
        bytes: u32,
    ) -> u64 {
        let request_id = self.next_io_request_id;
        self.next_io_request_id += 1;
        let key = IoEventKey {
            complete_at_nanos: complete_at.as_nanos() as u64,
            request_id,
        };
        self.io_events.insert(key, IoEvent {
            request_id,
            callback,
            context,
            status,
            bytes,
        });
        request_id
    }

    /// Unpark a task blocked on I/O completion or lock acquisition.
    pub fn unpark(&mut self, task_id: TaskId) {
        if self.blocked.remove(&task_id).is_some() {
            let key = self.make_wake_key(self.clock.now_nanos());
            self.ready_queue.insert(key, task_id);
        }
    }
}
```

### 2.4 Internal Mechanics

```rust
impl DstScheduler {
    /// Run one step of a task. Handles the returned TaskAction.
    fn run_one_step(&mut self, task_id: TaskId) {
        let now = self.clock.now_nanos();
        self.last_run.insert(task_id, now);

        let task = self.tasks.get_mut(&task_id).unwrap();
        let action = (task.step_fn)(&mut task.context);

        match action {
            TaskAction::Complete => {
                self.tasks.remove(&task_id);
                self.last_run.remove(&task_id);
            }
            TaskAction::Yield => {
                // Re-enqueue at current time (lets other same-time tasks interleave)
                let key = self.make_wake_key(now);
                self.ready_queue.insert(key, task_id);
            }
            TaskAction::Park(reason) => {
                match &reason {
                    BlockReason::Time(deadline) => {
                        // Will be unparked when clock reaches deadline
                        // (advance_to_next_event handles this)
                    }
                    BlockReason::IoCompletion(_) | BlockReason::Lock(_) | BlockReason::Channel(_) => {
                        // Unparked explicitly by unpark() when event/lock/channel resolves
                    }
                }
                self.blocked.insert(task_id, reason);
            }
        }
    }

    /// Generate a deterministic wake key with PRNG-derived tiebreak.
    fn make_wake_key(&mut self, wake_time_nanos: u64) -> WakeKey {
        use rand::Rng;
        WakeKey {
            wake_time_nanos,
            tiebreak: self.rng.gen(),
        }
    }

    /// Jump clock to the earliest pending event or time-blocked task.
    fn advance_to_next_event(&mut self) {
        let next_io = self.io_events.keys().next().map(|k| k.complete_at_nanos);
        let next_time_block = self.blocked.values().filter_map(|r| match r {
            BlockReason::Time(d) => Some(d.as_nanos() as u64),
            _ => None,
        }).min();

        let next = match (next_io, next_time_block) {
            (Some(a), Some(b)) => a.min(b),
            (Some(a), None) => a,
            (None, Some(b)) => b,
            (None, None) => return, // No events — caller detects deadlock
        };

        self.clock.set(Duration::from_nanos(next));
        self.drain_ready_io_events();
        self.unpark_time_blocked_tasks();
    }

    /// Fire I/O completion callbacks for events whose time has arrived.
    fn drain_ready_io_events(&mut self) {
        let now_nanos = self.clock.now_nanos();
        while let Some(entry) = self.io_events.first_entry() {
            if entry.key().complete_at_nanos > now_nanos {
                break;
            }
            let (_key, event) = entry.remove_entry();
            // Fire the callback (this is what transitions Flushing→Flushed in real code)
            // SAFETY: context pointer validity is the same contract as production Device usage.
            unsafe {
                (event.callback)(event.context, event.status, event.bytes);
            }
            // Unpark any task waiting on this request_id
            let waiting: Vec<TaskId> = self.blocked.iter()
                .filter(|(_, r)| matches!(r, BlockReason::IoCompletion(id) if *id == event.request_id))
                .map(|(tid, _)| *tid)
                .collect();
            for tid in waiting {
                self.unpark(tid);
            }
        }
    }

    /// Unpark tasks whose Time deadline has passed.
    fn unpark_time_blocked_tasks(&mut self) {
        let now = self.clock.now();
        let to_unpark: Vec<TaskId> = self.blocked.iter()
            .filter(|(_, r)| matches!(r, BlockReason::Time(d) if *d <= now))
            .map(|(tid, _)| *tid)
            .collect();
        for tid in to_unpark {
            self.unpark(tid);
        }
    }
}
```

### 2.5 Deadlock Detection

Deadlock is detected structurally, not heuristically:

```
DEADLOCK iff:
    ready_queue.is_empty()
    AND io_events.is_empty()
    AND no BlockReason::Time tasks exist
    AND blocked.is_not_empty()
```

This means every remaining task is waiting for a Lock, Channel, or IoCompletion that will never arrive. The scheduler reports the exact set of blocked tasks and their reasons.

**Diagnostic output on deadlock:**
```rust
pub struct DeadlockReport {
    pub blocked_tasks: Vec<(TaskId, String, BlockReason)>,  // (id, name, reason)
    pub steps_completed: u64,
    pub sim_time: Duration,
    pub last_10_events: Vec<TraceEvent>,  // From SimulationTrace
}
```

### 2.6 Starvation Detection

```rust
impl DstScheduler {
    fn check_starvation(&self) {
        let now = self.clock.now_nanos();
        for (task_id, last) in &self.last_run {
            if self.blocked.contains_key(task_id) {
                continue; // Blocked tasks can't be starved (they're not trying to run)
            }
            let gap = now.saturating_sub(*last);
            if gap > self.starvation_threshold_nanos {
                // Emit warning — not a hard failure, but indicates unfairness
                log::warn!(
                    "Starvation warning: task {:?} not scheduled for {:.3}s sim-time",
                    task_id,
                    Duration::from_nanos(gap).as_secs_f64()
                );
            }
        }
    }
}
```

Starvation threshold is configurable (default: 30s sim-time). It's a warning, not an assertion failure — some scenarios intentionally create unfairness to test liveness.

### 2.7 thread::spawn → Task Spawn Mapping

In `sync.rs` under `#[cfg(feature = "simulation")]`, `thread::spawn` routes to the scheduler:

```rust
// faster-core/src/sync.rs — simulation tier extension

#[cfg(all(not(loom), feature = "simulation"))]
pub(crate) mod thread {
    use std::time::Duration;

    /// In simulation, yield_now is a reschedule point.
    /// The harness intercepts this to return TaskAction::Yield.
    pub fn yield_now() {
        // Signal to the step function wrapper that we should yield.
        // Implementation: the step function checks a thread-local flag after
        // each FasterKv operation. If set, returns TaskAction::Yield.
        crate::sim_hooks::request_yield();
    }

    /// In simulation, sleep charges virtual time instead of blocking.
    pub fn sleep(duration: Duration) {
        crate::sim_hooks::request_sleep(duration);
    }

    // thread::spawn routing is handled at the harness level (see Section 4).
    // Inside simulation, FasterKv's background threads (grow, checkpoint) are
    // spawned as scheduler tasks by the DstRunner rather than calling std::thread::spawn.
}
```

---

## 3. SimDevice (Simulated I/O)

### 3.1 Design Goals

- Implement the existing `Device` trait — no changes to the trait itself
- Async completion model: `write_async` returns `Submitted` (not `CompletedSync`)
- Configurable queue depth, I/O latency, and failure rate
- Completions fire at scheduled sim-time via the DstScheduler
- `truncate_until` charges O(n) sim-time without real work

### 3.2 Configuration

```rust
// faster-dst/src/sim_device_v2.rs

use std::time::Duration;

/// Configuration for simulated I/O behavior.
#[derive(Clone, Debug)]
pub struct SimIoConfig {
    /// Maximum concurrent I/O operations. write_async returns QueueFull above this.
    pub queue_depth: usize,
    /// Base I/O latency (sim-time from submit to completion).
    pub io_latency: Duration,
    /// Random jitter ± added to base latency (seeded PRNG).
    pub io_jitter: Duration,
    /// Probability [0.0, 1.0] that a write fails with IoStatus::Error.
    pub write_failure_rate: f64,
    /// Probability [0.0, 1.0] that a read fails.
    pub read_failure_rate: f64,
    /// Sector size (must be power of 2).
    pub sector_size: u32,
    /// Segment size in bytes.
    pub segment_size: u64,
    /// Virtual cost per byte for truncate_until (models O(n) lock hold).
    pub truncate_ns_per_byte: u64,
}

impl Default for SimIoConfig {
    fn default() -> Self {
        Self {
            queue_depth: 64,
            io_latency: Duration::from_millis(1),
            io_jitter: Duration::from_micros(100),
            write_failure_rate: 0.0,
            read_failure_rate: 0.0,
            sector_size: 512,
            segment_size: 1 << 30, // 1 GiB
            truncate_ns_per_byte: 0, // No virtual cost by default (production: truncate is no-op)
        }
    }
}

impl SimIoConfig {
    /// Configuration that triggers Bug 1 pattern: small queue, moderate latency.
    pub fn bug1_pressure() -> Self {
        Self {
            queue_depth: 4,
            io_latency: Duration::from_millis(10),
            io_jitter: Duration::from_millis(5),
            ..Default::default()
        }
    }

    /// Configuration that triggers Bug 2 pattern: O(n) truncation cost.
    pub fn bug2_truncate_cost() -> Self {
        Self {
            truncate_ns_per_byte: 1, // 1 ns/byte → 1 second per GB
            ..Default::default()
        }
    }
}
```

### 3.3 Struct Definition

```rust
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use rand::SeedableRng;
use rand_chacha::ChaChaRng;

/// Pending I/O operation awaiting completion.
struct PendingIo {
    /// Sim-time when this I/O completes.
    complete_at_nanos: u64,
    /// Scheduler request ID for unparking the waiting task.
    request_id: u64,
    /// The Device callback to fire on completion.
    callback: IoCompletionCallback,
    /// Raw context pointer for the callback.
    context: *mut u8,
    /// Status to deliver (Success or injected Error).
    status: IoStatus,
    /// Bytes to report as transferred.
    bytes: u32,
}

/// Simulated storage device with async completion queue and pressure control.
///
/// Implements the `Device` trait from faster-core. All I/O is deferred to
/// the DstScheduler's event queue — completions fire at scheduled sim-time.
pub struct SimDevice {
    // --- Configuration (immutable after construction) ---
    config: SimIoConfig,

    // --- Shared state ---
    /// In-memory backing store (shared with harness for snapshot/restore).
    storage: Arc<SimulatedStorage>,
    /// Virtual clock (shared with scheduler).
    clock: Arc<SimClock>,

    // --- Mutable state (single-threaded, but Mutex for Device: Send + Sync) ---
    /// Deterministic PRNG for latency jitter and fault injection.
    rng: Mutex<ChaChaRng>,
    /// Pending I/O operations not yet completed.
    pending: Mutex<VecDeque<PendingIo>>,

    // --- Metrics ---
    submitted: AtomicU64,
    completed: AtomicU64,
    queue_full_count: AtomicU64,
}

impl SimDevice {
    pub fn new(
        config: SimIoConfig,
        storage: Arc<SimulatedStorage>,
        clock: Arc<SimClock>,
        seed: u64,
    ) -> Self {
        Self {
            config,
            storage,
            clock,
            rng: Mutex::new(ChaChaRng::seed_from_u64(seed)),
            pending: Mutex::new(VecDeque::new()),
            submitted: AtomicU64::new(0),
            completed: AtomicU64::new(0),
            queue_full_count: AtomicU64::new(0),
        }
    }
}
```

### 3.4 Device Trait Implementation

```rust
impl Device for SimDevice {
    fn sector_size(&self) -> u32 {
        self.config.sector_size
    }

    fn segment_size(&self) -> u64 {
        self.config.segment_size
    }

    fn max_outstanding_io(&self) -> u32 {
        self.config.queue_depth as u32
    }

    unsafe fn write_async(
        &self,
        source: *const u8,
        offset: u64,
        len: u32,
        callback: IoCompletionCallback,
        context: *mut u8,
    ) -> IoRequestResult {
        let mut pending = self.pending.lock().unwrap();

        // Back-pressure: reject if queue is full
        if pending.len() >= self.config.queue_depth {
            self.queue_full_count.fetch_add(1, Ordering::Relaxed);
            return IoRequestResult::QueueFull;
        }

        // Actually write data to backing store (real data, simulated timing)
        {
            let mut data = self.storage.data.write().unwrap();
            let end = offset as usize + len as usize;
            if data.len() < end {
                data.resize(end, 0);
            }
            // SAFETY: source is valid for len bytes per Device trait contract.
            unsafe {
                std::ptr::copy_nonoverlapping(source, data[offset as usize..end].as_mut_ptr(), len as usize);
            }
        }

        // Compute completion time with jitter
        let latency = self.compute_latency();
        let complete_at = self.clock.now_nanos() + latency.as_nanos() as u64;

        // Determine status (fault injection)
        let status = if self.should_fail_write() {
            IoStatus::Error(-5) // EIO
        } else {
            IoStatus::Success
        };

        pending.push_back(PendingIo {
            complete_at_nanos: complete_at,
            request_id: 0, // Set by scheduler integration
            callback,
            context,
            status,
            bytes: len,
        });

        self.submitted.fetch_add(1, Ordering::Relaxed);
        IoRequestResult::Submitted
    }

    unsafe fn read_async(
        &self,
        offset: u64,
        dest: *mut u8,
        len: u32,
        callback: IoCompletionCallback,
        context: *mut u8,
    ) -> IoRequestResult {
        // Reads complete synchronously (data is in memory) but still go through
        // the completion queue for deterministic callback ordering.
        let data = self.storage.data.read().unwrap();
        let end = (offset as usize + len as usize).min(data.len());
        let available = end.saturating_sub(offset as usize);
        // SAFETY: dest is valid for len bytes per Device trait contract.
        unsafe {
            if available > 0 {
                std::ptr::copy_nonoverlapping(
                    data[offset as usize..end].as_ptr(),
                    dest,
                    available,
                );
            }
            // Zero fill remainder (if reading past end of data)
            if available < len as usize {
                std::ptr::write_bytes(dest.add(available), 0, len as usize - available);
            }
        }
        drop(data);

        let status = if self.should_fail_read() {
            IoStatus::Error(-5)
        } else {
            IoStatus::Success
        };

        // Enqueue completion with ~zero latency (reads are from memory)
        let complete_at = self.clock.now_nanos() + 100; // 100ns nominal read latency
        let mut pending = self.pending.lock().unwrap();
        pending.push_back(PendingIo {
            complete_at_nanos: complete_at,
            request_id: 0,
            callback,
            context,
            status,
            bytes: len,
        });
        self.submitted.fetch_add(1, Ordering::Relaxed);
        IoRequestResult::Submitted
    }

    fn read_sync(&self, offset: u64, dest: &mut [u8]) -> io::Result<u32> {
        let data = self.storage.data.read().unwrap();
        let end = (offset as usize + dest.len()).min(data.len());
        let available = end.saturating_sub(offset as usize);
        dest[..available].copy_from_slice(&data[offset as usize..end]);
        if available < dest.len() {
            dest[available..].fill(0);
        }
        Ok(dest.len() as u32)
    }

    fn write_sync(&self, offset: u64, source: &[u8]) -> io::Result<u32> {
        let mut data = self.storage.data.write().unwrap();
        let end = offset as usize + source.len();
        if data.len() < end {
            data.resize(end, 0);
        }
        data[offset as usize..end].copy_from_slice(source);
        Ok(source.len() as u32)
    }

    fn truncate_until(&self, offset: u64) {
        // Do NOT zero memory. Instead, charge virtual time proportional to offset.
        // This models the O(n) lock-hold that caused Bug 2 without needing real data.
        let cost_nanos = offset * self.config.truncate_ns_per_byte;
        if cost_nanos > 0 {
            self.clock.charge(Duration::from_nanos(cost_nanos));
        }
    }

    fn poll_completions(&self) -> u32 {
        let now_nanos = self.clock.now_nanos();
        let mut pending = self.pending.lock().unwrap();
        let mut completed = 0u32;

        // Drain all completions whose time has arrived
        while let Some(front) = pending.front() {
            if front.complete_at_nanos > now_nanos {
                break;
            }
            let io = pending.pop_front().unwrap();
            // Fire the callback — this transitions Flushing→Flushed in real FASTER code
            // SAFETY: same contract as production Device — context pointer must be valid.
            unsafe {
                (io.callback)(io.context, io.status, io.bytes);
            }
            completed += 1;
            self.completed.fetch_add(1, Ordering::Relaxed);
        }
        completed
    }

    fn size(&self) -> u64 {
        self.storage.data.read().unwrap().len() as u64
    }

    fn close(&self) {
        // No-op for simulated device.
    }
}
```

### 3.5 Helper Methods

```rust
impl SimDevice {
    /// Compute I/O latency with deterministic jitter.
    fn compute_latency(&self) -> Duration {
        use rand::Rng;
        let mut rng = self.rng.lock().unwrap();
        let jitter_ns = if self.config.io_jitter.is_zero() {
            0i64
        } else {
            let max = self.config.io_jitter.as_nanos() as i64;
            rng.gen_range(-max..=max)
        };
        let base_ns = self.config.io_latency.as_nanos() as i64;
        Duration::from_nanos((base_ns + jitter_ns).max(1) as u64)
    }

    fn should_fail_write(&self) -> bool {
        if self.config.write_failure_rate <= 0.0 { return false; }
        use rand::Rng;
        self.rng.lock().unwrap().gen_bool(self.config.write_failure_rate)
    }

    fn should_fail_read(&self) -> bool {
        if self.config.read_failure_rate <= 0.0 { return false; }
        use rand::Rng;
        self.rng.lock().unwrap().gen_bool(self.config.read_failure_rate)
    }

    /// Metrics snapshot for invariant checking.
    pub fn metrics(&self) -> SimDeviceMetrics {
        SimDeviceMetrics {
            submitted: self.submitted.load(Ordering::Relaxed),
            completed: self.completed.load(Ordering::Relaxed),
            queue_full_count: self.queue_full_count.load(Ordering::Relaxed),
            pending_count: self.pending.lock().unwrap().len(),
            storage_bytes: self.storage.data.read().unwrap().len() as u64,
        }
    }
}

#[derive(Clone, Debug)]
pub struct SimDeviceMetrics {
    pub submitted: u64,
    pub completed: u64,
    pub queue_full_count: u64,
    pub pending_count: usize,
    pub storage_bytes: u64,
}
```

### 3.6 Data Flow: write_async Lifecycle

```
Writer task calls FasterKv::upsert()
  → allocator fills page → page sealed
  → maintenance() calls flush_sealed_pages()
    → flush_page() calls device.write_async(page_data, ...)
      ┌─────────────────────────────────────────────────────────┐
      │ SimDevice::write_async():                               │
      │  1. Check pending.len() >= queue_depth → QueueFull      │
      │  2. Copy data into backing store (real memcpy)          │
      │  3. Compute complete_at = now + latency ± jitter        │
      │  4. Push PendingIo to queue                             │
      │  5. Return Submitted                                    │
      └─────────────────────────────────────────────────────────┘

  → caller gets Submitted (page is in Flushing state)

Later, maintenance() or retry loop calls device.poll_completions():
      ┌─────────────────────────────────────────────────────────┐
      │ SimDevice::poll_completions():                          │
      │  1. Check pending queue front: complete_at <= now?      │
      │  2. Pop and fire callback(context, Success, bytes)      │
      │     → callback CASes page Flushing→Flushed              │
      │  3. Return count of completed I/Os                      │
      └─────────────────────────────────────────────────────────┘
```

**Why this catches Bug 1:** With `queue_depth=4` and 8 writers filling pages fast, the queue fills up. `write_async` returns `QueueFull`, flush reverts `Flushing→Sealed`. If the retry loop doesn't call `poll_completions()` to drain the queue, no pages ever reach `Flushed`, head never advances, writers are permanently stuck.

---

## 4. Scenario + Seed Framework

### 4.1 ScenarioConfig

```rust
// faster-dst/src/concurrency/config.rs

use std::time::Duration;

/// Complete configuration for a concurrency simulation scenario.
#[derive(Clone, Debug)]
pub struct ScenarioConfig {
    // --- Workload shape ---
    /// Number of writer tasks.
    pub writers: usize,
    /// Number of reader tasks (0 for write-only workloads).
    pub readers: usize,
    /// Operations per writer before completing.
    pub ops_per_writer: usize,

    // --- Resource configuration ---
    /// FasterKv buffer size in pages.
    pub buffer_pages: usize,
    /// Enable lossy mode (eviction with truncation).
    pub lossy: bool,
    /// Mutable fraction for the hybrid log.
    pub mutable_fraction: f64,

    // --- I/O configuration ---
    pub io_config: SimIoConfig,

    // --- Simulation bounds ---
    /// Maximum virtual time before the scenario is aborted.
    pub duration_sim_secs: u64,
    /// PRNG seed. Fully determines the execution.
    pub seed: u64,
}

impl Default for ScenarioConfig {
    fn default() -> Self {
        Self {
            writers: 4,
            readers: 0,
            ops_per_writer: 10_000,
            buffer_pages: 16,
            lossy: false,
            mutable_fraction: 0.5,
            io_config: SimIoConfig::default(),
            duration_sim_secs: 60,
            seed: 0,
        }
    }
}
```

### 4.2 Assertions

```rust
// faster-dst/src/concurrency/assertions.rs

use std::time::Duration;

/// Assertions that must hold for a scenario to pass.
#[derive(Clone, Debug)]
pub struct ScenarioAssertions {
    /// Minimum sustained throughput (ops/sec) in any 1-second sim-time window.
    /// Violation indicates a throughput cliff (Bug 1 pattern).
    pub min_throughput: Option<u64>,

    /// Maximum duration of zero operations.
    /// Violation indicates a stall or deadlock.
    pub max_stall_duration: Option<Duration>,

    /// Maximum memory usage in bytes (backing store size).
    /// Violation indicates unbounded growth.
    pub memory_bound: Option<u64>,

    /// All writer tasks must complete within the sim-time budget.
    /// Default: true. Failure = deadlock or livelock.
    pub all_writers_complete: bool,

    /// Maximum latency for any single maintenance() call (sim-time).
    /// Violation indicates O(n) lock hold (Bug 2 pattern).
    pub max_maintenance_latency: Option<Duration>,
}

impl Default for ScenarioAssertions {
    fn default() -> Self {
        Self {
            min_throughput: Some(100), // Very conservative default
            max_stall_duration: Some(Duration::from_secs(5)),
            memory_bound: None,
            all_writers_complete: true,
            max_maintenance_latency: None,
        }
    }
}
```

### 4.3 DstRunner

```rust
// faster-dst/src/concurrency/runner.rs

use super::{ScenarioConfig, ScenarioAssertions, SimDevice, SimClock};
use std::sync::Arc;

/// Outcome of a simulation run.
#[derive(Debug)]
pub enum RunOutcome {
    Pass {
        stats: ExecutionStats,
    },
    Fail {
        assertion: String,
        stats: ExecutionStats,
        trace_tail: Vec<TraceEvent>,
    },
    Deadlock {
        blocked_tasks: Vec<(TaskId, String, BlockReason)>,
        stats: ExecutionStats,
        trace_tail: Vec<TraceEvent>,
    },
}

/// Execution statistics collected during the run.
#[derive(Clone, Debug)]
pub struct ExecutionStats {
    pub total_ops: u64,
    pub sim_duration: Duration,
    pub wall_duration: Duration,
    pub ops_per_sec_timeline: Vec<(Duration, f64)>,
    pub min_throughput_1s: f64,
    pub max_stall: Duration,
    pub queue_full_events: u64,
    pub device_metrics: SimDeviceMetrics,
}

/// Runs a concurrency scenario with full deterministic simulation.
pub struct DstRunner {
    config: ScenarioConfig,
    assertions: ScenarioAssertions,
}

impl DstRunner {
    pub fn new(config: ScenarioConfig, assertions: ScenarioAssertions) -> Self {
        Self { config, assertions }
    }

    /// Execute the scenario. Returns Pass/Fail/Deadlock.
    pub fn run(&self) -> RunOutcome {
        let wall_start = std::time::Instant::now();

        // 1. Build simulation infrastructure
        let clock = Arc::new(SimClock::new());
        let storage = SimulatedStorage::new();
        let device = SimDevice::new(
            self.config.io_config.clone(),
            storage.clone(),
            clock.clone(),
            self.config.seed,
        );

        // 2. Build FasterKv with SimDevice
        let kv = FasterKvBuilder::new()
            .hash_table_size(1 << 14) // 16K buckets (small for test)
            .log_device(Box::new(device))
            .buffer_size(self.config.buffer_pages)
            .mutable_fraction(self.config.mutable_fraction)
            .build()
            .expect("FasterKv construction failed");

        // 3. Create scheduler
        let mut scheduler = DstScheduler::new(self.config.seed, clock.clone());

        // 4. Spawn writer tasks
        let stats_collector = Arc::new(StatsCollector::new());
        for writer_id in 0..self.config.writers {
            let kv_ref = &kv;
            let ops = self.config.ops_per_writer;
            let stats = stats_collector.clone();
            let seed = self.config.seed.wrapping_add(writer_id as u64);

            scheduler.spawn(format!("writer-{writer_id}"), move |ctx| {
                // Each step does one upsert + optional maintenance
                let step = ctx.get_local::<usize>().copied().unwrap_or(0);
                if step >= ops {
                    return TaskAction::Complete;
                }

                let session = ctx.get_local::<FasterSession>()
                    .expect("session must be installed by harness");

                // Key derivation: writer_id * stride + step (deterministic, seed-independent)
                let key = (writer_id as u64) * 1_000_000 + step as u64;
                let value = step as u64;

                match session.upsert(&key, &value) {
                    Ok(_) => {
                        stats.record_op(clock.now());
                    }
                    Err(_) => {
                        // Operation aborted (buffer full, retries exhausted)
                        stats.record_abort(clock.now());
                    }
                }

                // Periodic maintenance (every 100 ops)
                if step % 100 == 0 {
                    session.maintenance();
                }

                ctx.set_local(step + 1);
                TaskAction::Yield // Let other tasks interleave
            });
        }

        // 5. Spawn maintenance task
        scheduler.spawn("maintenance".into(), move |ctx| {
            kv.maintenance();
            TaskAction::Yield
        });

        // 6. Run to completion
        let outcome = scheduler.run_to_completion();
        let wall_duration = wall_start.elapsed();

        // 7. Collect stats and check assertions
        let stats = stats_collector.finalize(clock.now(), wall_duration);
        match outcome {
            SchedulerOutcome::AllComplete { .. } => {
                self.check_assertions(&stats)
            }
            SchedulerOutcome::Deadlock { blocked_tasks, .. } => {
                RunOutcome::Deadlock {
                    blocked_tasks: blocked_tasks.iter()
                        .map(|(id, reason)| (*id, scheduler.task_name(*id), reason.clone()))
                        .collect(),
                    stats,
                    trace_tail: scheduler.trace().last_n(20),
                }
            }
        }
    }

    fn check_assertions(&self, stats: &ExecutionStats) -> RunOutcome {
        if let Some(min) = self.assertions.min_throughput {
            if stats.min_throughput_1s < min as f64 {
                return RunOutcome::Fail {
                    assertion: format!(
                        "min_throughput: expected >= {min} ops/s, got {:.0} ops/s",
                        stats.min_throughput_1s
                    ),
                    stats: stats.clone(),
                    trace_tail: vec![], // Filled by caller
                };
            }
        }
        if let Some(max_stall) = self.assertions.max_stall_duration {
            if stats.max_stall > max_stall {
                return RunOutcome::Fail {
                    assertion: format!(
                        "max_stall: expected <= {:?}, got {:?}",
                        max_stall, stats.max_stall
                    ),
                    stats: stats.clone(),
                    trace_tail: vec![],
                };
            }
        }
        if let Some(bound) = self.assertions.memory_bound {
            if stats.device_metrics.storage_bytes > bound {
                return RunOutcome::Fail {
                    assertion: format!(
                        "memory_bound: expected <= {} bytes, got {} bytes",
                        bound, stats.device_metrics.storage_bytes
                    ),
                    stats: stats.clone(),
                    trace_tail: vec![],
                };
            }
        }
        RunOutcome::Pass { stats: stats.clone() }
    }
}
```

### 4.4 Seed Strategy

```rust
// faster-dst/src/concurrency/seeds.rs

/// Seed classes for the concurrency test suite.
pub mod regression {
    /// Seeds that reproduce known bugs. NEVER remove these.
    /// Each entry: (seed, scenario_name, bug_description, commit_sha)
    pub const REGRESSION_SEEDS: &[(u64, &str, &str, &str)] = &[
        // Populated as bugs are found. Example:
        // (0xDEAD_BEEF_42, "pipeline_deadlock", "Bug 1: QueueFull breaks flush", "abc123"),
    ];
}

pub mod canary {
    /// Fixed seeds chosen to exercise specific code paths.
    pub const CANARY_SEEDS: &[u64] = &[
        0x0000_0000_0000_0001,  // Minimal: tests basic scheduling
        0xFFFF_FFFF_FFFF_FFFF,  // Edge: max seed value
        0x5555_5555_5555_5555,  // Alternating bits
        0x0000_DEAD_0000_BEEF,  // Arbitrary but stable
        0x1234_5678_9ABC_DEF0,  // Sequential digits
    ];
}

/// Generate exploration seeds from a root seed.
pub fn exploration_seeds(root_seed: u64, count: usize) -> Vec<u64> {
    use rand::SeedableRng;
    use rand::Rng;
    use rand_chacha::ChaChaRng;
    let mut rng = ChaChaRng::seed_from_u64(root_seed);
    (0..count).map(|_| rng.gen()).collect()
}
```

### 4.5 Catching Bug 1 (Pipeline Deadlock)

```rust
#[test]
fn dst_bug1_pipeline_deadlock() {
    // This scenario creates the conditions for Bug 1:
    // - Many writers filling pages rapidly
    // - Small I/O queue that saturates quickly
    // - Writers must make progress despite QueueFull
    let config = ScenarioConfig {
        writers: 8,
        ops_per_writer: 50_000,
        buffer_pages: 16,
        lossy: false,
        io_config: SimIoConfig {
            queue_depth: 4,         // Small queue → frequent QueueFull
            io_latency: Duration::from_millis(10),  // Slow I/O → queue stays full longer
            io_jitter: Duration::from_millis(5),
            ..SimIoConfig::default()
        },
        seed: 0xBUG1_SEED_001,
        duration_sim_secs: 120,
        ..ScenarioConfig::default()
    };

    let assertions = ScenarioAssertions {
        // Bug 1 caused throughput to drop from 200K to 14 ops/s.
        // Any sustained throughput above 100 ops/s proves the pipeline isn't stuck.
        min_throughput: Some(100),
        // Bug 1 caused permanent stall. 10s max stall is generous but catches it.
        max_stall_duration: Some(Duration::from_secs(10)),
        all_writers_complete: true,
        ..ScenarioAssertions::default()
    };

    let outcome = DstRunner::new(config, assertions).run();
    match outcome {
        RunOutcome::Pass { stats } => {
            // Verify QueueFull actually occurred (scenario exercised the path)
            assert!(stats.queue_full_events > 0,
                "Scenario didn't trigger QueueFull — increase writers or decrease queue_depth");
        }
        RunOutcome::Fail { assertion, .. } => {
            panic!("Pipeline progress assertion failed: {assertion}");
        }
        RunOutcome::Deadlock { blocked_tasks, .. } => {
            panic!("DEADLOCK detected with {} blocked tasks: {blocked_tasks:?}",
                blocked_tasks.len());
        }
    }
}
```

### 4.6 Catching Bug 2 (Lossy Starvation)

```rust
#[test]
fn dst_bug2_lossy_truncate_starvation() {
    // This scenario creates the conditions for Bug 2:
    // - Lossy mode with eviction + truncation
    // - Virtual truncation cost models O(n) lock hold
    // - Writers must continue despite increasing truncation cost
    let config = ScenarioConfig {
        writers: 4,
        ops_per_writer: 100_000,
        buffer_pages: 8,            // Small buffer → frequent eviction
        lossy: true,                // Enables truncation path
        mutable_fraction: 0.2,     // Aggressive flush trigger
        io_config: SimIoConfig {
            queue_depth: 32,
            io_latency: Duration::from_millis(1),
            // Key: model O(n) truncation cost (1 ns/byte → 1s per GB written)
            truncate_ns_per_byte: 1,
            ..SimIoConfig::default()
        },
        seed: 0xBUG2_SEED_001,
        duration_sim_secs: 300,     // Long enough for truncation cost to grow
        ..ScenarioConfig::default()
    };

    let assertions = ScenarioAssertions {
        // Bug 2 caused 0 ops/s after ~200s.
        min_throughput: Some(50),
        // Bug 2 stalls grew to multi-second, then permanent.
        max_stall_duration: Some(Duration::from_secs(15)),
        // Bug 2 caused 13 GB RSS. Bound memory to catch unbounded growth.
        memory_bound: Some(512 * 1024 * 1024), // 512 MB
        // Bug 2 caused maintenance() to take O(n) seconds.
        max_maintenance_latency: Some(Duration::from_secs(2)),
        all_writers_complete: true,
        ..ScenarioAssertions::default()
    };

    let outcome = DstRunner::new(config, assertions).run();
    match outcome {
        RunOutcome::Pass { stats } => {
            // Verify truncation path was actually exercised
            assert!(stats.sim_duration > Duration::from_secs(30),
                "Scenario too short to accumulate truncation cost");
        }
        RunOutcome::Fail { assertion, .. } => {
            panic!("Lossy starvation assertion failed: {assertion}");
        }
        RunOutcome::Deadlock { .. } => {
            panic!("DEADLOCK in lossy eviction path");
        }
    }
}
```

### 4.7 Campaign Runner

```rust
// faster-dst/src/concurrency/campaign.rs

/// Run a concurrency campaign across multiple seeds and scenarios.
pub fn run_concurrency_campaign(
    scenarios: &[(&str, ScenarioConfig, ScenarioAssertions)],
    seeds: &[u64],
) -> CampaignReport {
    let mut report = CampaignReport::default();

    for (name, base_config, assertions) in scenarios {
        for &seed in seeds {
            let mut config = base_config.clone();
            config.seed = seed;

            let outcome = DstRunner::new(config.clone(), assertions.clone()).run();
            report.total += 1;

            match outcome {
                RunOutcome::Pass { .. } => report.passed += 1,
                RunOutcome::Fail { assertion, stats, .. } => {
                    report.failures.push(CampaignFailure {
                        scenario: name.to_string(),
                        seed,
                        assertion,
                        stats,
                    });
                }
                RunOutcome::Deadlock { blocked_tasks, stats, .. } => {
                    report.failures.push(CampaignFailure {
                        scenario: name.to_string(),
                        seed,
                        assertion: format!("DEADLOCK: {} tasks blocked", blocked_tasks.len()),
                        stats,
                    });
                }
            }
        }
    }
    report
}
```

---

## 5. Integration Points

### 5.1 Feature Flag

The existing feature flag is `simulation` on `faster-core` (Cargo.toml line 45). DST v2.0 uses this same flag — no new feature needed.

```toml
# faster-core/Cargo.toml (no change needed — already exists)
[features]
simulation = []
```

The `faster-dst` crate enables it as a dependency feature:

```toml
# faster-dst/Cargo.toml
[dependencies]
faster-core = { path = "../faster-core", features = ["simulation"] }
rand_chacha = "0.3"
```

Gate: `#[cfg(feature = "simulation")]` on `faster-core` internals; no feature gate on `faster-dst` code (it's always a test crate).

### 5.2 sync.rs Changes

The existing `sync.rs` has three tiers. Changes needed for DST v2.0 in the **simulation tier only**:

#### 5.2.1 Time: Replace Instant/SystemTime

```rust
// faster-core/src/sync.rs — replace simulation time block (lines 88-89)

// Current:
// #[cfg(feature = "simulation")]
// pub(crate) use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

// New:
#[cfg(feature = "simulation")]
pub(crate) use std::time::Duration;
#[cfg(feature = "simulation")]
pub(crate) use crate::sim_time::SimInstant as Instant;
// SystemTime: keep std for now (only used in checkpoint metadata, not timing-critical)
#[cfg(feature = "simulation")]
pub(crate) use std::time::{SystemTime, UNIX_EPOCH};
```

#### 5.2.2 Thread: Intercept yield_now and sleep

```rust
// faster-core/src/sync.rs — replace simulation thread block (line 67)

// Current:
// #[cfg(all(not(loom), feature = "simulation"))]
// pub(crate) use std::thread;

// New:
#[cfg(all(not(loom), feature = "simulation"))]
pub(crate) mod thread {
    pub use std::thread::{JoinHandle, Thread, current, panicking, park, park_timeout};

    /// Under simulation, yield_now signals a reschedule point.
    pub fn yield_now() {
        crate::sim_hooks::on_yield();
    }

    /// Under simulation, sleep charges virtual time.
    pub fn sleep(duration: std::time::Duration) {
        crate::sim_hooks::on_sleep(duration);
    }

    /// Under simulation, spawn registers a task with the scheduler.
    /// In Phase 1 (real threads), this falls through to std::thread::spawn.
    /// In Phase 2 (green threads), this routes to the DstScheduler.
    pub fn spawn<F, T>(f: F) -> JoinHandle<T>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        // Phase 1: use real threads (determinism comes from I/O timing only)
        std::thread::spawn(f)
        // Phase 2 (future): route to scheduler
        // crate::sim_hooks::spawn_task(f)
    }
}
```

#### 5.2.3 Mutex/RwLock: Instrumented (Phase 2 Preparation)

For Phase 1, keep real `Mutex`/`RwLock` (existing code, no changes). Phase 2 will replace them:

```rust
// Phase 2 (future, not in initial PR):
// #[cfg(all(not(loom), feature = "simulation"))]
// pub(crate) use crate::sim_sync::{SimMutex as Mutex, SimRwLock as RwLock, SimRwLockReadGuard as RwLockReadGuard};
```

Phase 2 `SimMutex` sketch (for reference, not part of Phase 1 deliverable):

```rust
/// Scheduler-aware mutex. On contention, parks the current task
/// instead of blocking the OS thread.
pub struct SimMutex<T> {
    inner: std::cell::UnsafeCell<T>,
    held_by: std::cell::Cell<Option<TaskId>>,
    waiters: std::cell::RefCell<VecDeque<TaskId>>,
}

impl<T> SimMutex<T> {
    pub fn lock(&self) -> SimMutexGuard<'_, T> {
        let current_task = sim_hooks::current_task_id();
        if self.held_by.get().is_some() {
            // Park this task — scheduler will wake us when the lock is released
            self.waiters.borrow_mut().push_back(current_task);
            sim_hooks::park(BlockReason::Lock(self.id()));
            // When we wake up, we hold the lock
        }
        self.held_by.set(Some(current_task));
        SimMutexGuard { mutex: self }
    }
}

impl<T> Drop for SimMutexGuard<'_, T> {
    fn drop(&mut self) {
        self.mutex.held_by.set(None);
        // Wake next waiter
        if let Some(waiter) = self.mutex.waiters.borrow_mut().pop_front() {
            sim_hooks::unpark(waiter);
        }
    }
}
```

### 5.3 Device: No Trait Changes

`SimDevice` implements the existing `Device` trait as-is. No modifications to `device.rs` are needed. The trait already provides:

| Method | SimDevice Behavior |
|--------|-------------------|
| `write_async()` | Returns `Submitted` (not `CompletedSync`); defers callback |
| `read_async()` | Returns `Submitted`; reads from backing store |
| `poll_completions()` | Drains pending queue, fires callbacks at sim-time |
| `max_outstanding_io()` | Returns `config.queue_depth` |
| `truncate_until()` | Charges O(n) sim-time via `clock.charge()` |

The only `faster-core` change is adding `sim_time.rs` and `sim_hooks.rs` modules (gated behind `#[cfg(feature = "simulation")]`).

### 5.4 sim_hooks.rs (New Module)

```rust
// faster-core/src/sim_hooks.rs
// Compiled only under #[cfg(feature = "simulation")]

use std::time::Duration;

/// Thread-local simulation hook state.
std::thread_local! {
    static YIELD_REQUESTED: std::cell::Cell<bool> = std::cell::Cell::new(false);
    static SLEEP_REQUESTED: std::cell::Cell<Option<Duration>> = std::cell::Cell::new(None);
}

/// Called from sync::thread::yield_now() under simulation.
pub fn on_yield() {
    YIELD_REQUESTED.with(|f| f.set(true));
}

/// Called from sync::thread::sleep() under simulation.
pub fn on_sleep(duration: Duration) {
    SLEEP_REQUESTED.with(|f| f.set(Some(duration)));
}

/// Check and clear the yield flag. Called by DstRunner's task step wrapper.
pub fn take_yield_request() -> bool {
    YIELD_REQUESTED.with(|f| f.replace(false))
}

/// Check and clear the sleep request. Called by DstRunner's task step wrapper.
pub fn take_sleep_request() -> Option<Duration> {
    SLEEP_REQUESTED.with(|f| f.take())
}

/// Request a yield from the current task. Used by buggify! injection points.
pub fn request_yield() {
    on_yield();
}

/// Request a sleep for the current task. Used for virtual time modeling.
pub fn request_sleep(duration: Duration) {
    on_sleep(duration);
}
```

### 5.5 CI Integration

```bash
# PR gate (every PR, ~5 min):
cargo test --features simulation -p faster-dst --test dst_concurrency \
    -- --regression-seeds --canary-seeds --explore 20

# Nightly extended (~30 min):
cargo test --features simulation -p faster-dst --test dst_concurrency \
    -- --regression-seeds --canary-seeds --explore 200

# Single seed reproduction (developer workflow):
cargo test --features simulation -p faster-dst --test dst_concurrency \
    -- --seed 0xDEADBEEF42

# Full campaign smoke test:
cargo test --features simulation -p faster-dst --test dst_campaign \
    -- --scenarios concurrency --seeds 0..100
```

**Test file structure:**

```
faster-dst/
  tests/
    dst_concurrency.rs       # Concurrency scenario tests (Bug 1, Bug 2, etc.)
    dst_campaign.rs          # Campaign runner integration tests
  src/
    concurrency/
      mod.rs                 # Module root
      config.rs              # ScenarioConfig, SimIoConfig
      assertions.rs          # ScenarioAssertions
      runner.rs              # DstRunner
      seeds.rs               # Regression/canary/exploration seeds
      campaign.rs            # Campaign runner
    scheduler_v2.rs          # DstScheduler with priority queue
    sim_device_v2.rs         # SimDevice with async completions
    clock.rs                 # SimClock (extended)
```

### 5.6 What Does NOT Change

| Component | Why No Change |
|-----------|--------------|
| `Device` trait (`device.rs:229-302`) | SimDevice implements it as-is |
| `InMemoryDevice`, `NullDevice` | Untouched; SimDevice is a new impl |
| Existing 1003 DST crash-recovery scenarios | Different scenario family; no interaction |
| Existing 25 loom tests | Different feature flag (`loom` vs `simulation`) |
| Production code paths (no `simulation` feature) | All changes behind `#[cfg(feature = "simulation")]` |
| `Cargo.toml` features | `simulation = []` already exists |

---

*— Éowyn, Deterministic Simulation Testing Expert*
*"The sword that was broken shall be reforged — and so shall our confidence in pipeline progress."*
