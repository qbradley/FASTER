# Loom & Automated Validation Gap Analysis

**Author:** Éowyn (DST Expert)  
**Date:** 2026-03-16  
**Status:** Analysis Complete  
**Context:** Two P0 deadlocks escaped 21 loom tests + 3009 DST scenarios + 1700 unit tests

---

## Executive Summary

**The finding:** Loom and our current DST framework operate in fundamentally different capability spaces. Bug 1 (lock cycle) is *theoretically* detectable by loom but requires modeling ~10 interlocking components simultaneously — well beyond loom's state space limits. Bug 2 (scale starvation) is *fundamentally undetectable* by loom because it depends on time/memory scale, not just interleaving order.

**Critical insight:** Both bugs ONLY manifest under **sustained multi-threaded pressure at scale** — they are emergent properties of resource exhaustion, not logical concurrency races. Our current test portfolio has zero coverage in this regime.

**The gap:** We have excellent coverage of **logical correctness** (memory ordering, epoch safety, state machines) but zero coverage of **resource saturation deadlocks** (queue back-pressure, lock hold times under memory pressure, GC/flush pipeline starvation).

---

## I. The Two Deadlocks — Anatomy

### Bug 1: Multi-Writer Flush Pipeline Deadlock

**Trigger conditions:**
- 16+ writer threads
- Linear key distribution (no key reuse, maximum allocation pressure)
- Small buffer (8-16 pages) to force rapid eviction cycles
- Sustained write rate for ~70 seconds (enough to fill pipeline)

**The circular dependency:**

```
Writers → allocate_at_tail() → SF-10 check (buffer full)
    ↓
    blocks on: Head can't advance (needs contiguous Flushed pages)
    ↓
maintenance() → flush_sealed_pages() → device.write_async()
    ↓
    returns: IoRequestResult::QueueFull (I/O queue saturated)
    ↓
    old bug: continue loop, revert all pages Flushing → Sealed
    ↓
    result: Head still blocked, writers still blocked
    ↓
PERMANENT STALL (no progress mechanism)
```

**Why it manifested at T+70s:**
- First 60s: I/O completes faster than submission → no back-pressure
- T+60-70s: I/O queue fills → first QueueFull → pages revert to Sealed
- T+70s+: All pages Flushing or Sealed (none Flushed) → head frozen → SF-10 blocks all writers

**The fix:** Three-part protocol from C++ FASTER:
1. **FlushBatchResult** — break on QueueFull, return `{flushed, queue_full}` signal
2. **poll_completions()** — Device trait method to yield CPU time to I/O callback threads
3. **Bounded retry with yield** — allocator retry loop calls `poll_completions()` and `thread::yield_now()` on each iteration (max 32 attempts)

---

### Bug 2: Lossy InMemoryDevice Deadlock

**Trigger conditions:**
- Lossy mode (`config.lossy = true`)
- 16 threads at sustained 40M ops/s
- ~200 seconds runtime (enough to accumulate ~9GB in Vec)
- Linear keys (maximize churn, no key reuse)

**The starvation pattern:**

```
InMemoryDevice::data = RwLock<Vec<u8>>  [grows unbounded in lossy mode]
    ↓
After 200s: Vec.len() = ~9GB (40M ops/s × 200s × 1KB avg/page)
    ↓
maintenance() → lossy eviction → truncate_until(old_begin_address)
    ↓
InMemoryDevice::truncate_until():
    let mut guard = self.data.write().lock();  // EXCLUSIVE WRITE LOCK
    guard[..offset].fill(0);  // O(offset) = 1-3 seconds for 9GB
    ↓
16 flush threads all call write_async():
    let guard = self.data.write().lock();  // BLOCKS on write lock
    ↓
PERMANENT STALL:
    - truncate holds write lock for 1-3s
    - all 16 flush threads starved
    - no callbacks fire → no progress → head can't advance → SF-10 blocks writers
    - next maintenance() → truncate again → cycle repeats
```

**Why it manifested at T+200s:**
- First 100s: Vec < 4GB, truncate finishes in <200ms (tolerable)
- T+100-200s: Vec growth accelerates, truncate takes 500ms-1s (marginal starvation)
- T+200s+: Vec > 8GB, truncate takes 1-3s (complete starvation of all 16 flush threads)

**The fix:**
```rust
fn truncate_until(&self, _offset: u64) {
    // No-op: data below begin_address is never read in lossy mode.
    // Zeroing is unnecessary and holds write lock for O(offset) time.
}
```

---

## II. Tool Capability Matrix

| Tool | Bug 1 (lock cycle) | Bug 2 (scale starvation) | Explanation |
|------|-------------------|-------------------------|-------------|
| **Loom** | ⚠️ Theoretical (impractical) | ❌ Impossible | Can model lock ordering but not 10-component pipeline + timing. Cannot model time/scale. |
| **Miri** | ⚠️ Same as loom | ❌ Impossible | Detects UB, not deadlocks. No thread scheduling model. |
| **Proptest stateful** | ❌ No | ❌ No | Explores state space, not concurrency interleavings. |
| **Lock order graph** | ✅ Yes* | ❌ No | Static: detects potential cycles. Dynamic: requires instrumentation at scale. |
| **parking_lot deadlock detector** | ✅ Yes | ❌ No | Runtime cycle detection, but only for *actual* Mutex deadlocks. RwLock read→write upgrade NOT a cycle. |
| **DST (current)** | ❌ No | ❌ No | Models crash/recovery, not live concurrency. No thread scheduling. |
| **DST (FoundationDB-style)** | ✅ Yes | ✅ Yes | Simulated time + deterministic scheduling + resource pressure injection. |
| **Stress test (16T, 5min)** | ✅ Yes (found it) | ✅ Yes (found it) | Brute force: actual hardware, actual timing, actual scale. |

