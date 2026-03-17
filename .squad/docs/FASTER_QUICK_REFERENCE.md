# FASTER Rust Codebase — Quick Reference Table

## 1. ALLOCATOR API

| Method | Signature | Returns | Notes |
|--------|-----------|---------|-------|
| `new()` | `MallocFixedPageSize::new() -> Self` | Allocator | No params; pre-allocs page 0 |
| `allocate()` | `() -> LogicalAddress` | Address | Lock-free; never fails (panics only on extreme exhaustion) |
| `get()` | `(addr: LogicalAddress) -> &T` | Reference | Unsafe internally; resolve address to item |
| `get_ptr()` | `(addr: LogicalAddress) -> *mut T` | Raw pointer | For direct memory access |
| `free()` | `(addr: LogicalAddress)` | None | Epoch-gated if attached; otherwise immediate |
| `free_immediate()` | `(addr: LogicalAddress)` | None | Synchronous free-list push (no epoch) |
| `count()` | `() -> u64` | Total items | Ever bump-allocated; excludes free-list reuse |
| `set_epoch()` | `(epoch: Arc<EpochTable>)` | None | Attach epoch for deferred cleanup |

**Constants:**
- `ITEMS_PER_PAGE = 2^20 = 1,048,576` items/page
- `PAGE_ALIGNMENT = 64` bytes (cache-line)
- `T` must be ≥ 8 bytes, 8-byte aligned

---

## 2. PAGE MANAGEMENT API

| Type | Constructor | Key Methods |
|------|-------------|-------------|
| `PageState` | Enum: Free, Open, Sealed, Flushing, Flushed, Evicted | `from_u8()` |
| `AtomicPageState` | `new(state: PageState)` | `load()`, `try_transition(from, to) -> bool` |
| `PageFrame` | Allocated via PageTable | Holds one 32MB page; sector-aligned |
| `PageTable` | `new(buffer_size_pages, page_size, sector_size)` | `get_frame(page)`, `get_or_allocate_frame(page)` |

**Lifecycle:**
```
Free → Open → Sealed → Flushing → Flushed → Evicted → (Free, recycle)
```

---

## 3. HYBRID LOG ALLOCATOR API

| Method | Signature | Returns | Notes |
|--------|-----------|---------|-------|
| `new()` | `(buffer_size: usize, mutable_frac: f64, sector_size: usize) -> Self` | Allocator | Must validate params in constructor |
| `try_allocate()` | `(size: u32) -> Option<LogicalAddress>` | Option | `None` if cross page boundary or sealed |
| `advance_to_next_page()` | `() -> Option<LogicalAddress>` | Option | Seal current, open next |
| `get_physical_address()` | `(addr: LogicalAddress) -> Option<*mut u8>` | Option | Raw pointer to memory |
| `shift_read_only_to_tail()` | `()` | None | Advance read-only boundary |
| `is_mutable()` | `(addr: LogicalAddress) -> bool` | bool | In mutable region? |
| `is_safe_read_only()` | `(addr: LogicalAddress) -> bool` | bool | In safe read-only? |
| `is_in_memory()` | `(addr: LogicalAddress) -> bool` | bool | In memory (not evicted)? |
| `tail_address()`, `head_address()`, `read_only_address()`, `begin_address()` | `() -> LogicalAddress` | Address | Boundary snapshots |
| `seal()` | `()` | None | Prevent all future allocations |
| `page_table()` | `() -> &PageTable` | Reference | Access underlying page table |

---

## 4. HASH TABLE API

| Type/Method | Signature | Returns | Notes |
|-------------|-----------|---------|-------|
| `HashTable::new()` | `(log2_size: u32) -> Self` | Table | Creates 2^log2_size buckets; range [1..30] |
| `num_buckets()` | `() -> u64` | Count | Power of 2 |
| `bucket()` | `(hash: KeyHash) -> &HashBucket` | Reference | Hot-path; O(1) mask + lookup |
| `find_entry()` | `(hash: KeyHash) -> Option<(Entry, &AtomicSlot)>` | Option | Scans primary + overflow chain |
| `find_or_create_entry()` | `(hash, initial_addr) -> Result<FindOrCreateResult>` | Result | Two-phase insert protocol |
| `update_entry()` | `(slot, old, new_addr) -> bool` | bool | CAS the entry address |

