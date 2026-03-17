# FASTER Rust: Detailed Data Structure Reference

## Quick File Index

| Topic | File | Lines |
|-------|------|-------|
| Operation Status | `/status.rs` | 64-140 |
| Pending Operation Type | `/store/session.rs` | 135-144 |
| PendingOperation struct | `/store/session.rs` | 154-169 |
| PendingIoContext struct | `/store/pending_io.rs` | 248-394 |
| PendingIoManager struct | `/store/pending_io.rs` | 403-576 |
| FasterSession struct | `/store/session.rs` | 190-212 |
| HashBucketEntry struct | `/hash/bucket.rs` | 100-247 |
| AtomicHashBucketEntry struct | `/hash/bucket.rs` | 334-415 |
| HashBucket struct | `/hash/bucket.rs` | 465-550 |
| HashTable struct | `/hash/table.rs` | 103-150 |
| HashIndex struct | `/hash/index.rs` | 103-180 |

---

## 1. Status/Operation Status

### File: `/status.rs` (lines 64-140)

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[must_use]
pub enum OperationStatus {
    Ok,                  // Operation completed successfully
    Pending,             // Record on disk; I/O issued
    NotFound,            // Key does not exist
    Created,             // New record created
    InPlaceUpdated,      // Updated in-place in mutable region
    CopyUpdated,         // Copied to tail and updated
    Deleted,             // Tombstoned
    Aborted,             // Cancelled
}
```

**Size:** ~1-4 bytes (enum discriminant)
**Stored:** Returned directly from operations, NOT in an array
**Pending ops instead tracked in:** `/store/session.rs:200` (`Vec<PendingOperation>`)

---

## 2. Pending Operations

### File: `/store/session.rs` (lines 135-169)

#### PendingOpType Enum (lines 135-144)
```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingOpType {
    Read,
    Upsert,
    Rmw,
    Delete,
}
```

#### PendingOperation struct (lines 154-169)
```rust
pub struct PendingOperation<F: Functions> {
    pub op_type: PendingOpType,         // Operation type
    pub key: F::Key,                     // Key (generic)
    pub input: Option<F::Input>,         // Input value (generic, optional)
    pub context: F::Context,             // User context (generic)
    pub address: LogicalAddress,         // Record's disk address
    pub record_layout: RecordLayout,      // Key/value layout
    pub key_hash: KeyHash,               // Pre-computed hash
}
```

**Size:** Variable, depends on F::Key, F::Input, F::Context
- For `SimpleFunctions<u64, u64>`: ~48 bytes
- LogicalAddress: 8 bytes, RecordLayout: 12+ bytes, KeyHash: 8 bytes

**Queue location:** `/store/session.rs:200`
```rust
pub struct FasterSession<F: Functions> {
    pending_ops: Vec<PendingOperation<F>>,    // ← Un-dispatched
    io_contexts: Vec<PendingIoContext<F>>,    // ← In-flight
    // ...
}
```

**Operations:**
- `enqueue_pending(op)` — line 276: `self.pending_ops.push(op)`
- `drain_pending_ops()` — line 288: `std::mem::take(&mut self.pending_ops)`

---

## 3. Pending I/O Context

### File: `/store/pending_io.rs` (lines 248-394)

#### PendingIoContext struct (lines 248-267)
```rust
pub struct PendingIoContext<F: Functions> {
    id: u64,                                    // Unique context ID
    created_at: Instant,                        // Creation timestamp
    timeout: Duration,                          // Timeout duration
    operation: PendingOperation<F>,              // Operation to retry
    buffer: Option<AlignedBuffer>,              // I/O buffer
    record_offset: usize,                       // Byte offset in buffer
    completed: Arc<AtomicBool>,                 // ← Completion flag
    io_status: Arc<Mutex<Option<IoStatus>>>,    // ← Status (mutex!)
    bytes_transferred: Arc<AtomicU32>,          // ← Bytes transferred
}
```

**Size:**
- Core struct: ~120 bytes
- Arc pointers (3): 24 bytes
- Total: ~144 bytes + buffer payload

**Key shared state (Arc):**
- `completed`: Set by device callback (line 180) with `Ordering::Release`
- `io_status`: Mutex-wrapped because callback & poller can race
- `bytes_transferred`: Set by device callback (line 179)

#### Polling (lines 357-360)
```rust
pub fn is_completed(&self) -> bool {
    self.completed.load(Ordering::Acquire)  // Acquire to see callback writes
}

