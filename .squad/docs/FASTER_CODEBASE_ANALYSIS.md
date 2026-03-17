# FASTER Rust Codebase Analysis for Memory Pressure Tests

## 1. ALLOCATOR (`rust/crates/faster-core/src/allocator.rs`)

### Public API: `MallocFixedPageSize<T>`

**Constructor:**
```rust
pub fn new() -> Self
```
- No parameters (all defaults internal)
- **Requirements:** `T` must be ≥ 8 bytes and 8-byte aligned (for free-list linkage)
- Pre-allocates first page (ITEMS_PER_PAGE items) to avoid CAS on first alloc
- Returns `Self` (not `Result`) — panics on invalid size/alignment

**Page Parameters (constants):**
- `ITEMS_PER_PAGE_BITS = 20` → `ITEMS_PER_PAGE = 1,048,576` items per page
- Each item is stored at a logical address via `LogicalAddress`
- Pages are cache-line aligned (64 bytes) and heap-allocated
- Total memory per page = `ITEMS_PER_PAGE * sizeof(T)` (zeroed on allocation)

**Public Methods:**

```rust
pub fn allocate(&self) -> LogicalAddress
```
- Lock-free allocation
- Fast path: pop from free list if available
- Slow path: bump-allocate from current page; allocates new page on demand
- Always succeeds (panics only on extreme exhaustion, requires > 8M pages)
- Returns `LogicalAddress` (not `Option`)

```rust
pub fn get(&self, addr: LogicalAddress) -> &T
pub fn get_ptr(&self, addr: LogicalAddress) -> *mut T
```
- Resolve `LogicalAddress` → `&T` reference (fast, unsafe internally)
- `get_ptr()` returns raw `*mut T` for direct access

```rust
pub fn free(&self, addr: LogicalAddress)
```
- Return item to free list for reuse
- **If epoch attached:** defers free-list push until all threads pass current epoch (prevents ABA)
- **Without epoch:** immediate free-list push (unsafe in concurrent scenarios)

```rust
pub fn free_immediate(&self, addr: LogicalAddress)
```
- Synchronously push to free list (no epoch deferral)
- Use only when epoch is not attached

```rust
pub fn count(&self) -> u64
```
- Returns total items ever bump-allocated (excluding free-list reuse)
- **Does not** subtract freed items

```rust
pub fn set_epoch(&self, epoch: Arc<EpochTable>)
```
- Attach epoch table for deferred cleanup on `free()`

### Free-List Behavior:
- **Data structure:** Treiber stack (lock-free, 16-bit ABA tag)
- **Allocation:** CAS-pop from head; if empty, fallback to bump allocation
- **Deallocation:** CAS-push to head; if epoch attached, deferred until safe epoch
- **Tag:** 16-bit ABA counter (bits 48..63), address in lower 48 bits
- **Freed items store:** next-pointer in first 8 bytes of the item itself

### Allocation Failure:
- **Panics** on: layout calculation overflow, allocation failure from global allocator
- **Never returns `Err`** — the API is infallible
- Worst case: heap exhaustion → `handle_alloc_error` → abort

---

## 2. BUFFER POOL / PAGE MANAGEMENT (`rust/crates/faster-core/src/hybrid_log/page.rs`)

### `PageFrame` - Physical Memory Slab

**Constructor:**
```rust
pub fn new(page_num: Page, sector_size: usize) -> Result<Self, Error>
```
- Allocates single frame for one page (default 32 MB = 2^25 bytes)
- Sector-aligned (typically 512 bytes for disk I/O)
- Non-null guaranteed; fails only on OOM

**Page Lifecycle States:**
```rust
pub enum PageState {
    Free    = 0,       // Not allocated or recycled
    Open    = 1,       // Current tail (accepting writes)
    Sealed  = 2,       // No more writes; ready to flush
    Flushing= 3,       // Write in progress to device
    Flushed = 4,       // Written to device
    Evicted = 5,       // Evicted from memory; data on device only
}
```

**Atomic Transitions:**
```rust
impl AtomicPageState {
    pub fn try_transition(&self, from: PageState, to: PageState) -> bool
}
```
- CAS-based state machine; only one thread succeeds
- `Free → Open`, `Sealed → Flushing`, `Flushing → Flushed`, `Flushed → Evicted`

