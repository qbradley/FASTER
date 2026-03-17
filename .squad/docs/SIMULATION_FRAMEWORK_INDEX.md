# FASTER Simulation Framework Documentation Index

## Overview

This directory contains comprehensive documentation for designing a deterministic simulation testing framework for the FASTER key-value store. The analysis covers all critical components: the KV store, hybrid log allocator, flush pipeline, epoch coordination, and session model.

## Documents

### 1. **FASTER_SIMULATION_FRAMEWORK_GUIDE.md** (692 lines, 23KB)
Complete technical reference for all FASTER components. **Start here for deep dives.**

**Covers:**
- **Section 1:** FasterKv store structure, configuration, CRUD methods, maintenance() algorithm
- **Section 2:** HybridLogAllocator - address regions, try_allocate(), shift_read_only_to_tail()
- **Section 3:** Page state machine, PageFrame allocation, PageTable structure
- **Section 4:** Flush pipeline - FlushRequest, FlushBatchResult, async/sync flush, callback safety
- **Section 5:** EpochTable coordination - lifecycle, protect/unprotect, safe-epoch computation
- **Section 6:** Session model - FasterSession, SessionGuard, UnsafeContext for batching
- **Section 7:** Synchronization summary table - atomics, locks, lock-free patterns
- **Section 8:** Constants and defaults
- **Section 9:** Simulation framework requirements and critical invariants

### 2. **SIMULATION_TESTING_QUICK_REFERENCE.md** (326 lines, 9KB)
Visual diagrams and implementation checklist. **Use this for quick lookups and testing scenarios.**

**Covers:**
- Operation lifecycle flowchart
- Address space visualization with invariants
- Page state machine diagram
- Epoch coordination timeline
- Flush pipeline execution flow
- Key structures for instrumentation
- Invariant checks (code snippets)
- 4 detailed testing scenarios with assertions
- Mock points for deterministic control
- Verification checklist

## Key Finding: Critical Architecture Patterns

### 1. The SF-10 Pattern (Buffer Full Check)
**Location:** `log_allocator.rs` lines 132-137, kv.rs lines 241-310

The critical safety invariant is checked in two places:
```rust
// During allocation (allocate_at_tail callback path)
if (next_page.wrapping_sub(head_page) as usize) >= buffer_size {
    return None;  // Trigger maintenance()
}

// During shutdown (FasterKv::drop)
// Drain all pending flushes before deallocating allocator
```

This pattern prevents the tail from wrapping around and overwriting the head.

### 2. The Epoch Coordination Pattern
**Location:** `epoch/table.rs` lines 279-389

Three key synchronization points:
1. **Protection** (Relaxed + Release pair): Thread stores epoch snapshot
2. **Bumping** (SeqCst): Global epoch advances with total ordering
3. **Draining** (Acquire scans): Safe epoch computed conservatively

The SeqCst on bump is critical - it prevents a thread from reading stale epoch and storing it after the bump happens.

### 3. The Flush Callback Pattern
**Location:** `flush.rs` lines 157-192, 263-267

Raw pointer safety guaranteed by:
- Context allocated via `TypedIoContext::new` (heap-allocated)
- Page table pointer `*const PageTable` captured in context
- Callback fires while allocator still alive (SF-10 guarantees)
- Page table outlives all in-flight flushes by construction

### 4. The maintenance() Algorithm
**Location:** `kv.rs` lines 1676-1769

Five-phase coordination:
1. **Shift** read-only boundary (on pressure/mutable threshold)
2. **Flush** sealed pages to device (async write_async)
3. **Poll** completions (callbacks execute, Flushing → Flushed)
4. **Evict** pages from memory (if under pressure)
5. **Yield** on back-pressure to let I/O threads run

This is the main tuning point for buffer management - called on allocation failure.

## Components Requiring Instrumentation

| Component | Location | Key State | Test Focus |
|-----------|----------|-----------|------------|
| Allocator | `log_allocator.rs` | begin, head, ro, sro, tail | SF-10 invariant, page fill behavior |
| Page States | `page.rs` | AtomicPageState enum | State machine transitions |
| Epoch | `epoch/table.rs` | current_epoch, safe_to_reclaim | Coordination, drain timing |
| Flush | `flush.rs` | FlushCallbackContext | Async callback execution |
| Session | `session.rs` | pending_ops, io_contexts | Thread affinity, epoch nesting |

## Testing Strategy

### Unit Level
1. **Allocator Tests**
   - Page boundary crossing behavior
   - SF-10 buffer full condition
   - Address monotonicity on shift

2. **Epoch Tests**
   - Safe epoch computation with multiple threads
   - Drain callback execution timing
   - Reentrant guard nesting

3. **Flush Tests**
   - Page state transitions (Sealed → Flushing → Flushed)
   - Callback context lifecycle
   - Short write detection and retry

### Integration Level
1. **Buffer Pressure Scenario**
   - Fill buffer to trigger maintenance
   - Verify cascade: shift → flush → evict → continue

2. **Concurrent Operations**
   - Multiple sessions with overlapping epochs
   - Verify safe epoch not advanced too early