pub fn try_complete(mut self) -> Result<CompletedIo<F>, Self> {
    if !self.is_completed() { return Err(self); }
    
    let status = self.io_status.lock()
        .expect("poisoned")
        .take()
        .unwrap_or(IoStatus::Success);
    
    Ok(CompletedIo {
        operation: self.operation,
        buffer: self.buffer.take().expect("already consumed"),
        record_offset: self.record_offset,
        status,
    })
}
```

**Queue location:** `/store/session.rs:205`
```rust
io_contexts: Vec<PendingIoContext<F>>,   // In-flight I/O
```

**Operations:**
- `enqueue_io_context(ctx)` — line 296: `self.io_contexts.push(ctx)`
- `take_completed_io()` — line 304-317: Linear scan, filter completed, rebuild vec

---

## 4. Pending I/O Manager

### File: `/store/pending_io.rs` (lines 403-576)

#### PendingIoManager struct (lines 403-412)
```rust
pub struct PendingIoManager {
    buffer_pool: BufferPool,                 // Sector-aligned buffers
    page_size: u32,                          // Log page size (32 MB typical)
    sector_size: u32,                        // Disk sector size (4096 typical)
    config: PendingIoConfig,
}
```

#### PendingIoConfig struct (lines 57-73)
```rust
pub struct PendingIoConfig {
    pub default_timeout: Duration,          // Default: 30s
    pub max_retries: u32,                   // Default: 3
    pub cancel_on_drop: bool,               // Default: true
}
```

#### Key methods

**issue_read() — lines 448-527**
```rust
pub fn issue_read<F: Functions>(
    &self,
    pending_op: PendingOperation<F>,
    device: &dyn Device,
) -> Result<PendingIoContext<F>, PendingIoError> {
    // 1. Align read to sector boundary
    let addr = pending_op.address;
    let page = addr.page();
    let offset_in_page = addr.offset().0;
    let sector_start = (offset_in_page / self.sector_size) * self.sector_size;
    let record_offset = (offset_in_page - sector_start) as usize;
    
    // 2. Calculate read size (min 8 KiB, aligned to sector)
    let min_read = offset_in_page - sector_start + DEFAULT_READ_CHUNK;  // 8 KiB
    let read_size = align_to_sector(min_read, self.sector_size);
    
    // 3. Calculate device offset
    let device_offset = page.0 as u64 * self.page_size as u64 + sector_start as u64;
    
    // 4. Allocate buffer from pool
    let mut buffer = self.buffer_pool.acquire(read_size as usize);
    
    // 5. Create Arc-shared completion state
    let completed = Arc::new(AtomicBool::new(false));
    let io_status = Arc::new(Mutex::new(None));
    let bytes_transferred = Arc::new(AtomicU32::new(0));
    
    // 6. Wrap callback context
    let io_ctx = TypedIoContext::new(ReadCallbackContext {
        completed: Arc::clone(&completed),
        io_status: Arc::clone(&io_status),
        bytes_transferred: Arc::clone(&bytes_transferred),
    });
    
    // 7. Submit to device
    let result = unsafe {
        device.read_async(
            device_offset,
            buffer.as_mut_ptr(),
            read_size,
            read_completion_callback as IoCompletionCallback,
            io_ctx.as_raw(),
        )
    };
    
    match result {
        IoRequestResult::Submitted | IoRequestResult::CompletedSync => {
            Ok(PendingIoContext {
                id: next_context_id(),
                created_at: Instant::now(),
                timeout: self.config.default_timeout,
                operation: pending_op,
                buffer: Some(buffer),
                record_offset,
                completed,
                io_status,
                bytes_transferred,
            })
        }
        // Error cases omitted
    }
}
```

**Callback — lines 173-182**
```rust
unsafe fn read_completion_callback(context: *mut u8, status: IoStatus, bytes: u32) {
    let ctx = unsafe { TypedIoContext::<ReadCallbackContext>::from_raw(context) };
    
    *ctx.io_status.lock().expect("io_status mutex poisoned") = Some(status);
    ctx.bytes_transferred.store(bytes, Ordering::Release);  // Release
    ctx.completed.store(true, Ordering::Release);           // Release
}
```

---

## 5. FasterSession State

### File: `/store/session.rs` (lines 190-450)

#### Main struct (lines 190-212)
```rust
pub struct FasterSession<F: Functions> {
    epoch_thread: crate::epoch::EpochThread,      // Epoch slot
    epoch_table: Arc<EpochTable>,                  // Shared epoch
    