### `PageTable` - Circular Buffer of Frames

**Constructor:**
```rust
pub fn new(
    buffer_size_pages: usize,  // Must be power of 2
    page_size: usize,          // Typically 2^25 bytes
    sector_size: usize         // Typically 512 bytes
) -> Self
```

**Public Methods:**

```rust
pub fn get_frame(&self, page: Page) -> Option<&PageFrame>
pub fn get_or_allocate_frame(&self, page: Page) -> &PageFrame
```
- `get_frame()`: Returns frame if allocated; `None` if not yet allocated
- `get_or_allocate_frame()`: CAS-allocates new frame if needed (lazy allocation)
- Circular indexing: `page_num % buffer_size_pages`

```rust
pub fn num_frames(&self) -> usize
pub fn buffer_size_pages(&self) -> usize
```
- Total slots in the circular buffer

### Page Pool Behavior:
- **Circular buffer:** `num_frames` slots, indexed by page number mod size
- **Lazy allocation:** Frames allocated on first access (CAS on slot)
- **No deallocation during operation:** Frames are recycled (Free → Open) when evicted
- **Exhaustion:** If tail wraps onto head before pages are evicted, allocation fails

---

## 3. HYBRID LOG DIRECTORY STRUCTURE

Files in `/rust/crates/faster-core/src/hybrid_log/`:
- **`mod.rs`** — module exports (HybridLogAllocator, PageTable, PageFrame, etc.)
- **`log_allocator.rs`** — HybridLogAllocator (bump allocator + region boundaries)
- **`page.rs`** — PageFrame, PageState, PageTable (circular page buffer)
- **`flush.rs`** — PageFlusher (async/sync writes to device)
- **`eviction.rs`** — PageEvictor (reclaims flushed pages)
- **`regions.rs`** — AddressRegion, AddressInfo (region tracking)
- **`record_ops.rs`** — LogRecordReader/Writer (record serialization)
- **`scan.rs`** — LogScanIterator (sequential scan)

### `HybridLogAllocator` - Main Allocator

**Constructor:**
```rust
pub fn new(
    buffer_size_pages: usize,  // Power of 2; in-memory page count
    mutable_fraction: f64,     // 0.0..=1.0; fraction kept mutable
    sector_size: usize         // 512 for typical disk I/O
) -> Self
```

**Region Boundaries** (mutually advancing: `begin ≤ head ≤ ro ≤ sro ≤ tail`):
```rust
pub fn begin_address(&self) -> LogicalAddress    // Start of valid on-disk data
pub fn head_address(&self) -> LogicalAddress     // Start of in-memory region
pub fn read_only_address(&self) -> LogicalAddress// Start of read-only region
pub fn safe_read_only_address(&self) -> LogicalAddress // Safe to read (all threads observed)
pub fn tail_address(&self) -> LogicalAddress    // Next allocation point
```

**Allocation:**
```rust
pub fn try_allocate(&self, size: u32) -> Option<LogicalAddress>
```
- Lock-free CAS on `tail_address`
- Returns `Some(addr)` if allocation fits in current page
- Returns `None` if would cross page boundary OR if sealed
- **Does NOT panic** on failure — caller must handle via `advance_to_next_page()`

```rust
pub fn advance_to_next_page(&self) -> Option<LogicalAddress>
```
- Seals current page (`Open → Sealed`)
- Moves tail to start of next page
- Returns first address of new page, or `None` if circular buffer full

**Mutable Region Management:**
```rust
pub fn is_mutable(&self, addr: LogicalAddress) -> bool
pub fn is_safe_read_only(&self, addr: LogicalAddress) -> bool
pub fn is_in_memory(&self, addr: LogicalAddress) -> bool

pub fn shift_read_only_to_tail(&self)  // Advance read-only to current tail
```

**Physical Access:**
```rust
pub fn get_physical_address(&self, addr: LogicalAddress) -> Option<*mut u8>
```
- Convert logical address → raw `*mut u8` pointer
- Returns `None` if page not allocated

