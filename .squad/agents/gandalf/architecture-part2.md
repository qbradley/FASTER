<!-- Architecture Part 2 of 3: Sections 6–10 -->
<!-- Author: Gandalf (Lead / System Architect) -->
<!-- Assembled with Parts 1 and 3 into the final Rust FASTER architecture document -->

## 6. Operations Design

This section specifies the complete operation flow for the four fundamental FASTER
operations — Read, Upsert, RMW (Read-Modify-Write), and Delete — translated into
idiomatic Rust with the **no-async-in-core** constraint. Every operation follows a
two-path model: a *fast path* that completes synchronously when the record is in
the mutable in-memory region, and a *slow path* that issues asynchronous I/O and
delivers results through a completion callback when the record resides on disk or
in the read-only region.

### 6.1 Operation Result Model

Before describing individual operations, we establish the unified result type that
every operation returns:

```rust
/// The outcome of a FASTER operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// Operation completed successfully.
    /// Contains fine-grained sub-status for callers who need it.
    Ok(OkKind),

    /// Record is on disk; I/O has been issued and a completion callback will fire.
    /// The caller MUST NOT access output buffers until the pending operation completes.
    Pending,

    /// Key was not found (Read/Delete on absent key).
    NotFound,

    /// Operation must be retried (epoch shift, CAS contention, CPR phase transition).
    /// The session internally retries a bounded number of times before surfacing this.
    RetryLater,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OkKind {
    /// Read: value was copied to output.
    ReadSuccess,
    /// Upsert/RMW: record was updated in-place in the mutable region.
    InPlaceUpdated,
    /// Upsert/RMW: a new record was created at the log tail (copy-update or insert).
    CreatedRecord,
    /// Delete: tombstone was written.
    Deleted,
}
```

**Rationale:** A flat `Result<Status, FasterError>` separates *operational outcomes*
(NotFound, Pending) from *system errors* (I/O failure, corruption). `Status` is
not an error — it's a control-flow signal. True errors (device failure, OOM,
invariant violation) use `FasterError`:

```rust
pub type OperationResult = Result<Status, FasterError>;

#[derive(Debug, thiserror::Error)]
pub enum FasterError {
    #[error("I/O error on device: {0}")]
    Io(#[from] std::io::Error),

    #[error("Record corruption detected at address {address:#x}")]
    Corruption { address: u64 },

    #[error("Session is not active (dropped or not yet created)")]
    SessionInactive,

    #[error("Epoch protection violated: thread not registered")]
    EpochViolation,

    #[error("Checkpoint error: {0}")]
    Checkpoint(String),
}
```

### 6.2 Fast Path vs. Slow Path

Every operation enters through the session, which holds epoch protection for the
duration of the call. The internal dispatch determines the path based on where
the target record lives relative to the hybrid log address boundaries:

```
┌──────────────────────────────────────────────────────────────────┐
│  HYBRID LOG ADDRESS SPACE                                        │
│                                                                  │
│  BeginAddress          HeadAddress      ReadOnlyAddress   Tail   │
│  ──────┼───────────────┼────────────────┼──────────────────┼──── │
│        │  ON DISK       │  READ-ONLY     │  MUTABLE         │    │
│        │  (evicted)     │  (in-memory    │  (in-memory      │    │
│        │                │   immutable)   │   mutable)       │    │
│        │                │                │                  │    │
│        │  SLOW PATH:    │  MEDIUM PATH:  │  FAST PATH:      │    │
│        │  async I/O +   │  copy to tail  │  in-place op     │    │
│        │  callback      │  (no I/O)      │  (no allocation) │    │
└──────────────────────────────────────────────────────────────────┘
```

**Fast path** (mutable region, `address >= read_only_address`):
- Record is in memory and mutable.
- Read: direct copy to output. Upsert: in-place write. RMW: in-place modify. Delete: set tombstone bit.
- No allocation, no I/O. Returns `Status::Ok(...)` immediately.
- Concurrent access is safe: records in the mutable region are accessed under epoch protection, and individual record headers are updated via atomic CAS.

**Medium path** (read-only region, `head_address <= address < read_only_address`):
- Record is in memory but immutable (flushed to disk or being checkpointed).
- Read: direct copy (still in memory). Upsert/RMW/Delete: allocate a new record at the log tail, copy-update, CAS the hash entry to the new address.
- No disk I/O needed, but requires allocation and a CAS loop.
- Returns `Status::Ok(OkKind::CreatedRecord)`.

**Slow path** (on-disk, `address < head_address`):
- Record has been evicted from memory.
- Issue an asynchronous read to the device, register a completion callback.
- Return `Status::Pending` to the caller.
- When I/O completes, the callback fires, the operation retries internally against the now-loaded page, and the result is delivered to the caller's completion context.

### 6.3 Read Operation

```
Read(key, &mut output) → OperationResult
```

**Flow:**

1. **Hash lookup:** Compute `key.hash()`, extract bucket index and 14-bit tag. Scan the bucket (8 entries + overflow chain) for a matching tag.
2. **No match:** Return `Ok(Status::NotFound)`.
3. **Match found:** Extract the 48-bit logical address from the hash entry.
4. **Walk the version chain:** Follow `previous_address` pointers in record headers until we find a record whose key actually matches (tag collisions are possible). If the chain terminates without a match, return `NotFound`.
5. **Address classification:**
   - `address >= read_only_address` → **Fast path.** Call `Functions::read()` with a shared reference to the in-memory value. Copy result to `output`. Return `Ok(ReadSuccess)`.
   - `head_address <= address < read_only_address` → **Medium path** (same as fast path for reads — the record is still in memory, just immutable). Call `Functions::read()`. Return `Ok(ReadSuccess)`.
   - `address < head_address` → **Slow path.** Record is on disk. Allocate a `PendingContext::Read { key, output_slot, user_context }`. Issue `device.read_async(page_address, aligned_buffer, completion_callback)`. Return `Pending`.
6. **Tombstone check:** If the matched record has the tombstone bit set, return `NotFound`.
7. **Read cache (future):** If read cache is enabled, check it before going to disk. On a read cache miss after disk I/O completes, optionally insert the loaded record into the read cache.

**Pending completion for Read:**
When the device I/O completes, the core invokes the registered callback with the
loaded page. The callback:
- Locates the record within the loaded page.
- Calls `Functions::read()` with the on-disk record's value.
- Copies the result into the pending context's output slot.
- Marks the pending operation as complete.

### 6.4 Upsert Operation

```
Upsert(key, value, &mut output) → OperationResult
```

**Flow:**

1. **Hash lookup:** `FindOrCreateEntry(hash)` — returns existing entry or creates a new empty entry in the bucket.
2. **Key not found (new entry):**
   - Allocate a new record at the log tail: `hlog.allocate(record_size)`.
   - Write `RecordInfo` header (version, previous_address = invalid).
   - Write key and value via `Functions::upsert()` (which handles serialization).
   - CAS the hash entry from empty → new address. On CAS failure (another thread inserted first), retry from step 1.
   - Return `Ok(CreatedRecord)`.
3. **Key found, address in mutable region:**
   - Call `Functions::upsert()` with `old_value: Some(&existing_value)`.
   - The function writes the new value in-place.
   - Return `Ok(InPlaceUpdated)`.
4. **Key found, address in read-only or on-disk:**
   - Allocate a new record at the tail.
   - Set `previous_address` to the old record's address (version chain).
   - Write key and new value.
   - CAS the hash entry to point to the new record. On CAS failure, retry.
   - Return `Ok(CreatedRecord)`.

**Critical invariant: Upsert NEVER goes pending.** Even if the old record is on
disk, we don't need to read it — we simply create a new record at the tail. The
old on-disk record becomes unreachable once the hash entry is updated and will
eventually be garbage-collected or compacted. This is a fundamental property
inherited from both C++ and C# implementations.

**CPR interaction:** During checkpoint `IN_PROGRESS` phase, if the existing record
belongs to version `v` and the current thread is in version `v+1`, the new record
must be created with version `v+1` and the old record must not be modified (it
belongs to the checkpoint). The `previous_address` chain preserves the `v` record
for recovery.

### 6.5 RMW (Read-Modify-Write) Operation

```
Rmw(key, input, &mut output) → OperationResult
```

RMW is the most complex operation because it has three sub-paths (initial, in-place,
copy-update) and is the only mutation that can go pending.

**Flow:**

1. **Hash lookup:** `FindOrCreateEntry(hash)`.
2. **Key not found → Initial-update path:**
   - Call `Functions::rmw_need_initial_update(key, input)`. If `false`, return `NotFound` (the user declines to create a default).
   - Allocate a new record at the tail.
   - Call `Functions::rmw_initial(key, input, &mut new_value, &mut output)` to initialize the value (e.g., set a counter to 0, then apply the modification).
   - CAS into hash table. On failure, retry.
   - Return `Ok(CreatedRecord)`.
3. **Key found, address in mutable region → In-place update path:**
   - Call `Functions::rmw_in_place(key, input, &mut existing_value, &mut output)`.
   - If the function returns `InPlaceOk`, the value was modified atomically in place. Return `Ok(InPlaceUpdated)`.
   - If the function returns `NeedsNewRecord` (e.g., variable-length value grew), fall through to the copy-update path.
4. **Key found, address in read-only region → Copy-update path:**
   - Call `Functions::rmw_need_copy_update(key, input, &old_value)`. If `false`, skip (treat as not-found or no-op per user logic).
   - Allocate a new record at the tail.
   - Call `Functions::rmw_copy_update(key, input, &old_value, &mut new_value, &mut output)`.
   - CAS hash entry. On failure, retry.
   - Return `Ok(CreatedRecord)`.
5. **Key found, address on disk → Slow path (pending):**
   - The old value must be read from disk before we can apply the modification.
   - Allocate `PendingContext::Rmw { key, input, user_context }`.
   - Issue async I/O to load the page containing the record.
   - Return `Pending`.
   - On I/O completion, the callback reads the old value from the loaded page, then executes the copy-update sub-path (step 4) against it.

**Tombstone handling:** If the matched record is a tombstone, treat it as key-not-found
and go to the initial-update path (step 2).

**Expired record handling (future):** If an expiration callback is configured and
the record is expired, treat it as key-not-found even if a value exists. This
enables TTL semantics without separate cleanup.

### 6.6 Delete Operation

```
Delete(key) → OperationResult
```

**Flow:**

1. **Hash lookup:** `FindEntry(hash)`.
2. **Key not found:** Return `Ok(Status::NotFound)`. Delete is idempotent.
3. **Key found, address in mutable region:**
   - Set the tombstone bit in the record header via atomic CAS on the `RecordInfo` u64.
   - Optionally call `Functions::delete(key, &mut value)` for cleanup callbacks.
   - Attempt hash chain elision: if the tombstoned record's `previous_address` is invalid, clear the hash entry entirely (saves future lookups).
   - Return `Ok(Deleted)`.
4. **Key found, address in read-only or on-disk:**
   - Allocate a new tombstone record at the log tail (key + tombstone header, no value).
   - CAS hash entry to point to the tombstone.
   - Return `Ok(Deleted)`.

**Critical invariant: Delete NEVER goes pending.** Like Upsert, we never need to
read the old value from disk — we simply create a tombstone at the tail. The
old record becomes unreachable.

### 6.7 Pending Operation Model

This is the crux of the no-async-in-core design. When an operation goes to the
slow path (record on disk), the core does NOT use `async`/`await` or `Future`.
Instead, it uses a **completion callback** model that is runtime-agnostic.

#### Pending Context Lifecycle

```
1. Operation discovers record is on disk (address < head_address).

2. Core allocates a PendingContext on the heap:
   ┌─────────────────────────────────────┐
   │ PendingContext                       │
   │   operation: PendingOp::Read { .. } │
   │   key: K (owned clone)              │
   │   hash: KeyHash                     │
   │   output_slot: *mut Output          │
   │   completion: CompletionSlot        │
   │   user_context: C (user data)       │
   │   retry_count: u32                  │
   └─────────────────────────────────────┘

3. Core issues async I/O to the Device trait:
   device.read_async(
       segment_id,
       offset,
       aligned_buffer,
       io_callback,       // function pointer: fn(*mut u8, IoResult)
       pending_context,   // passed as opaque pointer
   )

4. Core returns Status::Pending to the caller.

5. [Time passes — I/O completes in the device layer]

6. The device calls io_callback(buffer, result, pending_context).
   The callback:
   a. Locates the record in the loaded buffer.
   b. Re-executes the operation logic (now with the record in memory).
   c. Writes the result into the pending context.
   d. Enqueues the completed context into the session's completion queue.

7. Caller retrieves results via one of:
   a. session.complete_pending(wait: true)  — blocks until all pending ops complete.
   b. session.complete_pending(wait: false) — processes only already-completed ops.
   c. session.try_complete_pending()        — non-blocking, returns count completed.
```

