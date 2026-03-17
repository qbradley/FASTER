# FASTER-Rust Compaction Scanner Analysis

## 1. ADDRESS TYPES

### File: `rust/crates/faster-core/src/address.rs`

#### `LogicalAddress` Type
- **Type Definition**: `pub struct LogicalAddress(u64)` — newtype over u64 (line 184)
- **Layout**: 48-bit address = 23-bit page index + 25-bit offset within page (lines 10-14)
  - Default page size: 2^25 = 32 MB
  - Total addressable: 256 TB (8,388,608 pages × 32 MB)
- **Key Constants** (lines 50-72):
  - `ADDRESS_BITS: u32 = 48`
  - `OFFSET_BITS: u32 = 25`
  - `PAGE_BITS: u32 = 23`
  - `MAX_OFFSET: u32 = 33,554,431`
  - `MAX_PAGE: u32 = 8,388,607`
  - `MAX_ADDRESS: u64 = 281,474,976,710,655` (2^48 - 1)

#### Special Values
- **`INVALID`** (line 198): `Self(1)` — Sentinel for "address field present but not yet pointing to a valid record"
- **`ZERO`** (line 201): `Self(0)` — Empty hash bucket slot
- **`MIN_VALID`** (line 204): `Self(2)` — First valid address
- **`MAX`** (line 208): `Self(MAX_ADDRESS)` — Largest representable address

#### Key Methods
| Method | Line | Signature | Purpose |
|--------|------|-----------|---------|
| `new()` | 226 | `pub const fn new(page: Page, offset: Offset) -> Self` | Constructor from page + offset |
| `from_raw()` | 248 | `pub const fn from_raw(raw: u64) -> Self` | Construct from raw u64 (masks upper 16 bits) |
| `raw()` | 254 | `pub const fn raw(self) -> u64` | Get raw u64 representation |
| `page()` | 269 | `pub const fn page(self) -> Page` | Extract page number (bits 25..48) |
| `offset()` | 284 | `pub const fn offset(self) -> Offset` | Extract offset (bits 0..25) |
| `is_valid()` | 303 | `pub const fn is_valid(self) -> bool` | Check if > INVALID (true only if >= MIN_VALID) |
| `is_null()` | 309 | `pub const fn is_null(self) -> bool` | Check if address == 0 |
| `in_read_cache()` | 318 | `pub const fn in_read_cache(self) -> bool` | Check if bit 47 set |

#### `Page` and `Offset` Newtypes
- **`Page`** (line 94): `pub struct Page(pub u32)` — page number (0 to 8,388,607)
- **`Offset`** (line 128): `pub struct Offset(pub u32)` — offset within page (0 to 33,554,431)

#### `AtomicLogicalAddress`
- Atomic wrapper (mentioned in imports, used in allocator for thread-safe address boundaries)

---

## 2. RECORD FORMAT

### File: `rust/crates/faster-core/src/record/`

#### Record Header (`RecordInfo`)
**File**: `record_info.rs`

- **Type Definition** (line 79): `pub struct RecordInfo(u64)` — packed 8-byte header
- **Bit Layout** (lines 9-25):
  ```
  63    62    61    60..48           47..0
  ┌─────┬─────┬─────┬──────────────┬───────────────────────────────┐
  │ Fin │ Tom │ Inv │ Version (13) │ Previous Address (48)         │
  └─────┴─────┴─────┴──────────────┴───────────────────────────────┘
  ```

#### Bit Fields
| Field | Bits | Mask | Purpose |
|-------|------|------|---------|
| Previous Address | 0-47 | `0x0000_FFFF_FFFF_FFFF` | 48-bit LogicalAddress to prior version |
| Checkpoint Version | 48-60 | `0x1FFF_0000_0000_0000` | 13-bit version (0-8191) |
| Invalid | 61 | `0x2000_0000_0000_0000` | Record superseded in mutable region |
| Tombstone | 62 | `0x4000_0000_0000_0000` | Logical deletion marker |
| Final | 63 | `0x8000_0000_0000_0000` | CPR coordination flag |