    // PENDING OPERATION QUEUES:
    pending_ops: Vec<PendingOperation<F>>,         // Un-dispatched operations
    io_contexts: Vec<PendingIoContext<F>>,         // In-flight I/O
    
    serial_number: u64,                            // Operation counter
    in_epoch: bool,                                // Epoch protection flag
    _not_send: PhantomData<*const ()>,             // Thread-affine
}
```

#### Critical methods

**enqueue_pending — line 276-277**
```rust
pub(crate) fn enqueue_pending(&mut self, op: PendingOperation<F>) {
    self.pending_ops.push(op);
}
```

**drain_pending_ops — line 287-289**
```rust
pub(crate) fn drain_pending_ops(&mut self) -> Vec<PendingOperation<F>> {
    std::mem::take(&mut self.pending_ops)  // O(1), moves ownership
}
```

**enqueue_io_context — line 296-297**
```rust
pub(crate) fn enqueue_io_context(&mut self, ctx: PendingIoContext<F>) {
    self.io_contexts.push(ctx);
}
```

**take_completed_io — line 304-317** [O(n)]
```rust
pub(crate) fn take_completed_io(&mut self) -> Vec<CompletedIo<F>> {
    let mut completed = Vec::new();
    let mut still_pending = Vec::new();

    for ctx in self.io_contexts.drain(..) {  // ← Drain all
        match ctx.try_complete() {
            Ok(c) => completed.push(c),       // ← Keep if done
            Err(ctx) => still_pending.push(ctx), // ← Keep if pending
        }
    }

    self.io_contexts = still_pending;  // ← Rebuild vec
    completed
}
```

**wait_for_all_io — line 329-350** [BLOCKING, SPINS]
```rust
pub(crate) fn wait_for_all_io(&mut self) -> Vec<CompletedIo<F>> {
    let mut completed = Vec::with_capacity(self.io_contexts.len());

    for ctx in self.io_contexts.drain(..) {
        let mut pending_ctx = ctx;
        loop {
            match pending_ctx.try_complete() {
                Ok(c) => {
                    completed.push(c);
                    break;
                }
                Err(ctx) => {
                    thread::yield_now();     // ← SPIN-WAIT
                    pending_ctx = ctx;
                }
            }
        }
    }

    completed
}
```

**pending_count — line 369**
```rust
pub fn pending_count(&self) -> usize {
    self.pending_ops.len() + self.io_contexts.len()  // O(1)
}
```

**cancel_pending — line 429-441** [O(n)]
```rust
pub fn cancel_pending(&mut self, context_id: u64) -> Result<(), PendingIoError> {
    let pos = self.io_contexts
        .iter()
        .position(|ctx| ctx.id() == context_id);  // ← Linear search
    match pos {
        Some(idx) => {
            let _removed = self.io_contexts.swap_remove(idx);
            Ok(())
        }
        None => Err(PendingIoError::Cancelled { context_id }),
    }
}
```

---

## 6. Hash Bucket Entry (64-bit packed)

### File: `/hash/bucket.rs` (lines 100-247)

#### HashBucketEntry struct (lines 136-138)
```rust
#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(transparent)]
pub struct HashBucketEntry(u64);  // Exactly 8 bytes
```

#### Bit layout
```
Bits: 63     62     61..48         47..0
      ┌──────┬──────┬───────────┬──────────────┐
      │Tent  │Rsvd  │ Tag (14)  │ Address(48)  │
      │(1)   │(1)   │           │              │
      └──────┴──────┴───────────┴──────────────┘