#### CompletionSlot: The Runtime-Agnostic Callback

The `CompletionSlot` is how the core communicates completion without knowing about
any async runtime:

```rust
/// A slot that can be signaled when a pending operation completes.
/// This is the core's ONLY interface to the outside world for pending ops.
pub enum CompletionSlot {
    /// No notification requested. Caller will poll via complete_pending().
    None,

    /// A raw callback function pointer + data pointer.
    /// Used by the C FFI layer and by sync callers who want a callback.
    Callback {
        func: unsafe extern "C" fn(result: *const OperationResult, context: *mut c_void),
        context: *mut c_void,
    },

    /// A waker to be notified. Used by async runtime adapters.
    /// The adapter stores a std::task::Waker here. When the operation completes,
    /// core calls waker.wake() — this is the ONLY async-runtime-touching code,
    /// and it lives in the adapter, not in core.
    Waker(std::task::Waker),
}
```

**Rationale for this design:**

1. **No async runtime dependency:** `std::task::Waker` is in the standard library,
   not in Tokio or any runtime. Core merely calls `waker.wake()` — it has no idea
   what happens next.
2. **Naturally maps to blocking:** A sync adapter can use a `Condvar`-based waker
   that blocks the calling thread until completion.
3. **Naturally maps to Futures:** An async adapter wraps the pending operation in
   a `Future` that registers its `Waker` in the `CompletionSlot`. When `wake()` is
   called, the runtime polls the `Future`, which reads the result from the
   `PendingContext`.
4. **C FFI compatible:** The callback variant accepts a C function pointer, enabling
   C callers to get async notifications.
5. **Zero-cost for fast path:** When the operation completes on the fast path (no
   I/O), no `CompletionSlot` is ever allocated or consulted.

#### Thread-Local Completion Queue

Each session maintains a completion queue where finished pending operations are
enqueued by I/O callbacks:

```rust
struct SessionPendingState {
    /// Pending operations awaiting I/O completion.
    /// Keyed by a monotonically increasing serial number.
    pending: HashMap<u64, Box<PendingContext<K, V, C>>>,

    /// Completed operations ready for caller retrieval.
    /// Populated by I/O completion callbacks.
    completed: VecDeque<Box<PendingContext<K, V, C>>>,

    /// Number of outstanding I/O requests (for backpressure).
    io_outstanding: AtomicU32,
}
```

`complete_pending(wait: bool)`:
- Dequeues from `completed`.
- For each completed context, invokes the user's `Functions::read_completion()` or
  `Functions::rmw_completion()` callback (if provided).