**Entry Layout:**
- 14-bit tag (from key hash)
- 40-bit log address
- 1-bit tentative flag

**Overflow:**
- `OverflowBucketPool::new()` - Creates pool
- `allocate()` - Get new overflow bucket
- `get(addr)` - Resolve address to bucket
- `free(addr)` - Return to free list

---

## 5. STORE API

| Type | Constructor | Key Methods |
|------|-------------|-------------|
| `FasterKv<F>` | `new(config, functions, device)` | `new_session()`, `dispose_session()`, CRUD ops |
| `FasterSession<F>` | Via `store.new_session()` | `begin_unsafe()`, `complete_pending()`, `next_serial()` |
| `SimpleFunctions<K,V>` | `new()` or `default()` | Default read/upsert/rmw/delete |
| `CounterFunctions<V>` | `new()` | RMW increment semantics |

**CRUD Operations:**
```rust
store.upsert(&mut session, &key, &value, ctx) -> OperationStatus
store.read(&mut session, &key, &input, &mut output, ctx) -> OperationStatus
store.rmw(&mut session, &key, &input, &mut output, ctx) -> OperationStatus
store.delete(&mut session, &key, ctx) -> OperationStatus
store.complete_pending(&mut session) -> CompletePendingResult
```

**Config Parameters:**
```rust
pub struct FasterKvConfig {
    pub hash_index_size_log2: usize,        // 4..30 (default 20)
    pub buffer_size_pages: usize,           // Power of 2 (default 16)
    pub mutable_fraction: f64,              // 0.0..1.0 (default 0.9)
    pub sector_size: usize,                 // Power of 2 (default 512)
    pub eviction_policy: EvictionPolicy,    // max_in_memory_pages, batch_size
    pub grow_config: GrowConfig,
    pub auto_compact: bool,                 // (default false)
}
```

**EvictionPolicy:**
```rust
pub struct EvictionPolicy {
    pub max_in_memory_pages: u32,       // Default 256
    pub eviction_batch_size: u32,       // Default 4
}
```

---

## 6. TEST PATTERNS

### Basic CRUD Test
```rust
let store = FasterKv::new(config, SimpleFunctions::default(), InMemoryDevice::new());
let mut session = store.new_session();

store.upsert(&mut session, &key, &value, ());
let mut out: Option<u64> = None;
store.read(&mut session, &key, &0u64, &mut out, ());
assert_eq!(out, Some(value));

store.dispose_session(session);
```

### Concurrent Test
```rust
let store = Arc::new(FasterKv::new(config, SimpleFunctions::default(), device));

let handles: Vec<_> = (0..num_threads)
    .map(|thread_id| {
        let store = Arc::clone(&store);
        thread::spawn(move || {
            let mut session = store.new_session();
            // operations...
            store.dispose_session(session);
        })
    })
    .collect();

for h in handles {
    h.join().unwrap();
}
```

### Memory Pressure Config
```rust
FasterKvConfig {
    hash_index_size_log2: 10,           // Small: 1K buckets
    buffer_size_pages: 4,               // Tiny: 4 pages
    mutable_fraction: 0.5,              // Forces page sealing
    sector_size: 512,
    eviction_policy: EvictionPolicy {
        max_in_memory_pages: 3,
        eviction_batch_size: 1,
    },
    grow_config: GrowConfig::default(),
    auto_compact: false,
}
```

---

## 7. DEVICES

| Type | Usage | Notes |
|------|-------|-------|
| `NullDevice` | `NullDevice::new()` | Discards all writes; for in-memory testing |
| `InMemoryDevice` | `InMemoryDevice::new()` | Stores pages in Vec<u8>; test device |
| `SyncFileDevice` | `SyncFileDevice::open("path", page_size)?` | File-based synchronous I/O |

---