#### RecordInfo Methods
| Method | Line | Signature | Purpose |
|--------|------|-----------|---------|
| `new()` | 99 | `pub const fn new(prev_addr, version, invalid, tombstone, final_bit) -> Self` | Constructor |
| `from_raw()` | 139 | `pub const fn from_raw(raw: u64) -> Self` | From raw bits |
| `raw()` | 145 | `pub const fn raw(self) -> u64` | Get raw bits |
| `is_null()` | 151 | `pub const fn is_null(self) -> bool` | All bits zero? |
| `previous_address()` | 168 | `pub const fn previous_address(self) -> LogicalAddress` | Extract prior version pointer |
| `checkpoint_version()` | 184 | `pub const fn checkpoint_version(self) -> u16` | Extract version (0-8191) |
| `is_invalid()` | 193 | `pub const fn is_invalid(self) -> bool` | Check invalid bit (bit 61) |
| `is_tombstone()` | 202 | `pub const fn is_tombstone(self) -> bool` | Check tombstone bit (bit 62) |
| `is_final()` | 210 | `pub const fn is_final(self) -> bool` | Check final bit (bit 63) |
| `with_invalid()` | 216 | `pub const fn with_invalid(self) -> Self` | Return copy with invalid bit set |
| `with_tombstone()` | 222 | `pub const fn with_tombstone(self) -> Self` | Return copy with tombstone bit set |
| `with_previous_address()` | 234 | `pub const fn with_previous_address(self, addr) -> Self` | Replace previous address |

#### `AtomicRecordInfo`
- **Type Definition** (line 335 in record_info.rs): `pub struct AtomicRecordInfo(AtomicU64)`
- Supports atomic CAS operations for setting invalid/tombstone flags

#### Record Layout (`RecordLayout`)
**File**: `layout.rs`

- **Type Definition** (line 98): Struct with computed offsets
  ```rust
  pub struct RecordLayout {
      key_offset: usize,      // Always 8 (right after 8-byte header)
      value_offset: usize,    // 8-byte aligned after key
      total_size: usize,      // 8-byte aligned including padding
  }
  ```

#### Layout Computation
| Constant | Value | Purpose |
|----------|-------|---------|
| `RECORD_HEADER_SIZE` | 8 | RecordInfo header bytes |
| `RECORD_ALIGNMENT` | 8 | All records 8-byte aligned |

#### RecordLayout Methods
| Method | Line | Signature | Purpose |
|--------|------|-----------|---------|
| `compute()` | 119 | `pub const fn compute(key_size, value_size) -> Self` | Pre-compute layout for sizes |
| `for_kv()` | 135 | `pub fn for_kv<K: Key, V: Value>(key, value) -> Self` | Compute from actual instances |
| `key_offset()` | 141 | `pub const fn key_offset(&self) -> usize` | Byte offset to key data |
| `value_offset()` | 147 | `pub const fn value_offset(&self) -> usize` | Byte offset to value data |
| `total_size()` | 153 | `pub const fn total_size(&self) -> usize` | Total record size with padding |

**Example Layout for u64 key + u64 value:**
```
Offset 0:   [ RecordInfo (8 bytes)  ]
Offset 8:   [ Key (8 bytes)         ]  ← key_offset = 8
Offset 16:  [ Value (8 bytes)       ]  ← value_offset = 16
Offset 24:  [padding to 8 align]
total_size = 24
```

#### Record Serialization/Deserialization
| Function | Line | Signature | Purpose |
|----------|------|-----------|---------|
| `write_record()` | 187 | `pub fn write_record<K, V>(buf, info, key, value, layout)` | Write full record to buffer |
| `read_record_info()` | 219 | `pub fn read_record_info(buf) -> RecordInfo` | Read 8-byte header |
| `read_key()` | 234 | `pub fn read_key<K: Key>(buf, layout) -> K` | Deserialize key from layout |
| `read_value()` | 244 | `pub fn read_value<V: Value>(buf, layout) -> V` | Deserialize value from layout |
| `record_size()` | 253 | `pub fn record_size<K, V>(key, value) -> usize` | Total size for given K/V |
| `pad_alignment()` | 52 | `pub const fn pad_alignment(size, alignment) -> usize` | Round up to alignment |

---

## 3. HYBRID LOG

### File: `rust/crates/faster-core/src/hybrid_log/`

#### Main Allocator Type
**File**: `log_allocator.rs`, lines 37-61

