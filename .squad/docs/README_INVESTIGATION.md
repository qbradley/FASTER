# FASTER Rust Codebase Investigation — Complete Documentation

This directory contains a comprehensive analysis of the FASTER Rust codebase for writing memory pressure tests.

## 📄 Documents Included

### 1. **FASTER_CODEBASE_ANALYSIS.md** (744 lines)
Complete technical deep-dive covering:
- **Allocator** (`MallocFixedPageSize<T>`) — public API, constructor args, page size, allocation strategy, free-list behavior, failure modes
- **Buffer Pool & Page Management** — PageFrame lifecycle, PageState state machine, PageTable circular buffer, lazy allocation
- **Hybrid Log Directory** — all files listed, HybridLogAllocator with region boundaries (begin/head/read_only/safe_read_only/tail)
- **Hash Table** (`HashTable`, `HashIndex`) — constructor, operations, bucket layout, overflow chains, capacity/load factor
- **Store** (`FasterKv<F>`) — constructor args, configuration, CRUD operations, session model, small store example
- **Existing Tests** — test file inventory, patterns used (basic CRUD, concurrent, stress), test helpers
- **Build/Test Commands** — precheckin script, checkin script, manual test commands

### 2. **FASTER_TEST_EXAMPLES.md** (493 lines)
Production-ready test code examples:
- Quick setup template for tiny, small, and medium stores
- 9 complete, compilable test functions:
  1. Allocator free-list reuse
  2. Page state transitions
  3. Hybrid log page sealing under pressure
  4. Concurrent memory pressure (8 threads)
  5. Allocator exhaustion stress (1M items)
  6. Hash collision stress (10K keys, 256 buckets)
  7. Page flush & eviction cycle
  8. RMW under memory pressure
  9. Pending I/O completion
- Helper macros and utilities

### 3. **FASTER_QUICK_REFERENCE.md** (283 lines)
Quick-lookup tables and checklists:
- **12 sections** with API summary tables:
  1. Allocator API (8 methods)
  2. Page Management API
  3. Hybrid Log Allocator API
  4. Hash Table API
  5. Store API
  6. Test patterns (CRUD, concurrent, pressure config)
  7. Devices (NullDevice, InMemoryDevice, SyncFileDevice)
  8. Test command cheatsheet
  9. Key constants and addresses
  10. Error handling scenarios
  11. Memory math (storage calculations)
  12. Filename → Struct mapping

## 🎯 Quick Start for Writing Memory Pressure Tests

### Configuration Parameters You'll Need:

**For tight memory pressure (forces eviction):**
```rust
FasterKvConfig {
    hash_index_size_log2: 10,           // 1,024 buckets (small)
    buffer_size_pages: 4,               // 4 × 32MB = 128MB total
    mutable_fraction: 0.5,              // Forces pages to seal quickly
    sector_size: 512,
    eviction_policy: EvictionPolicy {
        max_in_memory_pages: 3,         // Keep only 3 pages
        eviction_batch_size: 1,         // Evict one at a time
    },
    grow_config: GrowConfig::default(),
    auto_compact: false,
}
```

### Key Struct Names & Constructor Patterns:

| Component | Constructor | File |
|-----------|-------------|------|
| **Allocator** | `MallocFixedPageSize::<T>::new()` | `src/allocator.rs` |
| **PageTable** | `PageTable::new(buffer_size, page_size, sector_size)` | `src/hybrid_log/page.rs` |
| **HybridLogAllocator** | `HybridLogAllocator::new(buffer_size, mutable_frac, sector_size)` | `src/hybrid_log/log_allocator.rs` |
| **HashTable** | `HashTable::new(log2_size)` | `src/hash/table.rs` |
| **FasterKv** | `FasterKv::new(config, functions, device)` | `src/store/kv.rs` |
| **Session** | `store.new_session()` | `src/store/session.rs` |

### Critical API Methods:

**Allocator:**
- `allocate() -> LogicalAddress` — lock-free, never fails
- `free(addr)` — epoch-gated or immediate
- `get(addr) -> &T` — resolve address to item

**HybridLogAllocator:**
- `try_allocate(size) -> Option<LogicalAddress>` — returns None on page boundary
- `advance_to_next_page() -> Option<LogicalAddress>` — seal current, open next
- `is_mutable(addr)`, `is_in_memory(addr)` — region checks

**Store:**
- `upsert(&mut session, key, value, ctx) -> OperationStatus`
- `read/rmw/delete` — same pattern
- `complete_pending(&mut session) -> CompletePendingResult`

### Test Devices:

```rust
use faster_core::{InMemoryDevice, NullDevice};

InMemoryDevice::new()  // For full integration tests
NullDevice::new()      // For memory tests (no disk I/O)
```

## 📊 Key Findings Summary

### Memory Pressure Parameters:

| Parameter | Small Value | Large Value | Effect |
|-----------|------------|------------|--------|
| `buffer_size_pages` | 2-4 | 32-64 | Tight vs loose memory |
| `mutable_fraction` | 0.3-0.5 | 0.9 | Forces/delays page sealing |
| `max_in_memory_pages` | 1-3 | 256 | Triggers frequent eviction |
| `eviction_batch_size` | 1 | 8 | Coarse vs fine eviction |
| `hash_index_size_log2` | 8-10 | 20 | More collisions → longer chains |

### Allocation & Failure Modes:

1. **Allocator** — panics on layout overflow or global allocator failure; never returns `Err`
2. **HybridLogAllocator** — returns `None` on page boundary or circular buffer full
3. **HashTable** — no explicit capacity limit (overflow chains grow)
4. **PageTable** — lazy allocation via CAS; fails if page not allocated

