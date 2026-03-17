# FASTER Rust Crate: Pending I/O and Hash Index Subsystem Analysis

## Overview

This document provides a detailed exploration of the **pending I/O management**, **status tracking**, and **hash index subsystems** in the FASTER Rust crate (`/home/azureuser/FASTER/rust/crates/faster-core/src/`).

Key files analyzed:
- **Pending I/O:** `/store/pending_io.rs` (1,061 lines)
- **Session/Operations:** `/store/session.rs` (1,611 lines), `/store/operations.rs` (1,799 lines)
- **Hash Index:** `/hash/index.rs` (58,177 bytes), `/hash/table.rs` (58,002 bytes), `/hash/bucket.rs` (77,102 bytes)
- **Status:** `/status.rs` (25,915 bytes)
- **Store main:** `/store/kv.rs` (3,104 lines)

---

## 1. STATUS ARRAY / OPERATION STATUS SUBSYSTEM

### Location
- **Primary:** `/home/azureuser/FASTER/rust/crates/faster-core/src/status.rs` (lines 1-200+)

### No Direct "status_array" - Instead: Enum-based Status Tracking

The codebase does **not** have a `status_array` structure like traditional FASTER implementations. Instead, it uses a **union enum**:

```rust
// /status.rs:64-140
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[must_use]
pub enum OperationStatus {
    Ok,                  // Operation completed successfully
    Pending,             // Record on disk; async I/O issued
    NotFound,            // Key does not exist
    Created,             // New record created at log tail
    InPlaceUpdated,      // Existing record updated in-place in mutable region
    CopyUpdated,         // Record copied to tail and updated
    Deleted,             // Record tombstoned
    Aborted,             // Operation cancelled or could not proceed
}
```

**Size:** 
- `OperationStatus` = 1 byte (enum with `#[repr(?)]` – actually probably 4 bytes on most platforms due to discriminant)
- **No implicit array** — status is returned directly from operations
- Callers match on the enum value to decide control flow

**Tracking Method:**
- Synchronous operations return status immediately
- **Pending operations** enqueued in `Vec<PendingOperation<F>>` instead of a status array
- See Section 2 below

**Grows:** 
- N/A — not a growable structure
- Only the **session's pending queue** grows (see Section 2)

---

## 2. PENDING I/O SUBSYSTEM

### Location
- **Primary:** `/home/azureuser/FASTER/rust/crates/faster-core/src/store/pending_io.rs` (1,061 lines)
- **Session Integration:** `/store/session.rs` (lines 190-450)

### Architecture Overview

```
Operation returns Pending
    ↓
PendingOperation<F> enqueued in session.pending_ops: Vec<T>
    ↓
session.drain_pending_ops() → Vec<PendingOperation<F>>
    ↓
PendingIoManager::issue_read() → PendingIoContext<F>
    ↓
session.enqueue_io_context() → io_contexts: Vec<PendingIoContext<F>>
    ↓
Device callback signals completion via Arc<AtomicBool>
    ↓
session.take_completed_io() → Vec<CompletedIo<F>>
    ↓
Caller retries operation with data now in memory
```

### Data Structures

#### 2.1 PendingOperation<F> (Session-level queue)

**Location:** `/store/session.rs:154-169`

```rust
pub struct PendingOperation<F: Functions> {
    pub op_type: PendingOpType,              // Read, Upsert, Rmw, Delete
    pub key: F::Key,                         // Original key
    pub input: Option<F::Input>,             // User input
    pub context: F::Context,                 // User context
    pub address: LogicalAddress,             // Record's disk address
    pub record_layout: RecordLayout,          // Key/value layout
    pub key_hash: KeyHash,                   // Cached hash
}
```

**Size:**
- **Variable** — depends on `F::Key`, `F::Input`, `F::Context` generic parameters
- For `SimpleFunctions<u64, u64>`: ~48 bytes (u64 key + u64 input + u64 context + address + layout + hash)

**Tracking Location:** `/store/session.rs:200`

```rust
pub struct FasterSession<F: Functions> {
    pending_ops: Vec<PendingOperation<F>>,      // ← Un-dispatched operations
    io_contexts: Vec<PendingIoContext<F>>,      // ← In-flight I/O
    // ... other fields
}
```

