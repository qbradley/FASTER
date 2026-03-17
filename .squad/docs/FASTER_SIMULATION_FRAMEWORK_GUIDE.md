# FASTER Codebase Structure for Deterministic Simulation Testing

## 1. FasterKv Store Core (store/kv.rs)

### Main Struct: `FasterKv<F: Functions>`
**Key Fields:**
```rust
pub struct FasterKv<F: Functions> {
    pub(crate) hash_index: HashIndex,           // In-memory hash table
    pub(crate) allocator: HybridLogAllocator,   // Lock-free bump allocator for log
    pub(crate) functions: F,                    // User callbacks (read/upsert/rmw/delete)
    device: Box<dyn Device>,                    // Storage abstraction
    pub(crate) epoch_table: Arc<EpochTable>,    // Shared epoch coordination
    session_pool: SessionPool<F>,               // Session management
    flusher: PageFlusher,                       // I/O to device
    evictor: PageEvictor,                       // In-memory eviction
    pending_io_mgr: PendingIoManager,           // Async read completion
    grow_manager: GrowManager,                  // Hash index resize
    config: FasterKvConfig,                     // Configuration snapshot
    compaction_lock: Mutex<()>,                 // Serializes compaction
    compaction_policy: Option<Box<dyn CompactionPolicy>>,
}
```

**Thread Safety:** `Send + Sync` (unsafe impls at lines 237-239)
- All internal components are `Send + Sync`
- Mutation only via atomics
- Operations through `!Send` `FasterSession` handles

### FasterKvConfig
```rust
pub struct FasterKvConfig {
    pub hash_index_size_log2: usize,        // 20 → 1M buckets (default)
    pub buffer_size_pages: usize,           // In-memory pages (must be power of 2)
    pub mutable_fraction: f64,              // 0.9 default (90% mutable)
    pub sector_size: usize,                 // 512 default
    pub eviction_policy: EvictionPolicy,
    pub grow_config: GrowConfig,
    pub auto_compact: bool,                 // Enable auto-compaction
    pub lossy: bool,                        // LRU cache mode (discard on eviction)
}
```

### Key Public Methods

#### CRUD Operations (Lines 866-1073)
All follow same pattern: enter epoch → internal operation → exit epoch → dispatch pending I/O

```rust
pub fn read(
    &self,
    session: &mut FasterSession<F>,
    key: &F::Key,
    input: &F::Input,
    output: &mut F::Output,
    context: F::Context,
) -> OperationOutcome<F::Context>

pub fn upsert(
    &self,
    session: &mut FasterSession<F>,
    key: &F::Key,
    input: &F::Input,
    context: F::Context,
) -> OperationOutcome<F::Context>

pub fn rmw(
    session: &mut FasterSession<F>,
    key: &F::Key,
    input: &F::Input,
    output: &mut F::Output,
    context: F::Context,
) -> OperationOutcome<F::Context>

pub fn delete(
    &self,
    session: &mut FasterSession<F>,
    key: &F::Key,
    context: F::Context,
) -> OperationOutcome<F::Context>
```

**Note:** All CRUD methods pass `on_alloc_failure: Some(&|| self.maintenance())` callback to allocator

#### maintenance() Method (Lines 1676-1769)
**Purpose:** Manage buffer pressure by shifting read-only boundary, flushing pages, and evicting

**Algorithm:**
1. **Check conditions:**
   - Buffer pressure: `in_memory_pages + 2 >= buffer_size`
   - Eager flush watermark: `in_memory_pages * 4 >= buffer_size * 3` (75% capacity)
   - Mutable fraction threshold exceeded

2. **Shift read-only boundary** if any condition met:
   - Advances `read_only_address` to match `tail_address`
   - Seals all intermediate pages (Open → Sealed)

3. **Force eviction on buffer pressure:**
   - Calls `evictor.evict_pages()`
   - If lossy mode: advances `begin_address` to match `head_address` and invalidates hash entries

4. **Flush sealed pages** to device:
   - Calls `flusher.flush_sealed_pages()`
   - On QueueFull: breaks loop, triggers policy

5. **Poll device completions:**
   - Allows flush callbacks to fire (Flushing → Flushed transitions)