**Sealing:**
```rust
pub fn seal(&self)
```
- Prevents all future allocations (for recovery/shutdown)

---

## 4. HASH TABLE (`rust/crates/faster-core/src/hash/`)

### Files:
- **`hash.rs`** — FasterHash, KeyHash, Hashable trait
- **`bucket.rs`** — HashBucket (7 entries + overflow pointer), HashBucketEntry
- **`table.rs`** — HashTable (latch-free concurrent array of buckets)
- **`index.rs`** — HashIndex (wraps HashTable + EpochTable)
- **`overflow.rs`** — OverflowBucketPool
- **`mod.rs`** — module exports

### `HashTable` - Latch-Free Hash Index

**Constructor:**
```rust
pub fn new(log2_size: u32) -> Self
```
- Creates `2^log2_size` buckets (must be power of 2)
- Range: `log2_size` in `[1..=30]` → `[2 buckets .. 2^30 buckets]`
- All entries zero-initialized (EMPTY state)

**Public Methods:**

```rust
pub fn num_buckets(&self) -> u64
pub fn log2_buckets(&self) -> u32
```

```rust
pub fn bucket(&self, hash: KeyHash) -> &HashBucket
pub fn bucket_by_index(&self, index: u64) -> &HashBucket
```
- `bucket(hash)` — hot path: `index = hash & (num_buckets - 1)` (branchless)
- `bucket_by_index()` — for grow/iteration by index

```rust
pub fn find_entry(
    &self, 
    hash: KeyHash
) -> Option<(HashBucketEntry, &AtomicHashBucketEntry)>
```
- Linear scan within bucket + overflow chain
- Returns entry snapshot + reference to atomic slot for CAS

```rust
pub struct FindOrCreateResult<'a> {
    pub entry: HashBucketEntry,
    pub slot: &'a AtomicHashBucketEntry,
    pub created: bool,
}

pub fn find_or_create_entry(
    &self,
    hash: KeyHash,
    initial_addr: LogicalAddress
) -> Result<FindOrCreateResult, String>
```
- Two-phase insert: tentative CAS then commit (clear tentative bit)
- Returns slot reference for subsequent `update_entry()` CAS
- Tentative entries are skipped by lookups until committed

```rust
pub fn update_entry(
    slot: &AtomicHashBucketEntry,
    old_entry: HashBucketEntry,
    new_addr: LogicalAddress
) -> bool
```
- CAS the entry's address (e.g., to finalize insert or chain version)

### Bucket Layout

```rust
pub const BUCKET_NUM_ENTRIES: usize = 7;  // Per 64-byte bucket

pub struct HashBucket {
    entries: [AtomicHashBucketEntry; 7],
    overflow: AtomicU64,  // Address of next overflow bucket (or 0)
}
```

**Entry Format:**
```rust
pub struct HashBucketEntry {
    tag: u16,                   // Top 14 bits of key hash
    address: LogicalAddress,    // 40-bit log address
    tentative: bool,            // 1 bit; set during 2-phase insert
}
```

### Overflow Buckets

**`OverflowBucketPool`:**
```rust
pub fn new() -> Self
```
- Wraps `MallocFixedPageSize<HashBucket>`
- Allocates buckets on demand; reuses freed buckets

```rust
pub fn allocate(&self) -> LogicalAddress
pub fn get(&self, addr: LogicalAddress) -> &HashBucket
pub fn free(&self, addr: LogicalAddress)
```

**Overflow Chain:**
- When primary bucket (7 entries) + existing overflow chain full
- CAS new overflow bucket's address into last bucket's overflow pointer
- Chain is traversed linearly during find operations

### Capacity/Load Factor:
- **No explicit load factor enforcement** in the table itself
- **Grow manager** (separate subsystem) monitors load factor and resizes table
- Typical threshold: `load_factor_threshold = 0.75` (configurable)

---

## 5. STORE (`rust/crates/faster-core/src/store/`)

### Files:
- **`kv.rs`** — FasterKv (main store), FasterKvConfig
- **`builder.rs`** — FasterKvBuilder (fluent API)
- **`session.rs`** — FasterSession (thread-affine operation handle)
- **`functions.rs`** — Functions trait, SimpleFunctions, CounterFunctions
- **`operations.rs`** — CRUD operation implementations
- **`batch.rs`** — Batch operation API
- **`pending_io.rs`** — I/O completion handling