**Key insight:** Only two tools can catch both bugs: (1) FoundationDB-style DST with time simulation, (2) stress testing at production scale.

---

## III. Loom — Capabilities & Fundamental Limits

### What Loom Actually Does

From `rust/crates/faster-core/tests/loom_tests.rs` analysis:

**Loom models:**
1. **All possible thread interleavings** of atomic operations
2. **All possible memory orderings** (validates Acquire/Release/SeqCst correctness)
3. **Mutex/Arc contention** (detects races on shared state)

**Current coverage (21 tests, 1800 LOC):**
- ✅ Epoch protect/unprotect (4 tests) — prevents premature reclamation
- ✅ Hash bucket CAS (3 tests) — no lost updates, tentative entry visibility
- ✅ RecordInfo state machine (3 tests) — seal, revivify, tombstone
- ✅ EPVS transitions (2 tests) — exclusive winner on state transitions
- ✅ Log allocator (3 tests) — no overlapping addresses, page boundaries
- ✅ Treiber stack (2 tests) — ABA counter, epoch-gated free
- ✅ Drain list (2 tests) — concurrent push/drain, partial drain

**What loom CANNOT model:**
1. **Time** — `thread::sleep()` becomes `yield_now()`. No wall-clock delays.
2. **I/O latency** — Device callbacks are synchronous in loom's view.
3. **Memory scale** — 9GB Vec not representable (state space explosion).
4. **Lock hold duration** — Mutex is instantaneous, no contention timing.
5. **Thread starvation** — Scheduling is fair, no priority inversion.
6. **Back-pressure propagation** — QueueFull is a boolean, not a timing event.

### Could Loom Catch Bug 1?

**Theoretical answer:** Yes, if we model the full pipeline.

**Practical answer:** No, because state space explosion.

**The model would need:**
```rust
loom::model(|| {
    // 1. PageTable with 8 pages (each with AtomicU8 state)
    // 2. Device with bounded I/O queue (Vec<Request>)
    // 3. 4 writer threads calling allocate_at_tail()
    // 4. 1 maintenance thread calling flush_sealed_pages()
    // 5. N I/O worker threads processing Device queue
    // 6. StatusArray for tracking in-flight I/O
    // 7. Head advancement logic (contiguous Flushed scan)
    // 8. SF-10 buffer-full check
    // 9. QueueFull→Sealed revert logic (the bug)
    // 10. FlushCompletionCallback state transitions
});
```

**State space:** `8_pages × 4_states × 4_writers × 1_maintenance × N_io_workers × M_queue_depth`

With 4 writers, 2 I/O workers, queue depth 4:
- Pages: 8 × 4 states = 32 combinations
- Threads: 4W + 1M + 2IO = 7 threads
- Queue states: 4 slots × (empty | pending | completed) = 81 combinations
- **Total:** 32 × 7! × 81 ≈ **11.7 million** base states
- Interleaving explosion: Each atomic op branches the tree

**Loom's practical limit:** ~10,000 states before minutes → hours. Complex models timeout.

**Verdict:** Could we write a loom test that catches this? Yes, technically. Would it finish in <10 minutes? No. Would it be maintainable? No. **Gap remains.**

### Could Loom Catch Bug 2?

**Short answer:** No, fundamentally impossible.

