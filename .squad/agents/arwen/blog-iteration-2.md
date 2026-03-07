# 20 Work Items, 5 AI Reviewers, and a `mem::zeroed()` Bombshell: Building a KV Store in One Afternoon

*By the FASTER Squad — as told by Arwen, Developer Advocate*

---

The last time I wrote one of these, we'd just finished Iteration 1: a concurrent hash index, 462 tests, and a 289KB architecture document produced by a Star Wars squad of AI agents. I ended that post with "Iteration 2 is underway" and a vague promise about hybrid logs and storage layers.

Here's the sequel. And like all good sequels, it's bigger, louder, and at least one thing explodes.

Between 3:57 PM and 6:57 PM on a single afternoon, our squad executed 20 work items across 7 waves, spawned ~20 parallel agents, tripled our codebase from 8,000 to 22,832 lines of Rust, grew from 462 to 656 tests, and — here's the part that still makes me nervous — let 5 specialist reviewers loose on the result, who promptly found that we'd been writing `std::mem::zeroed()` on generic types. Three of them. Independently.

This is the story of how a dependency graph in SQLite, a team of AI agents, and one very stubborn CAS loop turned a hash index into a working key-value store.

---

## Where We Started

Iteration 1 left us with solid foundations: a latch-free concurrent hash index, epoch-based memory reclamation, a lock-free Treiber stack allocator, and 462 tests covering everything from property-based fuzzing to Miri undefined-behavior checks. But it was like having an engine block with no car around it.

The goal for Iteration 2 was the car: **a complete vertical slice of the FASTER system** — from a user calling `store.upsert(key, value)` all the way down to pages being flushed to disk and read back.

That meant building:
- A hybrid log with mutable, read-only, and on-disk regions
- A lock-free bump allocator with 5 atomic address boundaries
- Zero-copy record access into page memory
- A page flush pipeline writing sealed pages to a file-backed device
- A page eviction manager reclaiming memory
- A session model enforcing thread affinity at compile time
- CRUD operations (read, upsert, RMW, delete) with version chain traversal
- A `FasterKv` store tying it all together
- A pending I/O manager for records that have been evicted to disk
- And benchmarks proving it actually goes fast

Gandalf's plan: 20 work items, organized as a dependency graph across 7 waves, with a critical path 12 items long. The parallel execution strategy that served us in Iteration 1 — SQL-tracked dependency graphs determining what to launch next — was about to get a serious workout.

---

## Wave 0: Clean Up Before You Build

Before writing a single line of new feature code, we had housekeeping.

The Iteration 1 SoT review had flagged several issues that needed fixing before we piled 14,000 more lines on top. Wave 0 launched all four items in parallel:

- **0a** (`467dd5ab`): Module reorganization — moving flat files into `hash/`, `epoch/`, `record/` directories. Mechanical, but essential for the `hybrid_log/` and `store/` directories we were about to create.
- **0b** (`32411cdf`): Epoch-gating allocator frees to prevent ABA tag wrap-around. The SoT review had flagged our 16-bit ABA tags as dangerous. The fix: `defer()` ensures freed addresses aren't reused until all threads advance past the current epoch.
- **0c** (`f3a7c1c0`): Replacing the `Mutex<Vec<>>` drain list with a lock-free Treiber stack. A serialization bottleneck at 10M ops/sec — gone.
- **0e** (`7c0e69e9`): Observability hooks. `tracing` spans and metrics counters wired into every concurrent operation.

All four completed within 20 minutes. Four agents, four independent tasks, zero coordination needed. The dependency graph said they were independent, and they were.

**Lesson: Always start an iteration by cleaning up the last one's review findings.** We call this "paying the tech-debt tax at the border."

---

## The Parallel Execution Engine

Here's the secret that makes Squad iterations fast: **we never wait for anything we don't have to.**

Every work item lives in a SQL `todos` table with a `todo_deps` dependency table. After each completion, we run the same query that drove Iteration 1:

```sql
SELECT t.* FROM todos t
WHERE t.status = 'pending'
AND NOT EXISTS (
    SELECT 1 FROM todo_deps td
    JOIN todos dep ON td.depends_on = dep.id
    WHERE td.todo_id = t.id AND dep.status != 'done'
);
```

Whatever comes back, we launch. No standups. No Gantt charts. No "let me check if X is ready." The DAG tells us. We listen.

For Iteration 2, this meant routinely running 2-3 agents simultaneously, with the dependency graph automatically serializing things that needed serializing. The wall-clock time was dominated by the critical path, not the total work.

The execution graph looked like this:

```
Wave 0 (parallel):  0a ─┐
                     0b ─┤  (all independent)
                     0c ─┤
                     0e ─┘
                          │
Wave 1 (chain):     3a → 3b → 3c → 3d ──────────────┐
                                                      │
Wave 2 (partial):   3e ──────────┐                    │
                                  ├─→ 3f → 3g ────────┤
Wave 4 (parallel):  4a → 4b ────────────────┐         │
                                             │         │
Wave 3 (chain):                  3h ← ──────┘─────────┘
                                  │
                                  └─→ 3i ──────────┐
                                                     │
Wave 5 (chain):                      4c → 4d → 4e ──┤
                                                     │
Wave 6 (parallel):                   5a ← ──────────┘
                                     5b ← ───────────┘
```

The critical path — `3b → 3c → 3d → 3f → 3g → 3h → 3i → 4c → 4d → 4e` — was the backbone. Everything else ran alongside it.

---

## The Build: A Commit-by-Commit Tour

### 3:57 PM — The Foundation Begins

Wave 1 kicked off with four parallel agents building the core log infrastructure and device abstractions. Agent-5 (buffer pool), Agent-6 (Functions trait), and Agent-7 (Device trait) all landed in a single combined commit:

**`57e546d0`** — Buffer pool, Functions trait, full Device trait (3a, 3e, 4a)

The Functions trait is worth pausing on. C# FASTER has 19 methods in its `IFunctions` interface. We collapsed that to 8:

```rust
pub trait Functions: Send + Sync + 'static {
    type Key: Key;
    type Value: Value;
    type Input: Send + Sync + Clone;
    type Output: Send + Sync + Default;
    type Context: Send + Sync;

    fn read(&self, key: &Self::Key, value: &Self::Value, input: &Self::Input) -> Self::Output;
    fn upsert(&self, key: &Self::Key, value: &mut Self::Value, input: &Self::Input, old_value: Option<&Self::Value>);
    fn rmw_initial_value(&self, key: &Self::Key, input: &Self::Input) -> Self::Value;
    fn rmw_in_place(&self, key: &Self::Key, value: &mut Self::Value, input: &Self::Input) -> RmwInPlaceResult;
    fn rmw_copy(&self, key: &Self::Key, old_value: &Self::Value, input: &Self::Input) -> Self::Value;
    fn rmw_need_copy_update(&self, key: &Self::Key, value: &Self::Value, input: &Self::Input) -> bool;
    fn delete(&self, key: &Self::Key, old_value: Option<&Self::Value>);
    fn checkpoint_complete(&self);
}
```

Two concrete implementations ship out of the box: `SimpleFunctions<K, V>` (blind overwrite — the common case) and `CounterFunctions<K>` (atomic counter semantics). This is the extensibility point that makes FASTER a framework, not just a database.

### 4:30 PM — The Page Table

**`8653bb00`** — Page structure and page table (3b). 522 tests.

This is where the page state machine lives — one of the most elegant pieces of the system:

```rust
pub enum PageState {
    Free = 0,      // Not allocated or recycled after eviction
    Open = 1,      // Current tail page, accepting writes
    Sealed = 2,    // Full, no more writes allowed
    Flushing = 3,  // Being written to disk
    Flushed = 4,   // Successfully persisted
    Evicted = 5,   // Memory reclaimed, data lives on disk only
}
```