3. **I/O Completion Ordering**
   - Control callback timing deterministically
   - Test race conditions: flush vs evict, epoch bump vs drain

### Invariant Verification
- **SF-10:** `tail_page - head_page < buffer_size` ✓
- **Address ordering:** `begin ≤ head ≤ ro ≤ sro ≤ tail` ✓
- **Page states:** All pages in valid lifecycle states ✓
- **Epoch monotonicity:** `safe_epoch ≤ current_epoch` ✓
- **Callback safety:** Callbacks fire only when safe ✓

## Design Constraints for Simulation

### Must Support Determinism
1. **Clock Control**
   - Virtual time for I/O completion ordering
   - Epoch bump timing controlled by test

2. **Thread Scheduling**
   - Interleave threads deterministically
   - Control which thread wins CAS races

3. **Device I/O**
   - Mock device with controlled completion timing
   - Inject failures (short writes, errors)

### Must Preserve Invariants
1. **Lock-Free Patterns**
   - CAS loops require proper Acquire/Release semantics
   - Cannot insert arbitrary synchronization points

2. **Callback Safety**
   - Callbacks must execute with allocator alive
   - Cannot delay shutdown indefinitely

3. **Epoch Coordination**
   - Ordering must respect SeqCst semantics
   - Cannot reorder epoch bumps arbitrarily

## Quick Reference: Key Type Signatures

### FasterKv Core
```rust
pub struct FasterKv<F: Functions> { ... }
pub fn maintenance(&self)
pub fn read(&self, session, key, input, output, context) -> OperationOutcome
pub fn upsert(&self, session, key, input, context) -> OperationOutcome
pub fn rmw(&self, session, key, input, output, context) -> OperationOutcome
pub fn delete(&self, session, key, context) -> OperationOutcome
```

### HybridLogAllocator
```rust
pub fn try_allocate(&self, size: u32) -> Option<LogicalAddress>
pub fn advance_to_next_page(&self) -> Option<LogicalAddress>
pub fn shift_read_only_to_tail(&self)
pub fn is_mutable(&self, addr: LogicalAddress) -> bool
pub fn is_in_memory(&self, addr: LogicalAddress) -> bool
```

### PageFlusher
```rust
pub fn flush_page(&self, page, page_table, device, valid_bytes) -> Result<bool, FlushError>
pub fn flush_page_sync(&self, page, page_table, device, valid_bytes) -> Result<bool, FlushError>
pub fn flush_sealed_pages(&self, allocator, device) -> Result<FlushBatchResult, FlushError>
```

### EpochTable
```rust
pub fn register(self: &Arc<Self>) -> Option<EpochThread>
pub fn bump_current_epoch<F: FnOnce() + Send>(&self, callback: F)
pub fn defer<F: FnOnce() + Send>(&self, callback: F)
pub fn current_epoch(&self) -> u64
pub fn safe_epoch(&self) -> u64
```

### Session
```rust
pub fn new_session(&self) -> FasterSession<F>
pub fn session_scope<R>(&self, f: impl FnOnce(&mut FasterSession<F>, &Self) -> R) -> R
pub fn unsafe_context<'a>(&self, session: &'a mut FasterSession<F>) -> UnsafeContext<'a, F>
pub fn complete_pending(&self, session) -> Vec<(Output, Context)>
pub fn try_complete_pending(&self, session, wait: bool) -> CompletePendingResult
```

## Implementation Priorities

**Phase 1: Core Allocator**
- Mock page table and allocator state
- Test try_allocate() and SF-10 checks
- Verify address invariants

**Phase 2: Page States**
- Implement atomic state machine with CAS
- Test transitions: Open → Sealed → Flushing → Flushed → Evicted
- Verify callback context lifecycle

**Phase 3: Epoch Coordination**
- Mock epoch table with deterministic bumping
- Test safe-epoch computation
- Verify drain callback timing

**Phase 4: Flush Pipeline**
- Mock device with controllable I/O completion
- Test flush_page(), flush_sealed_pages()
- Verify callback execution order

**Phase 5: Integration**
- Full maintenance() cycle under buffer pressure
- Concurrent multi-session operations
- Edge cases: shutdown, errors, eviction

## File Locations

- **Main Guide:** `/home/azureuser/FASTER/FASTER_SIMULATION_FRAMEWORK_GUIDE.md`
- **Quick Reference:** `/home/azureuser/FASTER/SIMULATION_TESTING_QUICK_REFERENCE.md`
- **KV Store:** `/home/azureuser/FASTER/rust/crates/faster-core/src/store/kv.rs` (1800+ lines)
- **Allocator:** `/home/azureuser/FASTER/rust/crates/faster-core/src/hybrid_log/log_allocator.rs`
- **Flush:** `/home/azureuser/FASTER/rust/crates/faster-core/src/hybrid_log/flush.rs`
- **Epoch:** `/home/azureuser/FASTER/rust/crates/faster-core/src/epoch/table.rs` + `guard.rs`
- **Session:** `/home/azureuser/FASTER/rust/crates/faster-core/src/store/session.rs`

---

**Generated:** March 16, 2025
**Framework Version:** FASTER Rust Implementation
**Scope:** Complete architecture for deterministic simulation testing