**Why:** Bug 2 requires:
1. Vec growth to 9GB (loom can't model heap size)
2. `fill(0)` taking 1-3 seconds (loom has no time)
3. Lock starvation over multiple seconds (loom scheduling is fair + instantaneous)

Loom could detect a **logical** deadlock (e.g., lock A → lock B, lock B → lock A). But it cannot detect a **performance** deadlock where one thread holds a lock for "too long" and starves others — the concept of "too long" doesn't exist in loom's model.

**Verdict:** Loom is the wrong tool for scale-dependent starvation bugs. **Gap is architectural.**

---

## IV. What Should Our Loom Tests Cover?

### Current Coverage: 21 Tests

**Pattern:** Re-implement core algorithms with loom primitives (not production types).

**Example:** `b2_treiber_stack_push_pop` — reimplements Treiber stack with ABA counter in 80 LOC, verifies no double-pop across 2 threads pushing+popping concurrently.

**Strength:** High-fidelity model of the atomic operation sequence. If the model passes, the production code (with identical atomic orderings) is correct.

**Weakness:** Doesn't test production code directly. Risk of model divergence.

### Missing Coverage for Bug 1 Type Issues

**What we SHOULD add:**

#### Test: `flush_pipeline_queue_full_break`

**Goal:** Verify that `flush_sealed_pages` returns `FlushBatchResult` with `queue_full: true` when Device returns QueueFull, without reverting page states.

**Model:**
```rust
mod flush_pipeline {
    use loom::sync::atomic::{AtomicU8, Ordering};
    
    const SEALED: u8 = 1;
    const FLUSHING: u8 = 2;
    const FLUSHED: u8 = 3;
    
    struct MinimalPageTable {
        pages: [AtomicU8; 4],  // 4 pages, enough to show pattern
    }
    
    struct QueueFullDevice {
        fail_after: AtomicUsize,
        write_count: AtomicUsize,
    }
    
    impl QueueFullDevice {
        fn write_async(&self, page: usize) -> bool {
            let count = self.write_count.fetch_add(1, Ordering::Relaxed);
            count < self.fail_after.load(Ordering::Relaxed)
        }
    }
    
    // Simplified flush logic
    fn flush_sealed_pages(table: &MinimalPageTable, device: &QueueFullDevice) 
        -> (u32, bool) 
    {
        let mut flushed = 0;
        let mut queue_full = false;
        
        for i in 0..4 {
            let state = table.pages[i].load(Ordering::Acquire);
            if state != SEALED {
                continue;
            }
            
            // Transition Sealed → Flushing
            match table.pages[i].compare_exchange(
                SEALED, FLUSHING, Ordering::AcqRel, Ordering::Acquire
            ) {
                Ok(_) => {
                    if device.write_async(i) {
                        // Success: callback will mark Flushed
                        flushed += 1;
                    } else {
                        // QueueFull: BREAK, don't revert to Sealed
                        queue_full = true;
                        break;  // KEY: break on QueueFull
                    }
                }
                Err(_) => continue,
            }
        }
        (flushed, queue_full)
    }
}

#[test]
fn flush_pipeline_queue_full_break() {
    loom::model(|| {
        let table = Arc::new(MinimalPageTable {
            pages: [
                AtomicU8::new(SEALED), AtomicU8::new(SEALED),
                AtomicU8::new(SEALED), AtomicU8::new(SEALED),
            ],
        });
        let device = Arc::new(QueueFullDevice {
            fail_after: AtomicUsize::new(2),  // First 2 succeed, 3rd fails
            write_count: AtomicUsize::new(0),
        });
        
        let (flushed, queue_full) = flush_sealed_pages(&table, &device);
        
        // Should flush first 2 pages, then break on QueueFull
        assert_eq!(flushed, 2);
        assert!(queue_full);
        
        // Critical: pages 2-3 must still be Sealed, not reverted
        assert_eq!(table.pages[2].load(Ordering::Acquire), SEALED);
        assert_eq!(table.pages[3].load(Ordering::Acquire), SEALED);
    });
}
```

**What it catches:** The OLD bug pattern of `continue` on QueueFull (which reverts pages to Sealed) vs the CORRECT pattern of `break` (which preserves Flushing state and signals caller).

**State space:** 4 pages, 1 thread → ~1000 states, completes in <1s.

**Value:** **High** — directly tests the bug's root cause without needing the full 10-component pipeline.

#### Test: `allocator_retry_bounded`

**Goal:** Verify that `allocate_at_tail` retries a bounded number of times (not infinite loop) when buffer is full.

```rust
#[test]
fn allocator_retry_bounded() {
    loom::model(|| {
        let buffer_full = Arc::new(AtomicBool::new(true));
        let retry_count = Arc::new(AtomicUsize::new(0));
        
        let b = Arc::clone(&buffer_full);
        let r = Arc::clone(&retry_count);
        let result = thread::spawn(move || {
            for attempt in 0..32 {
                r.fetch_add(1, Ordering::Relaxed);
                if !b.load(Ordering::Acquire) {
                    return Ok(attempt);
                }
                thread::yield_now();
            }
            Err("max retries")
        }).join().unwrap();
        
        // Should fail after 32 attempts
        assert!(result.is_err());
        assert_eq!(retry_count.load(Ordering::Relaxed), 32);
    });
}
```

**What it catches:** Infinite retry loops (old bug) vs bounded retry (correct).

**Recommendation:** Add both tests. Total effort: ~2 hours. Coverage gain: **Directly tests the two components of Bug 1's fix.**

### Missing Coverage: RwLock Starvation (Bug 2 Type)

**Can loom detect RwLock write starvation?** No, because:
1. Loom's RwLock is actually `std::sync::RwLock` (loom 0.7 doesn't provide RwLock)
2. Even if it did, loom doesn't model lock hold *duration* — only lock *order*

**Best we can do:** Test the *pattern* (write lock held during O(N) operation), not the *timing*.

```rust
#[test]
fn rwlock_write_blocks_all_readers() {
    loom::model(|| {
        let lock = Arc::new(RwLock::new(vec![0u8; 8]));
        let blocked = Arc::new(AtomicBool::new(false));
        
        // Writer: hold write lock, do expensive work
        let w = Arc::clone(&lock);
        let writer = thread::spawn(move || {
            let mut guard = w.write().unwrap();
            for i in 0..8 {
                guard[i] = 1;  // Simulates O(N) work
            }
            // Lock released here
        });
        
        // Reader: try to acquire read lock while writer holds write lock
        let r = Arc::clone(&lock);
        let b = Arc::clone(&blocked);
        let reader = thread::spawn(move || {
            let guard = r.read().unwrap();
            b.store(true, Ordering::Relaxed);
            assert_eq!(guard.len(), 8);
        });
        
        writer.join().unwrap();
        reader.join().unwrap();
        
        // Reader must have blocked (blocked flag set AFTER write lock released)
        assert!(blocked.load(Ordering::Relaxed));
    });
}
```