- If `wait == true` and `pending` is non-empty, spins/parks until all pending ops
  complete (using the device's polling mechanism).

### 6.8 User-Facing Functions Trait

The `Functions` trait is the Rust equivalent of C#'s `IFunctions` and C++'s
template callback context. It defines user-provided semantics for all operations.

**Design principle:** Collapse C#'s 19 methods into ~8 core methods with sensible
defaults. Use associated types to reduce generic parameter explosion.

```rust
/// User-defined operation semantics for a FASTER store.
///
/// This trait defines how records are read, written, modified, and deleted.
/// Every method has a default implementation that handles the common case
/// (simple value copy / overwrite). Override only what you need.
pub trait Functions {
    /// The key type. Must be hashable and comparable.
    type Key: Hash + Eq + Clone;

    /// The value type stored in the log.
    type Value: Clone;

    /// Caller-provided input passed to RMW and Read operations.
    /// For simple KV stores, this is often `()`.
    type Input;

    /// Output produced by Read and RMW operations.
    /// Typically the same as Value, but can be a projection/aggregation.
    type Output: Default;

    /// Opaque user context carried through pending operations.
    /// Returned to the user on completion. Use `()` if not needed.
    type Context;

    // ── Read ──────────────────────────────────────────────────────────────

    /// Read a record's value and produce output.
    ///
    /// Called on the fast path (record in memory) and on the slow path
    /// (after I/O completion brings the record into memory).
    ///
    /// Default: clones the value into output (requires Value: Into<Output>).
    fn read(
        &self,
        key: &Self::Key,
        value: &Self::Value,
        input: &Self::Input,
        output: &mut Self::Output,
    );

    /// Called when a pending Read completes (optional notification).
    ///
    /// Default: no-op. Override to get notified of async completions
    /// with the user context that was passed to the original operation.
    fn read_completion(
        &self,
        _key: &Self::Key,
        _output: &Self::Output,
        _context: &Self::Context,
        _status: Status,
    ) {}

    // ── Upsert ────────────────────────────────────────────────────────────

    /// Write a value into a record.
    ///
    /// `old_value` is `Some` when updating an existing record in the mutable
    /// region (in-place update). It is `None` when creating a new record.
    ///
    /// Default: overwrites with the input value (requires Input = Value).
    fn upsert(
        &self,
        key: &Self::Key,
        value: &mut Self::Value,
        input: &Self::Input,
        old_value: Option<&Self::Value>,
        output: &mut Self::Output,
    );

    // ── RMW ───────────────────────────────────────────────────────────────

    /// Decide whether to create a new record when the key is not found.
    ///
    /// Default: returns true (always create).
    fn rmw_need_initial_update(
        &self,
        _key: &Self::Key,
        _input: &Self::Input,
    ) -> bool {
        true
    }

    /// Initialize a new record for a key that doesn't exist yet.
    ///
    /// Called when the key is not found and `rmw_need_initial_update` returned true.
    /// Write the initial value into `value`.
    fn rmw_initial(
        &self,
        key: &Self::Key,
        input: &Self::Input,
        value: &mut Self::Value,
        output: &mut Self::Output,
    );

    /// Update a record in-place in the mutable region.
    ///
    /// Returns `InPlaceOk` if the update succeeded, or `NeedsNewRecord` if the
    /// value needs to grow (e.g., variable-length append).
    ///
    /// Default: applies the same logic as `rmw_copy_update`.
    fn rmw_in_place(
        &self,
        key: &Self::Key,
        input: &Self::Input,
        value: &mut Self::Value,
        output: &mut Self::Output,
    ) -> RmwInPlaceResult;

    /// Decide whether to copy a read-only record before updating.
    ///
    /// Default: returns true (always copy-update).
    fn rmw_need_copy_update(
        &self,
        _key: &Self::Key,
        _input: &Self::Input,
        _old_value: &Self::Value,
    ) -> bool {
        true
    }

    /// Create a new record by copying and modifying an existing read-only record.
    ///
    /// Read from `old_value`, apply the modification, write to `new_value`.
    fn rmw_copy_update(
        &self,
        key: &Self::Key,
        input: &Self::Input,
        old_value: &Self::Value,
        new_value: &mut Self::Value,
        output: &mut Self::Output,
    );

    /// Called when a pending RMW completes (optional notification).
    fn rmw_completion(
        &self,
        _key: &Self::Key,
        _output: &Self::Output,
        _context: &Self::Context,
        _status: Status,
    ) {}

    // ── Delete ────────────────────────────────────────────────────────────

    /// Called when a record is being deleted (optional cleanup).
    ///
    /// Default: no-op.
    fn delete(
        &self,
        _key: &Self::Key,
        _value: &mut Self::Value,
    ) {}
}

/// Result of an in-place RMW update.
pub enum RmwInPlaceResult {
    /// Update applied successfully in-place.
    InPlaceOk,
    /// Value needs to be relocated (e.g., variable-length growth).
    /// Core will allocate a new record and call rmw_copy_update.
    NeedsNewRecord,
}
```

**Rationale for the simplification (19 → 8+2 completion callbacks):**

| C# Method | Rust Equivalent | Why Collapsed |
|------------|----------------|---------------|
| `SingleReader` / `ConcurrentReader` | `read()` | Rust's borrow checker enforces shared-ref safety; no need for two variants |
| `SingleWriter` / `ConcurrentWriter` | `upsert()` with `old_value: Option` | Unified via the `Option` parameter |
| `PostSingleWriter` / `PostCopyUpdater` / `PostInitialUpdater` | Integrated into the main methods | Separate "post" hooks add complexity without clear Rust benefit; the main method can do cleanup |
| `NeedInitialUpdate` / `InitialUpdater` | `rmw_need_initial_update()` + `rmw_initial()` | Same pattern, cleaner naming |
| `NeedCopyUpdate` / `CopyUpdater` | `rmw_need_copy_update()` + `rmw_copy_update()` | Same |
| `InPlaceUpdater` | `rmw_in_place()` | Returns enum instead of bool for clarity |
| `SingleDeleter` / `ConcurrentDeleter` | `delete()` | Unified; tombstone mechanics are internal |
| `DisposeSingleWriter` / `DisposeCopyUpdater` / `DisposeInitialUpdater` | Rust `Drop` trait on Value | RAII handles cleanup automatically |
| `ReadCompletionCallback` / `RMWCompletionCallback` | `read_completion()` / `rmw_completion()` | Same pattern, optional |

### 6.9 Error Handling Strategy

Operations return `Result<Status, FasterError>`:

- **`Ok(Status::Ok(...))`** — Operation completed successfully (fast or medium path).
- **`Ok(Status::Pending)`** — Operation issued I/O; result will be delivered via completion callback or `complete_pending()`.
- **`Ok(Status::NotFound)`** — Key does not exist (for Read/Delete). This is not an error.
- **`Ok(Status::RetryLater)`** — Transient condition (CAS contention, epoch shift). The session retries internally up to a configurable limit before surfacing this.
- **`Err(FasterError::Io(...))`** — Device I/O failure. The operation did not complete.
- **`Err(FasterError::Corruption(...))`** — Record integrity check failed.

**Pending operation errors** are delivered through the completion callback or retrievable
from `complete_pending()`. The pending context carries the error:

```rust
/// Result delivered when a pending operation completes.
pub struct PendingResult<O, C> {
    pub status: Result<Status, FasterError>,
    pub output: O,
    pub context: C,
}
```

---

## 7. Checkpoint & Recovery

FASTER's checkpoint/recovery system provides durability and crash recovery through
a cooperative, non-blocking state machine that coordinates all active threads. The
Rust implementation preserves the proven CPR (Concurrent Prefix Recovery) algorithm
from C++/C# while expressing the state machine as an idiomatic Rust enum with
explicit transitions.

### 7.1 Checkpoint Types

FASTER supports three checkpoint types, each with different trade-offs:

#### 7.1.1 Fold-Over Checkpoint

```
Before checkpoint:
  ┌──────────────────────────────────────────────────────┐
  │ HEAD          READ-ONLY           MUTABLE     TAIL   │
  │  ├──────────────┼─────────────────────┼─────────┤    │
  └──────────────────────────────────────────────────────┘

After fold-over:
  ┌──────────────────────────────────────────────────────┐
  │ HEAD     FLUSHED (was mutable)  NEW-MUTABLE   TAIL   │
  │  ├──────────────────────────────────┼──────────┤     │
  └──────────────────────────────────────────────────────┘
  The previously mutable region is flushed to disk and becomes read-only.
  New writes go to the fresh tail region.
```

- **Mechanism:** Flush the current in-memory mutable region to disk. The mutable
  region becomes read-only. New writes proceed at the tail.
- **Persisted:** Hybrid log (on-disk portion + flushed in-memory pages) + hash index pages + metadata.
- **Cost:** Lowest — no data copying, just flushing dirty pages.
- **Trade-off:** Recovery requires replaying the log from `flushedAddress` to `finalAddress`
  to reconstruct the hash index for records that were in the mutable region.
- **When to use:** High-throughput workloads where checkpoint frequency is high and
  recovery time is acceptable. Best for write-heavy workloads.

#### 7.1.2 Snapshot Checkpoint

```
During snapshot:
  ┌──────────────────────────────────────────────────────┐
  │ HEAD          READ-ONLY           MUTABLE     TAIL   │
  │  ├──────────────┼─────────────────────┼─────────┤    │
  └──────────────────────────────────────────────────────┘
                         │
                    snapshot.dat ← copy of in-memory pages
                         │
  Concurrent writes continue in the mutable region.
  CPR ensures snapshot captures a consistent point-in-time view.
```

- **Mechanism:** Copy the in-memory log pages to a separate snapshot file. The main
  log continues accepting writes concurrently (version separation via CPR).
- **Persisted:** Snapshot file + hash index pages + metadata.
- **Cost:** Higher — requires copying the in-memory log (proportional to memory size).
- **Trade-off:** Recovery is faster because the snapshot is a complete point-in-time
  image; no log replay needed.
- **When to use:** When fast recovery is critical and checkpoint frequency is low.

#### 7.1.3 Incremental Snapshot (MVP+)

- **Mechanism:** After an initial snapshot, subsequent checkpoints capture only the
  records that changed (delta log). Recovery applies deltas on top of the base snapshot.
- **Persisted:** Delta log file + metadata referencing the base snapshot.
- **Cost:** Medium — proportional to the number of changes since the last checkpoint,
  not the total log size.
- **When to use:** Update-heavy workloads where most records are unchanged between
  checkpoints. Dramatically reduces checkpoint I/O for large stores with low churn.

**MVP scope:** Fold-over and snapshot checkpoints. Incremental snapshot is deferred
to Full phase per the roadmap (Section 2, decisions.md) but the metadata format
reserves fields for it.

### 7.2 Checkpoint State Machine

The checkpoint state machine coordinates all threads to take a consistent checkpoint
without stopping operations. Each thread independently detects phase transitions
by comparing the global `SystemState` to its local copy, then executes
phase-specific logic before advancing.

#### State Representation

```rust
/// Global system state, packed into a single AtomicU64 for lock-free transitions.
///
/// Layout:
///   [63..56] Action (8 bits)  — what kind of state machine is running
///   [55..48] Phase  (8 bits)  — current phase within that state machine
///   [47..16] Version (32 bits) — monotonically increasing checkpoint version
///   [15..0]  Reserved (16 bits) — future use (e.g., sub-phase)
#[repr(transparent)]
pub struct SystemState(AtomicU64);

/// The type of coordinated action in progress.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Action {
    /// No coordinated action. Normal operation.
    None = 0,
    /// Full checkpoint (index + log).
    CheckpointFull = 1,
    /// Index-only checkpoint.
    CheckpointIndex = 2,
    /// Log-only checkpoint.
    CheckpointLog = 3,
    /// Hash table grow (double).
    GrowIndex = 4,
    /// Garbage collection / log compaction.
    GC = 5,
}

/// Phase within a checkpoint action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Phase {
    /// No active phase. Normal operation.
    Rest = 0,
    /// Prepare index checkpoint (allocate metadata, tokens).
    PrepIndexCheckpoint = 1,
    /// Write hash index pages to disk asynchronously.
    IndexCheckpoint = 2,
    /// Threads acknowledge checkpoint start; capture addresses.
    Prepare = 3,
    /// Version bump (v → v+1). Threads enter new version.
    /// Records created in v+1 are not part of the v checkpoint.
    InProgress = 4,
    /// Wait for all pending I/O operations to complete.
    WaitPending = 5,
    /// Wait for all page flushes to reach disk.
    WaitFlush = 6,
    /// Write metadata, invoke user completion callback.
    PersistenceCallback = 7,
}
```

#### Phase Transition Diagram (Full Checkpoint)

```
                    User calls store.checkpoint(CheckpointType::FoldOver)
                                          │
                                          ▼
  ┌──────────────────────────────────────────────────────────────────────┐
  │  REST (version = v)                                                  │
  │    Global state: Action::None, Phase::Rest, version=v               │
  └──────────────┬───────────────────────────────────────────────────────┘
                 │ Coordinator sets global state to:
                 │   Action::CheckpointFull, Phase::PrepIndexCheckpoint
                 ▼
  ┌──────────────────────────────────────────────────────────────────────┐
  │  PREP_INDEX_CHECKPOINT                                               │
  │    • Allocate unique checkpoint token (UUID).                        │
  │    • Initialize IndexCheckpointInfo and LogCheckpointInfo structs.   │
  │    • Each thread acknowledges by updating its local phase.           │
  │    • When all threads have acknowledged → advance to next phase.     │
  └──────────────┬───────────────────────────────────────────────────────┘
                 ▼
  ┌──────────────────────────────────────────────────────────────────────┐
  │  INDEX_CHECKPOINT                                                    │
  │    • Write hash table pages to disk asynchronously (page by page).   │
  │    • Write overflow buckets to disk.                                 │
  │    • Track outstanding I/O count via AtomicU32.                      │
  │    • Operations continue normally during this phase.                 │
  │    • When all index I/O completes → advance.                         │
  └──────────────┬───────────────────────────────────────────────────────┘
                 ▼
  ┌──────────────────────────────────────────────────────────────────────┐
  │  PREPARE (version = v)                                               │
  │    • Each thread records:                                            │
  │      - checkpoint_start_address = current tail_address               │
  │      - Its current serial number (operation counter)                 │
  │    • Threads close their mutable-region page references.             │
  │    • Thread acknowledges phase; when all acknowledge → advance.      │
  └──────────────┬───────────────────────────────────────────────────────┘
                 ▼
  ┌──────────────────────────────────────────────────────────────────────┐
  │  IN_PROGRESS (version = v+1)                                         │
  │    • Version bumps from v to v+1.                                    │
  │    • Threads entering this phase start creating v+1 records.         │
  │    • Any update to a v record triggers CPR (Copy-on-Write):          │
  │      The v record is preserved, a new v+1 record is created.         │
  │    • This ensures the checkpoint captures a consistent v snapshot.   │
  │    • Thread acknowledges; when all in v+1 → advance.                 │
  └──────────────┬───────────────────────────────────────────────────────┘
                 ▼
  ┌──────────────────────────────────────────────────────────────────────┐
  │  WAIT_PENDING                                                        │
  │    • Each thread completes all its pending I/O operations.           │
  │    • No new pending operations are created for version v.            │
  │    • When all threads report zero pending v ops → advance.           │
  └──────────────┬───────────────────────────────────────────────────────┘
                 ▼
  ┌──────────────────────────────────────────────────────────────────────┐
  │  WAIT_FLUSH                                                          │
  │    • Flush dirty pages from HeadAddress to checkpoint_start_address  │
  │      to the device (fold-over) or to a snapshot file (snapshot).     │
  │    • Track outstanding flush I/O via AtomicU32.                      │
  │    • When all flushes complete → advance.                            │
  └──────────────┬───────────────────────────────────────────────────────┘
                 ▼
  ┌──────────────────────────────────────────────────────────────────────┐
  │  PERSISTENCE_CALLBACK                                                │
  │    • Write checkpoint metadata to disk:                              │
  │      - IndexCheckpointInfo → info.dat + ht.dat                      │
  │      - LogCheckpointInfo → info.dat (+ snapshot.dat if snapshot)     │
  │    • Invoke user's on_checkpoint_complete callback (if registered).  │
  │    • Each thread acknowledges; when all acknowledge → advance.       │
  └──────────────┬───────────────────────────────────────────────────────┘
                 ▼
  ┌──────────────────────────────────────────────────────────────────────┐
  │  REST (version = v+1)                                                │
  │    Global state: Action::None, Phase::Rest, version=v+1             │
  │    Checkpoint complete. System resumes normal operation.             │
  └──────────────────────────────────────────────────────────────────────┘
```

#### Thread Coordination via Epoch Actions

Threads do not poll a shared "all threads done" counter. Instead, each thread
registers an epoch action that fires when it refreshes its epoch:

```rust
impl Session {
    /// Called on every operation (or periodically) to check for phase transitions.
    fn refresh_phase(&mut self) {
        let global = self.store.system_state.load(Ordering::Acquire);
        if global != self.local_system_state {
            self.handle_phase_transition(global);
        }
    }

    fn handle_phase_transition(&mut self, new_state: SystemState) {
        match (self.local_system_state.phase(), new_state.phase()) {
            (Phase::Rest, Phase::PrepIndexCheckpoint) => {
                // Acknowledge transition. No special action needed.
            }
            (Phase::PrepIndexCheckpoint, Phase::Prepare) => {
                // Record checkpoint start address and serial number.
                self.checkpoint_start_address = self.store.log.tail_address();
                self.checkpoint_serial_num = self.current_serial_num;
            }
            (Phase::Prepare, Phase::InProgress) => {
                // Enter new version. All subsequent records are v+1.
                self.version = new_state.version();
            }
            (Phase::InProgress, Phase::WaitPending) => {
                // Complete all pending operations for version v.
                self.complete_pending_for_version(new_state.version() - 1);
            }
            // ... other transitions
            _ => {}
        }
        // Mark this thread as having completed the transition.
        self.store.epoch.mark_phase_complete(self.thread_id);
        self.local_system_state = new_state;
    }
}
```

The coordinator (which can be any thread that initiated the checkpoint) advances
the global phase only after all registered threads have completed the current phase.
This is detected via the epoch system: when a safe epoch is reached where all
threads have acknowledged, the coordinator atomically advances the global state.

### 7.3 What Gets Persisted

A complete checkpoint produces the following on-disk artifacts:

```
{checkpoint_dir}/
├── index-checkpoints/
│   └── {token}/                    # UUID-named directory
│       ├── info.dat                # IndexCheckpointInfo (binary)
│       └── ht.dat                  # Hash table pages (raw binary)
│
├── log-checkpoints/
│   └── {token}/                    # UUID-named directory
│       ├── info.dat                # LogCheckpointInfo (binary)
│       ├── snapshot.dat            # (snapshot type only) Log snapshot
│       └── delta.dat               # (incremental only, future) Delta log
│
└── log/
    ├── log.0                       # Main log segment 0
    ├── log.1                       # Main log segment 1
    └── ...                         # Segments up to current tail
```

#### IndexCheckpointInfo (binary format)

```rust
/// Persisted index checkpoint metadata.
/// Written as a packed binary struct for fast serialization.
#[repr(C, packed)]
pub struct IndexCheckpointInfo {
    /// Format version (for forward compatibility). Currently 1.
    pub format_version: u32,
    /// Unique checkpoint token.
    pub token: Uuid,
    /// Number of main hash table buckets (power of 2).
    pub table_size: u64,
    /// Size of main hash table in bytes.
    pub num_ht_bytes: u64,
    /// Size of overflow buckets in bytes.
    pub num_ofb_bytes: u64,
    /// Number of overflow buckets allocated.
    pub ofb_count: u64,
    /// Log begin address at checkpoint time.
    pub log_begin_address: u64,
    /// Log address where checkpoint scan started.
    pub checkpoint_start_address: u64,
    /// Checksum (XOR of all preceding fields cast to u64).
    pub checksum: u64,
}
```

#### LogCheckpointInfo (binary format)

```rust
/// Persisted hybrid log checkpoint metadata.
#[repr(C)]
pub struct LogCheckpointInfo {
    /// Format version. Currently 1. Will increment for incremental snapshot support.
    pub format_version: u32,
    /// Unique checkpoint token.
    pub token: Uuid,
    /// Checkpoint type (fold-over = 0, snapshot = 1, incremental = 2).
    pub checkpoint_type: u8,
    /// Checkpoint version (v).
    pub version: u32,
    /// Next version (v+1) — for CPR recovery.
    pub next_version: u32,
    /// Address up to which the log has been flushed to the main device.
    pub flushed_logical_address: u64,
    /// Final logical address at checkpoint time (tail at PREPARE phase).
    pub final_logical_address: u64,
    /// Start logical address (begin of addressable log).
    pub start_logical_address: u64,
    /// Begin address (oldest non-truncated address).
    pub begin_address: u64,
    /// Head address at checkpoint time.
    pub head_address: u64,
    /// Snapshot-specific: final address in the snapshot file (−1 if not snapshot).
    pub snapshot_final_logical_address: i64,
    /// Incremental-specific: tail address in the delta log (−1 if not incremental).
    pub delta_tail_address: i64,
    /// Number of sessions with commit points.
    pub num_sessions: u32,
    // Followed by: num_sessions × SessionCommitInfo (variable-length)
    /// Checksum (XOR-based).
    pub checksum: u64,
}

/// Per-session checkpoint commit point.
#[repr(C)]
pub struct SessionCommitInfo {
    /// Session identifier (UUID).
    pub session_id: Uuid,
    /// Last committed operation serial number.
    pub serial_number: u64,
    /// Number of excluded (pending-at-checkpoint-time) operations.
    pub num_exclusions: u32,
    // Followed by: num_exclusions × u64 (excluded serial numbers)
}
```

**Format choice: binary, not text.**

Rationale:
- **Performance:** Binary is faster to serialize/deserialize (no parsing, no allocation for strings).
- **Size:** Binary is smaller (matters for metadata checksum computation).
- **Versioning:** The `format_version` field enables forward-compatible evolution.
  When adding fields, bump the version and append new fields at the end.
- **Trade-off vs. C#'s text format:** C#'s text `info.dat` was chosen for human
  debuggability. In Rust, we provide a `faster-inspect` CLI tool that can dump
  checkpoint metadata in human-readable form, achieving the same debuggability
  without runtime overhead.

**Cross-language compatibility:** The Rust on-disk format is NOT compatible with
C++ or C# checkpoint files. This is intentional:
- The Rust implementation may use different alignment, different metadata fields,
  and a different hash function.
- Cross-language migration would require an explicit export/import tool, which is
  out of scope for v1.
- The format is fully documented here and in the `faster-inspect` tool for
  third-party tooling.

### 7.4 Recovery Flow

Recovery reconstructs a `FasterKv` from a checkpoint. The process is deterministic
and single-threaded (no concurrent operations during recovery).

```
Recovery(checkpoint_token) → Result<RecoveryInfo, FasterError>
```

**Step 1: Locate checkpoint.**
```rust
// Find the latest valid checkpoint, or use a specific token.
let (index_info, log_info) = match token {
    Some(t) => load_checkpoint_metadata(checkpoint_dir, t)?,
    None    => find_latest_checkpoint(checkpoint_dir)?,
};

// Validate: index checkpoint must not be newer than log checkpoint.
if index_info.checkpoint_start_address > log_info.final_logical_address {
    return Err(FasterError::Checkpoint(
        "Index checkpoint is newer than log checkpoint".into()
    ));
}

// Validate checksums.
index_info.verify_checksum()?;
log_info.verify_checksum()?;
```

**Step 2: Restore hash index.**
```rust
// Read ht.dat into memory.
let ht_bytes = device.read_sync(index_path.join("ht.dat"))?;
hash_table.restore_from_bytes(&ht_bytes, index_info.table_size)?;

// Restore overflow buckets if present.
if index_info.num_ofb_bytes > 0 {
    let ofb_bytes = device.read_sync(index_path.join("ofb.dat"))?;
    hash_table.restore_overflow(&ofb_bytes)?;
}
```

**Step 3: Restore hybrid log.**

For **fold-over** checkpoints:
```rust
// Set log address boundaries from metadata.
log.set_begin_address(log_info.begin_address);
log.set_head_address(log_info.head_address);
log.set_read_only_address(log_info.flushed_logical_address);
log.set_tail_address(log_info.final_logical_address);

// Read log pages from flushed_address to final_address into memory.
// These pages were in the mutable region at checkpoint time and were
// flushed to disk as part of the fold-over.
let num_pages = pages_between(log_info.flushed_logical_address,
                              log_info.final_logical_address);
for page_idx in 0..num_pages {
    let page_address = log_info.flushed_logical_address + page_idx * page_size;
    let buffer = log.allocate_page(page_address);
    device.read_sync(log_segment_file(page_address), buffer)?;
}

// Scan recovered pages to undo v+1 records.
// Records with version > log_info.version were created after the checkpoint
// started (during IN_PROGRESS phase) and must be invalidated.
undo_next_version(&mut log, &hash_table, log_info.version)?;
```

For **snapshot** checkpoints:
```rust
// Read the snapshot file into memory.
let snapshot_data = device.read_sync(log_path.join("snapshot.dat"))?;
log.restore_from_snapshot(&snapshot_data, &log_info)?;

// For incremental: apply delta log on top of base snapshot.
if log_info.delta_tail_address >= 0 {
    let delta_data = device.read_sync(log_path.join("delta.dat"))?;
    log.apply_delta(&delta_data, &log_info)?;
}
```

**Step 4: Restore session commit points.**
```rust
let mut commit_points = Vec::with_capacity(log_info.num_sessions as usize);
for session_info in log_info.sessions() {
    commit_points.push(CommitPoint {
        session_id: session_info.session_id,
        serial_number: session_info.serial_number,
        exclusions: session_info.exclusions().to_vec(),
    });
}
```

**Step 5: Return recovery info to caller.**
```rust
/// Information about a completed recovery, returned to the caller.
pub struct RecoveryInfo {
    /// The checkpoint token that was recovered.
    pub token: Uuid,
    /// Per-session commit points. The caller uses these to resume sessions
    /// and determine which operations are durable.
    pub commit_points: Vec<CommitPoint>,
    /// The recovered version number.
    pub version: u32,
}

/// A session commit point from a recovered checkpoint.
pub struct CommitPoint {
    pub session_id: Uuid,
    /// The last operation serial number that is durable.
    pub serial_number: u64,
    /// Serial numbers of operations that went pending during the checkpoint
    /// and are NOT durable. The application must retry these.
    pub exclusions: Vec<u64>,
}
```

The caller can then resume sessions using the commit points:
```rust
let recovery_info = store.recover(Some(checkpoint_token))?;
for cp in &recovery_info.commit_points {
    let session = store.resume_session(cp.session_id, my_functions)?;
    // session.serial_number() == cp.serial_number
    // Application must replay operations after cp.serial_number,
    // excluding cp.exclusions.
}
```

### 7.5 On-Disk Format Versioning

All checkpoint metadata structures include a `format_version` field at offset 0.
This enables non-breaking format evolution:

- **Version 1** (MVP): Current format as specified above.
- **Future versions:** New fields are appended at the end. Readers that encounter
  a higher version than they support will read only the fields they understand,
  with unknown trailing bytes ignored. This is safe because:
  1. The checksum covers all bytes, so corruption is detected.
  2. New fields are additive (no reordering of existing fields).
  3. Readers validate that `format_version >= MINIMUM_SUPPORTED_VERSION`.

If a breaking format change is ever needed (e.g., field reordering), the
`format_version` is bumped to a new major number and the reader includes migration
logic.

---

## 8. Public API Design

This section specifies the complete public API surface of the Rust FASTER core crate.
The design prioritizes safety, ergonomics, and zero-cost abstraction while remaining
agnostic to any async runtime. Every type, trait, and method documented here is part
of the stable public contract.

### 8.1 Top-Level Type Architecture

```rust
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// The core crate's public type hierarchy
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

/// The FASTER key-value store.
///
/// Owns the hash index, hybrid log, epoch system, and device.
/// Thread-safe: can be shared across threads via Arc<FasterKv<K, V, D>>.
/// Operations are performed through Session handles, not directly on the store.
pub struct FasterKv<K, V, D: Device> { /* ... */ }

/// Builder for configuring and constructing a FasterKv instance.
pub struct FasterKvBuilder<K, V, D: Device> { /* ... */ }

/// A per-thread session handle for performing operations.
///
/// NOT Send — must be used on the thread that created it.
/// This is enforced at compile time via PhantomData<*const ()>.
/// Sessions borrow the FasterKv (via Arc) and hold epoch protection.
pub struct Session<'store, K, V, F: Functions<Key = K, Value = V>> { /* ... */ }

/// User-defined operation callbacks (see Section 6.8 for full definition).
pub trait Functions { /* ... */ }

/// Storage device abstraction.
pub trait Device: Send + Sync + 'static { /* ... */ }

/// Operation result (see Section 6.1).
pub type OperationResult = Result<Status, FasterError>;
```

**Relationship diagram:**

```
    Arc<FasterKv<K, V, D>>
         │
         │  store.new_session(functions)
         │
         ▼
    Session<'_, K, V, F>          ◄── NOT Send (thread-local)
         │
         │  session.read(&key, &input, &mut output)
         │  session.upsert(&key, &value, &mut output)
         │  session.rmw(&key, &input, &mut output)
         │  session.delete(&key)
         │  session.complete_pending(wait)
         │
         ▼
    OperationResult
         │
         ├── Ok(Status::Ok(OkKind))        Fast-path completion
         ├── Ok(Status::Pending)            Slow-path, I/O issued
         ├── Ok(Status::NotFound)           Key absent
         └── Err(FasterError)               System error
```

### 8.2 Generic Type Parameters

#### Key Bounds

```rust
/// Trait bound for keys stored in FASTER.
///
/// Keys must be hashable, comparable, cloneable (for pending operations),
/// and serializable to bytes (for on-disk storage).
pub trait Key: Hash + Eq + Clone + Send + Sync + 'static {
    /// Serialize this key into a byte buffer.
    /// Returns the number of bytes written.
    ///
    /// The buffer is guaranteed to be at least `serialized_size()` bytes.
    fn serialize(&self, buf: &mut [u8]) -> usize;

    /// Return the exact serialized size of this key in bytes.
    fn serialized_size(&self) -> usize;

    /// Deserialize a key from a byte buffer.
    fn deserialize(buf: &[u8]) -> Self;
}
```

**Rationale:** We use a custom `Key` trait rather than `AsRef<[u8]>` because:
1. `AsRef<[u8]>` doesn't cover deserialization.
2. Fixed-size keys (u64, u128) can implement zero-copy serialization.
3. Variable-length keys (String, Vec<u8>) need explicit size tracking.
4. We provide blanket implementations for common types:

```rust
// Blanket implementation for fixed-size numeric types.
macro_rules! impl_key_for_numeric {
    ($($t:ty),*) => {
        $(impl Key for $t {
            fn serialize(&self, buf: &mut [u8]) -> usize {
                let bytes = self.to_le_bytes();
                buf[..bytes.len()].copy_from_slice(&bytes);
                bytes.len()
            }
            fn serialized_size(&self) -> usize { std::mem::size_of::<$t>() }
            fn deserialize(buf: &[u8]) -> Self {
                Self::from_le_bytes(buf[..std::mem::size_of::<$t>()].try_into().unwrap())
            }
        })*
    }
}
impl_key_for_numeric!(u8, u16, u32, u64, u128, i8, i16, i32, i64, i128);

// Implementation for byte slices and strings.
impl Key for Vec<u8> { /* length-prefixed serialization */ }
impl Key for String  { /* length-prefixed UTF-8 serialization */ }
```

#### Value Handling: Fixed-Size vs. Variable-Length

```rust
/// Trait bound for values stored in FASTER.
///
/// Values must be cloneable and serializable. Unlike keys, values can
/// be variable-length and may be modified in-place.
pub trait Value: Clone + Send + Sync + 'static {
    /// Serialize this value into a byte buffer.
    fn serialize(&self, buf: &mut [u8]) -> usize;

    /// Return the exact serialized size.
    fn serialized_size(&self) -> usize;

    /// Deserialize a value from a byte buffer.
    fn deserialize(buf: &[u8]) -> Self;

    /// Whether this value type has a fixed serialized size.
    /// If true, the log can optimize allocation by pre-computing record sizes.
    ///
    /// Default: false (variable-length).
    fn is_fixed_size() -> bool { false }

    /// For fixed-size values, return the constant size.
    /// Panics if `is_fixed_size()` returns false.
    fn fixed_size() -> usize {
        panic!("fixed_size() called on variable-length value")
    }
}
```

**Design decision: inline storage (C++ model), not dual-log (C# model).**

All values are stored inline in the hybrid log — key and value are contiguous in the
same record. This is the C++ approach and is simpler, more cache-friendly, and avoids
the complexity of a separate object log with its own GC.

For variable-length values, the record size includes the serialized value bytes.
When a value grows (e.g., appending to a vector), the `rmw_in_place` callback
returns `NeedsNewRecord`, and the core allocates a new, larger record at the tail.

#### Device Trait

```rust
/// Abstraction over storage I/O backends.
///
/// Device implementations are provided per-platform and per-runtime:
/// - `FileDevice` — synchronous file I/O (default, works everywhere)
/// - `NullDevice` — no-op (for testing and benchmarking)
/// - `faster_tokio::TokioDevice` — Tokio-based async file I/O (in adapter crate)
/// - `faster_compio::CompioDevice` — io_uring-based async I/O (in adapter crate)
///
/// The core crate only depends on this trait, not on any concrete implementation.
pub trait Device: Send + Sync + 'static {
    /// Read `buf.len()` bytes from the given segment at the given offset.
    ///
    /// For synchronous devices: blocks until I/O completes, then calls `callback`.
    /// For async-capable devices: issues I/O and calls `callback` on completion.
    fn read_async(
        &self,
        segment_id: u64,
        offset: u64,
        buf: &mut [u8],
        callback: IoCallback,
        context: *mut u8,  // opaque pointer passed through to callback
    );

    /// Write `buf.len()` bytes to the given segment at the given offset.
    fn write_async(
        &self,
        segment_id: u64,
        offset: u64,
        buf: &[u8],
        callback: IoCallback,
        context: *mut u8,
    );

    /// Synchronous read (blocking). Used during recovery.
    fn read_sync(
        &self,
        segment_id: u64,
        offset: u64,
        buf: &mut [u8],
    ) -> Result<(), std::io::Error>;

    /// Synchronous write (blocking). Used during checkpoint metadata writes.
    fn write_sync(
        &self,
        segment_id: u64,
        offset: u64,
        buf: &[u8],
    ) -> Result<(), std::io::Error>;

    /// Physical sector size (for alignment). Typically 512 or 4096.
    fn sector_size(&self) -> u32;

    /// Maximum segment size in bytes.
    fn segment_size(&self) -> u64;

    /// Truncate all segments before the given address.
    /// Used during log compaction and old checkpoint cleanup.
    fn truncate_until(&self, segment_id: u64);
}

/// I/O completion callback signature.
///
/// `result`: Ok(bytes_transferred) or Err(io::Error).
/// `context`: the opaque pointer that was passed to read_async/write_async.
pub type IoCallback = unsafe extern "C" fn(
    result: IoResult,
    context: *mut u8,
);

pub type IoResult = Result<u32, std::io::Error>;
```

**Rationale for `unsafe extern "C" fn` callback:**
- Maximum interoperability with C FFI and with async runtime adapters.
- No allocation overhead (no `Box<dyn FnOnce>`).
- The `unsafe` is contained within the core — public API users never see it.
- The `context: *mut u8` opaque pointer pattern is standard for C-style callbacks.

### 8.3 FasterKv Store

```rust
impl<K: Key, V: Value, D: Device> FasterKv<K, V, D> {
    /// Create a new store using the builder (preferred).
    /// See FasterKvBuilder for configuration options.

    /// Create a new session for performing operations.
    ///
    /// Each thread should create its own session. Sessions are NOT Send.
    /// The session borrows the store via Arc, so the store outlives all sessions.
    pub fn new_session<F: Functions<Key = K, Value = V>>(
        self: &Arc<Self>,
        functions: F,
    ) -> Session<'_, K, V, F>;

    /// Resume a session from a previous checkpoint.
    ///
    /// `session_id` must match a session ID from the recovered checkpoint.
    /// Returns the session and the commit point (last durable serial number + exclusions).
    pub fn resume_session<F: Functions<Key = K, Value = V>>(
        self: &Arc<Self>,
        session_id: Uuid,
        functions: F,
    ) -> Result<(Session<'_, K, V, F>, CommitPoint), FasterError>;

    // ── Checkpointing ────────────────────────────────────────────────────

    /// Initiate a checkpoint. Returns the checkpoint token.
    ///
    /// This is non-blocking: the checkpoint proceeds in the background
    /// as threads cooperate through the state machine.
    /// Use `checkpoint_complete()` or the callback to wait for completion.
    pub fn checkpoint(
        &self,
        checkpoint_type: CheckpointType,
    ) -> Result<Uuid, FasterError>;

    /// Check if the most recent checkpoint has completed.
    pub fn is_checkpoint_complete(&self) -> bool;

    /// Block until the current checkpoint completes.
    pub fn wait_for_checkpoint(&self) -> Result<(), FasterError>;

    /// Register a callback invoked when a checkpoint completes.
    pub fn on_checkpoint_complete<F: Fn(Uuid) + Send + Sync + 'static>(
        &self,
        callback: F,
    );

    // ── Recovery ─────────────────────────────────────────────────────────

    /// Recover the store from a checkpoint.
    ///
    /// If `token` is None, recovers from the latest checkpoint.
    /// Must be called before any sessions are created.
    pub fn recover(&self, token: Option<Uuid>) -> Result<RecoveryInfo, FasterError>;

    // ── Index Management ─────────────────────────────────────────────────

    /// Double the hash index size.
    ///
    /// This is a coordinated operation (like checkpoint) that proceeds
    /// through a state machine as threads cooperate.
    pub fn grow_index(&self) -> Result<(), FasterError>;

    /// Return the number of entries in the hash index.
    pub fn entry_count(&self) -> u64;

    /// Return the hash index size in buckets.
    pub fn index_size(&self) -> u64;

    // ── Log Management ───────────────────────────────────────────────────

    /// Return a read-only accessor to the hybrid log.
    /// Used for iteration/scanning.
    pub fn log(&self) -> &LogAccessor<K, V>;

    /// Current tail address (next write position).
    pub fn tail_address(&self) -> u64;

    /// Current head address (oldest in-memory address).
    pub fn head_address(&self) -> u64;

    /// Current read-only address (boundary between mutable and read-only).
    pub fn read_only_address(&self) -> u64;

    /// Begin address (oldest non-truncated address).
    pub fn begin_address(&self) -> u64;
}
```

### 8.4 Session Lifecycle and Operations

```rust
/// A per-thread session for performing FASTER operations.
///
/// # Thread Safety
///
/// Sessions are NOT Send — they must be used on the thread that created them.
/// This is enforced at compile time. The reason is that sessions hold per-thread
/// epoch state and a mutable cursor into the pending operation queue. Sharing
/// across threads would violate epoch invariants.
///
/// # Lifecycle
///
/// Sessions follow RAII: when dropped, they complete all pending operations,
/// release epoch protection, and deregister from the store.
///
/// # Epoch Protection
///
/// Every operation call automatically enters/exits epoch protection (the
/// "BasicContext" model from C#). For performance-critical code paths that
/// perform many operations in a batch, use `unsafe_enter_epoch()` /
/// `unsafe_exit_epoch()` to amortize the overhead — but you must ensure
/// the epoch is exited before blocking.
pub struct Session<'store, K, V, F: Functions<Key = K, Value = V>> {
    // PhantomData<*const ()> makes this !Send + !Sync
    _not_send: PhantomData<*const ()>,
    // ... internal state
}

impl<'store, K: Key, V: Value, F: Functions<Key = K, Value = V>>
    Session<'store, K, V, F>
{
    // ── Core Operations ──────────────────────────────────────────────────

    /// Read the value for a key.
    ///
    /// On success, the result is written to `output` via `Functions::read()`.
    /// If the record is on disk, returns `Status::Pending`. Call
    /// `complete_pending()` to retrieve the result.
    pub fn read(
        &mut self,
        key: &K,
        input: &F::Input,
        output: &mut F::Output,
    ) -> OperationResult;

    /// Insert or update a value.
    ///
    /// Never goes pending (always completes immediately).
    pub fn upsert(
        &mut self,
        key: &K,
        value: &V,
        input: &F::Input,
        output: &mut F::Output,
    ) -> OperationResult;

    /// Read-modify-write: atomically read and update a value.
    ///
    /// May go pending if the record is on disk (must read before modify).
    pub fn rmw(
        &mut self,
        key: &K,
        input: &F::Input,
        output: &mut F::Output,
    ) -> OperationResult;

    /// Delete a key.
    ///
    /// Never goes pending (always completes immediately via tombstone).
    /// Idempotent: deleting a non-existent key returns NotFound.
    pub fn delete(&mut self, key: &K) -> OperationResult;

    // ── Pending Operation Management ─────────────────────────────────────

    /// Complete pending operations.
    ///
    /// If `wait` is true, blocks until ALL pending operations complete.
    /// If `wait` is false, processes only already-completed operations.
    ///
    /// Returns the number of operations completed.
    pub fn complete_pending(&mut self, wait: bool) -> Result<usize, FasterError>;

    /// Non-blocking: returns the number of pending operations that have
    /// completed since the last call to complete_pending / try_complete_pending.
    pub fn try_complete_pending(&mut self) -> Result<usize, FasterError>;

    /// Return the number of outstanding pending operations.
    pub fn pending_count(&self) -> usize;

    // ── Epoch Management (Advanced) ──────────────────────────────────────

    /// Manually enter epoch protection (unsafe context).
    ///
    /// When in this mode, operations skip the per-operation epoch enter/exit.
    /// You MUST call `unsafe_exit_epoch()` before blocking or yielding.
    ///
    /// # Safety
    /// Failing to exit the epoch before blocking will stall the epoch system
    /// and prevent garbage collection and checkpoint progress.
    pub unsafe fn unsafe_enter_epoch(&mut self);

    /// Exit epoch protection after a batch of operations.
    pub unsafe fn unsafe_exit_epoch(&mut self);

    // ── Metadata ─────────────────────────────────────────────────────────

    /// The session's unique identifier.
    pub fn id(&self) -> Uuid;

    /// The monotonically increasing operation serial number.
    /// Useful for tracking checkpoint commit points.
    pub fn serial_number(&self) -> u64;

    /// Refresh the session's view of the global state.
    /// Called implicitly by operations, but can be called explicitly.
    pub fn refresh(&mut self);
}

impl<'store, K, V, F: Functions<Key = K, Value = V>> Drop
    for Session<'store, K, V, F>
{
    fn drop(&mut self) {
        // 1. Complete all pending operations (best-effort).
        let _ = self.complete_pending(true);
        // 2. Release epoch protection.
        // 3. Deregister session from the store.
    }
}
```

### 8.5 FasterKvBuilder

```rust
/// Builder for constructing a configured FasterKv instance.
///
/// All settings have sensible defaults. The only required parameter is
/// the device (or a path for the default FileDevice).
pub struct FasterKvBuilder<K, V, D: Device> {
    _phantom: PhantomData<(K, V, D)>,
    // ... configuration fields
}

impl<K: Key, V: Value, D: Device> FasterKvBuilder<K, V, D> {
    /// Create a new builder with the given device.
    pub fn new(device: D) -> Self;

    /// Set the initial hash index size in number of buckets.
    /// Must be a power of 2. Default: 2^20 (1M buckets = 64 MB).
    pub fn index_size(mut self, num_buckets: u64) -> Self;

    /// Set the log page size in bytes.
    /// Must be a power of 2. Default: 2^25 (32 MB).
    pub fn page_size(mut self, bytes: u64) -> Self;

    /// Set the total in-memory log size in bytes.
    /// Must be a power of 2 and >= 2 * page_size.
    /// Default: 2^34 (16 GB).
    pub fn memory_size(mut self, bytes: u64) -> Self;

    /// Set the log segment size in bytes.
    /// Controls how large each on-disk log file grows before rolling.
    /// Must be a power of 2 and >= page_size.
    /// Default: 2^30 (1 GB).
    pub fn segment_size(mut self, bytes: u64) -> Self;

    /// Set the mutable fraction of the in-memory log.
    /// Must be in (0.0, 1.0]. Default: 0.9.
    ///
    /// A higher fraction means more records can be updated in-place
    /// (better write performance) but less read-only buffer space
    /// (more frequent page flushes).
    pub fn mutable_fraction(mut self, fraction: f64) -> Self;

    /// Set the checkpoint directory path.
    /// If not set, checkpoints are stored alongside the log device.
    pub fn checkpoint_dir(mut self, path: impl Into<PathBuf>) -> Self;

    /// Enable or disable pre-allocation of log pages.
    /// When true, all in-memory log pages are allocated at startup.
    /// Default: false (allocate lazily).
    pub fn preallocate_log(mut self, enable: bool) -> Self;

    /// Set the maximum number of concurrent pending I/O operations per session.
    /// Default: 4096.
    pub fn max_pending_per_session(mut self, count: u32) -> Self;

    /// Build the FasterKv instance.
    /// Returns an Arc-wrapped instance ready for session creation.
    pub fn build(self) -> Result<Arc<FasterKv<K, V, D>>, FasterError>;
}
```

**Validation in `build()`:** The builder validates all parameters at construction time
and returns a descriptive error if any constraint is violated (e.g., non-power-of-2
sizes, mutable_fraction out of range). This is fail-fast: no runtime panics.

### 8.6 Iterator / Scan API

FASTER supports scanning the hybrid log for all records. This is useful for
offline analytics, data export, and consistency verification.

```rust
/// Read-only accessor for the hybrid log.
pub struct LogAccessor<K, V> { /* ... */ }

impl<K: Key, V: Value> LogAccessor<K, V> {
    /// Scan all live records in the log from `begin_address` to `until_address`.
    ///
    /// Records are yielded in log order (oldest to newest).
    /// Tombstoned and invalidated records are skipped.
    /// For records that appear multiple times (version chain), only the latest is yielded.
    ///
    /// This is a heavyweight operation — it reads the entire on-disk log.
    pub fn scan(
        &self,
        begin_address: u64,
        until_address: u64,
    ) -> LogScanIterator<'_, K, V>;
}

/// Iterator over log records.
///
/// Yields (Key, Value, RecordMetadata) tuples.
/// Automatically issues I/O for on-disk pages and caches loaded pages.
pub struct LogScanIterator<'log, K, V> { /* ... */ }

impl<'log, K: Key, V: Value> Iterator for LogScanIterator<'log, K, V> {
    type Item = Result<ScanRecord<K, V>, FasterError>;

    fn next(&mut self) -> Option<Self::Item>;
}

pub struct ScanRecord<K, V> {
    pub key: K,
    pub value: V,
    pub metadata: RecordMetadata,
}

pub struct RecordMetadata {
    /// Logical address in the hybrid log.
    pub address: u64,
    /// Checkpoint version that created this record.
    pub version: u32,
    /// Whether this record is a tombstone (should not appear in scan, but
    /// available if the caller explicitly requests tombstones).
    pub is_tombstone: bool,
}
```

**Rationale for synchronous Iterator (not async Stream):** The scan API lives in
the core crate, which has no async dependencies. Async adapter crates can wrap
`LogScanIterator` in a `Stream` implementation that yields pages asynchronously.
The synchronous iterator blocks on I/O internally when it needs to load a page
from disk — this is acceptable for offline/batch operations.

### 8.7 Idiomatic Rust Patterns

#### Ownership Model

```
Arc<FasterKv<K, V, D>>
     │
     │  Shared ownership: multiple threads hold Arc clones
     │
     ├── Thread 1: Session<'_, K, V, MyFunctions>  (exclusive, !Send)
     │       │
     │       ├── read(&key, ...)    → borrows &mut self (exclusive)
     │       └── upsert(&key, ...) → borrows &mut self (exclusive)
     │
     ├── Thread 2: Session<'_, K, V, MyFunctions>  (exclusive, !Send)
     │       └── rmw(&key, ...)
     │
     └── Thread 3: Session<'_, K, V, MyFunctions>  (exclusive, !Send)
             └── delete(&key)
```

- **FasterKv** is `Send + Sync` (safe to share via Arc).
- **Session** is `!Send + !Sync` (enforced via `PhantomData<*const ()>`).
  - Operations take `&mut self`, preventing concurrent use of the same session.
  - This matches the mono-threaded session model from C#/C++ and is essential
    for epoch correctness.
- **Functions** implementors must be `Send + Sync` (shared across sessions if
  the same instance is reused, but typically each session gets its own).

#### No Raw Pointers in Public API

The public API exposes zero raw pointers. All pointer-based operations (hash table
CAS, record access, I/O buffers) are internal to the crate and wrapped in safe
abstractions. The `Device` trait uses raw pointers for I/O callbacks, but this is
an implementation detail — public users of the core API (including async adapter
crate authors) interact only with safe Rust types.

The C FFI layer (Section 9) necessarily uses raw pointers, but that is a separate
API surface explicitly marked `unsafe`.

#### Error Handling: Result, Not Panics

The core crate NEVER panics in production code paths. All fallible operations
return `Result`. The only panics are:
- Debug assertions (`debug_assert!`) for internal invariant checks (stripped in release).
- Explicit `unreachable!()` for logically impossible code paths.

#### Send + Sync Bounds

| Type | Send | Sync | Rationale |
|------|------|------|-----------|
| `FasterKv<K,V,D>` | ✅ | ✅ | Shared state protected by atomics and epochs |
| `Session<K,V,F>` | ❌ | ❌ | Thread-local epoch state; must stay on creator thread |
| `FasterKvBuilder` | ✅ | ✅ | Configuration only, no mutable shared state |
| `LogAccessor` | ✅ | ✅ | Read-only view of the log |
| `LogScanIterator` | ❌ | ❌ | Holds mutable iteration state and page cache |
| `Status` | ✅ | ✅ | Value type (Copy) |
| `FasterError` | ✅ | ✅ | Value type |

---

## 9. C FFI Layer

The `faster-ffi` crate exposes FASTER's functionality to C, C++, and any language
with C FFI support. The design uses opaque handles, integer error codes, and explicit
memory ownership to provide a safe, well-documented C API.

### 9.1 Opaque Handle Pattern

All Rust types are hidden behind opaque pointers. C callers never see the internal
layout — they hold only a pointer-sized handle and pass it to API functions.

```rust
// ── Opaque types (never dereferenced by C) ────────────────────────────

/// Opaque handle to a FasterKv store instance.
/// Created by `faster_kv_open()`, freed by `faster_kv_close()`.
#[repr(C)]
pub struct faster_kv_t {
    _opaque: [u8; 0],
}

/// Opaque handle to a session.
/// Created by `faster_session_open()`, freed by `faster_session_close()`.
#[repr(C)]
pub struct faster_session_t {
    _opaque: [u8; 0],
}

/// Opaque handle to a pending operation context.
/// Returned by operations that go pending. Freed after completion retrieval.
#[repr(C)]
pub struct faster_pending_t {
    _opaque: [u8; 0],
}
```

**Implementation pattern:** Each opaque pointer is actually a `Box<RealType>` cast
to a raw pointer via `Box::into_raw()`. On free, it is reconstituted via
`Box::from_raw()` and dropped. This ensures proper cleanup:

```rust
// Internal: create
fn wrap_handle<T>(value: T) -> *mut faster_kv_t {
    Box::into_raw(Box::new(value)) as *mut faster_kv_t
}

// Internal: free
unsafe fn unwrap_handle<T>(handle: *mut faster_kv_t) -> Box<T> {
    Box::from_raw(handle as *mut T)
}
```

### 9.2 Function Naming Convention

All exported functions follow the pattern:
```
faster_{noun}_{verb}[_{qualifier}]()
```

Examples:
- `faster_kv_open()` — open a store
- `faster_kv_close()` — close a store
- `faster_session_open()` — create a session
- `faster_session_read()` — perform a read
- `faster_session_complete_pending()` — complete pending operations
- `faster_error_message()` — get error details

### 9.3 Core API Functions

```c
/* ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
 * Store lifecycle
 * ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━ */

/// Open a FASTER store.
///
/// @param path         Null-terminated UTF-8 path for log storage.
///                     Pass NULL for in-memory-only (NullDevice).
/// @param index_size   Hash index size in buckets (must be power of 2).
/// @param log_size     Total in-memory log size in bytes.
/// @param page_size    Log page size in bytes.
/// @param out_store    On success, receives the store handle.
/// @return             0 on success, negative error code on failure.
int32_t faster_kv_open(
    const char* path,
    uint64_t index_size,
    uint64_t log_size,
    uint64_t page_size,
    faster_kv_t** out_store
);

/// Close a FASTER store and free all resources.
/// All sessions must be closed before calling this.
///
/// @param store        Store handle from faster_kv_open().
/// @return             0 on success, negative error code on failure.
int32_t faster_kv_close(faster_kv_t* store);

/// Initiate a checkpoint.
///
/// @param store        Store handle.
/// @param type         0 = fold-over, 1 = snapshot.
/// @param out_token    On success, receives 16-byte UUID token.
/// @return             0 on success, negative error code on failure.
int32_t faster_kv_checkpoint(
    faster_kv_t* store,
    int32_t type,
    uint8_t out_token[16]
);

/// Recover from a checkpoint.
///
/// @param store        Store handle (must be freshly opened, no sessions).
/// @param token        16-byte UUID token, or NULL for latest checkpoint.
/// @param out_info     On success, receives recovery info. Caller must
///                     free with faster_recovery_info_free().
/// @return             0 on success, negative error code on failure.
int32_t faster_kv_recover(
    faster_kv_t* store,
    const uint8_t token[16],
    faster_recovery_info_t** out_info
);

/* ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
 * Session lifecycle
 * ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━ */

/// User-provided function callbacks for C API.
/// C callers fill in this struct with function pointers.
typedef struct faster_functions_t {
    /// Read callback: copy value to output buffer.
    /// @param key, key_len     Key bytes.
    /// @param value, value_len Value bytes (from stored record).
    /// @param input, input_len User input bytes.
    /// @param output           Output buffer to write to.
    /// @param output_capacity  Max bytes available in output buffer.
    /// @param out_output_len   Actual bytes written to output.
    /// @param user_data        Opaque pointer from faster_session_open().
    void (*read)(
        const uint8_t* key, size_t key_len,
        const uint8_t* value, size_t value_len,
        const uint8_t* input, size_t input_len,
        uint8_t* output, size_t output_capacity, size_t* out_output_len,
        void* user_data
    );

    /// Upsert callback: produce the value to write.
    void (*upsert)(
        const uint8_t* key, size_t key_len,
        const uint8_t* input, size_t input_len,
        const uint8_t* old_value, size_t old_value_len,  /* NULL if new record */
        uint8_t* new_value, size_t new_value_capacity, size_t* out_new_value_len,
        void* user_data
    );

    /// RMW initial callback: create initial value for missing key.
    void (*rmw_initial)(
        const uint8_t* key, size_t key_len,
        const uint8_t* input, size_t input_len,
        uint8_t* value, size_t value_capacity, size_t* out_value_len,
        void* user_data
    );

    /// RMW in-place callback: update value in place.
    /// Returns 0 if update succeeded, 1 if needs new record (value grew).
    int32_t (*rmw_in_place)(
        const uint8_t* key, size_t key_len,
        const uint8_t* input, size_t input_len,
        uint8_t* value, size_t value_len, size_t value_capacity,
        size_t* out_new_value_len,
        void* user_data
    );

    /// RMW copy-update callback: produce new value from old + input.
    void (*rmw_copy_update)(
        const uint8_t* key, size_t key_len,
        const uint8_t* input, size_t input_len,
        const uint8_t* old_value, size_t old_value_len,
        uint8_t* new_value, size_t new_value_capacity, size_t* out_new_value_len,
        void* user_data
    );

    /// Optional: pending operation completion notification.
    /// Called from faster_session_complete_pending() for each completed op.
    void (*completion)(
        int32_t operation_type,  /* 0=read, 1=upsert, 2=rmw, 3=delete */
        int32_t status_code,
        const uint8_t* key, size_t key_len,
        const uint8_t* output, size_t output_len,
        void* user_context,
        void* user_data
    );
} faster_functions_t;

/// Open a session.
///
/// @param store        Store handle.
/// @param functions    User-provided callbacks. The struct is copied — the
///                     caller may free it after this call.
/// @param user_data    Opaque pointer passed to all callbacks.
/// @param out_session  On success, receives the session handle.
/// @return             0 on success, negative error code.
int32_t faster_session_open(
    faster_kv_t* store,
    const faster_functions_t* functions,
    void* user_data,
    faster_session_t** out_session
);

/// Close a session, completing all pending operations.
int32_t faster_session_close(faster_session_t* session);

/* ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
 * Operations
 * ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━ */

/// Perform a read.
///
/// @param session      Session handle.
/// @param key          Key bytes.
/// @param key_len      Key length.
/// @param input        Input bytes (passed to read callback).
/// @param input_len    Input length.
/// @param output       Buffer for output (filled by read callback).
/// @param output_capacity  Max bytes available in output.
/// @param out_output_len   Actual output bytes written (if status >= 0).
/// @param user_context Opaque pointer for pending completion callback.
/// @return             Status code (see faster_status_* constants).
int32_t faster_session_read(
    faster_session_t* session,
    const uint8_t* key, size_t key_len,
    const uint8_t* input, size_t input_len,
    uint8_t* output, size_t output_capacity, size_t* out_output_len,
    void* user_context
);

/// Perform an upsert. Never goes pending.
int32_t faster_session_upsert(
    faster_session_t* session,
    const uint8_t* key, size_t key_len,
    const uint8_t* value, size_t value_len,
    const uint8_t* input, size_t input_len,
    void* user_context
);

/// Perform an RMW. May go pending (returns FASTER_STATUS_PENDING).
int32_t faster_session_rmw(
    faster_session_t* session,
    const uint8_t* key, size_t key_len,
    const uint8_t* input, size_t input_len,
    uint8_t* output, size_t output_capacity, size_t* out_output_len,
    void* user_context
);

/// Perform a delete. Never goes pending.
int32_t faster_session_delete(
    faster_session_t* session,
    const uint8_t* key, size_t key_len
);

/// Complete pending operations.
///
/// @param session      Session handle.
/// @param wait         If non-zero, block until all pending ops complete.
/// @return             Number of completed operations, or negative error code.
int32_t faster_session_complete_pending(
    faster_session_t* session,
    int32_t wait
);
```

### 9.4 Error Model

```c
/* ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
 * Status codes (returned by all functions)
 * ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━ */

#define FASTER_OK                    0   /* Success */
#define FASTER_OK_IN_PLACE           1   /* Updated in-place */
#define FASTER_OK_CREATED            2   /* Created new record */
#define FASTER_OK_DELETED            3   /* Tombstone written */
#define FASTER_STATUS_PENDING        10  /* I/O pending, call complete_pending */
#define FASTER_STATUS_NOT_FOUND      11  /* Key not found */
#define FASTER_STATUS_RETRY          12  /* Transient, retry (internal) */

#define FASTER_ERR_IO               -1   /* I/O error */
#define FASTER_ERR_CORRUPTION       -2   /* Data corruption */
#define FASTER_ERR_INVALID_ARG      -3   /* Invalid argument */
#define FASTER_ERR_SESSION_INACTIVE -4   /* Session not active */
#define FASTER_ERR_EPOCH            -5   /* Epoch violation */
#define FASTER_ERR_CHECKPOINT       -6   /* Checkpoint error */
#define FASTER_ERR_NULL_POINTER     -7   /* NULL pointer argument */
#define FASTER_ERR_UNKNOWN          -99  /* Unknown error */

/// Get a human-readable error message for the last error.
///
/// @param buf          Buffer to write the message into.
/// @param buf_len      Size of the buffer.
/// @return             Number of bytes written (excluding null terminator),
///                     or negative if buffer is too small.
///
/// Thread-local: each thread has its own last-error message.
int32_t faster_error_message(char* buf, size_t buf_len);
```

**Rationale:** Integer error codes are universally understood by C callers. The
`faster_error_message()` function provides details when needed (analogous to
`strerror(errno)` or Win32's `FormatMessage`). The last error is stored in
thread-local storage, so it is safe to call from multiple threads.

### 9.5 Memory Ownership Rules

Clear ownership is critical for a C FFI. The rules are:

| Resource | Allocated By | Freed By | Rule |
|----------|-------------|----------|------|
| `faster_kv_t*` | `faster_kv_open()` | `faster_kv_close()` | Library owns |
| `faster_session_t*` | `faster_session_open()` | `faster_session_close()` | Library owns |
| `faster_recovery_info_t*` | `faster_kv_recover()` | `faster_recovery_info_free()` | Library allocates, caller frees via API |
| Key/value/input byte buffers | Caller | Caller | Caller owns; library reads during call only |
| Output byte buffer | Caller | Caller | Caller provides buffer; library writes into it |
| `faster_functions_t` | Caller | Caller | Library copies the struct; caller may free |
| `user_data` pointer | Caller | Caller | Library passes through to callbacks unchanged |
| `user_context` pointer | Caller | Caller | Library passes through to completion callback |

**Key principle:** The library NEVER frees memory it didn't allocate. The caller
NEVER frees memory it didn't allocate (except via library-provided free functions).
All buffers passed to the library are valid only for the duration of the call (the
library copies what it needs).

### 9.6 Callback Model for Pending Operations in C

When a C operation returns `FASTER_STATUS_PENDING`, the result will be delivered
through the `completion` callback in `faster_functions_t`:

```c
// Example C usage:
void my_completion(int32_t op_type, int32_t status,
                   const uint8_t* key, size_t key_len,
                   const uint8_t* output, size_t output_len,
                   void* user_context, void* user_data)
{
    if (op_type == 0 && status == FASTER_OK) {
        // Read completed. output contains the result.
        printf("Read completed: %.*s\n", (int)output_len, output);
    }
}

// Register the completion callback:
faster_functions_t funcs = {
    .read = my_read,
    .upsert = my_upsert,
    .rmw_initial = my_rmw_initial,
    .rmw_in_place = my_rmw_in_place,
    .rmw_copy_update = my_rmw_copy_update,
    .completion = my_completion,  // ← pending completion callback
};

// Issue a read (might go pending):
int32_t status = faster_session_read(session, key, key_len,
                                     NULL, 0,
                                     output_buf, sizeof(output_buf),
                                     &output_len,
                                     my_user_context);
if (status == FASTER_STATUS_PENDING) {
    // Result will arrive via my_completion callback
    // when we call complete_pending:
    faster_session_complete_pending(session, 1 /* wait */);
}
```

### 9.7 Header Generation

**Decision: use `cbindgen` with hand-curated overrides.**

- **`cbindgen`** generates the C header file from Rust source annotations. This
  ensures the header always matches the implementation.
- **Hand-curated overrides** are used for:
  - The `faster_functions_t` struct (complex callback signatures).
  - Documentation comments (cbindgen's doc generation is basic).
  - Platform-specific `#ifdef` guards.
- The generated header is checked into the repository and CI verifies it matches
  the Rust source (fail if stale).

```
crate: faster-ffi/
├── src/
│   ├── lib.rs          # FFI function implementations
│   ├── handles.rs      # Opaque handle management
│   └── error.rs        # Error code mapping + thread-local error message
├── cbindgen.toml       # cbindgen configuration
├── include/
│   └── faster.h        # Generated (+ hand-curated) C header
└── Cargo.toml
```

### 9.8 Thread Safety Documentation

Every FFI function documents its thread safety in the C header:

```c
/**
 * @brief Open a FASTER store.
 *
 * @thread_safety Thread-safe. May be called from any thread.
 * The returned handle may be used from multiple threads (each thread
 * should open its own session).
 */
int32_t faster_kv_open(...);

/**
 * @brief Perform a read operation.
 *
 * @thread_safety NOT thread-safe per session. Each session handle must
 * be used from a single thread at a time. Multiple sessions may be used
 * concurrently on different threads.
 */
int32_t faster_session_read(...);

/**
 * @brief Close a FASTER store.
 *
 * @thread_safety NOT thread-safe. All sessions must be closed before
 * calling this function. Must not be called concurrently with any other
 * operation on the same store.
 */
int32_t faster_kv_close(...);
```

---

## 10. Async Runtime Integration Layer

This is the keystone of the "no-async-in-core" architecture. The core crate is
purely synchronous, using completion callbacks and `std::task::Waker` (from the
standard library, NOT from any async runtime) as its only concession to the async
world. Thin adapter crates bridge the gap to specific async runtimes.

### 10.1 Architecture Overview

```
┌──────────────────────────────────────────────────────────────────────┐
│                        APPLICATION LAYER                             │
│                                                                      │
│  Sync caller          Tokio caller          Monoio caller            │
│  (blocking)           (async/await)         (async/await)            │
│      │                    │                     │                    │
│      ▼                    ▼                     ▼                    │
│  faster (core)       faster-tokio           faster-monoio            │
│  Session::read()     AsyncSession::read()   AsyncSession::read()     │
│      │                    │                     │                    │
│      │                    │ wraps in Future      │ wraps in Future    │
│      │                    │ + Waker              │ + Waker            │
│      │                    │                     │                    │
│      ▼                    ▼                     ▼                    │
│  ┌──────────────────────────────────────────────────────────────┐    │
│  │                  CORE: CompletionSlot                         │    │
│  │                                                              │    │
│  │  CompletionSlot::None     → sync: caller polls               │    │
│  │  CompletionSlot::Waker(w) → async: w.wake() on completion    │    │
│  │  CompletionSlot::Callback → C FFI: fn ptr invoked            │    │
│  └──────────────────────────────────────────────────────────────┘    │
│      │                    │                     │                    │
│      ▼                    ▼                     ▼                    │
│  ┌──────────────────────────────────────────────────────────────┐    │
│  │                  DEVICE TRAIT                                 │    │
│  │                                                              │    │
│  │  FileDevice        TokioDevice           MonoioDevice         │    │
│  │  (sync I/O)        (tokio::fs)           (io_uring)           │    │
│  │  (threadpool)      (epoll/IOCP)          (io_uring)           │    │
│  └──────────────────────────────────────────────────────────────┘    │
└──────────────────────────────────────────────────────────────────────┘
```

### 10.2 Adapter Crates

Each async runtime gets its own thin adapter crate. These crates are small (~500-1500
lines each) and have exactly two responsibilities:

1. **Provide a runtime-native `Device` implementation** that uses the runtime's
   async I/O primitives.
2. **Provide an `AsyncSession` wrapper** that converts the core's `Status::Pending`
   + `CompletionSlot::Waker` pattern into `Future`s the runtime can `.await`.

```
Workspace layout:
  faster/                       # Core crate (0 async deps)
  faster-tokio/                 # Tokio adapter
  faster-compio/                # Compio adapter (io_uring)
  faster-monoio/                # Monoio adapter (thread-per-core + io_uring)
  faster-kimojio/               # Kimojio adapter (when available)
  faster-ffi/                   # C FFI crate
```

#### faster-tokio

```toml
[package]
name = "faster-tokio"

[dependencies]
faster = { path = "../faster" }
tokio = { version = "1", features = ["fs", "io-util", "rt", "sync"] }
```

**Provides:**
- `TokioDevice` — uses `tokio::fs::File` for async I/O via Tokio's threadpool
  (epoll on Linux, IOCP on Windows).
- `AsyncSession` — wraps `Session` operations in `Future`s.
- `AsyncLogScanStream` — wraps `LogScanIterator` in a `Stream`.

#### faster-compio

```toml
[package]
name = "faster-compio"

[dependencies]
faster = { path = "../faster" }
compio = { version = "0.x", features = ["runtime", "fs"] }
```

**Provides:**
- `CompioDevice` — uses compio's io_uring-based async I/O (Linux-only, zero-copy).
- `AsyncSession` and `AsyncLogScanStream` wrappers.

**Rationale for compio over tokio-uring:** compio provides a cross-platform io_uring
abstraction that also works on Windows (via IOCP) and macOS (via kqueue). It is
actively maintained and designed for the completion-based I/O model that FASTER
naturally fits.

#### faster-monoio

```toml
[package]
name = "faster-monoio"

[dependencies]
faster = { path = "../faster" }
monoio = { version = "0.x" }
```

**Provides:**
- `MonoioDevice` — uses monoio's thread-per-core io_uring backend.
- `AsyncSession` and `AsyncLogScanStream` wrappers.

**Rationale:** Monoio's thread-per-core model aligns perfectly with FASTER's
session-per-thread design. Each monoio worker thread owns a session and a
device — no cross-thread coordination for I/O.

#### faster-kimojio (Future)

Kimojio is a planned/emerging async runtime. The adapter crate will be created
when the runtime stabilizes. The architecture is designed to accommodate it without
any changes to the core crate — only a new adapter is needed.

### 10.3 How Adapters Work: The Future Bridge

The adapter pattern converts the core's synchronous-with-callbacks model into
`async`/`await`-compatible `Future`s. Here is the complete mechanism:

#### Step 1: AsyncSession Wraps Session

```rust
// In faster-tokio/src/session.rs

/// Async-capable session wrapper.
///
/// Wraps a core Session and provides async versions of all operations.
/// Must be used within a Tokio runtime context.
pub struct AsyncSession<'store, K, V, F: Functions<Key = K, Value = V>> {
    inner: Session<'store, K, V, F>,
}

impl<'store, K: Key, V: Value, F: Functions<Key = K, Value = V>>
    AsyncSession<'store, K, V, F>
{
    /// Async read. If the record is in memory, completes immediately
    /// (no runtime interaction). If on disk, suspends the Future until
    /// I/O completes.
    pub async fn read(
        &mut self,
        key: &K,
        input: &F::Input,
        output: &mut F::Output,
    ) -> OperationResult {
        // Fast path: try synchronous operation first.
        let result = self.inner.read(key, input, output);
        match result {
            Ok(Status::Pending) => {
                // Slow path: await the pending operation.
                PendingFuture::new(&mut self.inner).await
            }
            other => other,
        }
    }

    /// Complete all pending operations asynchronously.
    /// Yields to the runtime between completions to avoid starving
    /// other tasks.
    pub async fn complete_pending(&mut self) -> Result<usize, FasterError> {
        let mut completed = 0;
        while self.inner.pending_count() > 0 {
            completed += self.inner.try_complete_pending()?;
            if self.inner.pending_count() > 0 {
                // Yield to runtime, resume when more I/O completes.
                tokio::task::yield_now().await;
            }
        }
        Ok(completed)
    }
}
```

#### Step 2: PendingFuture Registers a Waker

```rust
// In faster-tokio/src/pending.rs

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};

/// A Future that resolves when a pending FASTER operation completes.
///
/// This is the bridge between the core's callback model and Tokio's
/// Future/Waker model. It works by registering the task's Waker in
/// the core's CompletionSlot. When I/O completes, the core calls
/// waker.wake(), the runtime re-polls this Future, and it reads the
/// result from the pending context.
pub struct PendingFuture<'session, 'store, K, V, F: Functions<Key = K, Value = V>> {
    session: &'session mut Session<'store, K, V, F>,
    serial_number: u64,  // identifies which pending op we're waiting for
    registered: bool,
}

impl<'session, 'store, K, V, F: Functions<Key = K, Value = V>> Future
    for PendingFuture<'session, 'store, K, V, F>
{
    type Output = OperationResult;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // Check if the operation has already completed.
        if let Some(result) = self.session.try_get_pending_result(self.serial_number) {
            return Poll::Ready(result);
        }

        // Register the waker so the core can notify us on I/O completion.
        if !self.registered {
            self.session.register_waker(self.serial_number, cx.waker().clone());
            self.registered = true;
        } else {
            // Waker may have changed (runtime may reassign tasks).
            self.session.update_waker(self.serial_number, cx.waker().clone());
        }

        // Check again (race: I/O may have completed between our first check
        // and the waker registration).
        if let Some(result) = self.session.try_get_pending_result(self.serial_number) {
            Poll::Ready(result)
        } else {
            Poll::Pending
        }
    }
}
```

#### Step 3: Core Wakes the Future on I/O Completion

Inside the core (NOT in the adapter), the I/O completion callback does:

```rust
// In faster/src/internal/pending.rs (CORE crate)

fn on_io_complete(pending_ctx: &mut PendingContext) {
    // 1. Re-execute the operation with the loaded page.
    let result = retry_operation_with_loaded_record(pending_ctx);

    // 2. Store the result.
    pending_ctx.result = Some(result);

    // 3. Signal completion via the CompletionSlot.
    match &pending_ctx.completion {
        CompletionSlot::None => {
            // Sync caller will pick this up via complete_pending().
        }
        CompletionSlot::Waker(waker) => {
            // Async adapter: wake the Future.
            waker.wake_by_ref();
        }
        CompletionSlot::Callback { func, context } => {
            // C FFI: invoke the callback.
            unsafe { func(&result as *const _, *context) };
        }
    }

    // 4. Move to completed queue for retrieval.
    session_pending.completed.push_back(pending_ctx);
}
```

**Key insight:** `std::task::Waker` is in the Rust standard library (`core::task`),
NOT in Tokio or any runtime. The core crate calls `waker.wake_by_ref()` without
knowing anything about Tokio, monoio, or any other runtime. This is the critical
design property that enables runtime-agnostic async support.

### 10.4 Device Trait Implementations Per Runtime

Each adapter provides a `Device` implementation using the runtime's native I/O
primitives:

#### TokioDevice

```rust
// In faster-tokio/src/device.rs

pub struct TokioDevice {
    base_path: PathBuf,
    segment_size: u64,
    sector_size: u32,
    runtime_handle: tokio::runtime::Handle,
}

impl Device for TokioDevice {
    fn read_async(
        &self,
        segment_id: u64,
        offset: u64,
        buf: &mut [u8],
        callback: IoCallback,
        context: *mut u8,
    ) {
        let handle = self.runtime_handle.clone();
        let path = self.segment_path(segment_id);
        let buf_ptr = buf.as_mut_ptr();
        let buf_len = buf.len();

        // Spawn a Tokio task to perform the read.
        // When it completes, invoke the core's callback.
        handle.spawn(async move {
            let result = match tokio::fs::File::open(&path).await {
                Ok(mut file) => {
                    file.seek(std::io::SeekFrom::Start(offset)).await?;
                    let slice = unsafe {
                        std::slice::from_raw_parts_mut(buf_ptr, buf_len)
                    };
                    file.read_exact(slice).await.map(|_| buf_len as u32)
                }
                Err(e) => Err(e),
            };
            // Invoke the core's completion callback from within Tokio.
            unsafe { callback(result, context) };
        });
    }

    // write_async: similar pattern with file.write_all()

    fn sector_size(&self) -> u32 { self.sector_size }
    fn segment_size(&self) -> u64 { self.segment_size }
    // ...
}
```

#### CompioDevice (io_uring-based)

```rust
// In faster-compio/src/device.rs

pub struct CompioDevice {
    base_path: PathBuf,
    segment_size: u64,
    sector_size: u32,
}

impl Device for CompioDevice {
    fn read_async(
        &self,
        segment_id: u64,
        offset: u64,
        buf: &mut [u8],
        callback: IoCallback,
        context: *mut u8,
    ) {
        let path = self.segment_path(segment_id);
        let buf_ptr = buf.as_mut_ptr();
        let buf_len = buf.len();

        // Submit an io_uring read via compio.
        // compio handles SQE submission and CQE polling.
        compio::runtime::spawn(async move {
            let file = compio::fs::File::open(&path).await.unwrap();
            let read_buf = Vec::with_capacity(buf_len);
            let (result, read_buf) = file.read_at(read_buf, offset).await;
            match result {
                Ok(n) => {
                    unsafe {
                        std::ptr::copy_nonoverlapping(
                            read_buf.as_ptr(), buf_ptr, n
                        );
                        callback(Ok(n as u32), context);
                    }
                }
                Err(e) => {
                    unsafe { callback(Err(e), context) };
                }
            }
        });
    }
    // ...
}
```

**Key difference from TokioDevice:** Compio uses completion-based I/O (io_uring)
natively, which is a natural fit for FASTER's callback model. The buffer ownership
semantics of io_uring (kernel owns the buffer during I/O) require careful handling
but enable true zero-copy I/O on Linux ≥ 5.1.

#### FileDevice (Synchronous, in core crate)

The core crate includes a basic synchronous device for callers who don't use any
async runtime:

```rust
// In faster/src/device/file_device.rs (CORE crate, no async deps)

pub struct FileDevice {
    base_path: PathBuf,
    segment_size: u64,
    sector_size: u32,
    thread_pool: ThreadPool,  // simple internal thread pool for async I/O
}

impl Device for FileDevice {
    fn read_async(
        &self,
        segment_id: u64,
        offset: u64,
        buf: &mut [u8],
        callback: IoCallback,
        context: *mut u8,
    ) {
        let path = self.segment_path(segment_id);
        let buf_ptr = buf.as_mut_ptr();
        let buf_len = buf.len();

        // Dispatch to internal thread pool for non-blocking behavior.
        self.thread_pool.execute(move || {
            let result = (|| {
                let mut file = std::fs::File::open(&path)?;
                file.seek(std::io::SeekFrom::Start(offset))?;
                let slice = unsafe {
                    std::slice::from_raw_parts_mut(buf_ptr, buf_len)
                };
                file.read_exact(slice)?;
                Ok(buf_len as u32)
            })();
            unsafe { callback(result, context) };
        });
    }
    // ...
}
```

This thread pool is intentionally minimal (no work stealing, no queue depth
optimization). It exists solely to provide baseline async I/O capability for the
core crate without any external dependencies. Production deployments should use a
runtime-native device for optimal performance.

### 10.5 Key Design Principle: No Runtime Types in Core's Public API

The core crate's public API contains ZERO references to Tokio, monoio, compio,
or any other async runtime. Specifically:

| Core API Element | Runtime-Specific Types | Status |
|-----------------|----------------------|--------|
| `FasterKv` | None | ✅ Clean |
| `Session` | None | ✅ Clean |
| `Functions` trait | None | ✅ Clean |
| `Status` enum | None | ✅ Clean |
| `Device` trait | None (`IoCallback` is `extern "C" fn`) | ✅ Clean |
| `CompletionSlot` | `std::task::Waker` (stdlib, not runtime) | ✅ Clean |
| `PendingContext` | None | ✅ Clean |

The **only** type that bridges the async world is `std::task::Waker`, which is
part of the Rust standard library (`core::task::Waker`). It has been stable since
Rust 1.36 and is runtime-agnostic by design.

**What this enables:**

1. **The core compiles with `#![no_std]` + `alloc`** (future goal, not MVP) because
   it has no dependency on `std::net`, `std::fs`, or any I/O library. The `FileDevice`
   is behind a `std` feature flag.
2. **New runtimes can be supported** by writing a ~500-line adapter crate. No changes
   to the core are needed. When kimojio ships, we write `faster-kimojio` and it
   works immediately.
3. **The core can be tested in isolation** using `NullDevice` (immediate callback,
   no real I/O) without any runtime dependency.
4. **C FFI callers** use the core directly (with `FileDevice` or their own `Device`
   implementation) without any Rust async runtime overhead.

### 10.6 PendingOperation Handle Pattern

For ergonomic async usage, adapters provide a `PendingOperation` handle that
implements `IntoFuture`:

```rust
// In faster-tokio/src/session.rs

/// A handle to a potentially-pending operation.
///
/// If the operation completed on the fast path, `.await` returns immediately.
/// If the operation went pending, `.await` suspends until I/O completes.
pub enum PendingOperation<O> {
    /// Operation completed synchronously (fast path).
    Complete(OperationResult, O),
    /// Operation is pending I/O (slow path).
    Pending(PendingFuture</* ... */>),
}

impl<O> IntoFuture for PendingOperation<O> {
    type Output = (OperationResult, O);
    type IntoFuture = impl Future<Output = Self::Output>;

    fn into_future(self) -> Self::IntoFuture {
        async move {
            match self {
                PendingOperation::Complete(result, output) => (result, output),
                PendingOperation::Pending(future) => future.await,
            }
        }
    }
}
```

**Usage from application code:**

```rust
// Tokio application code — clean, idiomatic async/await
use faster_tokio::{AsyncSession, TokioDevice};

#[tokio::main]
async fn main() {
    let device = TokioDevice::new("/data/faster", 1 << 30, 512);
    let store = FasterKvBuilder::new(device)
        .index_size(1 << 20)
        .memory_size(1 << 30)
        .build()
        .unwrap();

    let mut session = store.new_async_session(MyFunctions);

    // Read — might complete instantly or await I/O
    let mut output = 0u64;
    let status = session.read(&42u64, &(), &mut output).await?;
    println!("Read: {:?}, output: {}", status, output);

    // Upsert — always completes immediately (no .await needed, but
    // the API is uniform)
    let status = session.upsert(&42u64, &100u64, &(), &mut output).await?;

    // RMW — might await if record is on disk
    let status = session.rmw(&42u64, &1u64, &mut output).await?;

    // Batch: multiple concurrent reads
    let read1 = session.read(&1u64, &(), &mut out1);
    let read2 = session.read(&2u64, &(), &mut out2);
    let (r1, r2) = tokio::join!(read1, read2);
}
```

**Sync usage (no runtime):**

```rust
// No Tokio, no monoio — just the core crate + FileDevice
use faster::{FasterKvBuilder, Session, FileDevice};

fn main() {
    let device = FileDevice::new("/data/faster", 1 << 30, 512, 4);
    let store = FasterKvBuilder::new(device)
        .index_size(1 << 20)
        .memory_size(1 << 30)
        .build()
        .unwrap();

    let mut session = store.new_session(MyFunctions);

    // Sync read
    let mut output = 0u64;
    let status = session.read(&42u64, &(), &mut output)?;
    if status == Status::Pending {
        session.complete_pending(true)?;  // blocks until I/O done
    }
}
```

### 10.7 Runtime Adapter Comparison

| Feature | faster-tokio | faster-compio | faster-monoio | faster-kimojio |
|---------|-------------|---------------|---------------|----------------|
| I/O model | epoll/IOCP (poll-based) | io_uring (completion-based) | io_uring (completion-based) | TBD |
| Platform | Linux, Windows, macOS | Linux (≥5.1), Windows | Linux (≥5.1) | TBD |
| Threading | Multi-thread work-stealing | Flexible | Thread-per-core | TBD |
| FASTER fit | Good (mature, broad platform) | Excellent (native completion) | Excellent (session=thread) | TBD |
| Zero-copy I/O | No (copies through buffer pool) | Yes (io_uring buffers) | Yes (io_uring buffers) | TBD |
| Crate maturity | Production-ready | Growing | Growing | Planned |
| Recommended for | Default / cross-platform | Linux high-performance | Linux thread-per-core | Future |

**Recommendation:** Start with `faster-tokio` as the reference adapter (broadest
platform support, most mature ecosystem). Implement `faster-compio` second for
Linux io_uring performance. `faster-monoio` follows for thread-per-core deployments.
`faster-kimojio` is created when the runtime is available.

<!-- End of Part 2 (Sections 6–10) -->