6. **Evict again if needed:**
   - Under eager flush watermark or buffer pressure
   - In lossy mode: truncates device below `begin_address`

7. **Yield on back-pressure:**
   - If queue_full or buffer pressure: `thread::yield_now()` + poll again

#### Session Management (Lines 402-522)
```rust
pub fn new_session(&self) -> FasterSession<F>
pub fn dispose_session(&self, session: FasterSession<F>)
pub fn session_scope<R>(&self, f: impl FnOnce(...) -> R) -> R
pub fn unsafe_context<'a>(&self, session: &'a mut FasterSession<F>) -> UnsafeContext<'a, F>
```

**Key Pattern:** 
- Sessions are `!Send` (thread-affine)
- Created with `new_session()` and disposed explicitly
- Begin epoch protection via `session.begin_unsafe()` → returns `SessionGuard`
- Guard automatically unprotects on drop

#### Batch Operations (Lines 527-620)
```rust
pub fn batch_read(&self, session, keys, outputs) -> BatchResult
pub fn batch_upsert(&self, session, keys, inputs) -> BatchResult
pub fn batch_rmw(&self, session, keys, inputs, outputs) -> BatchResult
pub fn batch_delete(&self, session, keys) -> BatchResult
pub fn batch_execute(&self, session, ops, outputs) -> BatchResult  // Mixed ops
```

All use `unsafe_context()` with single epoch enter/exit for the batch.

#### Pending I/O (Lines 1122-1236)
```rust
pub fn complete_pending(&self, session) -> Vec<(F::Output, F::Context)>
pub fn complete_pending_sync(&self, session) -> Vec<(F::Output, F::Context)>
pub fn try_complete_pending(&self, session, wait: bool) -> CompletePendingResult
pub fn refresh(&self, session) -> CompletePendingResult  // Epoch bump + drain I/O
```

### Drop Implementation (Lines 241-310)
**Critical SF-10 Shutdown:**
1. Flush remaining Sealed pages synchronously
2. Spin-wait (5s timeout) for Flushing pages to complete
3. Close device (joins I/O worker threads)
4. Trigger field drop order (important for pointer safety in callbacks)

---

## 2. Hybrid Log Allocator (hybrid_log/log_allocator.rs)

### HybridLogAllocator Structure
```rust
pub struct HybridLogAllocator {
    page_table: PageTable,                          // Circular page frame buffer
    begin_address: AtomicLogicalAddress,            // Start of valid data on disk
    head_address: AtomicLogicalAddress,             // Start of in-memory region
    read_only_address: AtomicLogicalAddress,        // Boundary: ro ← / → mutable
    safe_read_only_address: AtomicLogicalAddress,   // All threads observed ro boundary
    tail_address: AtomicLogicalAddress,             // Next allocation point (bump)
    flushed_until_address: AtomicLogicalAddress,    // Progress of flush to device
    page_size: u32,                                 // 2^OFFSET_BITS (32 MB default)
    mutable_fraction_pages: u32,                    // Threshold for read-only shift
    sealed: AtomicBool,                             // Stop allocations flag
}
```

**Invariant:** `begin ≤ head ≤ read_only ≤ safe_read_only ≤ tail`

### Address Regions (Visualized)
```
┌──────────┬──────────────────┬───────────────────┬──────────────────┐
│ On disk  │   Read-only      │   Safe read-only  │   Mutable        │
│          │   (flushed,      │   (flushing or    │   (in-memory,    │
│          │   evictable)     │   flushed)        │   writable)      │
├──────────┼──────────────────┼───────────────────┼──────────────────┤
begin      head               read_only           safe_read_only     tail
```

### Key Methods