**Grows as:** `Vec` grows with `push()` when operations hit disk region (line 276-277)
**Drained by:** `drain_pending_ops()` returns and clears the vector (line 287-289)

#### 2.2 PendingIoContext<F> (In-flight I/O tracking)

**Location:** `/store/pending_io.rs:248-394`

```rust
pub struct PendingIoContext<F: Functions> {
    id: u64,                                    // Unique context ID
    created_at: Instant,                        // Timestamp for timeout tracking
    timeout: Duration,                          // Timeout duration
    operation: PendingOperation<F>,              // Original operation to retry
    buffer: Option<AlignedBuffer>,              // Owned I/O buffer
    record_offset: usize,                       // Byte offset within buffer
    completed: Arc<AtomicBool>,                 // Shared completion flag
    io_status: Arc<Mutex<Option<IoStatus>>>,    // Shared I/O status
    bytes_transferred: Arc<AtomicU32>,          // Shared byte count
}
```

**Size:**
- **Core struct:** ~120 bytes (id:u64, Instant, Duration, operation, offset, Option<buffer>)
- **+ Arc pointers:** 3 × Arc = 24 bytes
- **+ AtomicBool/U32/Mutex:** ~16 bytes
- **Total per context:** ~160 bytes (excluding buffer payload)

**Key Fields:**
- `completed: Arc<AtomicBool>` — Set by device callback (line 180)
- `io_status: Arc<Mutex<Option<IoStatus>>>` — Status written by callback (line 178)
- `bytes_transferred: Arc<AtomicU32>` — Bytes read, written by callback (line 179)

**Polling:**
```rust
// /pending_io.rs:358-360
pub fn is_completed(&self) -> bool {
    self.completed.load(Ordering::Acquire)  // ← Single atomic load
}

pub fn try_complete(mut self) -> Result<CompletedIo<F>, Self> {
    if !self.is_completed() { return Err(self); }
    // Move buffer and operation into CompletedIo
}
```

**Grows as:** `Vec` in session.io_contexts (line 296-297)

#### 2.3 CompletedIo<F> (Completed read result)

**Location:** `/store/pending_io.rs:191-239`

```rust
pub struct CompletedIo<F: Functions> {
    pub operation: PendingOperation<F>,        // Original operation
    pub buffer: AlignedBuffer,                 // Read data
    pub record_offset: usize,                  // Offset of record in buffer
    pub status: IoStatus,                      // I/O completion status
}
```

**Size:**
- Operation (see 2.1) + AlignedBuffer + status
- Buffer size: typically 8 KiB to 64 KiB (sector-aligned)

#### 2.4 PendingIoManager

**Location:** `/store/pending_io.rs:403-576`

```rust
pub struct PendingIoManager {
    buffer_pool: BufferPool,                    // Sector-aligned buffer pool
    page_size: u32,                             // Hybrid log page size (e.g., 32 MiB)
    sector_size: u32,                           // Device sector size (e.g., 4096)
    config: PendingIoConfig,
}
```

**Config:**
```rust
// /pending_io.rs:56-73
pub struct PendingIoConfig {
    pub default_timeout: Duration,              // Default: 30 seconds
    pub max_retries: u32,                       // Default: 3
    pub cancel_on_drop: bool,                   // Default: true
}
```

**Allocation Strategy:**
- `buffer_pool` creates 8 KiB aligned buffers (line 428)
- For each read: allocates sector-aligned chunk (min 8 KiB, max up to full page)

### Operation Flow (Read Hit Disk)

**Location:** `/store/operations.rs:19-40` (region-based dispatch)

1. **Operation hits on-disk region** → returns `Pending` (status.rs:79)
2. **Session enqueues:** `internal_read()` → `session.enqueue_pending(op)` 
3. **Manager issues I/O:**
   ```rust
   // /pending_io.rs:448-527
   let ctx = manager.issue_read(pending_op, device)?;
   // Returns PendingIoContext with:
   // - Device read submitted (async)
   // - Buffer allocated and owned
   // - Completion flag shared with callback via Arc
   ```
4. **Callback fires (on device thread):**
   ```rust
   // /pending_io.rs:173-182
   unsafe fn read_completion_callback(context: *mut u8, status: IoStatus, bytes: u32) {
       let ctx = unsafe { TypedIoContext::from_raw(context) };
       *ctx.io_status.lock() = Some(status);           // Write status
       ctx.bytes_transferred.store(bytes, Ordering::Release);
       ctx.completed.store(true, Ordering::Release);   // Signal completion
   }
   ```