### `FasterKv<F: Functions>` - Main Store

**Constructor:**
```rust
pub fn new(
    config: FasterKvConfig,
    functions: F,
    device: impl Device
) -> Self
```

**Configuration: `FasterKvConfig`**
```rust
pub struct FasterKvConfig {
    pub hash_index_size_log2: usize,        // 4..=30 (default 20 → 1M buckets)
    pub buffer_size_pages: usize,           // Power of 2 (default 16)
    pub mutable_fraction: f64,              // 0.0..1.0 (default 0.9)
    pub sector_size: usize,                 // Power of 2 (default 512)
    pub eviction_policy: EvictionPolicy,    // max_in_memory_pages, batch size
    pub grow_config: GrowConfig,            // Hash table grow parameters
    pub auto_compact: bool,                 // Enable auto-compaction (default false)
}
```

**Default Config Example:**
```rust
FasterKvConfig {
    hash_index_size_log2: 20,       // 1,048,576 buckets
    buffer_size_pages: 16,          // 16 × 32 MB = 512 MB in memory
    mutable_fraction: 0.9,          // ~14 pages (460 MB) mutable
    sector_size: 512,
    eviction_policy: EvictionPolicy {
        max_in_memory_pages: 256,   // Keep ≤ 256 pages in memory
        eviction_batch_size: 4,     // Evict 4 at a time
    },
    grow_config: GrowConfig::default(),
    auto_compact: false,
}
```

**Builder API:**
```rust
pub fn builder() -> FasterKvBuilder
    .hash_index_size_log2(usize) -> Self
    .buffer_size_pages(usize) -> Self
    .mutable_fraction(f64) -> Self
    .sector_size(usize) -> Self
    .eviction_policy(EvictionPolicy) -> Self
    .grow_threshold(f64) -> Self
    .build(functions, device) -> Result<FasterKv<F>, FasterError>
```

**Session Management:**
```rust
pub fn new_session(&self) -> FasterSession<F>
pub fn dispose_session(&self, session: FasterSession<F>)
```

**CRUD Operations** (all take `&self, &mut session, key, value/input, ctx`):
```rust
pub fn upsert(
    &self, 
    session: &mut FasterSession<F>,
    key: &F::Key,
    value: &F::Value,
    ctx: F::Context
) -> OperationStatus

pub fn read(
    &self,
    session: &mut FasterSession<F>,
    key: &F::Key,
    input: &F::Input,
    output: &mut F::Output,
    ctx: F::Context
) -> OperationStatus

pub fn rmw(
    &self,
    session: &mut FasterSession<F>,
    key: &F::Key,
    input: &F::Input,
    output: &mut F::Output,
    ctx: F::Context
) -> OperationStatus

pub fn delete(
    &self,
    session: &mut FasterSession<F>,
    key: &F::Key,
    ctx: F::Context
) -> OperationStatus
```

### Small Store Example:
```rust
let config = FasterKvConfig {
    hash_index_size_log2: 10,     // 1,024 buckets
    buffer_size_pages: 8,         // 8 × 32 MB = 256 MB
    mutable_fraction: 0.9,
    sector_size: 512,
    eviction_policy: EvictionPolicy::default(),
    grow_config: GrowConfig::default(),
    auto_compact: false,
};
let store = FasterKv::new(config, SimpleFunctions::default(), InMemoryDevice::new());
let mut session = store.new_session();
```

### `FasterSession<F>` - Thread-Affine Operation Handle

**Constructor:**
```rust
// Obtained via FasterKv::new_session()
pub fn begin_unsafe(&mut self) -> SessionGuard<'_, F>
```
- Enter epoch protection (required before operations)

**Operations:**
```rust
pub fn next_serial(&mut self) -> u64
pub fn pending_count(&self) -> usize
pub fn has_pending(&self) -> bool
pub fn complete_pending(&mut self) -> CompletePendingResult
```