```rust
pub struct HybridLogAllocator {
    /// The page table backing this allocator.
    page_table: PageTable,
    
    /// Start of valid data on disk.
    begin_address: AtomicLogicalAddress,
    /// Start of in-memory region (pages below are evicted).
    head_address: AtomicLogicalAddress,
    /// Start of read-only region (below: read-only, above: mutable).
    read_only_address: AtomicLogicalAddress,
    /// All threads have observed the read-only transition up to here.
    safe_read_only_address: AtomicLogicalAddress,
    /// Next allocation point (bump pointer).
    tail_address: AtomicLogicalAddress,
    
    /// How far the log has been flushed to disk.
    flushed_until_address: AtomicLogicalAddress,
    
    /// Bytes per page (2^OFFSET_BITS = 2^25 = 33,554,432).
    page_size: u32,
    /// Number of mutable pages before the read-only boundary advances.
    mutable_fraction_pages: u32,
    /// Whether the allocator is sealed (no more allocations).
    sealed: AtomicBool,
}
```

#### Address Regions
```
begin_addr ≤ head_addr ≤ read_only_addr ≤ safe_read_only_addr ≤ tail_addr
    ↓           ↓              ↓                      ↓              ↓
  [On-disk]  [Read-only  ]  [Safe read-only]  [Mutable         ]
             [in-memory  ]  [in-memory       ]  [in-memory       ]
```

#### Key Methods
| Method | Line | Signature | Purpose |
|--------|------|-----------|---------|
| `new()` | 71 | `pub fn new(buffer_pages, mutable_fraction, sector_size)` | Create allocator |
| `try_allocate()` | 105 | `pub fn try_allocate(&self, size: u32) -> Option<LogicalAddress>` | Lock-free CAS-based allocation |
| `get_physical_address()` | 174 | `pub fn get_physical_address(&self, addr) -> Option<*mut u8>` | Logical→physical address translation |
| `shift_read_only_to_tail()` | 194 | `pub fn shift_read_only_to_tail(&self)` | Advance read-only boundary to tail |
| `is_mutable()` | 234 | `pub fn is_mutable(&self, addr) -> bool` | Check if address in mutable region |
| `is_safe_read_only()` | 243 | `pub fn is_safe_read_only(&self, addr) -> bool` | Check if address in safe read-only region |
| `is_in_memory()` | 251 | `pub fn is_in_memory(&self, addr) -> bool` | Check if address in memory (head..tail) |
| `tail_address()` | 259 | `pub fn tail_address(&self) -> LogicalAddress` | Get current tail (next allocation point) |
| `read_only_address()` | 265 | `pub fn read_only_address(&self) -> LogicalAddress` | Get read-only boundary |
| `head_address()` | 271 | `pub fn head_address(&self) -> LogicalAddress` | Get head address (eviction boundary) |
| `begin_address()` | 277 | `pub fn begin_address(&self) -> LogicalAddress` | Get begin address (disk start) |
| `seal()` | 282 | `pub fn seal(&self)` | Seal allocator (no more allocations) |
| `page_table()` | 288 | `pub fn page_table(&self) -> &PageTable` | Get underlying page table |
| `advance_to_next_page()` | 298 | `pub fn advance_to_next_page() -> Option<LogicalAddress>` | Manually advance page when full |
| `page_size()` | 356 | `pub fn page_size(&self) -> u32` | Get bytes per page (33,554,432) |

#### Record Access Types
**File**: `record_ops.rs`

##### `RecordAccessor` (Read-only)
- **Type Definition** (line 39-44)
- **Constructor**: `pub unsafe fn new(ptr: *const u8, record_size: u32) -> Self` (line 59)
- **Key Methods**:
  - `record_info()` (line 74): Read RecordInfo header
  - `atomic_record_info()` (line 84): Get &AtomicRecordInfo for CAS operations
  - `key<K>()` (line 93): Deserialize key
  - `value<V>()` (line 99): Deserialize value
  - `key_ref()` (line 108): Get raw key bytes (zero-copy)
  - `value_ref()` (line 118): Get raw value bytes (zero-copy)
  - `as_slice()` (line 124): Get full record as &[u8]
  - `record_size()` (line 131): Get total record size
  - `from_log()` (line 144): Create from LogicalAddress via allocator