5. **Polling:** Session polls `ctx.is_completed()` with `Acquire` ordering
6. **Completion:** `ctx.try_complete()` moves buffer + operation into `CompletedIo`
7. **Retry:** Caller retries operation with data now in session memory

### Lock/Atomic Patterns

**Completion Flag:** `Arc<AtomicBool>` with `Ordering::Release` (write) / `Acquire` (read)
- Located: `/pending_io.rs:262, 359, 474`

**I/O Status:** `Arc<Mutex<Option<IoStatus>>>`
- Located: `/pending_io.rs:264, 378-382, 475`
- **Mutex is necessary** because status is written by callback and read by caller potentially concurrently

**Bytes Transferred:** `Arc<AtomicU32>` with `Ordering::Release` / `Acquire`
- Located: `/pending_io.rs:266, 363-364, 476`

**Global Context ID Generator:** `AtomicU64::fetch_add(1, Relaxed)`
- Located: `/pending_io.rs:42-53`
- Used for unique tracing/debugging IDs

### O(n) Operations (Scaling Concerns)

#### Session's pending_ops.drain()
- **Location:** `/session.rs:287-289`
- **Complexity:** O(count of un-dispatched operations)
- **Grows with:** Number of operations hitting disk between dispatch batches
- **Typical case:** Small (1-10), can grow large if backpressure stalls dispatch

#### Session's take_completed_io()
- **Location:** `/session.rs:304-317`
- **Complexity:** O(in-flight I/O contexts)
- **Implementation:** Linear scan + drain filter
  ```rust
  for ctx in self.io_contexts.drain(..) {
      match ctx.try_complete() {
          Ok(c) => completed.push(c),       // ← Keep if done
          Err(ctx) => still_pending.push(ctx), // ← Keep if pending
      }
  }
  ```
- **Grows with:** In-flight I/O request count

#### wait_for_all_io() (blocking drain)
- **Location:** `/session.rs:329-350`
- **Complexity:** O(in-flight × wait iterations)
- **Issue:** Spin-waits with `thread::yield_now()` in a loop
  ```rust
  loop {
      match pending_ctx.try_complete() {
          Ok(c) => { completed.push(c); break; }
          Err(ctx) => {
              thread::yield_now();              // ← Busy-wait
              pending_ctx = ctx;
          }
      }
  }
  ```

---

## 3. HASH INDEX SUBSYSTEM

### Location
- **Primary:** `/home/azureuser/FASTER/rust/crates/faster-core/src/hash/index.rs` (58 KB)
- **Hash Table:** `/hash/table.rs` (58 KB)
- **Bucket Layout:** `/hash/bucket.rs` (77 KB)
- **Overflow Pool:** `/hash/overflow.rs` (11 KB)

### Overall Architecture

```
HashIndex
├── HashTable (primary bucket array)
│   ├── buckets: Box<[HashBucket; 2^log2_size]>
│   └── overflow_pool: OverflowBucketPool
├── EpochTable (reclamation)
├── version: AtomicU32 (for table grows)
└── live_entry_count: AtomicU64
```

### 3.1 Hash Bucket Structure

**Location:** `/hash/bucket.rs:465-550`

**Memory Layout (64 bytes = 1 cache line):**
```
Byte:  0    8   16   24   32   40   48   56  64
     ┌────┬────┬────┬────┬────┬────┬────┬────┐
     │E0  │E1  │E2  │E3  │E4  │E5  │E6  │OVF │
     │8B  │8B  │8B  │8B  │8B  │8B  │8B  │8B │
     └────┴────┴────┴────┴────┴────┴────┴────┘
      7 entries + overflow pointer
```

**Rust Definition:**
```rust
// /hash/bucket.rs:500+ (implied structure)
#[repr(C, align(64))]
pub struct HashBucket {
    entries: [AtomicHashBucketEntry; 7],  // 56 bytes
    overflow: AtomicLogicalAddress,        // 8 bytes
}
```

**Bucket Constants:**
- **BUCKET_NUM_ENTRIES = 7** (line 463)
- **Cache-aligned:** `#[repr(C, align(64))]` (line 485)
- **Size:** Exactly 64 bytes

