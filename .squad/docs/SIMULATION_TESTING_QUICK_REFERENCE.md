# FASTER Simulation Testing Quick Reference

## Core Data Flow

### Operation Lifecycle
```
User Thread
    ↓
session.begin_unsafe()         // SessionGuard created
    ↓
store.read/upsert/rmw/delete() // CRUD operation
    ├─ InternalContext passed with on_alloc_failure callback
    ├─ Search hash_index
    ├─ Access allocator (may trigger maintenance() on alloc failure)
    ├─ If on_disk: queue PendingOp
    └─ Return OperationOutcome
    ↓
session.begin_unsafe() dropped // EpochGuard dropped → unprotect() → try_drain()
    ↓
store.dispatch_pending_io()    // Issue async device reads
    ↓
store.try_complete_pending()   // Poll for completed I/O
```

## Address Space Visualization

```
HYBRID LOG REGIONS:
┌────────────────────────────────────────────────────────────────┐
│  On-Disk          Read-Only          Mutable                   │
│  (evicted)        (sealed)           (writable)                 │
├────────────────────────────────────────────────────────────────┤
begin    head       read_only   safe_read_only    tail
 ↓        ↓            ↓             ↓              ↓
[====X====][===========][============][==============]→

INVARIANT: begin ≤ head ≤ read_only ≤ safe_read_only ≤ tail
SF-10: (tail_page - head_page) < buffer_size
```

## Page State Machine

```
      Free
        ↓
      Open ────→ Sealed
        ↓          ↓
        └──→ Flushing
             ↓
          Flushed
             ↓
          Evicted
             ↓
      (recycle to Free)

SYNCHRONIZATION:
- Free ↔ Open: On allocation/eviction
- Open → Sealed: When page fills OR shift_read_only_to_tail()
- Sealed → Flushing: CAS in flush_page()
- Flushing → Flushed: I/O completion callback
- Flushed → Evicted: PageEvictor marks for eviction
```

## Epoch Coordination

```
THREAD REGISTRATION:
table.register() → EpochThread
    ↓ claims slot [0..256)
    ↓ increments scan_limit

PROTECTED REGION:
thread.protect() → EpochGuard
    ├─ Increment reentrant counter
    ├─ Store current_epoch to thread's local slot (Release)
    └─ Return RAII guard

UNPROTECTED (on guard drop):
    ├─ Decrement reentrant counter
    ├─ If reentrant = 0:
    │   ├─ Clear local_current_epoch (Release)
    │   └─ Call try_drain()
    └─ ...

EPOCH BUMP (in maintenance):
table.bump_current_epoch(callback) → 
    ├─ Fetch-add current_epoch (SeqCst)
    ├─ Queue callback for prior epoch
    └─ Call try_drain()

SAFE EPOCH COMPUTATION:
compute_safe_epoch() →
    ├─ Scan entries [0..scan_limit] for active epochs
    ├─ Return min(active_epochs) - 1
    └─ Fast-path: if no pending drains, skip scan (~5ns vs ~100ns)
```

## Flush Pipeline

```
MAINTENANCE CYCLE:
    ↓
1. Shift read-only boundary (if conditions met):
   read_only_address = tail_address
   
2. Flush sealed pages:
   For page in [head_page..read_only_page):
       If state == Sealed:
           CAS: Sealed → Flushing
           write_async(page, offset, callback)
   
3. Poll device completions:
   device.poll_completions()  // Callbacks execute here
   
4. Evict pages (if under pressure):
   For page in evictable range:
       Transition Flushed → Evicted
       Reclaim memory
   
5. Yield on back-pressure + re-poll

CALLBACK EXECUTION:
    ↓
I/O Complete (device thread)
    ├─ flush_completion_callback()
    ├─ Validate bytes_transferred >= expected
    ├─ Update page.flushed_until (Release)
    ├─ CAS: Flushing → Flushed
    └─ Deallocate callback context
```

## Key Structures for Instrumentation

### State Snapshot (Capture per Operation)
```rust
pub struct LogSnapshot {
    // Addresses
    begin: LogicalAddress,
    head: LogicalAddress,
    read_only: LogicalAddress,
    safe_read_only: LogicalAddress,
    tail: LogicalAddress,
    
    // Epoch
    current_epoch: u64,
    safe_to_reclaim_epoch: u64,
    
    // Page states
    page_states: Vec<PageState>,  // [head..tail]
    
    // Pending
    pending_io_count: usize,
    pending_ops_count: usize,
}
```