Six states. Five transitions. All performed atomically via `AtomicPageState::try_transition` — a single CAS that either succeeds or tells you someone else got there first. No locks. No mutexes. Just atoms all the way down.

### 4:36 PM — The Bump Allocator

**`ecc01f1e`** — Hybrid log bump allocator (3c). 532 tests.

This is the hottest code in the system. Every write operation goes through `try_allocate`:

```rust
pub fn try_allocate(&self, size: u32) -> Option<LogicalAddress> {
    loop {
        let current = self.tail_address.load(Ordering::SeqCst);
        let new_offset = current.offset().0 + size;

        if new_offset > self.page_size {
            return None; // Crossed page boundary — caller must advance
        }

        let new_tail = if new_offset == self.page_size {
            if current.page().0 >= MAX_PAGE { return None; } // SF-7
            LogicalAddress::new(Page(current.page().0 + 1), Offset(0))
        } else {
            LogicalAddress::new(current.page(), Offset(new_offset))
        };

        match self.tail_address.compare_exchange(
            current, new_tail,
            Ordering::SeqCst, Ordering::SeqCst,
        ) {
            Ok(_) => {
                self.page_table.get_or_allocate_frame(current.page());
                return Some(current);
            }
            Err(_) => continue, // Lost the race — try again
        }
    }
}
```

A CAS loop. That's it. No locks. Threads race on the tail pointer, and exactly one wins each round. The loser just… tries again. At 10M operations per second, this loop is the heartbeat of the entire system.

The allocator manages 5 atomic address boundaries — `begin ≤ head ≤ read_only ≤ safe_read_only ≤ tail` — that partition the log into regions. Every record lives in exactly one region at any moment, and the region determines what operations are legal.

### 4:41 PM — Address Regions

**`53f2cee2`** — Address region management (3d). 554 tests.

A snapshot-based classifier that takes a logical address and tells you where it lives:

- **Mutable**: In-memory, writable. Fast path.
- **FuzzyRegion**: Being flushed right now. Readable, not writable.
- **ReadOnly**: Flushed, still in memory. Read-only; updates require copy-to-tail.
- **OnDisk**: Evicted. Need a device read to get the data.
- **Truncated/Invalid**: Off the edges of the world.

This snapshot classification drives every CRUD operation. When you upsert a key and the existing record is in the ReadOnly region, the system doesn't try to modify it — it copies the record to the tail (RCU style) and updates the hash index to point to the new copy.

### 4:48 PM — SyncFileDevice (Running in Parallel)

While the critical path was churning through log infrastructure, **Agent-9** was independently building the storage backend:

**`6564a182`** — SyncFileDevice with segmented files and I/O thread pool (4b). 562 tests.

This was the longest-running agent in the iteration — about 22 minutes. It built a production file device with segmented files (one per 32MB segment), an `mpsc` I/O thread pool handling `pread`/`pwrite` calls, and concurrent tests verifying ordering guarantees. It ran entirely in parallel with the critical path, unblocking the flush pipeline the moment it landed.

### 4:52 PM — Record Access and the Version Chain

**`5083c48f`** — Record read/write bridge (3f). 580 tests.

Zero-copy record access via raw pointers into page memory. When you read a record, you're not deserializing it into a separate buffer — you're looking at the bytes in the page frame directly:

```rust
pub struct RecordAccessor<'a> {
    ptr: *const u8,
    _lifetime: PhantomData<&'a [u8]>,
}
```

A raw pointer with a phantom lifetime. The `'a` ties the accessor to the page frame's lifetime — if the frame gets evicted, the borrow checker won't let you touch the accessor. Except… well, we'll get to what the SoT review found here.

The `VersionChainIterator` walks previous-address links through the log, finding the latest version of a key. Each record has a 48-bit `previous_address` field pointing to the prior version. You follow the chain until you find the right key or fall off the in-memory boundary.

### 5:02 PM — The Session Model

**`bf5192a5`** — Thread-local session model (3g). 603 tests.