### 3.2 Hash Bucket Entry (Packed 64-bit)

**Location:** `/hash/bucket.rs:100-247`

**Bit Layout (matches C++ `hash_bucket.h`):**
```
Bits: 63     62      61..48           47..0
     ┌──────┬──────┬────────────────┬─────────────────────┐
     │Tent  │Rsvd  │    Tag (14)    │    Address (48)     │
     │(1)   │(1)   │                │                     │
     └──────┴──────┴────────────────┴─────────────────────┘
      └─ Tentative: entry being inserted concurrently
      └─ Reserved: future use (read-cache bit in v2)
      └─ Tag: 14-bit hash fingerprint for fast rejection
      └─ Address: 48-bit logical address in hybrid log
```

**Rust Structure:**
```rust
// /hash/bucket.rs:136-138
#[derive(Clone, Copy)]
#[repr(transparent)]
pub struct HashBucketEntry(u64);  // Exactly 8 bytes
```

**Atomic Wrapper:**
```rust
// /hash/bucket.rs:334-335
#[repr(transparent)]
pub struct AtomicHashBucketEntry(AtomicU64);  // 8 bytes
```

**Size:** 8 bytes per entry, 7 entries per bucket = 56 bytes

**Key Operations:**
```rust
// /hash/bucket.rs:152-247
pub fn address(&self) -> LogicalAddress;
pub fn tag(&self) -> u16;
pub fn is_tentative(&self) -> bool;
pub fn is_empty(&self) -> bool;
pub fn with_tentative(&self) -> Self;
pub fn without_tentative(&self) -> Self;

// Atomic operations
pub fn load(&self, order: Ordering) -> HashBucketEntry;
pub fn compare_exchange(&self, current, new, success_order, fail_order) -> Result;
```

### 3.3 Hash Table

**Location:** `/hash/table.rs:103-150`

**Structure:**
```rust
pub struct HashTable {
    buckets: Box<[HashBucket]>,           // Power-of-two array
    num_buckets: u64,                      // 2^log2_buckets
    log2_buckets: u32,
    overflow_pool: OverflowBucketPool,
}
```

**Size (for 2^16 buckets):**
- `buckets`: 65,536 buckets × 64 bytes = **4 MB**
- `overflow_pool`: Grows on demand (initial page = 1M × 64 bytes = 64 MB)

**Constraints:**
```rust
// /hash/table.rs:119-124
pub const MIN_LOG2_SIZE: u32 = 1;    // 2 buckets
pub const MAX_LOG2_SIZE: u32 = 30;   // 2^30 ≈ 1 billion buckets = 64 GB
```

**Key Operations:**
```rust
// /hash/table.rs: (impl signatures)
pub fn find_entry(hash: KeyHash) -> Option<(HashBucketEntry, &AtomicHashBucketEntry)>;
pub fn find_or_create_entry(hash: KeyHash, addr: LogicalAddress) -> FindOrCreateResult;
pub fn update_entry(slot, old, new) -> bool;  // CAS
```

### 3.4 Hash Index (Epoch-wrapped)

**Location:** `/hash/index.rs:103-180`

**Structure:**
```rust
pub struct HashIndex {
    table: HashTable,                    // Latch-free hash table
    epoch: Arc<EpochTable>,              // Shared epoch for GC
    version: AtomicU32,                  // Table version (grows)
    live_entry_count: AtomicU64,         // Approx entry count
}
```

**Two-phase Insert Protocol:**
1. **Phase 1 — Reserve:** `find_or_create()` CAS-inserts with TENTATIVE bit set
   - Lookups skip tentative entries (not visible to readers)
2. **Phase 2 — Commit:** After log write, `update()` clears TENTATIVE bit
   - Entry now visible to concurrent readers

**Thread Registration:**
```rust
// /hash/index.rs:205-207
pub fn register_thread(&self) -> Option<EpochThread> {
    EpochTable::register(&self.epoch)
}
```

---

## 4. GARBAGE COLLECTION & MAINTENANCE

### 4.1 Entry Invalidation (Page Eviction)

**Location:** `/hash/index.rs:367-480` — `invalidate_entries_in_range()`

**Purpose:** Remove entries whose addresses fall in a page being evicted