**Lifecycle:**
- Create via `store.new_session()`
- Must call `begin_unsafe()` to enter epoch (guard-based scope)
- Issue operations via `store.upsert()`, `store.read()`, etc.
- Call `complete_pending()` to drain async I/O
- Drop or `dispose_session()` to clean up

---

## 6. EXISTING TESTS (`rust/crates/faster-core/tests/`)

### Test Files:
1. **`integration_basics.rs`** — Foundation module composition (address, hash, allocator, record)
2. **`e2e_tests.rs`** — Full-stack CRUD, concurrency, multi-threaded access
3. **`concurrent_stress.rs`** — Epoch, allocator, bucket stress with 4-16 threads
4. **`hash_index_correctness.rs`** — Hash table correctness (find, insert, update)
5. **`hash_index_concurrent.rs`** — Concurrent hash table operations
6. **`checkpoint_recovery_tests.rs`** — Checkpoint/recovery scenarios
7. **`compaction_integration.rs`** — Compaction correctness
8. **`epvs_integration.rs`** — EPVS (versioning) scenarios
9. **`variable_length.rs`** — Variable-length key/value records
10. **`batch_tests.rs`** — Batch operation API
11. **`loom_tests.rs`** — Loom concurrency model checker
12. **`miri_tests.rs`** — Miri UB detection
13. **`property_tests.rs`** — Property-based testing
14. **`quickstart_verify.rs`** — Quickstart guide verification
15. **`write_pending_completion.rs`** — Pending I/O completion
16. **`common/mod.rs`** — Test helpers (addr, fill_bucket, hash_key)

### Test Helpers (`common/mod.rs`):
```rust
pub fn addr(page: u32, offset: u32) -> LogicalAddress
pub fn fill_bucket(bucket: &HashBucket, base_tag: u16, base_addr: LogicalAddress)
pub fn hash_key(key: u64) -> (u16, u64)  // Returns (tag, hash_value)
```

### Common Test Pattern (from `e2e_tests.rs`):

**Small Store Creation:**
```rust
fn small_store() -> FasterKv<SimpleFunctions<u64, u64>> {
    let config = FasterKvConfig {
        grow_config: GrowConfig::default(),
        hash_index_size_log2: 10,     // 1,024 buckets
        buffer_size_pages: 8,
        mutable_fraction: 0.9,
        sector_size: 512,
        eviction_policy: EvictionPolicy::default(),
        auto_compact: false,
    };
    FasterKv::new(config, SimpleFunctions::default(), InMemoryDevice::new())
}
```

**Basic CRUD Test:**
```rust
#[test]
fn crud_lifecycle() {
    let store = small_store();
    let mut s = store.new_session();

    // Upsert
    let status = store.upsert(&mut s, &1u64, &100u64, ());
    assert!(status.is_success());

    // Read
    let mut out: Option<u64> = None;
    let status = store.read(&mut s, &1u64, &0u64, &mut out, ());
    assert_eq!(status, OperationStatus::Ok);
    assert_eq!(out, Some(100));

    // Delete
    let status = store.delete(&mut s, &1u64, ());
    assert_eq!(status, OperationStatus::Deleted);

    store.dispose_session(s);
}
```

**Concurrent Test:**
```rust
#[test]
fn concurrent_upsert_and_read() {
    let store = small_store();
    let store = Arc::new(store);

    let handles: Vec<_> = (0..8)
        .map(|thread_id| {
            let store = Arc::clone(&store);
            thread::spawn(move || {
                let mut s = store.new_session();
                for i in 0..1000 {
                    let key = (thread_id * 1000 + i) as u64;
                    store.upsert(&mut s, &key, &(key * 7), ());
                }
                store.dispose_session(s);
            })
        })
        .collect();

    for h in handles {
        h.join().unwrap();
    }
}
```

**Stress Test (concurrent_stress.rs):**
```rust
fn epoch_stress_with_threads(num_threads: usize, cycles_per_thread: usize) {
    let table = Arc::new(EpochTable::new());
    let barrier = Arc::new(Barrier::new(num_threads));

    let handles: Vec<_> = (0..num_threads)
        .map(|_| {
            let table = Arc::clone(&table);
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                let et = table.register().expect("register");
                barrier.wait();  // Synchronize start
                
                for i in 0..cycles_per_thread {
                    let guard = et.protect();
                    if i % 100 == 0 {
                        table.bump_current_epoch_no_callback();
                    }
                    if i % 10 == 0 {
                        guard.refresh();
                    }
                    drop(guard);
                }
            })
        })
        .collect();

    for h in handles {
        h.join().expect("thread panicked");
    }
}
```

