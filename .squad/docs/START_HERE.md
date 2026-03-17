# 🚀 START HERE — FASTER Memory Pressure Test Documentation

Welcome! I've completed a comprehensive investigation of the FASTER Rust codebase. 

## 📚 Three Documents Ready for You

### 1. **FASTER_QUICK_REFERENCE.md** ← START HERE (5 min read)
**283 lines | Perfect for quick lookup**

Quick tables for:
- Allocator API (8 methods)
- Page Management & Hybrid Log
- Hash Table & Store operations
- Test patterns & examples
- Memory calculations

👉 **Best for:** Finding method signatures, understanding config parameters, checking error modes

---

### 2. **FASTER_TEST_EXAMPLES.md** ← THEN COPY EXAMPLES (10 min)
**493 lines | 9 ready-to-compile tests**

Copy-paste templates for:
- Test 1: Allocator free-list reuse
- Test 2: Page state transitions
- Test 3: Hybrid log under pressure
- **Test 4: Concurrent memory pressure** ← Best starting template
- Test 5-9: Various stress scenarios

👉 **Best for:** Getting working code immediately, adapting for your needs

---

### 3. **FASTER_CODEBASE_ANALYSIS.md** ← DEEP DIVE (20 min)
**744 lines | Complete technical reference**

Detailed sections on:
- Allocator internals & failure modes
- Buffer pool lifecycle & state machine
- Hybrid Log allocator with region boundaries
- Hash Table latch-free operations
- Store architecture & session model
- 15 existing test files & patterns
- Build & test commands

👉 **Best for:** Understanding design decisions, debugging, writing advanced tests

---

## ⚡ Quick Start (5 minutes)

### You need to write a memory pressure test? Do this:

**Step 1:** Copy this code:
```rust
use faster_core::store::{FasterKv, FasterKvConfig, SimpleFunctions};
use faster_core::{InMemoryDevice};
use faster_core::hybrid_log::EvictionPolicy;

#[test]
fn my_memory_pressure_test() {
    let config = FasterKvConfig {
        hash_index_size_log2: 10,           // Small
        buffer_size_pages: 4,               // Tight
        mutable_fraction: 0.5,              // Forces sealing
        sector_size: 512,
        eviction_policy: EvictionPolicy {
            max_in_memory_pages: 3,         // Trigger eviction
            eviction_batch_size: 1,
        },
        grow_config: Default::default(),
        auto_compact: false,
    };
    
    let store = FasterKv::new(config, SimpleFunctions::default(), InMemoryDevice::new());
    let mut session = store.new_session();
    
    // Add your test here...
    
    store.dispose_session(session);
}
```

**Step 2:** Check method signatures in **FASTER_QUICK_REFERENCE.md**, section 5 (Store API)

**Step 3:** Copy a full example from **FASTER_TEST_EXAMPLES.md** and adapt

**Step 4:** Add to `/home/azureuser/FASTER/rust/crates/faster-core/tests/memory_pressure.rs`

**Step 5:** Run:
```bash
cd /home/azureuser/FASTER/rust
cargo test --test memory_pressure
```

---

## 📋 Key Struct Names (Cheat Sheet)

| What | Constructor | File |
|------|-------------|------|
| **Allocator** | `MallocFixedPageSize::new()` | `src/allocator.rs` |
| **Hybrid Log** | `HybridLogAllocator::new(buffer_size, mutable_frac, sector_size)` | `src/hybrid_log/log_allocator.rs` |
| **Hash Table** | `HashTable::new(log2_size: u32)` | `src/hash/table.rs` |
| **Store** | `FasterKv::new(config, functions, device)` | `src/store/kv.rs` |
| **Session** | `store.new_session()` | `src/store/session.rs` |

**→ Full mapping in FASTER_CODEBASE_ANALYSIS.md section 12**

---

## 🎯 Memory Pressure Parameters

For **tight memory** (forces page sealing + eviction):

```rust
FasterKvConfig {
    hash_index_size_log2: 10,           // 1,024 buckets (small)
    buffer_size_pages: 4,               // 128 MB total
    mutable_fraction: 0.5,              // 64 MB mutable
    sector_size: 512,
    eviction_policy: EvictionPolicy {
        max_in_memory_pages: 3,         // Keep only 3 pages
        eviction_batch_size: 1,         // Evict one at a time
    },
    grow_config: GrowConfig::default(),
    auto_compact: false,
}
```