## 8. TEST COMMAND CHEATSHEET

```bash
cd rust/

# Format check
cargo fmt --all -- --check

# Lint
cargo clippy --workspace --all-targets -- -D warnings

# Run all tests
cargo test --workspace

# Run specific test
cargo test --test e2e_tests -- crud_lifecycle

# Run ignored tests
cargo test --workspace -- --ignored

# Run with backtrace
RUST_BACKTRACE=1 cargo test --workspace

# Pre-checkin (all gates)
./scripts/precheckin

# Pre-checkin skip tests (format + lint only)
./scripts/precheckin --skip-tests

# Commit with quality gates
./scripts/checkin -m "feat: description"
```

---

## 9. KEY ADDRESSES & CONSTANTS

| Constant | Value | Location | Notes |
|----------|-------|----------|-------|
| `ITEMS_PER_PAGE_BITS` | 20 | allocator.rs | Items per allocator page |
| `ITEMS_PER_PAGE` | 1,048,576 | allocator.rs | 2^20 |
| `OFFSET_BITS` | 25 | address.rs | Page size = 2^25 bytes = 32 MB |
| `PAGE_ALIGNMENT` | 64 | allocator.rs | Cache-line alignment |
| `DEFAULT_SECTOR_SIZE` | 512 | page.rs | Typical disk sector |
| `DEFAULT_PAGE_SIZE` | 2^25 | page.rs | 32 MB |
| `BUCKET_NUM_ENTRIES` | 7 | bucket.rs | Inline entries per bucket |
| `TAG_BITS` | 16 | allocator.rs | ABA tag in free list |

---

## 10. ERROR HANDLING

| Scenario | Behavior | API | Notes |
|----------|----------|-----|-------|
| Allocator exhaustion | Panic | `allocate()` | Returns `LogicalAddress` (infallible) |
| Page boundary cross | Return `None` | `try_allocate()` | Caller handles via `advance_to_next_page()` |
| Circular buffer full | Return `None` | `advance_to_next_page()` | Tail would lap head |
| Allocation failure | Panic | Global allocator | Via `handle_alloc_error` |
| Operation status | Return enum | CRUD ops | `OperationStatus::Ok`, `Pending`, `NotFound`, etc. |
| I/O error | Captured | `CompletedIo` | Result from pending I/O operations |

---

## 11. MEMORY MATH

**Default Store (1M buckets, 16 pages, 32MB/page):**
- Hash index: ~1M × 8 bytes = ~8 MB
- Hybrid log: 16 × 32 MB = 512 MB (in-memory capacity)
- Overflow buckets: ~64 MB (worst case, rarely hit)
- **Total: ~580 MB**

**Small Store (1K buckets, 4 pages):**
- Hash index: ~8 KB
- Hybrid log: 4 × 32 MB = 128 MB
- **Total: ~128 MB**

**Tiny Store (256 buckets, 2 pages):**
- Hash index: ~2 KB
- Hybrid log: 2 × 32 MB = 64 MB
- **Total: ~64 MB**

---

## 12. FILENAME → STRUCT MAPPING

| Struct | File | Path |
|--------|------|------|
| `MallocFixedPageSize<T>` | allocator.rs | `src/allocator.rs` |
| `PageFrame`, `PageState`, `PageTable` | page.rs | `src/hybrid_log/page.rs` |
| `HybridLogAllocator` | log_allocator.rs | `src/hybrid_log/log_allocator.rs` |
| `PageFlusher` | flush.rs | `src/hybrid_log/flush.rs` |
| `PageEvictor` | eviction.rs | `src/hybrid_log/eviction.rs` |
| `HashTable` | table.rs | `src/hash/table.rs` |
| `HashIndex` | index.rs | `src/hash/index.rs` |
| `OverflowBucketPool` | overflow.rs | `src/hash/overflow.rs` |
| `FasterKv<F>` | kv.rs | `src/store/kv.rs` |
| `FasterKvBuilder` | builder.rs | `src/store/builder.rs` |
| `FasterSession<F>` | session.rs | `src/store/session.rs` |