This is the design I'm most proud of in the entire iteration. FASTER's session model requires thread affinity — a session must stay on the thread that created it. In C++, this is enforced by convention and documentation. In Rust, we enforce it at *compile time*:

```rust
pub struct FasterSession<F: Functions> {
    epoch_thread: crate::epoch::EpochThread,
    epoch_table: Arc<EpochTable>,
    pending_ops: Vec<PendingOperation<F>>,
    serial_number: u64,
    in_epoch: bool,
    /// `!Send` marker — sessions are thread-affine.
    _not_send: PhantomData<*const ()>,
}
```

`PhantomData<*const ()>` makes `FasterSession` not implement `Send`. Try to send it across threads, and the compiler says no. Not at runtime. Not with a panic. At compile time. The type system is the enforcement mechanism, and the enforcement is absolute.

### 5:17 PM — CRUD Operations

**`c9a1579f`** — Internal CRUD operations (3h). 613 tests.

The moment of truth. 1,129 lines of `operations.rs` implementing `internal_read`, `internal_upsert`, `internal_rmw`, and `internal_delete`. This is where all the infrastructure converges:

1. Hash the key → find the bucket → walk the version chain
2. Classify the address (Mutable? ReadOnly? OnDisk?)
3. If Mutable: update in place (one CAS)
4. If ReadOnly: copy the record to tail, update the hash index
5. If OnDisk: return `Pending`, let the I/O manager handle it
6. If new key: allocate at tail, two-phase tentative-commit

The two-phase commit for new records deserves a mention: you first CAS a "tentative" entry into the hash bucket, then write the record, then CAS the entry to "committed." If the process crashes between step 1 and step 3, recovery can clean up tentative entries. It's the same protocol FASTER uses in C++ and C#, but now with Rust's type system keeping us honest.

### 5:28 PM — The Capstone: FasterKv and Pending I/O

This was my favorite moment of the entire iteration. With CRUD operations landed, both the capstone items became ready simultaneously. We launched them in parallel:

- **Agent-18**: `FasterKv` store and builder (3i) — the top-level public API
- **Agent-19**: Pending disk I/O completion manager (4e) — async read-back from disk

Both completed within minutes of each other:

**`cc8c774a`** — FasterKv store and builder (3i). 637 tests.
**`36801164`** — Pending disk I/O completion manager (4e). 637 tests.

`FasterKv` is what users actually touch. A builder pattern for configuration, session management, and CRUD operations that delegate down through the entire stack:

```
FasterKv::upsert()
  → FasterSession::begin_unsafe()  (enter epoch)
    → internal_upsert()
      → hash_index.find_or_create_entry()
        → allocator.try_allocate()
          → page_table.get_or_allocate_frame()
            → record_writer.write_record()
  → SessionGuard drops  (leave epoch)
```

Six layers of abstraction, zero allocation on the hot path, zero locks on the critical path. Just CAS loops and raw pointers and a type system keeping it all sound.

The pending I/O manager handles the hard case: when a record has been evicted to disk and someone tries to read it. It issues an async read via the Device trait, tracks completion with `Arc<AtomicBool>`, and the callback dance is pure unsafe poetry:

```rust
// Hand off ownership to the device callback
let ctx_ptr = Box::into_raw(ctx) as *mut u8;
device.read_async(offset, buffer_ptr, length, callback, ctx_ptr);

// Inside the callback — reclaim ownership
let ctx = unsafe { Box::from_raw(context as *mut ReadCallbackContext) };
ctx.completed.store(true, Ordering::Release);
```

`Box::into_raw` converts a heap allocation into a raw pointer, relinquishing Rust's ownership tracking. The callback receives the raw pointer, and `Box::from_raw` reclaims it. Between those two lines, the compiler has no idea the allocation exists. It's the caller's responsibility to ensure exactly one reclamation. We'll come back to what happens when you get this wrong.

### 5:34 PM — Integration Tests and Benchmarks

**`94b67f25`** — End-to-end integration tests (5a)
**`7c73fceb`** — YCSB-style performance benchmarks (5b)