**Implementation:**
```rust
pub fn invalidate_entries_in_range(&self, begin: LogicalAddress, end: LogicalAddress) -> u64 {
    let mut invalidated = 0u64;
    let pool = self.table.overflow_pool();

    for bucket_idx in 0..self.table.num_buckets() {      // ← O(n buckets)
        let bucket = &self.table_buckets()[bucket_idx as usize];
        let mut current = bucket;

        loop {
            for i in 0..BUCKET_NUM_ENTRIES {              // ← O(n overflow entries)
                let entry = current.entry(i).load(Ordering::Acquire);
                if entry.is_empty() { continue; }

                let addr_raw = entry.address().raw();
                if addr_raw >= begin_raw && addr_raw < end_raw {
                    // CAS to EMPTY with AcqRel on success
                    if current.entry(i).compare_exchange(
                        entry, HashBucketEntry::EMPTY,
                        Ordering::AcqRel, Ordering::Relaxed,
                    ).is_ok() {
                        invalidated += 1;
                    }
                }
            }

            // Follow overflow chain
            let overflow_addr = current.overflow_address().load(Ordering::Acquire);
            if overflow_addr == LogicalAddress::ZERO { break; }
            current = pool.get(overflow_addr);
        }
    }

    if invalidated > 0 {
        self.live_entry_count.fetch_sub(invalidated, Ordering::Relaxed);
    }
    invalidated
}
```

**Complexity:** **O(num_buckets × avg_overflow_chain_depth × BUCKET_NUM_ENTRIES)**
- Typical: O(n) where n = total entry count
- Runs during page eviction (batch operation, not hot path)

### 4.2 Tentative Entry Cleanup

**Location:** `/hash/index.rs:486-603` — `cleanup_tentative_entries_for_recovery()`

**Purpose:** Remove stale tentative entries left by crashed or aborted inserts during recovery

**⚠️ CRITICAL SAFETY NOTE (lines 492-524):**
> This method MUST ONLY be called during recovery when no insert sessions are active.
> 
> If called concurrently with active `find_or_create()`, the CAS can delete a live 
> tentative entry, causing SILENT DATA LOSS. The inserter's commit CAS will fail 
> (slot now EMPTY), and the record is lost with no error.

**Implementation:**
```rust
pub fn cleanup_tentative_entries_for_recovery(&self) -> u64 {
    let mut cleaned = 0u64;
    let pool = self.table.overflow_pool();

    for bucket_idx in 0..self.table.num_buckets() {
        let bucket = &self.table_buckets()[bucket_idx as usize];
        let mut current = bucket;

        loop {
            for i in 0..BUCKET_NUM_ENTRIES {
                let entry = current.entry(i).load(Ordering::Acquire);
                if !entry.is_empty() && entry.is_tentative() {
                    if current.entry(i).compare_exchange(
                        entry, HashBucketEntry::EMPTY,
                        Ordering::AcqRel, Ordering::Relaxed,
                    ).is_ok() {
                        cleaned += 1;
                    }
                }
            }

            let overflow_addr = current.overflow_address().load(Ordering::Acquire);
            if overflow_addr == LogicalAddress::ZERO { break; }
            current = pool.get(overflow_addr);
        }
    }

    if cleaned > 0 {
        self.live_entry_count.fetch_sub(cleaned, Ordering::Relaxed);
    }
    cleaned
}
```

**Complexity:** **O(num_buckets × avg_overflow_chain_depth × BUCKET_NUM_ENTRIES)**
- Full table scan, comparable to `invalidate_entries_in_range()`

---

## 5. LOCK & ATOMIC PATTERNS

### 5.1 Bucket Entry Access (Hottest Path)

**Lookup with tag filtering:**
```rust
// /hash/bucket.rs:352 (load in lookup path)
let entry = bucket.entry(i).load(Ordering::Acquire);
if entry.tag() != search_tag { continue; }  // ← Tag mismatch, skip
```

**Ordering:**
- `Acquire`: See the record written to the log before entry was stored
- No locking — lock-free atomic load

**Two-phase insert:**
1. **Phase 1 (CAS with tentative):**
   ```rust
   atomic.compare_exchange(
       HashBucketEntry::EMPTY,
       new_entry.with_tentative(),
       Ordering::AcqRel,   // ← Acquire to see prior state, Release to publish
       Ordering::Acquire,  // ← Failed CAS still needs to see actual value
   )
   ```