```

#### Key operations (lines 152-247)

```rust
impl HashBucketEntry {
    pub const EMPTY: Self = Self(0);                   // Sentinel
    
    pub const fn new(tag: u16, addr: LogicalAddress, tentative: bool) -> Self {
        // Packs tag, address, tentative into u64
    }
    
    pub const fn address(&self) -> LogicalAddress;
    pub const fn tag(&self) -> u16;
    pub const fn is_tentative(&self) -> bool;
    pub const fn is_empty(&self) -> bool;
    
    pub const fn with_tentative(&self) -> Self;       // Set tentative bit
    pub const fn without_tentative(&self) -> Self;    // Clear tentative bit
}
```

#### AtomicHashBucketEntry (lines 334-415)

```rust
#[repr(transparent)]
pub struct AtomicHashBucketEntry(AtomicU64);  // Atomic wrapper

impl AtomicHashBucketEntry {
    pub const fn new(entry: HashBucketEntry) -> Self;
    
    pub fn load(&self, order: Ordering) -> HashBucketEntry;
    pub fn store(&self, entry: HashBucketEntry, order: Ordering);
    
    pub fn compare_exchange(
        &self,
        current: HashBucketEntry,
        new_val: HashBucketEntry,
        success: Ordering,
        failure: Ordering,
    ) -> Result<HashBucketEntry, HashBucketEntry>;
}
```

---

## 7. Hash Bucket

### File: `/hash/bucket.rs` (lines 465-550)

#### Memory layout
```
Byte offset:  0      8     16     24     32     40     48     56    64
             ┌───────┬───────┬───────┬───────┬───────┬───────┬───────┬────┐
             │Entry 0│Entry 1│Entry 2│Entry 3│Entry 4│Entry 5│Entry 6│OVF │
             │ 8 B   │ 8 B   │ 8 B   │ 8 B   │ 8 B   │ 8 B   │ 8 B   │ 8 B│
             └───────┴───────┴───────┴───────┴───────┴───────┴───────┴────┘
              7 AtomicHashBucketEntry (56 B)    AtomicLogicalAddress (8 B)
             └─────────────── 64 bytes total (1 cache line) ──────────────┘