##### `MutableRecordAccessor` (Read-write)
- **Type Definition** (line 171-176)
- **Constructor**: `pub unsafe fn new(ptr: *mut u8, record_size: u32) -> Self` (line 190)
- **Write Methods**:
  - `write_record_info()` (line 272): Write header
  - `write_key()` (line 278): Serialize and write key
  - `write_value()` (line 286): Serialize and write value
  - `write_full_record()` (line 295): Write complete record
  - `value_mut_ptr()` (line 250): Get mutable pointer to value data

##### `VersionChainIterator`
- **Type Definition** (line 520): Walks previous_address chain
- **Constructor**: `pub fn new(allocator) -> Self` (line 525)
- **Iterator**: Implements `Iterator<Item = (LogicalAddress, RecordAccessor)>`

#### Scan/Iteration
**File**: `scan.rs`

##### `ScanOptions`
- **Type Definition** (line 42):
  ```rust
  pub struct ScanOptions {
      pub include_tombstones: bool,   // Default: false
      pub include_invalid: bool,      // Default: false
      pub in_memory_only: bool,       // Default: false
  }
  ```

##### `ScanRecord`
- **Type Definition** (line 84):
  ```rust
  pub struct ScanRecord {
      pub address: LogicalAddress,        // Record's logical address
      pub record_info: RecordInfo,        // 8-byte header
      pub key_size: u32,                  // Serialized key size
      pub value_size: u32,                // Serialized value size
      key_data: Vec<u8>,                  // Raw key bytes
      value_data: Vec<u8>,                // Raw value bytes
  }
  ```
- **Key Methods**:
  - `key_bytes()` (line 107): Get raw key bytes
  - `value_bytes()` (line 112): Get raw value bytes

##### `LogScanIterator`
- **Type Definition** (line 138):
  ```rust
  pub struct LogScanIterator<'a> {
      allocator: &'a HybridLogAllocator,
      current: LogicalAddress,            // Current scan position
      end: LogicalAddress,                // Exclusive upper bound
      layout: RecordLayout,               // Pre-computed record layout
      options: ScanOptions,               // Scan filters
      page_size: u32,                     // Cached from allocator
  }
  ```
- **Constructor**: `pub fn new(allocator, start, end, key_size, value_size, options) -> Self` (line 167)
- **Iterator**: Implements `Iterator<Item = ScanRecord>` — yields matching records

#### Page Management
**File**: `page.rs`

- **Default page size** (line 31): `DEFAULT_PAGE_SIZE = 1 << OFFSET_BITS = 2^25 = 33,554,432 bytes (32 MB)`
- **Default sector size** (line 28): `DEFAULT_SECTOR_SIZE = 512 bytes`

##### `PageState` Enum
- **States** (lines 43-56):
  - `Free` — page not allocated or evicted
  - `Open` — current writable page
  - `Sealed` — no more writes allowed
  - `Flushing` — async flush in progress
  - `Flushed` — successfully written to disk
  - `Evicted` — removed from memory (data on disk only)

---

## 4. HASH INDEX

### File: `rust/crates/faster-core/src/hash/`

#### Main Index Type
**File**: `index.rs`, lines 103-116

```rust
pub struct HashIndex {
    /// The latch-free concurrent hash table.
    table: HashTable,
    
    /// Epoch coordination table for safe memory reclamation.
    epoch: Arc<EpochTable>,
    
    /// Current table version (0 or 1). Toggled on each grow.
    version: AtomicU32,
    
    /// Approximate count of live entries.
    live_entry_count: AtomicU64,
}
```

#### Key Methods
| Method | Line | Signature | Purpose |
|--------|------|-----------|---------|
| `new()` | 143 | `pub fn new(log2_size: u32) -> Self` | Create with 2^log2_size buckets |
| `with_epoch()` | 173 | `pub fn with_epoch(log2_size, epoch) -> Self` | Create with shared epoch table |
| `register_thread()` | 205 | `pub fn register_thread() -> Option<EpochThread>` | Register for epoch participation |
| `find()` | 239 | `pub fn find(hash) -> Option<(HashBucketEntry, &AtomicHashBucketEntry)>` | Lookup existing entry |
| `find_or_create()` | 271 | `pub fn find_or_create(hash, initial_addr) -> FindOrCreateResult` | Find or create tentative entry |
| `update()` | 323 | `pub fn update(slot, old, new) -> bool` | CAS-update entry (commit/invalidate) |
| `invalidate_entries_in_range()` | 341 | Scan and zero entries with addresses in [begin, end) |