**→ Full details in FASTER_QUICK_REFERENCE.md section 6**

---

## 🔍 What You Get in Each Document

### FASTER_QUICK_REFERENCE.md
```
✓ 8 allocator methods with signatures
✓ Page state machine diagram
✓ HybridLogAllocator API (10 methods)
✓ HashTable operations
✓ CRUD test patterns
✓ Config examples
✓ Error handling
✓ Memory math
✓ Struct → File mapping
```

### FASTER_TEST_EXAMPLES.md
```
✓ Quick setup template
✓ 9 complete, compilable test functions
✓ Allocator stress tests
✓ Hash collision tests
✓ Concurrent pressure tests (8 threads)
✓ RMW under pressure
✓ Page flushing cycles
✓ Pending I/O completion
✓ Helper macros
```

### FASTER_CODEBASE_ANALYSIS.md
```
✓ Allocator design & internals
  └─ Free-list (Treiber stack, 16-bit ABA)
  └─ Bump allocation strategy
  └─ Failure modes (panic vs Option)

✓ Buffer pool & pages
  └─ Page lifecycle (6 states)
  └─ Atomic transitions (CAS)
  └─ Circular buffer mechanics

✓ Hybrid log allocator
  └─ 5 region boundaries
  └─ Allocation strategy
  └─ Page sealing & advancement

✓ Hash table
  └─ Latch-free concurrent design
  └─ Bucket layout (7 entries + overflow)
  └─ Two-phase insert protocol

✓ Store architecture
  └─ FasterKv & FasterKvConfig
  └─ CRUD operations
  └─ Session model
  └─ Devices

✓ 15 existing test files + patterns
✓ Build scripts (precheckin, checkin)
✓ All test commands
```

---

## ✅ Verification Checklist

Before writing your test:

- [ ] Read **FASTER_QUICK_REFERENCE.md** section 5 (Store API)
- [ ] Review **FASTER_TEST_EXAMPLES.md** Test 4 (Concurrent)
- [ ] Know these constants:
  - [ ] `ITEMS_PER_PAGE = 2^20 = 1,048,576`
  - [ ] Page size = `2^25 = 32 MB`
  - [ ] Bucket entries = `7 per 64-byte bucket`
- [ ] Understand error modes:
  - [ ] `try_allocate()` returns `Option` (not `Result`)
  - [ ] `allocate()` panics on extreme exhaustion
  - [ ] `advance_to_next_page()` returns `None` if circular buffer full
- [ ] Know your test device:
  - [ ] `InMemoryDevice::new()` for integration tests
  - [ ] `NullDevice::new()` for memory-only tests

---

## 📞 Document Navigation

**Need method signatures?**
→ FASTER_QUICK_REFERENCE.md section 1-5

**Need working code?**
→ FASTER_TEST_EXAMPLES.md (9 tests ready to copy)

**Need to understand internals?**
→ FASTER_CODEBASE_ANALYSIS.md sections 1-4

**Need test patterns?**
→ FASTER_CODEBASE_ANALYSIS.md section 6 + FASTER_TEST_EXAMPLES.md

**Need command reference?**
→ FASTER_QUICK_REFERENCE.md section 8

---

## 🎓 Next Steps

1. **Open** FASTER_QUICK_REFERENCE.md (5 min read)
2. **Copy** Test 4 from FASTER_TEST_EXAMPLES.md
3. **Adapt** config using QUICK_REFERENCE tables
4. **Add** to `/home/azureuser/FASTER/rust/crates/faster-core/tests/`
5. **Run** with `cargo test --test <your_test>`
6. **Validate** with `./scripts/precheckin --skip-tests`

---

## 📊 Documentation Stats

| Document | Lines | Size | Focus |
|----------|-------|------|-------|
| FASTER_QUICK_REFERENCE.md | 283 | 9.8 KB | Tables & quick lookup |
| FASTER_TEST_EXAMPLES.md | 493 | 14 KB | Copy-paste code examples |
| FASTER_CODEBASE_ANALYSIS.md | 744 | 22 KB | Deep technical details |
| **TOTAL** | **1,520** | **46 KB** | Complete reference |

---

**Ready?** Open **FASTER_QUICK_REFERENCE.md** now! 🚀

Files are in: `/home/azureuser/`