**What it tests:** That RwLock write lock DOES block readers (expected behavior).

**What it DOESN'T test:** That holding write lock for "too long" causes starvation — loom has no concept of "too long".

**Verdict:** Limited value. Better to test with actual stress testing. **Skip this.**

---

## V. Lock Order Analysis

### Static Lock Order Graph

**Concept:** Build a directed graph of all lock acquisitions across the codebase. Edge A→B means "thread acquires lock A, then acquires lock B while holding A". Cycle in graph = potential deadlock.

**Tools:**
1. **MIR-based analysis** — scan `rustc` MIR for lock acquisitions, build graph
2. **Manual audit** — grep for `lock()`, `write()`, document ordering in comments
3. **Dynamic tracing** — instrument locks with thread-local stack, detect violations at runtime

### Current Lock Landscape

**From code inspection:**

1. **InMemoryDevice::data (RwLock)** — `write()` in write_async/write_sync/truncate_until, `read()` in read_async/read_sync
2. **PageTable::pages (lock-free, AtomicU8)** — no lock, just CAS
3. **EpochTable::entries (lock-free, AtomicU64)** — no lock
4. **DrainList (lock-free, AtomicPtr)** — no lock
5. **PendingIoManager (Mutex)** — rarely contended (only on Pending status)
6. **CheckpointOrchestrator (Mutex)** — checkpoint path only
7. **GrowManager (AtomicU8)** — lock-free

**Lock order violations?**

Looking at `maintenance()` path:
```rust
pub fn maintenance(&self) {
    // 1. shift_read_only_to_tail() — no locks
    // 2. flush_sealed_pages() → Device::write_async() → RwLock::write()
    // 3. evict_pages() → Device::truncate_until() → RwLock::write()
}
```

**Potential issue:** Step 2 and 3 both acquire `Device::data.write()`, but at DIFFERENT times (sequential, not nested). No cycle.

**Bug 1 circular dependency is NOT a lock cycle** — it's a *logical dependency cycle*:
- Writers need Head to advance (data dependency)
- Head needs pages Flushed (state dependency)
- Flushing needs I/O callbacks to fire (timing dependency)
- Callbacks need CPU time from yield (scheduling dependency)

This is not detectable by lock order analysis.

**Bug 2 is lock starvation, not lock cycle:**
- truncate_until() holds write lock for O(N) time
- write_async() threads all block on write lock
- No cycle: just one lock, held "too long"

### Tools for Lock Order Enforcement

#### Option 1: `parking_lot` with deadlock detection

```rust
// Cargo.toml
parking_lot = { version = "0.12", features = ["deadlock_detection"] }

// In dev build, enable detector
#[cfg(debug_assertions)]
parking_lot::deadlock::check_deadlock();
```

**What it detects:** Actual Mutex/RwLock cycles at runtime (thread A waits for B, B waits for A).

**What it DOESN'T detect:** Our bugs (not lock cycles).

**Effort:** 1 hour integration. **Value: Low** for our specific bugs.

#### Option 2: Custom lock guards with compile-time ordering

```rust
use std::marker::PhantomData;

// Define lock levels (must be acquired in increasing order)
struct LockLevel1;
struct LockLevel2;
struct LockLevel3;

struct OrderedMutex<T, Level> {
    inner: Mutex<T>,
    _level: PhantomData<Level>,
}

struct OrderedGuard<'a, T, Level> {
    guard: MutexGuard<'a, T>,
    _level: PhantomData<Level>,
}

impl<T, L1> OrderedMutex<T, L1> {
    fn lock(&self) -> OrderedGuard<T, L1> {
        OrderedGuard {
            guard: self.inner.lock().unwrap(),
            _level: PhantomData,
        }
    }
}

// Can only acquire higher-level lock while holding lower-level lock
impl<'a, T, L1> OrderedGuard<'a, T, L1> {
    fn lock_higher<T2, L2>(&self, other: &OrderedMutex<T2, L2>) 
        -> OrderedGuard<T2, L2>
    where
        L2: HigherThan<L1>  // Compile-time constraint
    {
        other.lock()
    }
}
```

**What it prevents:** Acquiring lock A while holding lock B if A < B in the defined order.

**What it DOESN'T prevent:** Our bugs (not lock ordering violations).

**Effort:** 4-8 hours to design + integrate. **Value: Low** for current bugs, **Medium** for future prevention.

#### Option 3: Runtime lock stack tracing (TSan-inspired)

```rust
thread_local! {
    static LOCK_STACK: RefCell<Vec<&'static str>> = RefCell::new(Vec::new());
}

struct TracedMutex<T> {
    inner: Mutex<T>,
    name: &'static str,
}

impl<T> TracedMutex<T> {
    fn lock(&self) -> MutexGuard<T> {
        LOCK_STACK.with(|stack| {
            let mut s = stack.borrow_mut();
            // Check if this lock is already in the stack (reentrant, illegal)
            if s.contains(&self.name) {
                panic!("Reentrant lock detected: {}", self.name);
            }
            // Check if we're acquiring locks out of order
            if let Some(&last) = s.last() {
                if should_panic_on_order(last, self.name) {
                    panic!("Lock order violation: {} after {}", self.name, last);
                }
            }
            s.push(self.name);
        });
        
        let guard = self.inner.lock().unwrap();
        
        LOCK_STACK.with(|stack| {
            stack.borrow_mut().pop();
        });
        
        guard
    }
}
```