#### Underlying Hash Table
**File**: `table.rs`, lines 82-117

```rust
pub struct HashTable {
    /// Primary bucket array (power-of-two sized, cache-line aligned).
    buckets: Box<[HashBucket]>,
    
    /// Number of buckets (always power of 2).
    num_buckets: u64,
    
    /// Log2 of num_buckets.
    log2_buckets: u32,
    
    /// Pool for overflow bucket allocation.
    overflow_pool: OverflowBucketPool,
}
```

#### Hash Bucket Structure
**File**: `bucket.rs`, lines 463-510

```rust
pub const BUCKET_NUM_ENTRIES: usize = 7;  // 7 inline entries per bucket

pub struct HashBucket {
    entries: [AtomicHashBucketEntry; 7],   // 7 × 8 bytes = 56 bytes
    // 8-byte overflow pointer makes 64 bytes (one cache line)
}
```

#### Hash Bucket Entry
**File**: `bucket.rs`, lines 100-247

- **Type Definition** (line 138): `pub struct HashBucketEntry(u64)` — packed u64
- **Bit Layout** (lines 108-114):
  ```
  63      62      61..48            47..0
  ┌───────┬───────┬────────────────┬─────────────────────────────┐
  │ Tent  │ Rsvd  │   Tag (14)     │   Address (48)              │
  └───────┴───────┴────────────────┴─────────────────────────────┘
  ```

#### HashBucketEntry Methods
| Method | Line | Signature | Purpose |
|--------|------|-----------|---------|
| `new()` | 160 | `pub const fn new(tag, address, tentative)` | Constructor |
| `from_raw()` | 179 | `pub const fn from_raw(raw: u64) -> Self` | From raw bits |
| `raw()` | 185 | `pub const fn raw(self) -> u64` | Get raw bits |
| `address()` | 194 | `pub const fn address(self) -> LogicalAddress` | Extract 48-bit address |
| `tag()` | 206 | `pub const fn tag(self) -> u16` | Extract 14-bit tag (0-16383) |
| `is_tentative()` | 216 | `pub const fn is_tentative(self) -> bool` | Check tentative bit |
| `is_empty()` | 225 | `pub const fn is_empty(self) -> bool` | Check if all zeros |
| `with_tentative()` | 234 | `pub const fn with_tentative(self)` | Return copy with tentative set |
| `without_tentative()` | 244 | `pub const fn without_tentative(self)` | Return copy with tentative cleared |

#### Two-Phase Insert Protocol
1. **Phase 1 (Reserve)**: CAS empty slot → tentative entry
   - `find_or_create()` does this, returns `FindOrCreateResult { created: true, entry, slot }`
2. **Write Record**: Allocate and write record to hybrid log
3. **Phase 2 (Commit)**: CAS tentative entry → committed entry
   - `update(slot, old_tentative, new_committed)` does this

#### Key Hash Type
**File**: `hash.rs` (re-exported from `hash::hash.rs`)

- **`KeyHash`** — 64-bit hash value with:
  - `tag()` method extracts 14-bit fingerprint (bits 48-61)
  - Used in bucket selection: `bucket_index = hash.raw() % num_buckets`

---

## 5. STORE OPERATIONS

### File: `rust/crates/faster-core/src/store/`

#### Main Store Type
**File**: `kv.rs`, lines 135-175

```rust
pub struct FasterKv<F: Functions> {
    hash_index: HashIndex,
    allocator: HybridLogAllocator,
    functions: F,
    device: Box<dyn Device>,
    epoch_table: Arc<EpochTable>,
    session_pool: SessionPool<F>,
    flusher: PageFlusher,
    evictor: PageEvictor,
    grow_manager: GrowManager,
    checkpoint_orchestrator: CheckpointOrchestrator,
    pending_io_manager: PendingIoManager,
    config: FasterKvConfig,
}
```

#### FasterKvConfig
**File**: `kv.rs`, lines 106-132

```rust
pub struct FasterKvConfig {
    pub hash_index_size_log2: usize,       // Log2 of bucket count (default: 20 → 1M)
    pub buffer_size_pages: usize,          // In-memory page frames (default: 16, must be power of 2)
    pub mutable_fraction: f64,             // Fraction of buffer that's mutable (default: 0.9)
    pub sector_size: usize,                // Page alignment (default: 512)
    pub eviction_policy: EvictionPolicy,   // How to handle page eviction
    pub grow_config: GrowConfig,           // Hash table grow settings
}
```