2. **Phase 2 (CAS clearing tentative):**
   ```rust
   atomic.compare_exchange(
       old_tentative,
       committed_entry,
       Ordering::AcqRel,   // ← Same rationale
       Ordering::Acquire,
   )
   ```

### 5.2 Overflow Chain Traversal

**Pointer load (chain following):**
```rust
// /hash/index.rs:467 (in invalidate_entries_in_range)
let overflow_addr = current.overflow_address().load(Ordering::Acquire);
if overflow_addr == LogicalAddress::ZERO { break; }
current = pool.get(overflow_addr);
```

**Ordering:** `Acquire` ensures the overflow bucket's entries are fully initialized when loaded

### 5.3 Version & Live Count (Loose Consistency)

**Entry count tracking:**
```rust
// /hash/index.rs:115
live_entry_count: AtomicU64,

// Updates use Relaxed (not on hot path)
self.live_entry_count.fetch_add(1, Ordering::Relaxed);
self.live_entry_count.fetch_sub(invalidated, Ordering::Relaxed);
```

**Purpose:** Approximate count (doesn't need to be exact), used for diagnostics

**Version for table grows:**
```rust
// /hash/index.rs:112
version: AtomicU32,  // Toggled on grow completion
```

---

## 6. STORE & SESSION MODULES

### Location & Purpose

**Files:**
- `/store/mod.rs` (71 lines) — Module exports
- `/store/kv.rs` (3,104 lines) — Main FasterKv store
- `/store/session.rs` (1,611 lines) — Session state & pending operations
- `/store/operations.rs` (1,799 lines) — Internal CRUD implementations
- `/store/batch.rs` (517 lines) — Batch operations
- `/store/builder.rs` (491 lines) — Builder pattern
- `/store/functions.rs` (988 lines) — User-defined callbacks
- `/store/pending_io.rs` (1,061 lines) — I/O manager (covered above)

### Main Components

#### 6.1 FasterKv (Top-level Store)

**Location:** `/store/kv.rs:1-100`

```rust
pub struct FasterKv<F: Functions> {
    // Core subsystems
    config: FasterKvConfig,
    hash_index: HashIndex,
    allocator: HybridLogAllocator,
    device: Arc<dyn Device>,
    epoch_table: Arc<EpochTable>,
    pending_io_manager: PendingIoManager,
    
    // Session management
    session_pool: SessionPool<F>,
    
    // Maintenance subsystems
    checkpoint_orchestrator: CheckpointOrchestrator,
    grow_manager: GrowManager,
    compaction_orchestrator: CompactionOrchestrator,
    recovery_manager: RecoveryManager,
}
```

**Size:** Large (contains multiple Arc-wrapped subsystems)

#### 6.2 FasterSession<F> (Pending Operations Storage)

**Location:** `/store/session.rs:190-212` (already covered in Section 2)

```rust
pub struct FasterSession<F: Functions> {
    epoch_thread: EpochThread,
    epoch_table: Arc<EpochTable>,
    pending_ops: Vec<PendingOperation<F>>,      // ← Un-dispatched ops
    io_contexts: Vec<PendingIoContext<F>>,      // ← In-flight I/O
    serial_number: u64,
    in_epoch: bool,
    _not_send: PhantomData<*const ()>,
}
```

**Key Methods (lines 276-450):**
- `enqueue_pending(op)` — Add to pending_ops
- `drain_pending_ops()` — Return and clear pending_ops
- `enqueue_io_context(ctx)` — Add to io_contexts
- `take_completed_io()` — Poll and filter completed I/O
- `wait_for_all_io()` — Spin-wait for all I/O to complete
- `pending_count()` — Returns pending_ops.len() + io_contexts.len()
- `cancel_pending(id)` — Remove by ID
- `cancel_all_expired()` — Remove timeout-expired contexts

#### 6.3 SessionStats (Diagnostics)

**Location:** `/store/session.rs:76-83`

```rust
pub struct SessionStats {
    pub operations_completed: u64,      // Serial number
    pub pending_count: usize,           // pending_ops.len() + io_contexts.len()
    pub current_serial: u64,
}
```

---

## 7. SUMMARY TABLE: SCALING CHARACTERISTICS

| Component | Data Structure | Size | Grows When | O(n) Operations | Bottleneck |
|-----------|---|---|---|---|---|
| **OperationStatus** | Enum | 1-4 B | Never | N/A | N/A |
| **pending_ops Vec** | `Vec<PendingOperation<F>>` | Variable per op | Records hit disk | `drain()` is O(n) | Many pending reads |
| **io_contexts Vec** | `Vec<PendingIoContext<F>>` | ~160 B each | I/O issued | `take_completed_io()` is O(n) | High I/O concurrency |
| **Hash Table** | `Box<[HashBucket; 2^log2]>` | 64 B × 2^log2 | Table grow (rare) | `find()` is O(1)–O(chain) | Overflow chains |
| **Overflow Pool** | Page allocator | 64 MB per page | Overflow chains | `get()` is O(1) | Pool growth |
| **live_entry_count** | `AtomicU64` | 8 B | Every insert/delete | N/A | Atomic contention |
| **invalidate_in_range()** | Full table scan | — | Page eviction | O(n buckets × chain depth) | Large tables |
| **cleanup_tentative()** | Full table scan | — | Recovery | O(n buckets × chain depth) | Large tables |

---

## 8. ATOMIC ORDERING SUMMARY

| Operation | Ordering | Why |
|-----------|----------|-----|
| Bucket entry load (lookup) | `Acquire` | See record written before entry stored |
| CAS (insert tentative) | `AcqRel` / `Acquire` | Acquire prior state, Release new entry |
| CAS (commit) | `AcqRel` / `Acquire` | Same as above |
| CAS (invalidate/cleanup) | `AcqRel` / `Relaxed` | Success: publish removal; Failure: skip |
| Overflow chain load | `Acquire` | See overflow bucket initialization |
| Completion flag (I/O) | `Release` (write) / `Acquire` (read) | Synchronize device callback with poller |
| live_entry_count updates | `Relaxed` | Loose consistency, diagnostic only |

---

## 9. FILE REFERENCE GUIDE

| Subsystem | File | Key Lines |
|-----------|------|-----------|
| **Status** | `/status.rs` | 64–140: enum OperationStatus |
| **Pending Operations** | `/store/session.rs` | 190–212: FasterSession struct; 276–289: enqueue/drain; 304–317: take_completed_io |
| **Pending I/O Manager** | `/store/pending_io.rs` | 403–433: PendingIoManager; 248–394: PendingIoContext; 448–527: issue_read |
| **I/O Callback** | `/store/pending_io.rs` | 173–182: read_completion_callback |
| **Hash Bucket Entry** | `/hash/bucket.rs` | 100–247: HashBucketEntry; 334–415: AtomicHashBucketEntry |
| **Hash Bucket** | `/hash/bucket.rs` | 465–550: HashBucket layout |
| **Hash Table** | `/hash/table.rs` | 103–150: HashTable struct |
| **Hash Index** | `/hash/index.rs` | 103–180: HashIndex struct; 205–241: find/find_or_create/update |
| **Invalidation** | `/hash/index.rs` | 427–480: invalidate_entries_in_range() |
| **Cleanup** | `/hash/index.rs` | 559–603: cleanup_tentative_entries_for_recovery() |
| **Store** | `/store/kv.rs` | 1–100: FasterKv struct definition |

---

## 10. KEY TAKEAWAYS

1. **No "status_array"** — Status is returned as an enum and doesn't grow. Pending ops are tracked in session Vecs instead.

2. **Two-level pending queue:**
   - `pending_ops: Vec` — Un-dispatched operations (hit disk)
   - `io_contexts: Vec` — In-flight I/O with Arc-shared completion flags

3. **Hash index is completely latch-free:**
   - All operations use atomic CAS on 64-bit packed entries
   - Overflow chains extend buckets on demand
   - Full table scans (invalidate, cleanup) are O(n) but batch operations

4. **Atomics use AcqRel on CAS for memory ordering** across insert, commit, invalidation, and cleanup.

5. **Scaling concerns:**
   - `take_completed_io()` scans all in-flight contexts (O(n I/O))
   - `invalidate_entries_in_range()` and `cleanup_tentative()` scan entire hash table (O(n entries))
   - `wait_for_all_io()` spins, potentially busy-waiting

6. **Buffer management:** I/O reads allocate sector-aligned buffers from a pool; buffers are returned after completion.