#### try_allocate(size) (Lines 105-174)
**Lock-free CAS loop on `tail_address`**
- Returns `LogicalAddress` for allocation or `None` on boundary/full
- Checks:
  - `new_offset <= page_size` (doesn't cross page boundary)
  - **SF-7:** `current.page().0 < MAX_PAGE` (prevent overflow)
  - **SF-10:** `next_page.wrapping_sub(head_page) < buffer_size` (prevent tail from lapping head)
- On page fill: transitions Open → Sealed, allocates next page

#### advance_to_next_page() (Lines 305-356)
**Called when try_allocate returns None (page boundary)**
- Transitions tail to next page
- **SF-7 & SF-10** checks (same as try_allocate)
- Returns `LogicalAddress` of new page or `None` on buffer full

#### shift_read_only_to_tail() (Lines 200-235)
**Advances read-only region**
- Loop: CAS `read_only_address` ← `tail_address` (monotonic)
- Loop: CAS `safe_read_only_address` ← `tail_address`
- All intermediate pages: seal if still Open

#### Query Methods (Lines 237-286)
```rust
pub fn is_mutable(&self, addr: LogicalAddress) -> bool        // sro ≤ addr < tail
pub fn is_safe_read_only(&self, addr: LogicalAddress) -> bool // head ≤ addr < sro
pub fn is_in_memory(&self, addr: LogicalAddress) -> bool      // head ≤ addr < tail
pub fn tail_address(&self) -> LogicalAddress
pub fn read_only_address(&self) -> LogicalAddress
pub fn head_address(&self) -> LogicalAddress
pub fn begin_address(&self) -> LogicalAddress
```

### Address Constants
- `OFFSET_BITS = 25` → `page_size = 2^25 = 33,554,432 bytes ≈ 32 MB`
- `MAX_PAGE` in address module: max page number (guards against overflow)

---

## 3. Page State Machine & Page Table (hybrid_log/page.rs)

### PageState Lifecycle
```rust
pub enum PageState {
    Free = 0,       // Not allocated or recycled
    Open = 1,       // Current tail page (writable)
    Sealed = 2,     // No more writes allowed
    Flushing = 3,   // Currently being written to device
    Flushed = 4,    // Successfully written to device
    Evicted = 5,    // Evicted from memory (data on disk only)
}
```

**Typical Transition:** `Free → Open → Sealed → Flushing → Flushed → Evicted → Free`

### AtomicPageState
```rust
pub struct AtomicPageState {
    inner: AtomicU8,  // Uses CAS for transitions
}

impl AtomicPageState {
    pub fn try_transition(&self, expected: PageState, desired: PageState) -> bool
        // Ordering: AcqRel (success), Acquire (failure)
}
```

### PageFrame
```rust
pub struct PageFrame {
    data: NonNull<u8>,          // Sector-aligned allocation
    size: usize,                // page_size bytes
    layout: Layout,             // For deallocation
    state: AtomicPageState,     // Current state
    flushed_until: AtomicU32,   // Bytes flushed to device (fetch_max)
}
```

**Memory:** Uses `std::alloc::alloc_zeroed` with custom `Layout` for sector alignment (512 bytes default)

### PageTable
- Circular buffer: maps `Page(logical) → PageFrame`
- Power-of-2 size for modulo arithmetic
- Lazy allocation: frames allocated on first access to page

**Constants:**
- `DEFAULT_SECTOR_SIZE = 512`
- `DEFAULT_PAGE_SIZE = 1 << OFFSET_BITS = 2^25`

---

## 4. Flush Pipeline (hybrid_log/flush.rs)

### FlushRequest
```rust
pub struct FlushRequest {
    pub page: Page,            // Logical page being flushed
    pub device_offset: u64,    // Byte offset on device
    pub valid_bytes: u32,      // Bytes to write (may be < page_size for tail)
}
```

### FlushBatchResult
```rust
pub struct FlushBatchResult {
    pub flushed: u32,          // Number of pages submitted
    pub queue_full: bool,      // Device back-pressure flag
}
```

### PageFlusher
```rust
pub struct PageFlusher {
    sector_size: u32,  // Alignment for I/O
    page_size: u32,    // Frame size
}
```

**Key Methods:**

#### flush_page() - Async (Lines 225-309)
```rust
pub fn flush_page(
    &self,
    page: Page,
    page_table: &PageTable,
    device: &dyn Device,
    valid_bytes: u32,
) -> Result<bool, FlushError>
```

**Steps:**
1. Get frame from page_table
2. CAS transition: `Sealed → Flushing` (fails if not Sealed)
3. Compute write_size (sector-aligned, with CRC trailer space)
4. Write CRC-32C trailer
5. Allocate `FlushCallbackContext` via `TypedIoContext::new`
6. Call `device.write_async()` with `flush_completion_callback`
7. On QueueFull/Error: revert Sealed state and reclaim context

**Returns:** `Ok(true)` if submitted, `Ok(false)` if already flushing/flushed

#### flush_page_sync() - Sync (Lines 318-370)
Same as async but:
- Uses `device.write_sync()`
- Completes page state transition in-line
- Returns immediately on write completion

#### flush_sealed_pages() - Batch (Lines 381-419)
```rust
pub fn flush_sealed_pages(
    &self,
    allocator: &HybridLogAllocator,
    device: &dyn Device,
) -> Result<FlushBatchResult, FlushError>
```

**Algorithm:**
1. Scan from `head_page` to `read_only_page` (exclusive)
2. For each Sealed page: call `flush_page()` async
3. On QueueFull: break loop and return partial result
4. Return flushed count and queue_full flag

### Flush Callback (Lines 157-192)
```rust
unsafe fn flush_completion_callback(
    context: *mut u8,
    status: IoStatus,
    bytes_transferred: u32
)
```

**Safety Model:**
- Context is `FlushCallbackContext` allocated via `TypedIoContext::new`
- Contains raw pointer to `PageTable` (valid because allocator outlives flushes)
- Callback executed while `PageTable` still alive (guaranteed by SF-10 shutdown)

**On Success:**
1. Validate `bytes_transferred >= expected`
2. Update page's `flushed_until` with `fetch_max`
3. CAS transition: `Flushing → Flushed`

**On Error:**
- Leave page in `Flushing` state (retry on next maintenance cycle)

### CRC Trailer (Lines 442-472)
```rust
fn write_crc_trailer(&self, frame: &PageFrame, valid_bytes: u32, write_size: u32)
```
- Computes CRC-32C over valid data range
- Writes 8-byte trailer at `[write_size-8..write_size]`
- Skips if trailer would overlap valid data

---

## 5. Epoch Table & Guards (epoch/table.rs, epoch/guard.rs)

### EpochTable (Core Coordination)
```rust
pub struct EpochTable {
    current_epoch: CachePadded<AtomicU64>,              // Global monotonic counter
    safe_to_reclaim_epoch: CachePadded<AtomicU64>,     // Min safe epoch for reclamation
    table: Box<[CachePadded<EpochEntry>]>,             // Per-thread entries
    drain_list: DrainList,                             // Deferred callbacks
    scan_limit: CachePadded<AtomicUsize>,              // High-water mark for scan
    free_list: Mutex<Vec<usize>>,                      // Slot indices
}
```

**Constants (epoch/mod.rs):**
- `MAX_THREADS = 256`
- `INITIAL_EPOCH = 1`
- `INACTIVE_EPOCH = 0` (sentinel for unprotected thread)
- `PHASE_COUNT = 16` (checkpoint phases)

### Epoch Lifecycle
```
register() → claim slot → EpochThread
    ↓
protect() → snapshot current_epoch → EpochGuard (RAII)
    ↓
unprotect() → clear local epoch → drain callbacks
    ↓
bump_current_epoch(callback) → advance global epoch → queue callback
    ↓
defer(callback) → queue without bumping epoch
    ↓
try_drain() → compute safe epoch → fire ready callbacks
```

### Key Methods

#### register() (Lines 140-168)
```rust
pub fn register(self: &Arc<Self>) -> Option<EpochThread>
```
- Claims slot from free_list (mutex protected, rare operation)
- Stores `(index + 1)` to thread_id for sentinel checking
- Updates scan_limit if needed
- Returns `EpochThread` handle

#### protect() (Lines 201-214)
```rust
pub(crate) fn protect(&self, entry_index: usize)
```
- Increment reentrant counter (Relaxed, single-writer)
- If reentrant 0→1: store current epoch (Release ordering)
- Reentrant: preserves outermost epoch snapshot

**Memory Ordering:**
- `current_epoch` load: Relaxed (conservative: stale is safe)
- Local store: Release (pairs with Acquire in compute_safe_epoch)

#### unprotect() (Lines 228-245)
```rust
pub(crate) fn unprotect(&self, entry_index: usize)
```
- Decrement reentrant counter (Relaxed)
- If reentrant 1→0:
  - Clear local_current_epoch (Release)
  - Call `try_drain()` opportunistically

#### bump_current_epoch() (Lines 279-286)
```rust
pub fn bump_current_epoch<F: FnOnce() + Send + 'static>(&self, callback: F)
```
- Fetch-add current_epoch (SeqCst for total ordering)
- Queue callback with prior epoch
- Call `try_drain()`

**SeqCst Justification:**
- Prevents thread from reading stale global epoch and storing it *after* bump
- Otherwise: safe epoch computed too aggressively → callback fires too early

#### defer() (Lines 311-316)
```rust
pub fn defer<F: FnOnce() + Send + 'static>(&self, callback: F)
```
- Queue callback to current epoch (SeqCst load)
- Don't bump epoch
- Useful for deferring without epoch cost

#### try_drain() (Lines 329-345)
```rust
pub(crate) fn try_drain(&self)
```
- Fast-path: if `!drain_list.has_pending()` return early (~5ns)
- Compute safe epoch (expensive ~100ns for 256-entry scan)
- If safe > old_safe: update and fire ready callbacks

#### compute_safe_epoch() (Lines 367-389)
```rust
fn compute_safe_epoch(&self) -> u64
```
- Load current_epoch (Acquire)
- Scan entries `[0..scan_limit]` for active epochs (Acquire per entry)
- Return `min(active_epochs) - 1`
- If no threads active: returns `current_epoch - 1`

**Optimization:** Scan only `scan_limit` (high-water mark) instead of all 256 slots

### EpochGuard & EpochThread

#### EpochGuard (RAII, !Send + !Sync)
```rust
pub struct EpochGuard<'a> {
    table: &'a EpochTable,
    entry_index: usize,
    _not_send: PhantomData<*const ()>,  // !Send + !Sync enforcement
}

impl<'a> EpochGuard<'a> {
    pub fn epoch(&self) -> u64          // Get protected epoch
    pub fn refresh(&self)               // Update to current epoch
}
// Drop → unprotect()
```

**Thread-Affinity:** Must be dropped on same thread that created it

#### EpochThread (Per-Thread Handle)
```rust
pub struct EpochThread {
    table: Arc<EpochTable>,
    entry_index: usize,
    unregistered: bool,
}

impl EpochThread {
    pub fn protect(&self) -> EpochGuard<'_>
    pub fn refresh(&self)               // No-op if not protected
    pub fn unregister(&mut self)        // Explicit cleanup
}
// Drop → deregister()
```

---

## 6. Session Model (store/session.rs)

### FasterSession
```rust
pub struct FasterSession<F: Functions> {
    epoch_thread: EpochThread,          // Epoch registration
    pending_ops: VecDeque<PendingOp>,   // Queued pending operations
    io_contexts: Vec<PendingIoContext>, // In-flight I/O
    _not_send: PhantomData<*const ()>,  // !Send enforcement
}
```

**Thread-Affine:** Cannot be sent to another thread

### SessionGuard (RAII)
```rust
pub struct SessionGuard<'a, F: Functions> {
    session: &'a mut FasterSession<F>,
    guard: EpochGuard<'a>,  // Epoch protection
}

impl Drop {
    fn drop(&mut self) {
        // guard dropped → epoch unprotect
        // try_drain fires pending epoch callbacks
    }
}
```

### UnsafeContext (Batch API)
```rust
pub struct UnsafeContext<'a, F: Functions> {
    guard: EpochGuard<'a>,  // Long-held epoch protection
    session: &'a mut FasterSession<F>,
}

impl<'a, F: Functions> UnsafeContext<'a, F> {
    pub fn refresh(&self)   // Update epoch periodically
    pub fn upsert(...)      // Operations without per-op epoch overhead
    pub fn batch_read(...)
    pub fn batch_upsert(...)
}
// Drop → guard dropped → epoch unprotect
```

**Purpose:** Amortize epoch enter/exit cost across batch by holding guard for entire batch

---

## 7. Synchronization Points Summary

### Atomics (Acquire/Release Pairs)
| Component | Operation | Synchronization |
|-----------|-----------|-----------------|
| Epoch | current_epoch bump | SeqCst (total ordering requirement) |
| Epoch | protect/compute_safe | Acquire/Release pair |
| Allocator | tail_address CAS | AcqRel success, Acquire failure |
| Allocator | read_only shift | AcqRel CAS |
| Page State | try_transition | AcqRel success, Acquire failure |
| Flush | flushed_until update | Release (callback from I/O thread) |

### Locks (Rare, Off Hot-Path)
| Component | Purpose | Location |
|-----------|---------|----------|
| EpochTable::free_list | Slot allocation | Mutex (registration, rare) |
| FasterKv::compaction_lock | Serialize compaction | Mutex (manual/auto compact) |

### Lock-Free Patterns
| Component | Pattern | Implementation |
|-----------|---------|-----------------|
| Allocator tail | Bump allocator | CAS loop on tail_address |
| Page allocation | Lazy allocation | PageTable with AtomicPtr |
| Page state machine | Lock-free FSM | CAS transitions via AtomicPageState |
| Epoch tracking | Coordination | AtomicU64 per thread + drain list |
| Flush callbacks | Async completion | Device enqueues callback, I/O thread executes |

---

## 8. Key Constants & Defaults

### Page & Memory
```rust
OFFSET_BITS = 25                // Bits for offset within page
PAGE_SIZE = 2^25 = 33,554,432  // ~32 MB
SECTOR_SIZE = 512              // Alignment for I/O
```

### Hash Index
```rust
hash_index_size_log2 = 20      // 2^20 = 1M buckets default
```

### Buffer Management
```rust
buffer_size_pages = 16         // Default in-memory pages
mutable_fraction = 0.9         // 90% mutable, 10% read-only
```

### Epoch
```rust
MAX_THREADS = 256
INITIAL_EPOCH = 1
INACTIVE_EPOCH = 0
PHASE_COUNT = 16
```

### Shutdown (SF-10)
```rust
TIMEOUT = Duration::from_secs(5)  // Wait for flush completion
POLL_INTERVAL = Duration::from_millis(1)
```

---

## 9. Simulation Testing Framework Requirements

### Key Components to Instrument

1. **Allocator State Snapshots**
   - Capture `(begin, head, read_only, safe_read_only, tail)` on each operation
   - Track page state transitions
   - Verify SF-10 invariant: `tail - head < buffer_size`

2. **Epoch Coordination**
   - Mock `EpochTable` to control epoch bumps
   - Deterministically fire drain callbacks
   - Verify safe-epoch computation

3. **Flush Pipeline**
   - Mock `Device::write_async` to control callback timing
   - Simulate I/O completion ordering
   - Track page transitions: Sealed → Flushing → Flushed

4. **Page State Machine**
   - Verify atomic state transitions
   - Detect invalid transitions
   - Track page lifecycle per generation

5. **Session/Thread Affinity**
   - Enforce `!Send` at compile time (already done)
   - Verify epoch guard drops on correct thread
   - Simulate concurrent sessions

6. **Maintenance Cycles**
   - Control when `maintenance()` fires
   - Verify read-only shift, flush, eviction order
   - Test buffer pressure scenarios

### Critical Invariants to Verify

1. **Buffer Overflow:** `tail_page - head_page < buffer_size`
2. **Address Monotonicity:** `begin ≤ head ≤ read_only ≤ safe_read_only ≤ tail`
3. **Page Lifecycle:** Pages follow FSM, no invalid transitions
4. **Epoch Progress:** Safe epoch advances monotonically
5. **Shutdown (SF-10):** All pending flushes complete before allocator drop
6. **Callback Execution:** Drain callbacks fire only after safe epoch reached
7. **Flush Ordering:** Only Sealed pages transition to Flushing

### Suggested Simulation Features

- **Time control:** Deterministic virtual clock for I/O completion ordering
- **State injection:** Inject page states, epoch values for edge case testing
- **Failure injection:** Simulate I/O errors, queue full conditions
- **Concurrency control:** Deterministic scheduling of multiple sessions
- **Trace generation:** Record all state transitions for post-mortem analysis