#### Session Management
**File**: `kv.rs`, lines 369-380

| Method | Line | Signature | Purpose |
|--------|------|-----------|---------|
| `new_session()` | 369 | `pub fn new_session(&self) -> FasterSession<F>` | Create thread-local session |
| `dispose_session()` | 377 | `pub fn dispose_session(&self, session)` | Clean up session |
| `session_scope()` | 405 | `pub fn session_scope<R>(&self, f) -> R` | Execute closure with auto-managed session |

#### FasterSession Type
**File**: `session.rs`, lines 196-211

```rust
pub struct FasterSession<F: Functions> {
    epoch_thread: EpochThread,
    epoch_table: Arc<EpochTable>,
    pending_ops: Vec<PendingOperation<F>>,    // Queued disk operations
    io_contexts: Vec<PendingIoContext<F>>,    // In-flight I/O contexts
    serial_number: u64,                       // Monotonic operation counter
    in_epoch: bool,
    _not_send: PhantomData<*const ()>,        // !Send — thread-affine
}
```

#### Session Methods
| Method | Line | Signature | Purpose |
|--------|------|-----------|---------|
| `new()` | 215 | (internal) Create new session via SessionPool |
| `begin_unsafe()` | 239 | `pub fn begin_unsafe(&mut self) -> SessionGuard<'_, F>` | Enter epoch protection |
| `end_unsafe()` | 250 | `pub fn end_unsafe(&mut self)` | Leave epoch protection |
| `next_serial()` | 262 | `pub fn next_serial(&mut self) -> u64` | Get next operation serial number |
| `enqueue_pending()` | 275 | `pub fn enqueue_pending(&mut self, op)` | Queue pending disk operation |
| `drain_pending_ops()` | 286 | `pub fn drain_pending_ops(&mut self) -> Vec<...>` | Get all pending operations |
| `enqueue_io_context()` | 295 | `pub fn enqueue_io_context(&mut self, ctx)` | Add in-flight I/O context |
| `take_completed_io()` | 303 | `pub fn take_completed_io(&mut self) -> Vec<...>` | Poll for completed I/O |
| `wait_for_all_io()` | 328 | `pub fn wait_for_all_io(&mut self) -> Vec<...>` | Spin-wait for all I/O completion |

#### CRUD Operations
**File**: `kv.rs`

##### Read
| Item | Value |
|------|-------|
| **Method** | `pub fn read(...)` @ line 731 |
| **Signature** | `pub fn read(&self, session, key, input, output, context) -> OperationStatus` |
| **Returns** | `OperationStatus::{Ok, NotFound, Pending, ...}` |
| **Calls** | `internal_read()` (operations.rs) |
| **Behavior** | Looks up key in hash index, reads value, calls `Functions::read()` callback |

##### Upsert
| Item | Value |
|------|-------|
| **Method** | `pub fn upsert(...)` @ line 773 |
| **Signature** | `pub fn upsert(&self, session, key, input, context) -> OperationStatus` |
| **Returns** | `OperationStatus::{Created, InPlaceUpdated, CopyUpdated, Pending, ...}` |
| **Calls** | `internal_upsert()` (operations.rs) |
| **Behavior** | Two-phase: (1) find_or_create in hash index (2) write record to log |

##### Delete
| Item | Value |
|------|-------|
| **Method** | `pub fn delete(...)` @ line 853 |
| **Signature** | `pub fn delete(&self, session, key, context) -> OperationStatus` |
| **Returns** | `OperationStatus::{Deleted, NotFound, Pending, ...}` |
| **Calls** | `internal_delete()` (operations.rs) |
| **Behavior** | Marks record as tombstone (in-place if mutable, copy if read-only) |

##### RMW
| Item | Value |
|------|-------|
| **Method** | `pub fn rmw(...)` @ line 813 |
| **Signature** | `pub fn rmw(&self, session, key, input, output, context) -> OperationStatus` |
| **Returns** | `OperationStatus::{Created, InPlaceUpdated, CopyUpdated, Pending, ...}` |
| **Calls** | `internal_rmw()` (operations.rs) |
| **Behavior** | Read-modify-write with callback-based update logic |

#### Functions Trait
**File**: `functions.rs`, lines 54-180+