```

#### Rust definition (inferred from lines 500-550)
```rust
#[repr(C, align(64))]  // Cache-aligned
pub struct HashBucket {
    entries: [AtomicHashBucketEntry; 7],     // 56 bytes
    overflow: AtomicLogicalAddress,          // 8 bytes
}
```

#### Constants (line 463)
```rust
pub const BUCKET_NUM_ENTRIES: usize = 7;
```

#### Size
- Exactly **64 bytes** — fits in one cache line
- No padding or overhead
- All 8 buckets in primary array are perfectly aligned

---

## 8. Hash Table

### File: `/hash/table.rs` (lines 103-150)

#### HashTable struct
```rust
pub struct HashTable {
    buckets: Box<[HashBucket]>,               // Primary bucket array
    num_buckets: u64,                         // 2^log2_buckets
    log2_buckets: u32,
    overflow_pool: OverflowBucketPool,        // For overflow chains
}
```

#### Size constraints
```rust
pub const MIN_LOG2_SIZE: u32 = 1;             // 2 buckets minimum
pub const MAX_LOG2_SIZE: u32 = 30;            // 2^30 ≈ 1B buckets
```

#### Size example for 2^16 buckets
```
Primary buckets: 65,536 × 64 bytes = 4 MB
Overflow pool: Grows on demand (first page ≈ 64 MB)
```

#### Key methods
```rust
pub fn find_entry(hash: KeyHash) -> Option<(HashBucketEntry, &AtomicHashBucketEntry)>;
pub fn find_or_create_entry(hash: KeyHash, addr: LogicalAddress) -> FindOrCreateResult<'_>;
pub fn update_entry(slot, old, new) -> bool;  // CAS operation
```

---

## 9. Hash Index

### File: `/hash/index.rs` (lines 103-180)

#### HashIndex struct
```rust
pub struct HashIndex {
    table: HashTable,                         // Latch-free hash table
    epoch: Arc<EpochTable>,                   // Epoch coordination
    version: AtomicU32,                       // Table version (grows)
    live_entry_count: AtomicU64,              // Approx live entry count
}
```

#### Key methods

**find — line 239**
```rust
pub fn find(&self, hash: KeyHash) -> Option<(HashBucketEntry, &AtomicHashBucketEntry)> {
    self.table.find_entry(hash)
}
```

**find_or_create — line 280**
```rust
pub fn find_or_create(
    &self,
    hash: KeyHash,
    addr: LogicalAddress,
) -> FindOrCreateResult<'_> {
    self.table.find_or_create_entry(hash, addr)
}
```

**update — line 349**
```rust
pub fn update(
    &self,
    slot: &AtomicHashBucketEntry,
    old: HashBucketEntry,
    new: HashBucketEntry,
) -> bool {
    let success = self.table.update_entry(slot, old, new);
    if success && !old.is_empty() && new.is_empty() {
        self.live_entry_count.fetch_sub(1, Ordering::Relaxed);
    }
    success
}
```

**invalidate_entries_in_range — line 427** [O(n)]
```rust
pub fn invalidate_entries_in_range(
    &self,
    begin: LogicalAddress,
    end: LogicalAddress,
) -> u64 {
    let mut invalidated = 0u64;
    for bucket_idx in 0..self.table.num_buckets() {
        for i in 0..BUCKET_NUM_ENTRIES {
            let entry = bucket.entry(i).load(Ordering::Acquire);
            if entry.is_empty() { continue; }
            if entry.address() in [begin, end) {
                if bucket.entry(i).compare_exchange(
                    entry, HashBucketEntry::EMPTY,
                    Ordering::AcqRel, Ordering::Relaxed,
                ).is_ok() {
                    invalidated += 1;
                }
            }
        }
        // Follow overflow chain...
    }
    if invalidated > 0 {
        self.live_entry_count.fetch_sub(invalidated, Ordering::Relaxed);
    }
    invalidated
}
```

**cleanup_tentative_entries_for_recovery — line 559** [O(n), RECOVERY ONLY]
```rust
pub fn cleanup_tentative_entries_for_recovery(&self) -> u64 {
    // Same as invalidate_entries_in_range, but checks is_tentative()
    // ⚠️ MUST ONLY be called during recovery (no active inserts)
    // ⚠️ Calling concurrently with find_or_create() causes SILENT DATA LOSS
}
```

---

## Summary: Sizes & Locations

| Component | Location | Size | Notes |
|-----------|----------|------|-------|
| OperationStatus | `/status.rs:64` | 1-4 B | Enum, not stored |
| PendingOperation<F> | `/session.rs:154` | 48 B+ | Generic, variable |
| PendingIoContext<F> | `/pending_io.rs:248` | ~160 B | + Arc overhead |
| PendingIoManager | `/pending_io.rs:403` | 40-80 B | Shared, single |
| FasterSession<F> | `/session.rs:190` | ~200 B | + pending_ops Vec + io_contexts Vec |
| HashBucketEntry | `/bucket.rs:136` | 8 B | Exact, packed u64 |
| AtomicHashBucketEntry | `/bucket.rs:334` | 8 B | Atomic wrapper |
| HashBucket | `/bucket.rs:465` | 64 B | Cache-aligned |
| HashTable (2^16) | `/table.rs:103` | 4 MB | buckets only |
| HashIndex | `/hash/index.rs:103` | ~100 B | Small overhead |