### Critical Invariant Checks
```rust
// 1. Buffer overflow
assert!(tail_page - head_page < buffer_size, "SF-10 violation");

// 2. Address ordering
assert!(begin <= head && head <= read_only && 
        read_only <= safe_read_only && safe_read_only <= tail,
        "Address invariant violation");

// 3. Epoch monotonicity
assert!(safe_to_reclaim_epoch <= current_epoch,
        "Epoch ordering violation");

// 4. Page state validity
for page in head_page..tail_page {
    let state = page_table.get_frame(page).state();
    // Verify state is in {Free, Open, Sealed, Flushing, Flushed, Evicted}
}

// 5. Drain callback safety
// All callbacks for epoch E must fire before bumping to E+2
```

## Testing Scenarios

### Scenario 1: Buffer Pressure
```
Setup:
  - buffer_size = 8 pages
  - mutable_fraction = 0.9 → ~7 pages mutable
  
Steps:
  1. Fill log with 6 pages of data
  2. Write 8th page (trigger read-only shift)
  3. Verify: read_only_address advanced to tail
  4. Verify: page states changed to Sealed
  
Assert:
  - No allocation failures
  - Maintenance cycle ran
  - Pages queued for flush
```

### Scenario 2: Flush Completion
```
Setup:
  - Mock device with controlled I/O completion
  
Steps:
  1. Write page to reach Sealed state
  2. Call maintenance() → flush_sealed_pages()
  3. Verify CAS: Sealed → Flushing
  4. Simulate device.poll_completions()
  5. Verify callback fires → Flushing → Flushed
  
Assert:
  - Page transitions correctly
  - flushed_until updated correctly
  - No memory leaks (callback context freed)
```

### Scenario 3: Epoch Coordination
```
Setup:
  - Thread 1: holds EpochGuard at epoch 5
  - Thread 2: attempts bump_current_epoch()
  
Steps:
  1. Thread 2 calls bump_current_epoch()
  2. Verify: current_epoch becomes 6
  3. Thread 2 tries to drain (should not fire, T1 holding epoch 5)
  4. Thread 1 releases guard
  5. Thread 1 epoch unprotect → try_drain()
  6. Verify: callback for epoch 5 fires
  
Assert:
  - Safe epoch not advanced too early
  - Callback fires only when safe
```

### Scenario 4: Lossy Mode
```
Setup:
  - config.lossy = true
  - Small buffer (4 pages)
  
Steps:
  1. Write pages until eviction triggered
  2. Call maintenance()
  3. Verify: evict_pages() marks Flushed → Evicted
  4. Verify: begin_address advanced to head_address
  5. Verify: hash_index.invalidate_entries_in_range() called
  
Assert:
  - Old data silently discarded
  - begin advances monotonically
  - Hash entries for truncated range invalidated
```

## Mock Points for Deterministic Testing

### Device Write
```rust
// Mock to control when I/O completes
pub trait Device {
    fn write_async(
        &self,
        src: *const u8,
        offset: u64,
        size: u32,
        callback: IoCompletionCallback,
        context: *mut u8,
    ) -> IoRequestResult {
        // In deterministic test:
        // 1. Record request: (offset, size, callback, context)
        // 2. Return IoRequestResult::Submitted
        // 3. Control when callback fires via deterministic scheduler
    }
    
    fn poll_completions(&self) {
        // In deterministic test:
        // Dequeue requested completions from queue
        // Execute callbacks in order
    }
}
```

### Epoch Bump
```rust
// Mock to control when epoch bumps
pub trait EpochTable {
    fn bump_current_epoch(&self, callback: Box<dyn Fn()>) {
        // In deterministic test:
        // 1. Increment current_epoch
        // 2. Queue callback with prior epoch
        // 3. Control drain via explicit try_drain() call
    }
}
```

### Session Creation
```rust
// Mock to control thread assignment
pub trait SessionPool {
    fn create_session() -> FasterSession {
        // In deterministic test:
        // 1. Register thread with specific ID
        // 2. Return session tied to that ID
        // 3. Track all sessions for shutdown verification
    }
}
```

## Verification Checklist

- [ ] **Buffer Overflow**: Verify SF-10 check prevents tail lapping head
- [ ] **Address Ordering**: Check monotonicity of begin ≤ head ≤ ro ≤ sro ≤ tail
- [ ] **Page States**: Verify all pages in valid states, no orphaned pages
- [ ] **Epoch Progress**: Ensure safe epoch advances monotonically
- [ ] **Drain Callbacks**: Verify callbacks fire only after safe epoch reached
- [ ] **Flush Ordering**: Only Sealed pages transition to Flushing
- [ ] **I/O Safety**: All pending flushes complete before shutdown (SF-10)
- [ ] **Thread Affinity**: Sessions/guards cannot cross thread boundaries
- [ ] **Reentrant Epochs**: Nested guards preserve outermost epoch snapshot
- [ ] **Hash Index**: Invalidated entries don't return stale data

---

**File Location:** `/home/azureuser/FASTER/FASTER_SIMULATION_FRAMEWORK_GUIDE.md`
