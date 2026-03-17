# FASTER Deterministic Simulation Testing Framework
## Complete Documentation Package

This directory contains comprehensive analysis and documentation for designing a deterministic simulation testing framework for the FASTER key-value store.

---

## 📚 Documentation Files

### 1. **FASTER_SIMULATION_FRAMEWORK_GUIDE.md** (692 lines)
The definitive technical reference for all FASTER components.

**Sections:**
- FasterKv store architecture (field-by-field breakdown)
- HybridLogAllocator (address regions, try_allocate algorithm, SF-10 check)
- Page state machine (lifecycle, transitions, PageFrame allocation)
- Flush pipeline (async/sync flush, callback safety model, CRC trailer)
- Epoch table coordination (lifecycle, protect/unprotect, safe-epoch computation)
- Session model (FasterSession, SessionGuard, UnsafeContext batching)
- Complete synchronization table (atomics, locks, lock-free patterns)
- Key constants and defaults
- Simulation framework requirements and invariants

**Best for:** Understanding the complete architecture, deep technical dives, implementation details

### 2. **SIMULATION_TESTING_QUICK_REFERENCE.md** (326 lines)
Visual diagrams, flows, and practical testing guidance.

**Contents:**
- Operation lifecycle flowchart
- Address space visualization with invariants
- Page state machine diagram
- Epoch coordination timeline
- Flush pipeline execution flow
- State snapshot structure (for instrumentation)
- Invariant checks (code snippets ready to use)
- 4 detailed testing scenarios (setup, steps, assertions)
- Mock points for deterministic testing
- Verification checklist (10 items)

**Best for:** Quick lookups, understanding data flow, creating test scenarios

### 3. **SIMULATION_FRAMEWORK_INDEX.md** (250 lines)
Navigation guide and critical architecture insights.

**Covers:**
- Overview of all three documents
- 4 critical architecture patterns with exact line numbers
- Components requiring instrumentation (table format)
- Complete testing strategy (unit, integration, invariant verification)
- Design constraints for simulation
- Quick reference of key type signatures
- Implementation priorities (5 phases)
- File locations for all source code

**Best for:** Planning implementation, understanding what needs to be built, quick answers

---

## 🎯 Quick Navigation

### I want to understand...

**The overall architecture**
→ Start with SIMULATION_FRAMEWORK_INDEX.md "Critical Architecture Patterns"

**How the buffer allocator works**
→ FASTER_SIMULATION_FRAMEWORK_GUIDE.md Section 2 (HybridLogAllocator)

**When the system triggers maintenance**
→ FASTER_SIMULATION_FRAMEWORK_GUIDE.md Section 1 (maintenance() algorithm, lines 1676-1769)

**How epoch coordination works**
→ FASTER_SIMULATION_FRAMEWORK_GUIDE.md Section 5 (EpochTable)

**How pages transition states**
→ SIMULATION_TESTING_QUICK_REFERENCE.md "Page State Machine" diagram

**What to test first**
→ SIMULATION_FRAMEWORK_INDEX.md "Implementation Priorities"

**Specific testing scenarios**
→ SIMULATION_TESTING_QUICK_REFERENCE.md "Testing Scenarios" (4 complete examples)

**All the critical invariants**
→ SIMULATION_TESTING_QUICK_REFERENCE.md "Critical Invariant Checks"

---

## 🔑 Key Findings

### Critical Patterns to Simulate

1. **SF-10 Buffer Full Check**
   - Prevents tail from wrapping and overwriting head
   - Checked via CAS: `(next_page - head_page) < buffer_size`
   - Also enforced at shutdown: drains all pending flushes before deallocating

2. **Epoch SeqCst Ordering**
   - Global epoch advance uses SeqCst to prevent stale reads
   - Thread stores epoch with Release, bump uses SeqCst, drain scans with Acquire
   - Critical for safe-epoch computation

3. **Flush Callback Lifetime Safety**
   - Raw pointer to PageTable valid because allocator outlives all in-flight flushes
   - Guaranteed by SF-10 shutdown pattern
   - Callbacks allocated via TypedIoContext and deallocated in callback

4. **maintenance() Five-Phase Algorithm**
   - Shift: Advance read-only boundary on pressure
   - Flush: Issue async writes to device
   - Poll: Execute callbacks (Flushing → Flushed)
   - Evict: Reclaim memory if needed
   - Yield: Let I/O threads run on back-pressure

### Data Structures

| Component | Key Fields | Location |
|-----------|-----------|----------|
| FasterKv | hash_index, allocator, epoch_table, flusher, evictor, device | store/kv.rs |
| HybridLogAllocator | begin, head, read_only, safe_read_only, tail | hybrid_log/log_allocator.rs |
| PageState | Free, Open, Sealed, Flushing, Flushed, Evicted | hybrid_log/page.rs |
| EpochTable | current_epoch, safe_to_reclaim_epoch, table, drain_list | epoch/table.rs |
| FasterSession | epoch_thread, pending_ops, io_contexts | store/session.rs |

### Synchronization

- **Lock-free:** Allocator CAS loop, page state CAS, epoch atomic ops
- **Acquire/Release pairs:** protect() Release ↔ compute_safe_epoch() Acquire
- **SeqCst:** Epoch bump (total ordering requirement)
- **Mutexes:** Only for rare operations (registration, compaction)

### Critical Invariants