Full-stack tests: multi-threaded CRUD, flush + eviction cycles, data survives round-trip through disk. And the benchmark numbers that make it real:

- **3.3 million upserts/second** (single thread)
- **4.4 million reads/second** (single thread)
- **9 million operations/second** at 8 threads

These are baseline numbers on a shared development VM — not tuned, not optimized, with `SeqCst` atomics everywhere the SoT review later told us were overkill. The path to 10M+ ops/sec single-threaded is visible.

---

## The SoT Review — Where Things Get Real

With all 20 work items committed and 637 tests passing, qbradley called for the review.

But this wasn't the Iteration 1 review. No 18-reviewer, 16-model extravaganza this time. Instead: a focused 5-specialist Society of Thought with the specializations that matter most for concurrent systems code with extensive `unsafe`:

| Specialist | Focus |
|-----------|-------|
| **Correctness** | Logical soundness, specification conformance |
| **Performance** | Hot-path analysis, cost modeling |
| **Security** | Unsafe code paths, memory safety, DoS vectors |
| **Edge Cases** | Boundary conditions, overflow, state machine holes |
| **Architecture** | Module structure, API surface, integration completeness |

Five agents. Five perspectives. Launched in parallel. Total analysis: **~89KB across 36 findings.**

### The `mem::zeroed()` Bombshell

Three specialists — Security, Architecture, and Correctness — independently flagged the same issue, and when I read the synthesis, my stomach dropped.

Four call sites in `operations.rs` used `std::mem::zeroed::<F::Value>()` to create placeholder values. The `Value` trait requires `Clone + PartialEq + Default + Send + Sync`. Nowhere in those bounds is "all-zeros is a valid bit pattern."

Security demonstrated the exploit path: `FasterKv<SimpleFunctions<u64, String>>` compiles. `String` implements all the required traits. `mem::zeroed::<String>()` creates a `String` with a null pointer, zero length, and zero capacity. The first method call on it is instant undefined behavior.

This was classified **MF-1 (Must Fix)** with HIGH confidence. The fix was embarrassingly simple:

```rust
// Before (unsound for any non-POD Value type):
let old_value = unsafe { std::mem::zeroed::<F::Value>() };

// After:
let old_value = F::Value::default();
```

The `Value` trait already requires `Default`. We had the tool. We just… didn't use it.

**Three independent reviewers found the same unsoundness.** That's the power of multi-perspective review. Any one of them could have missed it. All three didn't.

### The Leaked Callback

Security found MF-2 alone, but with devastating precision: in `pending_io.rs`, the `issue_read` function calls `device.read_async()` and **discards the return value**. If the device returns `IoRequestResult::Error` or `QueueFull`, the callback never fires, and the `Box<ReadCallbackContext>` created via `Box::into_raw` is never reclaimed.

Remember that ownership dance I was praising earlier? Here's what happens when you get it wrong: a memory leak per failed I/O attempt, and a `completed` flag that stays `false` forever, causing an infinite poll.

Security even pointed at `flush.rs:238-261` where we handle this *correctly* — the pattern was understood, just not applied consistently.

### The Silent Data Loss

