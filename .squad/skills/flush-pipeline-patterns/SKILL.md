# Skill: Flush Pipeline Deadlock Prevention

## When to Use

When implementing or debugging async I/O pipelines where:
- Memory allocator depends on eviction making progress
- Eviction depends on pages being flushed
- Flushing depends on I/O workers completing callbacks
- Writers can overwhelm the pipeline faster than I/O can complete

This is the **three-point structural deadlock** pattern found in log-structured storage systems.

## Pattern

### The Deadlock Triangle

```
Writers → Allocator (needs space)
            ↓
        Eviction (needs flushed pages)
            ↓
        Flusher (needs I/O callbacks to fire)
            ↓
        I/O Workers (need CPU time)
            ⤴ (unblocks allocator)
```

**Deadlock occurs when**: All pages are Flushing, eviction can't advance (needs Flushed), allocator retries endlessly waiting for space, and I/O workers never get CPU time to fire callbacks.

### The Three-Fix Protocol

Based on C++ FASTER pattern: `RETURN_NOT_OK` + `TryComplete` + `DoThrottling`

#### Fix A: Break on QueueFull

```rust
// ❌ WRONG: Continue scanning, making zero progress
for page in sealed_pages {
    match device.flush(page) {
        QueueFull => continue,  // scans remaining pages, all fail
        Ok(_) => flushed += 1,
    }
}

// ✅ CORRECT: Break immediately and signal caller
for page in sealed_pages {
    match device.flush(page) {
        QueueFull => {
            queue_full = true;
            break;  // stop scanning, let caller handle
        }
        Ok(_) => flushed += 1,
    }
}
return FlushBatchResult { flushed, queue_full };
```

#### Fix B: Poll Completions + Bounded Retry

```rust
// Add to Device trait
fn poll_completions(&self) -> usize {
    // Give I/O callback threads CPU time
    std::thread::yield_now();
    0  // or actual completion count
}

// In allocator retry loop
const MAX_ALLOC_RETRIES: usize = 32;
for attempt in 0..MAX_ALLOC_RETRIES {
    match self.try_allocate() {
        Ok(page) => return Ok(page),
        Err(_) => {
            self.maintenance();
            self.device.poll_completions();
            std::thread::yield_now();
        }
    }
}
return Err(BufferFull);
```

#### Fix C: Maintenance Integration

```rust
pub fn maintenance(&self) {
    // Flush sealed pages
    let result = self.flush_sealed_pages();
    
    // ✅ NEW: Always poll after flush batch
    self.device.poll_completions();
    
    // ✅ NEW: Extra poll+yield on pressure
    if result.queue_full || self.buffer_full() {
        std::thread::yield_now();
        self.device.poll_completions();
    }
    
    // Try eviction
    self.evict_pages();
}
```

### Testing the Fix

```rust
#[test]
fn multi_writer_forward_progress() {
    let device = SlowDevice::new(Duration::from_millis(50));
    let store = FasterKv::new(small_config(), device);
    
    // 3 writer threads, tiny buffer
    // Should complete without deadlock
    scope(|s| {
        for _ in 0..3 {
            s.spawn(|| {
                for i in 0..10000 {
                    store.upsert(i, i);  // triggers eviction
                }
            });
        }
    });
}
```

Use `FaultInjectingDevice` to inject QueueFull errors deterministically.

## Confidence: High

## Learned From

- **Multi-Writer Deadlock Analysis (2026-03-10)**: Full code inventory identifying the three-point deadlock structure
- **Multi-Writer Deadlock Fix (2026-03-11)**: Implementation of all three fixes, 6 previously-ignored tests now passing
- **C++ FASTER reference**: `RETURN_NOT_OK` + `TryComplete` + `DoThrottling` pattern

## Key Insights

1. **QueueFull is a signal, not an error** — it means "I/O workers need CPU time"
2. **Single retry is insufficient** — pipeline stalls need bounded retry with yield
3. **Discarded results hide problems** — `let _ = flush(...)` can't detect zero-progress
4. **Callbacks need CPU time** — `yield_now()` is essential in retry loops
5. **Buffer needs 4× headroom** — async flush means tail can't lap unflushed pages

## Anti-Patterns

❌ **Continue scanning on QueueFull** → zero progress, wastes CPU
❌ **Unbounded retry without yield** → starves I/O threads, infinite loop
❌ **Ignoring flush results** → can't detect pipeline stalls
❌ **Buffer = target size** → no headroom for async I/O, immediate deadlock

## Key Files

- `rust/crates/faster-core/src/hybrid_log/flush.rs` (Fix A)
- `rust/crates/faster-core/src/hybrid_log/operations.rs` (Fix B: allocator retry)
- `rust/crates/faster-core/src/kv.rs` (Fix C: maintenance integration)
- `rust/crates/faster-core/src/device/device.rs` (poll_completions trait)
- `tests/multi_writer_pressure_tests.rs` (deadlock reproduction tests)