### Test Devices:
```rust
use faster_core::{NullDevice, InMemoryDevice, SyncFileDevice};

// NullDevice — discards all writes (for testing without I/O)
NullDevice::new()

// InMemoryDevice — stores pages in Vec<u8>
InMemoryDevice::new()

// SyncFileDevice — synchronous file-based device
SyncFileDevice::open("path", page_size)?
```

---

## 7. BUILD & TEST COMMANDS

### Script: `rust/scripts/precheckin`

**Usage:**
```bash
./rust/scripts/precheckin [--skip-tests]
```

**Gates (in order):**
1. `cargo fmt --all -- --check` — Code formatting
2. `cargo clippy --workspace --all-targets -- -D warnings` — Linting
3. `cargo test --doc --workspace` — Doctests (unless `--skip-tests`)
4. `cargo nextest run --workspace --all-targets` OR `cargo test --workspace` (unless `--skip-tests`)

**Exit:** 0 on all pass, non-zero on first failure

### Script: `rust/scripts/checkin`

**Usage:**
```bash
./rust/scripts/checkin [--skip-tests] -m "commit message"
```

**Process:**
1. Runs `./rust/scripts/precheckin` with optional `--skip-tests`
2. Stages changes and commits with supplied message
3. Auto-appends Co-authored-by trailer

### Manual Test Commands:

```bash
cd rust

# Format and fix
cargo fmt --all

# Check formatting
cargo fmt --all -- --check

# Lint
cargo clippy --workspace --all-targets -- -D warnings

# Run all tests
cargo test --workspace

# Run specific test
cargo test --test e2e_tests -- crud_lifecycle

# Run ignored tests (marked with #[ignore])
cargo test --workspace -- --ignored

# Run with backtrace
RUST_BACKTRACE=1 cargo test --workspace

# Run doctests
cargo test --doc --workspace

# Build docs
cargo doc --workspace --no-deps --open
```

---

## 8. MEMORY PRESSURE TEST DESIGN GUIDANCE

### Key Parameters for Pressure:

**Buffer Configuration:**
- `buffer_size_pages`: Controls total in-memory capacity
  - Small value (4-8) → forces aggressive eviction
  - Medium value (16-32) → balanced
  - Large value (64+) → loose constraints
  
- `mutable_fraction`: Controls how much stays writable
  - Low (0.5) → pages seal quickly → high flush/eviction
  - High (0.9) → pages stay open longer → lower pressure initially

**Eviction Policy:**
```rust
EvictionPolicy {
    max_in_memory_pages: u32,     // Target for page reclamation
    eviction_batch_size: u32,     // How many to evict at once
}
```
- Tight `max_in_memory_pages` (e.g., 4) → frequent eviction
- Small `eviction_batch_size` (e.g., 1) → evict one page per check

**Hash Index:**
- `hash_index_size_log2`: Lower → more collisions → longer overflow chains
- Default 20 (1M buckets) is loose; reduce to 10-14 for stress

### Test Patterns:

1. **Allocation Exhaustion:**
   - Pre-allocate many items in allocator
   - Verify free-list reuse works correctly
   - Monitor `count()` (total ever allocated)

2. **Page Lifecycle:**
   - Write data to fill mutable region
   - Watch pages transition: `Open → Sealed → Flushing → Flushed → Evicted`
   - Verify tail doesn't lap head

3. **Concurrent Pressure:**
   - N threads inserting simultaneously
   - Monitor lock-free CAS success rates
   - Verify no lost insertions or duplicates

4. **Eviction Correctness:**
   - Fill buffer to trigger eviction
   - Verify evicted data can be read back (from device)
   - Check `head_address` advances monotonically

5. **Flush Batching:**
   - Large write bursts
   - Monitor device I/O queue behavior
   - Verify pages flush in order