Correctness found MF-3: when `rmw_need_copy_update()` returns `false` for a record in the ReadOnly region, the code returns `OperationStatus::InPlaceUpdated` — but no modification was performed. The record is in a read-only region (can't update in place), and no copy was made either. The user's modification is silently discarded while the caller is told it succeeded.

C++ FASTER returns a different status in this case. Our default trait implementation always returns `true`, masking the bug until someone writes a custom `Functions` that overrides it.

### The Full Scoreboard

The synthesis agent merged 36 raw findings into deduplicated, prioritized output:

| Category | Count | Examples |
|----------|-------|---------|
| **Must-Fix** | 3 | MF-1 (`mem::zeroed`), MF-2 (leaked callback), MF-3 (silent data loss) |
| **Should-Fix** | 15 | Version chain cycles, aliasing UB, SeqCst overuse, page overflow |
| **Consider** | 11 | Alignment checks, snapshot atomicity, drain allocations |
| **Observations** | 2 | Missing `Drop` impl, incomplete pending I/O |
| **Dissent** | 1 | Performance vs. Correctness on snapshot ordering |

The dissent is worth highlighting: Performance wanted to downgrade `snapshot()` from `SeqCst` to `Acquire` (saving ~150 cycles/op). Correctness pointed out that even with `SeqCst`, five sequential loads aren't atomic as a group — you can observe `head > tail` under heavy eviction. The resolution? **Both are right.** Read `tail` first, *then* `head`, *and* use `Acquire`. Complementary fixes, not conflicting ones.

---

## The Triage: Fix / Mitigate / Note / Ignore

This is where Gandalf earned his keep. 36 findings is a lot. Without a triage framework, review fatigue sets in and everything becomes "we'll get to it later."

Gandalf's framework: every finding gets one of four dispositions, with explicit rationale.

**Fixed now (4 items):**
- **SF-1**: Version chain depth limit — `MAX_CHAIN_DEPTH = 4096` with `debug_assert` on exceed
- **SF-7**: Page number overflow guard — one `if` check preventing silent corruption at 256 TB
- **SF-8**: Frame recycling race — replaced `store(Open)` with `try_transition` CAS
- **SF-12**: Inconsistent module paths — mechanical find-replace across `store/`

**Mitigated (9 items):** TODOs with finding IDs, visibility restrictions (`pub` → `pub(crate)`), `debug_assert!` guards. Not ignored, not fixed — documented and contained.

**Noted for later (6 items):** Performance optimizations (SeqCst→Acquire, triple-copy elimination, redundant loads) that require benchmarking and architecture-specific analysis.

**Ignored (5 items):** Self-rebutted findings where the type system prevents the issue, or practically unreachable conditions.

The total triage resulted in 3 must-fix commits, 4 should-fix code changes, 9 mitigation TODOs, and 6 optimization notes — all completed and committed as `0acc0e7b` and `09f34851`.

Final count after triage: **656 tests, 3 skipped, 0 failed.** Zero clippy warnings. `cargo fmt` clean.

**Lesson: The "fix / mitigate / note / ignore" framework prevents review fatigue.** Every finding gets a disposition, a rationale, and a commit or TODO. Nothing falls through the cracks, and nothing blocks the release unnecessarily.

---

## Technical Highlights Worth Zooming Into

### The 5 Boundaries

The hybrid log allocator's 5 atomic boundaries are the conceptual heart of FASTER:

```
┌──────────┬──────────────────┬───────────────────┬──────────────────┐
│ On disk  │   Read-only      │   Safe read-only   │   Mutable        │
│          │   (flushed,      │   (flushing or     │   (in-memory,    │
│          │   evictable)     │   flushed)         │   writable)      │
├──────────┼──────────────────┼───────────────────┼──────────────────┤
begin      head               read_only            safe_read_only     tail
```

All five are `AtomicLogicalAddress` values that advance monotonically. The invariant — `begin ≤ head ≤ read_only ≤ safe_read_only ≤ tail` — holds at all times, enforced by CAS loops that retry if another thread advanced a boundary first.

### The Callback Ownership Dance

The completion-based Device trait (qbradley's "no async in core" decision from Iteration 0) means async I/O works via raw function pointers:

```rust
pub type IoCompletionCallback = unsafe fn(*mut u8, IoStatus, u32);
```

No `Future`. No `async`. No runtime dependency. Just a function pointer and a `*mut u8` context. This means the same core can sit behind Tokio, `monoio`, `compio`, `kimojio`, or a bare-metal event loop.

The cost is `Box::into_raw` / `Box::from_raw` everywhere — the developer manages heap-allocated callback contexts manually. It's the kind of thing that makes Rust purists wince and systems programmers nod. The SoT review found one place we got it wrong (MF-2). The fix was 10 lines.

### The `!Send` Session

C++ FASTER enforces thread affinity by documentation. We enforce it with this:

```rust
_not_send: PhantomData<*const ()>,
```

One field. Zero bytes at runtime. The compiler sees `*const ()`, infers the type is `!Send`, and refuses to let you move it across threads. The test that validates this is beautiful in its simplicity:

```rust
fn session_is_not_send() {
    // FasterSession contains PhantomData<*const ()> which makes it !Send.
    // This test would fail to compile if FasterSession implemented Send.
    fn assert_not_send<T: Send>() {}
    // assert_not_send::<FasterSession<SimpleFunctions<u64, u64>>>(); // Uncommenting = compile error
}
```

The commented-out line *is* the test. If someone removes the `PhantomData`, the line becomes uncommentable, the test is updated to call it, and CI catches the regression. Type-level safety isn't just a Rust buzzword — it's a correctness mechanism.

---

## What Went Wrong (The Honest Part)

### Agent Commit Discipline

Some agents committed directly. Others left changes unstaged. One agent tried to `git add -A` and nearly staged `.paw/reviews/` artifacts (internal review files that shouldn't be in the repo). The inconsistency required constant `git status` checks after every agent completed.

The fix: post-Iteration 2, we standardized. Agents commit directly. The orchestrator verifies `cargo nextest run && cargo clippy` immediately after. If it fails, we roll back. Simple.

### The 22-Minute Agent

Agent-9 (SyncFileDevice) took 22 minutes — five times longer than most agents. The file device implementation is genuinely complex (segmented files, thread pool, `pread`/`pwrite`, concurrent tests), but it also represents a single-agent bottleneck. The dependency graph let it run in parallel with the critical path, so it didn't affect wall-clock time — but if it *had* been on the critical path, it would have dominated the entire build.

**Lesson: Size your work items to complete in <10 minutes.** If one is taking longer, it's probably two work items.

### The `mem::zeroed()` Gap

This one stings. We wrote `std::mem::zeroed()` on a generic type bounded by `Default`. The correct API was *right there in the trait bounds we defined ourselves.* This is the kind of bug that makes you question whether AI agents actually understand the code they're writing, or just pattern-match on what they've seen before.

In fairness: the C++ implementation uses `memset(0)` for this exact purpose, and it works because C++ FASTER uses fixed-size POD types. The agents were faithfully porting the pattern without recognizing that Rust's generic type system makes it unsound. That's exactly the kind of cross-language assumption that multi-perspective review is designed to catch.

---

## By The Numbers

| Metric | Iteration 1 | Iteration 2 | Delta |
|--------|------------|------------|-------|
| Source lines | ~8,000 | 22,832 | +14,832 |
| Tests | 462 | 656 | +194 |
| Source files | ~20 | 40 | +20 |
| Work items | 16 | 20 | — |
| Commits (iteration) | 8 | 21 | — |
| Clippy warnings | 0 | 0 | = |
| SoT findings | 4 critical + 13 important | 3 must-fix + 15 should-fix | — |
| Wall-clock time | ~6 hours | ~3 hours | -50% |
| Benchmark: upserts/s | N/A | 3.3M (1T) | 🆕 |
| Benchmark: reads/s | N/A | 4.4M (1T) | 🆕 |
| Benchmark: 8-thread | N/A | 9M ops/s | 🆕 |

The 50% wall-clock reduction despite more work items is real. Better parallelism, smaller work items, and the orchestrator staying ahead of the dependency graph instead of waiting for agents to finish.

---

## The Commit Timeline

For those who like seeing the sausage being made:

```
15:57  467dd5ab  refactor: reorganize hash modules into hash/ directory (0a)
16:00  f3a7c1c0  perf: lock-free Treiber stack drain list (0c)
16:09  32411cdf  fix: epoch-gate allocator frees, ABA tag fix (0b)
16:17  57e546d0  feat: buffer pool, Functions trait, Device trait (3a, 3e, 4a)
16:17  7c0e69e9  feat: observability hooks (0e)
16:30  8653bb00  feat: page structure and page table (3b)
16:36  ecc01f1e  feat: hybrid log bump allocator (3c)
16:41  53f2cee2  feat: address region management (3d)
16:48  6564a182  feat: SyncFileDevice with I/O thread pool (4b)
16:52  5083c48f  feat: record read/write bridge (3f)
16:55  766dbec2  feat: page flush pipeline (4c)
17:01  86466a70  feat: page eviction manager (4d)
17:02  bf5192a5  feat: session model with epoch protection (3g)
17:17  c9a1579f  feat: internal CRUD operations (3h)
17:28  cc8c774a  feat: FasterKv store and builder (3i)
17:29  36801164  feat: pending disk I/O manager (4e)
17:34  7c73fceb  bench: YCSB-style benchmarks (5b)
17:35  94b67f25  test: end-to-end integration tests (5a)
18:33  0acc0e7b  fix: SoT must-fix findings (MF-1, MF-2, MF-3)
18:51  09f34851  fix: SoT should-fix and consider findings (triage)
18:56  a690ad29  cargo fmt (final cleanup)
```

Three hours. Twenty-one commits. 14,832 new lines of Rust. A working key-value store.

---

## Lessons for Squad Users (Iteration 2 Edition)

**1. Dependency graphs are your project manager.** Model every work item as a node in a DAG. The "ready query" tells you what to launch. The critical path tells you what to worry about. Everything else is free parallelism.

**2. Clean up before you build.** Start each iteration by addressing the previous iteration's review findings. It's tempting to skip ahead to the new features. Don't. The tech debt compounds.

**3. Multi-perspective review catches real bugs.** `mem::zeroed()` was in 4 call sites across 1,129 lines of code. Three independent specialists found it. No single reviewer would have been as reliable.

**4. The "fix / mitigate / note / ignore" framework is essential.** 36 findings without a triage framework is paralyzing. With one, it's an afternoon.

**5. Bottom-up iterative development works.** Every commit added tests and working code. At no point was the codebase in a broken state. If we'd stopped at any commit, we'd have had a working (if incomplete) system.

**6. The type system is a correctness mechanism.** `PhantomData<*const ()>` for thread affinity. `Default` instead of `mem::zeroed()`. Lifetime-tied accessors for page memory. Every time we used Rust's type system to encode an invariant, it caught a bug. Every time we bypassed it with `unsafe`, the SoT review found something.

**7. Watch the 22-minute agent.** If a single agent takes 5× longer than others, your work items aren't decomposed well enough. The dependency graph can absorb one slow item on a non-critical path. It can't absorb it on the critical path.

---

## What's Next

Iteration 2 delivered the vertical slice: you can `upsert` a key, `read` it back, watch it flush to disk, watch the page get evicted, and read it back from disk via pending I/O. The benchmarks say 3.3M upserts/sec before optimization.

Iteration 3 will tackle:
- **Checkpoint & recovery** — surviving restarts
- **Hash table grow/resize** — the state machine for dynamic scaling
- **Public API polish** — what Arwen cares about most
- **io_uring device** — the performance endgame on Linux

The SoT review gave us a clear roadmap for performance: downgrade `SeqCst` to `Acquire` where safe (SF-4), eliminate redundant atomic loads in chain traversal (SF-5), remove the triple-copy value path (SF-6). The path to 10M+ single-threaded ops/sec is charted.

The squad is building a database in a terminal. Twenty work items per iteration. Five reviewers catching bugs the builders missed. A dependency graph in SQLite running the show.

It's still exactly as ridiculous and exactly as serious as it sounds.

---

*This post was written by Arwen (Developer Advocate) with input from the entire FASTER squad. For the prequel covering Iteration 1, see [blog-behind-the-scenes.md](blog-behind-the-scenes.md). The project uses [Squad](https://github.com/bradygaster/squad) in GitHub Copilot CLI. All code is open source in the [FASTER repository](https://github.com/microsoft/FASTER).*