### Lock-Free Mechanisms:

- **Allocator**: Treiber stack (free list) + bump pointer (fresh allocation)
- **HybridLogAllocator**: CAS on tail_address
- **HashTable**: CAS on bucket entries for two-phase insert
- **PageState**: CAS for atomic state transitions

### Epoch Integration:

- Sessions hold epoch guards during operation batches
- Allocator can attach EpochTable for deferred free-list cleanup (prevents ABA)
- Hash index uses epoch for GC of invalidated entries

## 🔬 Test Patterns in Codebase

**Foundation tests** (`integration_basics.rs`):
- Address ↔ RecordInfo round-trip
- Allocator → write record → read back
- Bucket entry manipulation
- Version chain pointer patterns

**E2E tests** (`e2e_tests.rs`):
- CRUD lifecycle (create/read/update/delete)
- Large-scale inserts (10K+ keys)
- Multi-threaded concurrent access
- Page sealing and flushing

**Stress tests** (`concurrent_stress.rs`):
- Epoch system with 4-16 threads
- Allocator concurrent allocation/free
- Bucket concurrent find/insert

**Helpers** (`tests/common/mod.rs`):
```rust
addr(page, offset) -> LogicalAddress
fill_bucket(bucket, base_tag, base_addr)
hash_key(key) -> (tag, hash_value)
```

## 🚀 Build & Test Commands

```bash
cd rust/

# Quick checks (format + lint only)
./scripts/precheckin --skip-tests

# Full quality gates (format → lint → tests)
./scripts/precheckin

# Commit with gates
./scripts/checkin -m "feat: memory pressure tests"

# Run specific test
cargo test --test e2e_tests -- crud_lifecycle

# Run with backtrace
RUST_BACKTRACE=1 cargo test --test memory_pressure_tests
```

## 📋 All Source Files Analyzed

### Core Allocator:
- `src/allocator.rs` — MallocFixedPageSize (2-level page directory + free list)

### Hybrid Log:
- `src/hybrid_log/mod.rs` — exports
- `src/hybrid_log/page.rs` — PageFrame, PageState, PageTable (circular buffer)
- `src/hybrid_log/log_allocator.rs` — HybridLogAllocator (bump + regions)
- `src/hybrid_log/flush.rs` — PageFlusher (async/sync writes)
- `src/hybrid_log/eviction.rs` — PageEvictor (reclamation)
- `src/hybrid_log/regions.rs` — region boundary tracking
- `src/hybrid_log/record_ops.rs` — record serialization
- `src/hybrid_log/scan.rs` — sequential scan iterator

### Hash Table:
- `src/hash/mod.rs` — exports
- `src/hash/hash.rs` — FasterHash, KeyHash, Hashable trait
- `src/hash/bucket.rs` — HashBucket (7 entries + overflow pointer)
- `src/hash/table.rs` — HashTable (latch-free CAS operations)
- `src/hash/index.rs` — HashIndex (wraps table + epoch)
- `src/hash/overflow.rs` — OverflowBucketPool (MallocFixedPageSize<HashBucket>)

### Store:
- `src/store/mod.rs` — exports
- `src/store/kv.rs` — FasterKv<F> (main API)
- `src/store/builder.rs` — FasterKvBuilder (fluent API)
- `src/store/session.rs` — FasterSession (thread-affine)
- `src/store/functions.rs` — Functions trait, SimpleFunctions, CounterFunctions
- `src/store/operations.rs` — CRUD implementation
- `src/store/batch.rs` — batch API
- `src/store/pending_io.rs` — I/O completion handling

### Tests:
- `tests/integration_basics.rs`
- `tests/e2e_tests.rs`
- `tests/concurrent_stress.rs`
- `tests/hash_index_correctness.rs`
- `tests/hash_index_concurrent.rs`
- `tests/checkpoint_recovery_tests.rs`
- `tests/compaction_integration.rs`
- `tests/epvs_integration.rs`
- `tests/variable_length.rs`
- `tests/batch_tests.rs`
- `tests/loom_tests.rs`
- `tests/miri_tests.rs`
- `tests/property_tests.rs`
- `tests/quickstart_verify.rs`
- `tests/write_pending_completion.rs`
- `tests/common/mod.rs` (helpers)

## 💾 File Locations in Your Environment

```
/home/azureuser/FASTER_CODEBASE_ANALYSIS.md     ← Detailed technical analysis
/home/azureuser/FASTER_TEST_EXAMPLES.md         ← 9 ready-to-use test examples
/home/azureuser/FASTER_QUICK_REFERENCE.md       ← API quick lookup tables
/home/azureuser/README_INVESTIGATION.md         ← This file

/home/azureuser/FASTER/                         ← FASTER repository
  rust/
    crates/faster-core/
      src/                                      ← Source code analyzed above
      tests/                                    ← All test files
    scripts/
      precheckin                                ← Quality gate script
      checkin                                   ← Commit with gates
```

## 🎓 Next Steps

1. **Copy one test example** from `FASTER_TEST_EXAMPLES.md`
2. **Adapt config parameters** using `FASTER_QUICK_REFERENCE.md` tables
3. **Verify struct names/methods** using the mapping table
4. **Run tests** with `cargo test --test <your_test>`
5. **Check quality gates** with `./scripts/precheckin --skip-tests`

All code examples are compilable and follow patterns used in the existing test suite.

---

**Document generated:** March 8, 2025  
**FASTER codebase location:** `/home/azureuser/FASTER`  
**Total analysis scope:** 15 source modules, 16 test files, 1520 lines of documentation