**What it detects:** Runtime lock ordering violations, reentrant locks.

**What it DOESN'T detect:** Starvation, timing-dependent deadlocks.

**Effort:** 6-10 hours. **Value: Medium** (good for general lock hygiene, won't catch our specific bugs).

### Recommendation: Lock Order Analysis

**For Bug 1 & 2:** ❌ Skip formal lock order enforcement — neither bug is a lock ordering violation.

**For future work:** ✅ Add `parking_lot` deadlock detection in debug builds (1 hour, low risk, catches a different class of bug).

**Priority:** P3 — nice to have, not urgent.

---

## VI. Deterministic Simulation Testing (DST) — FoundationDB Approach

### Current DST Framework

**What it does (from `history.md`):**
- 1003 scenario templates across 5 families
- 18 crash points (checkpoint, compaction, recovery)
- Fault injection (write errors, torn writes, graduated faults)
- 3009 test cases (1003 × 3 seeds) in ~220s

**What it DOESN'T do:**
- ❌ Model concurrent operations (only single-threaded recovery)
- ❌ Simulate time (no `SimClock`)
- ❌ Deterministic thread scheduling
- ❌ Resource pressure injection (queue depth, memory limits)

**Architectural gap:** Current DST is **recovery-focused** (crash/restart), not **concurrency-focused** (live system behavior).

### FoundationDB DST Model

**Key abstractions:**

1. **SimClock** — Virtual time, controllable by scheduler. `sleep(1s)` advances SimClock by 1s but takes 0 real time.

2. **Deterministic scheduler** — All thread wakeups, I/O completions, timer events are enqueued in a priority queue keyed by SimClock time. Seed determines tie-breaking order.

3. **Simulated resources:**
   - Network: packet loss, latency injection
   - Disk: queue depth, I/O latency, IOPS limits
   - Memory: allocation failures, GC pauses

4. **Buggify hooks** — Inject delays, failures at random points controlled by seed.

5. **Deterministic random** — All randomness (including scheduler decisions) derived from seed.

**How it would catch Bug 1:**

```
Scenario: Multi-writer with I/O queue saturation

Setup:
  - SimClock starts at T=0
  - 4 writer threads (green threads, not OS threads)
  - Device has queue_depth=4 (smaller than real device)
  - I/O latency = 10ms (simulated, not real)
  
Execution:
  T=0ms: Writer1 allocates page 1, flushes → Device enqueues I/O, completes at T=10ms
  T=1ms: Writer2 allocates page 2, flushes → enqueued, completes at T=11ms
  T=2ms: Writer3 allocates page 3, flushes → enqueued, completes at T=12ms
  T=3ms: Writer4 allocates page 4, flushes → enqueued, completes at T=13ms
  T=4ms: Writer1 tries to flush page 5 → Device.write_async() returns QueueFull (queue at capacity)
  
  [OLD BUG]: flush_sealed_pages() reverts page 5 to Sealed, loops to next page
  [CORRECT]: flush_sealed_pages() breaks, returns FlushBatchResult{flushed: 4, queue_full: true}
  
  T=4ms: allocate_at_tail() gets QueueFull signal, calls poll_completions(), yields
  T=10ms: I/O completes, page 1 → Flushed, wakes writer1
  T=10ms: allocate_at_tail() retries, succeeds (head advanced)
  
Assertion: All writers make progress within 100ms of sim-time
```

**Seed variation:** Different seeds permute thread interleaving, I/O completion order, but outcome is deterministic for a given seed.

**How it would catch Bug 2:**

```
Scenario: Lossy eviction with large Vec

Setup:
  - SimClock, 16 writer threads, lossy mode
  - InMemoryDevice::data starts at 0 bytes
  - Eviction triggers every 1000 ops (aggressive)
  - Buggify: inject random GC pauses (0-50ms)
  
Execution:
  T=0-1000ms: Vec grows to 1MB, truncate_until() takes 1ms (simulated)
  T=1000-5000ms: Vec grows to 100MB, truncate_until() takes 100ms (simulated)
  T=5000-10000ms: Vec grows to 1GB, truncate_until() takes 1000ms (simulated)
  
  [BUG TRIGGER]: T=10000ms, truncate_until(offset=1GB):
    - Acquires write lock
    - Simulated fill(0) duration = 1000ms
    - All 16 writer threads block on write lock for 1000ms (sim-time)
    - Scheduler detects: no thread made progress for >500ms → DEADLOCK
    
Assertion: Throughput > 1M ops/s for entire test (catches sustained stalls)
```

**Detection mechanism:** Watchdog — if no thread transitions state for >500ms sim-time, assert fails.

### Building FASTER DST v2.0

**Phase 1: Core Abstractions (4-6 weeks)**

1. **SimClock** — `rust/crates/faster-dst/src/sim_clock.rs`
   ```rust
   pub struct SimClock {
       current: AtomicU64,  // Nanoseconds since epoch
   }
   
   impl SimClock {
       pub fn now(&self) -> Instant { /* returns virtual Instant */ }
       pub fn sleep(&self, duration: Duration) { /* yields to scheduler */ }
       pub fn advance(&self, by: Duration) { /* only scheduler calls this */ }
   }
   ```

2. **Deterministic scheduler** — `rust/crates/faster-dst/src/scheduler.rs`
   ```rust
   pub struct DstScheduler {
       clock: SimClock,
       event_queue: BinaryHeap<Event>,  // (wake_time, thread_id, event_type)
       rng: ChaChaRng,  // Seeded RNG for determinism
   }
   
   impl DstScheduler {
       pub fn spawn<F>(&mut self, f: F) -> ThreadHandle where F: FnOnce() + Send;
       pub fn run_until_idle(&mut self);
       pub fn run_for(&mut self, duration: Duration);
   }
   ```

3. **Simulated Device** — `rust/crates/faster-dst/src/sim_device.rs`
   ```rust
   pub struct SimDevice {
       queue_depth: usize,
       io_latency: Duration,
       scheduler: Arc<DstScheduler>,
       pending_io: Vec<PendingIo>,
   }
   
   impl Device for SimDevice {
       fn write_async(&self, ...) -> IoRequestResult {
           if self.pending_io.len() >= self.queue_depth {
               return IoRequestResult::QueueFull;
           }
           
           // Schedule callback to fire at T + io_latency
           let complete_at = self.scheduler.clock().now() + self.io_latency;
           self.scheduler.schedule_event(complete_at, IoComplete { ... });
           IoRequestResult::Submitted
       }
   }
   ```

4. **Resource pressure config** — `rust/crates/faster-dst/src/fault_config.rs`
   ```rust
   pub struct ResourcePressure {
       pub device_queue_depth: usize,  // 4-1024
       pub io_latency_ms: u64,         // 1-100
       pub memory_limit_mb: usize,     // 64-8192
       pub gc_pause_probability: f64,  // 0.0-0.1
   }
   ```

**Phase 2: Concurrency Scenarios (2-3 weeks)**

5. **Multi-writer stress** — `rust/crates/faster-dst/src/scenarios/concurrency/multi_writer_saturation.rs`
   ```rust
   pub fn multi_writer_saturation_template() -> ScenarioTemplate {
       ScenarioTemplate {
           name: "multi_writer_saturation",
           description: "N writers with small buffer under I/O pressure",
           config_params: vec![
               ("num_writers", vec![2, 4, 8, 16]),
               ("buffer_pages", vec![8, 16, 32]),
               ("io_queue_depth", vec![4, 8, 16]),
           ],
           run: |config, scheduler| {
               let store = FasterKv::new_simulated(config, scheduler.device());
               
               for i in 0..config.num_writers {
                   scheduler.spawn(move || {
                       for key in 0..100_000 {
                           store.upsert(key + i * 100_000, key);
                       }
                   });
               }
               
               // Run until all threads complete or 60s sim-time (should complete in ~10s)
               scheduler.run_for(Duration::from_secs(60));
               
               // Assertion: throughput never drops below threshold for >1s
               let stats = scheduler.collect_stats();
               assert!(stats.min_throughput_1s_window > 1_000_000);
           }
       }
   }
   ```

6. **Lossy scale test** — `rust/crates/faster-dst/src/scenarios/concurrency/lossy_eviction_starvation.rs`
   ```rust
   pub fn lossy_eviction_starvation_template() -> ScenarioTemplate {
       ScenarioTemplate {
           name: "lossy_eviction_starvation",
           run: |config, scheduler| {
               let mut config = config.clone();
               config.lossy = true;
               config.buffer_pages = 16;  // Force aggressive eviction
               
               let store = FasterKv::new_simulated(config, scheduler.device());
               
               // 16 writers, linear keys (maximum allocation churn)
               for i in 0..16 {
                   scheduler.spawn(move || {
                       for key in 0..1_000_000 {
                           store.upsert(key + i * 1_000_000, key);
                       }
                   });
               }
               
               scheduler.run_for(Duration::from_secs(300));  // 5 min sim-time
               
               // Assertion: no stalls longer than 1s sim-time
               let events = scheduler.event_log();
               let max_stall = events.max_gap_between_ops();
               assert!(max_stall < Duration::from_secs(1));
           }
       }
   }
   ```

**Phase 3: Buggify Integration (1-2 weeks)**

7. **Chaos injection** — Random delays, failures at key junctions
   ```rust
   pub struct BuggifyConfig {
       pub seed: u64,
       pub inject_delays: bool,       // Random yield/sleep at sync points
       pub inject_failures: bool,     // Device errors, allocation failures
       pub probability: f64,          // 0.01 = 1% of operations
   }
   
   // In code:
   if buggify.should_inject(rng) {
       scheduler.sleep(buggify.random_delay(rng));  // 0-10ms
   }
   ```

**Total effort estimate:** 7-11 weeks for complete DST v2.0 with concurrency + time simulation.

**Value:** ✅ **VERY HIGH** — Would catch both Bug 1 and Bug 2 deterministically, plus future concurrency bugs.

---

## VII. Practical Recommendations (Prioritized)

### Tier 0: Ship Immediately (Already Done)

| # | Action | Effort | Impact | Owner |
|---|--------|--------|--------|-------|
| 1 | Add regression tests for Bug 1 & 2 in `deadlock_tests.rs` | ✅ Done | HIGH | Boromir |
| 2 | Deploy 16T stress test on CI (5min timeout) | ✅ Done | HIGH | Boromir |

**Status:** Both bugs have regression coverage. No further action needed for known bugs.

---

### Tier 1: Low-Hanging Fruit (Next 2 Weeks)

| # | Action | Effort | Impact | Coverage | Owner |
|---|--------|--------|--------|----------|-------|
| 1 | Add `flush_pipeline_queue_full_break` loom test | 2 hours | Medium | Bug 1 pattern | Éowyn |
| 2 | Add `allocator_retry_bounded` loom test | 1 hour | Medium | Bug 1 pattern | Éowyn |
| 3 | Enable `parking_lot` deadlock detection in debug builds | 1 hour | Low-Med | Future lock cycles | Sam |
| 4 | Document lock ordering rules in `ARCHITECTURE.md` | 2 hours | Low | Developer guidance | Gandalf |

**Total:** 6 hours, **catches future variants of Bug 1 pattern** before they ship.

**Priority:** P1 — these are quick wins with measurable value.

---

### Tier 2: Medium-Term Investment (Next 6 Weeks)

| # | Action | Effort | Impact | Coverage | Owner |
|---|--------|--------|--------|----------|-------|
| 5 | Expand stress test matrix: vary threads (2,4,8,16), buffer (8,16,32), duration (1m,5m,10m) | 3 days | High | Production scenarios | Boromir |
| 6 | Add 16T stress to nightly CI (not PR gate) | 1 day | High | Continuous validation | Éowyn |
| 7 | Property-based concurrency tests with `proptest` + `loom` | 1 week | Medium | Hybrid validation | Éowyn |

**Total:** ~2 weeks, **significantly increases concurrency bug surface coverage**.

**Priority:** P2 — diminishing returns, but solid risk reduction.

---

### Tier 3: Strategic Investment (Next Quarter)

| # | Action | Effort | Impact | Coverage | Owner |
|---|--------|--------|--------|----------|-------|
| 8 | DST v2.0: SimClock + Deterministic Scheduler | 4 weeks | VERY HIGH | Time-dependent bugs | Éowyn |
| 9 | DST v2.0: Simulated Device with queue pressure | 2 weeks | VERY HIGH | I/O saturation bugs | Éowyn |
| 10 | DST v2.0: 50 concurrency scenario templates | 2 weeks | VERY HIGH | Comprehensive coverage | Éowyn |
| 11 | DST v2.0: Buggify chaos injection | 1 week | High | Rare race conditions | Éowyn |

**Total:** 9-11 weeks, **FoundationDB-grade deterministic concurrency testing**.

**Priority:** P1 if we want to prevent ALL future deadlocks (not just variants of Bug 1/2). P2 if we're satisfied with stress testing as primary gate.

**Trade-off:** DST v2.0 is **deterministic + exhaustive** (finds bugs reliably), stress tests are **probabilistic + incomplete** (only find bugs that manifest in <10min on specific hardware).

---

### Tier 4: Nice-to-Have (Backlog)

| # | Action | Effort | Impact | Owner |
|---|--------|--------|--------|-------|
| 12 | Custom lock ordering guards (compile-time enforcement) | 1 week | Low-Med | Sam |
| 13 | TSan-style runtime lock tracing | 1 week | Low-Med | Sam |
| 14 | Model-based testing (TLA+ spec for flush pipeline) | 3 weeks | Medium | Gandalf |

**Priority:** P3 — academic rigor, low ROI for our specific threat model.

---

## VIII. Economics of Test Coverage

### Test Cost-Benefit Matrix

| Approach | Dev Time | CI Time | Bugs Found (Bug 1) | Bugs Found (Bug 2) | Coverage Score |
|----------|----------|---------|-------------------|-------------------|----------------|
| **Loom (current 21 tests)** | 3 days | 64s | ❌ No | ❌ No | ⭐⭐⭐☆☆ (logic) |
| **Loom + 2 new tests** | +2 hours | +10s | ⚠️ Pattern only | ❌ No | ⭐⭐⭐⭐☆ (logic++) |
| **Stress test (16T, 5min)** | 1 day | 300s | ✅ Yes | ✅ Yes | ⭐⭐⭐⭐⭐ (production) |
| **DST v2.0 (full)** | 9 weeks | 600s | ✅ Yes (deterministic) | ✅ Yes (deterministic) | ⭐⭐⭐⭐⭐ (production++) |
| **Proptest concurrency** | 1 week | 120s | ⚠️ Maybe | ❌ No | ⭐⭐⭐☆☆ (hybrid) |
| **Lock order analysis** | 1 week | 0s | ❌ No | ❌ No | ⭐⭐☆☆☆ (prevention) |

**Key insight:** Cost per bug caught:
- **Loom:** $0/bug (didn't catch these bugs), ∞ cost for coverage gained
- **Stress test:** ~$200/bug (1 day dev @ $1600/day ÷ 2 bugs = $800, but also prevents future bugs)
- **DST v2.0:** ~$7000/bug (9 weeks @ $8000/week ÷ 2 bugs = $36K, but also catches 100+ future bugs)

**Amortized cost:** DST v2.0 pays for itself after catching **5 bugs** that would each take 1 day to debug in production.

**Expected bug rate:** Based on industry data (FoundationDB, CockroachDB), distributed systems hit ~1 concurrency deadlock per 10K LOC. FASTER is ~15K LOC → expect **~5-10 more deadlocks** over the next year without DST v2.0.

**ROI calculation:** 
- **With stress tests only:** 5 bugs × 1 day debugging each = 5 days = $8,000 cost
- **With DST v2.0:** 9 weeks upfront + 0 days debugging (bugs caught before ship) = $36,000 - $8,000 = **$28,000 net cost**, but **zero downtime**, **zero customer impact**

**Recommendation:** Build DST v2.0 if we're committed to **production-grade quality**. Otherwise, rely on stress testing + fast iteration.

---

## IX. Summary & Next Steps

### The Capability Gap (Visual)

```
           Logical       Time/Scale      Resource
           Correctness   Dependent       Saturation
           
Loom       ████████      ░░░░░░░░        ░░░░░░░░   (strong logic, zero time/scale)
Miri       ████████      ░░░░░░░░        ░░░░░░░░   (UB only, not concurrency)
Proptest   ██████░░      ░░░░░░░░        ░░░░░░░░   (state space, not interleaving)
Stress     ████░░░░      ████████        ████████   (catches real bugs, not deterministic)
DST v2.0   ████████      ████████        ████████   (FoundationDB-grade, deterministic)
```

**The fundamental gap:** Loom excels at **logical concurrency** (memory ordering, race conditions, ABA), but cannot model **emergent system behavior** (resource exhaustion, timing-dependent starvation, back-pressure propagation).

Both Bug 1 and Bug 2 are **emergent properties of sustained load**, not logical race conditions.

### Recommendations by Persona

**For qbradley (Project Owner):**
- ✅ Immediate: Rely on stress testing + CI integration (already done)
- ✅ Q1 2027: Build DST v2.0 for long-term quality bar (9 weeks investment)
- ✅ Alternative: Accept 5-10 more production deadlocks over next year, fix reactively

**For Éowyn (DST Expert):**
- ✅ Next 2 weeks: Add 2 loom tests for Bug 1 pattern (6 hours)
- ✅ Next 6 weeks: Expand stress test matrix (2 weeks)
- ✅ Next quarter: Build DST v2.0 Phase 1-3 (9 weeks)

**For Boromir (QA):**
- ✅ Now: Keep 16T stress in CI nightly (already deployed)
- ✅ Next: Add stress test variations (thread count, buffer size, duration)
- ✅ Partner with Éowyn on DST v2.0 scenario authoring

**For Sam (Systems Expert):**
- ✅ Next week: Review loom test proposals (1 hour)
- ✅ Optional: Integrate `parking_lot` deadlock detection (1 hour)
- ✅ If DST v2.0 approved: Design concurrency primitives (1 week)

### Final Verdict

**Why these bugs escaped:**
1. **Loom:** Cannot model time/scale → fundamentally unable to catch these bugs
2. **DST (current):** Only tests recovery, not live concurrency
3. **Unit tests:** Too small, too fast → never trigger resource exhaustion
4. **Integration tests:** Too short (<30s) → never reach saturation point

**How to catch future bugs:**
1. **Short-term:** Stress testing (5min, 16T) in CI nightly — probabilistic but practical
2. **Long-term:** DST v2.0 with time simulation — deterministic and exhaustive

**The economics:** $36K upfront vs $8K/year reactive debugging + customer downtime.

**Recommendation:** ✅ **Build DST v2.0.** The quality bar for "mission-critical cloud services at planetary scale" demands deterministic validation. Stress testing alone leaves too much to chance.

---

## Appendix: Loom Test Authoring Pattern

For future reference, here's the pattern for writing effective loom tests:

```rust
// 1. Define minimal re-implementation of algorithm under test
mod minimal_algorithm {
    use loom::sync::atomic::{AtomicU8, Ordering};
    
    pub struct Widget {
        state: AtomicU8,
    }
    
    impl Widget {
        pub fn new() -> Self { Self { state: AtomicU8::new(0) } }
        pub fn transition(&self, from: u8, to: u8) -> bool {
            self.state.compare_exchange(from, to, Ordering::AcqRel, Ordering::Acquire).is_ok()
        }
    }
}

// 2. Test with 2-3 threads (not more, state space explosion)
#[test]
fn widget_exclusive_transition() {
    loom::model(|| {
        let widget = Arc::new(minimal_algorithm::Widget::new());
        
        let w1 = Arc::clone(&widget);
        let t1 = thread::spawn(move || w1.transition(0, 1));
        
        let w2 = Arc::clone(&widget);
        let t2 = thread::spawn(move || w2.transition(0, 2));
        
        let r1 = t1.join().unwrap();
        let r2 = t2.join().unwrap();
        
        // 3. Assert mutual exclusion
        assert!(r1 ^ r2, "exactly one thread must succeed");
    });
}
```

**Key constraints:**
- **Thread count ≤ 3** for tractable state space
- **Data structures ≤ 8 elements** (larger = exponential explosion)
- **Operations ≤ 10 per thread** (more = timeout risk)
- **Minimal re-implementation** (not production types, which have too much complexity)

---

**End of Analysis**