```rust
// 1. SF-10: Buffer overflow check
(tail_page - head_page) < buffer_size

// 2. Address ordering
begin ≤ head ≤ read_only ≤ safe_read_only ≤ tail

// 3. Epoch monotonicity
safe_to_reclaim_epoch ≤ current_epoch

// 4. Page states valid
PageState in {Free, Open, Sealed, Flushing, Flushed, Evicted}

// 5. Callback safety
Callbacks for epoch E fire only after safe_epoch ≥ E
```

---

## 🛠️ Implementation Roadmap

### Phase 1: Core Allocator
- Mock HybridLogAllocator state
- Test try_allocate() with SF-10 checks
- Verify address invariants

### Phase 2: Page States
- Atomic state machine with CAS
- Test: Open → Sealed → Flushing → Flushed → Evicted
- Verify callback context lifecycle

### Phase 3: Epoch Coordination
- Mock EpochTable with deterministic bumping
- Test safe-epoch computation
- Verify drain callback timing

### Phase 4: Flush Pipeline
- Mock Device with controllable I/O completion
- Test flush_page(), flush_sealed_pages()
- Verify callback execution order

### Phase 5: Integration
- Full maintenance() cycle under buffer pressure
- Concurrent multi-session operations
- Edge cases: shutdown, errors, eviction

---

## 📋 Testing Scenarios Included

1. **Buffer Pressure** - Fill log, trigger read-only shift, verify maintenance
2. **Flush Completion** - Track page transitions, verify callback execution
3. **Epoch Coordination** - Multi-thread protected regions, verify safe-epoch
4. **Lossy Mode** - Eviction with begin advancement and hash invalidation

Each scenario includes:
- Setup requirements
- Step-by-step execution
- Detailed assertions
- Expected invariant verification

---

## 📊 Documentation Statistics

- **Total Lines:** 1,268 across 3 files
- **Total Size:** 45 KB
- **Type Signatures:** 20+ detailed interfaces
- **Diagrams:** 6 flow diagrams
- **Code Snippets:** 30+ examples
- **Test Scenarios:** 4 complete examples
- **Invariant Checks:** 10 critical conditions

---

## 🔗 Source Code References

All documentation references actual source files with exact line numbers:

- **Store Core:** `rust/crates/faster-core/src/store/kv.rs` (1800+ lines)
  - FasterKv struct (lines 195-226)
  - CRUD methods (lines 866-1073)
  - maintenance() (lines 1676-1769)
  
- **Allocator:** `rust/crates/faster-core/src/hybrid_log/log_allocator.rs`
  - try_allocate() (lines 105-174)
  - shift_read_only_to_tail() (lines 200-235)
  
- **Flush Pipeline:** `rust/crates/faster-core/src/hybrid_log/flush.rs` (500+ lines)
  - flush_page() (lines 225-309)
  - flush_completion_callback() (lines 157-192)
  - flush_sealed_pages() (lines 381-419)
  
- **Epoch:** `rust/crates/faster-core/src/epoch/table.rs` (425 lines)
  - protect() (lines 201-214)
  - compute_safe_epoch() (lines 367-389)
  - bump_current_epoch() (lines 279-286)
  
- **Session:** `rust/crates/faster-core/src/store/session.rs`
  - FasterSession structure
  - Session lifecycle and epoch protection

---

## ✅ Verification Checklist

- [ ] Read SIMULATION_FRAMEWORK_INDEX.md (5 min)
- [ ] Review SIMULATION_TESTING_QUICK_REFERENCE.md diagrams (10 min)
- [ ] Study FASTER_SIMULATION_FRAMEWORK_GUIDE.md Section 1 (FasterKv) (15 min)
- [ ] Understand Section 2 (Allocator) and SF-10 check (15 min)
- [ ] Study Section 5 (Epoch) and safe-epoch computation (15 min)
- [ ] Review Section 4 (Flush) and callback safety (15 min)
- [ ] Study one testing scenario in detail (10 min)
- [ ] Map components to source files (10 min)
- [ ] Plan implementation phases (15 min)
- [ ] Ready to implement (ready!)

---

## 📞 Document Organization

```
/home/azureuser/FASTER/
├── README_SIMULATION.md (this file)
├── SIMULATION_FRAMEWORK_INDEX.md (start here for overview)
├── SIMULATION_TESTING_QUICK_REFERENCE.md (use for quick lookups)
├── FASTER_SIMULATION_FRAMEWORK_GUIDE.md (comprehensive reference)
└── rust/crates/faster-core/src/
    ├── store/kv.rs
    ├── store/session.rs
    ├── hybrid_log/log_allocator.rs
    ├── hybrid_log/flush.rs
    └── epoch/table.rs
```

---

## 🎓 How to Use These Documents

### First Time Reading
1. Start with SIMULATION_FRAMEWORK_INDEX.md (10 min read)
2. View the diagrams in SIMULATION_TESTING_QUICK_REFERENCE.md (5 min)
3. Pick a component of interest and deep-dive into FASTER_SIMULATION_FRAMEWORK_GUIDE.md

### Implementation
1. Read relevant section in FRAMEWORK_GUIDE.md
2. Check testing scenarios in QUICK_REFERENCE.md
3. Reference source code line numbers for exact implementation
4. Use verification checklist for test coverage

### Debugging
1. Check invariant checks in QUICK_REFERENCE.md
2. Review synchronization summary in FRAMEWORK_GUIDE.md
3. Trace operation flow in the diagrams
4. Verify against actual source code

---

**Generated:** March 16, 2025  
**Framework:** FASTER Rust Implementation  
**Scope:** Complete architecture for deterministic simulation testing  
**Status:** ✅ Ready for implementation