```rust
pub trait Functions: Send + Sync + 'static {
    type Key: Key;                          // Key type (must be Hashable)
    type Value: Value;                      // Value type (must be serializable)
    type Input: Send + Sync + Clone;        // Input for operations
    type Output: Send + Sync + Default;     // Output from operations
    type Context: Send + Sync;              // Opaque context
    
    // Core callbacks
    fn read(&self, key, value, input, output);
    fn read_completion(&self, key, output, context, status) { }
    fn upsert(&self, key, value, input, old_value, output);
    fn rmw_in_place(&self, ...);
    fn rmw_copy_update(&self, ...);
    fn rmw_initial(&self, ...);
    fn delete(&self, ...);
    
    // Optional: raw in-place support (bypass serialize/deserialize)
    const SUPPORTS_RAW_IN_PLACE: bool = false;
}
```

#### SimpleFunctions Implementation
**File**: `functions.rs` (likely around lines 400-500)

- **Generic**: `SimpleFunctions<K, V>` where K and V are both Key + Value
- **Semantics**: Full-value replacement on upsert
- **Input = Value**: Upsert input is the new value
- **Output = Value**: Read returns the value directly

---

## 6. EXISTING TESTS & EXAMPLES

### Integration Tests
**File**: `tests/integration_basics.rs`

#### Test: `address_to_record_info_round_trip`
- **Line**: 26
- **Shows**: LogicalAddress → RecordInfo → LogicalAddress (version chain pattern)
- **Tests**: All special values (ZERO, INVALID, MIN_VALID, MAX_PAGE/OFFSET)

#### Test: `record_info_mutation_chain`
- **Line**: 57
- **Shows**: Build RecordInfo incrementally (version, invalid, tombstone)

#### Test: `allocator_store_and_read_record`
- **Line**: 88
- **Shows**: Allocate address → write record → read back
- **Key pattern**: `RecordLayout::for_kv()`, `write_record()`, `read_record_info()`, `read_key()`

#### Test: `allocator_multiple_records_independent`
- **Line**: 125
- **Shows**: Allocate 100 records, verify all unique addresses and correct values

### Store Usage Examples
**File**: `tests/e2e_tests.rs` (and similar)

#### Basic Setup
```rust
let config = FasterKvConfig::default();
let device = NullDevice::new();
let store: FasterKv<SimpleFunctions<u64, u64>> = FasterKv::new(
    config,
    SimpleFunctions::default(),
    device,
);

let mut session = store.new_session();
{
    let _guard = session.begin_unsafe();
    // Operations
}
store.dispose_session(session);
```

#### Upsert and Read
```rust
let status = store.upsert(&mut session, &key, &value, context);
let status = store.read(&mut session, &key, &input, &mut output, context);
```

---

## COMPACTION SCANNER DESIGN CONSIDERATIONS

### Key Points for Implementation

1. **Address Iteration**: Use `LogicalAddress` with page/offset extractors
   - Scan from `begin_address()` to `tail_address()` via allocator
   - Check `is_in_memory()` before attempting access

2. **Record Detection**: Use `RecordLayout::compute()` to know size
   - Handle variable-length keys/values (must know sizes upfront)
   - Read header: `read_record_info()` at address

3. **Version Chain Following**: Use `RecordInfo::previous_address()`
   - Traverse backwards through `LogicalAddress` pointers
   - Stop when reaching `LogicalAddress::INVALID` or loop detection

4. **Filter Records**: Use `RecordInfo` flags
   - Skip tombstones: `is_tombstone()` (bit 62)
   - Skip invalid records: `is_invalid()` (bit 61) [superseded versions]
   - Skip non-final records if needed: `is_final()` (bit 63)

5. **Existing Scan Utilities**:
   - `LogScanIterator` provides page-boundary-aware forward scan
   - `ScanOptions` controls filtering (include_tombstones, include_invalid)
   - `VersionChainIterator` walks backwards through version chains

6. **Raw Memory Access**:
   - `HybridLogAllocator::get_physical_address()` for logical→physical
   - `RecordAccessor` for zero-copy record reads
   - All page memory is 8-byte aligned

7. **Critical Addresses**:
   - Page size: `allocator.page_size()` (default 33,554,432 bytes)
   - Region boundaries: head, read_only, safe_read_only, tail

