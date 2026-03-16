# Circular Buffer Deadlock Pattern — FASTER Core

## When to Use
Diagnosing permanent stalls in multi-writer scenarios with FASTER's hybrid log. Relevant when throughput suddenly drops to near-zero after initial burst.

## Pattern

### Deadlock Symptom
```
First 10-20 seconds: 200K+ ops/s (normal)
After buffer fills: ~14 ops/s (stalled)
Error messages: "SF-10: Cannot allocate page (out of buffer space)"
```

### Root Cause: 3-Point Circular Dependency
**FASTER's circular buffer requires 3 components to cooperate:**

1. **Writer threads** allocate pages until `tail_page >= head_page + buffer_size`
   - Location: `faster-core/src/log/log_allocator.rs:313-320`
   - Check: SF-10 error prevents allocation when buffer full

2. **Head advancement** only moves past **contiguously flushed** pages
   - Location: `faster-core/src/log/eviction.rs:171`
   - Logic: `head = first non-Flushed page from current head`

3. **Flush pipeline** may revert pages to `Sealed` on device back-pressure
   - Location: `faster-core/src/log/flush.rs:379`
   - Behavior: Early-exit loop on I/O queue full, leaves unflushed pages

**Deadlock cycle:**
```
Writers fill buffer → SF-10 blocks new allocations
    ↓
Head can't advance (needs contiguous Flushed pages)
    ↓
Flush pipeline hits back-pressure → reverts pages to Sealed
    ↓
Head still blocked (pages not Flushed) → Writers still blocked
    ↓
PERMANENT STALL
```

### Detecting the Deadlock
```bash
# Run multi-writer benchmark
cargo run --release --bin page-cache -- \
    --num-threads 2 \
    --distribution linear \
    --duration 60

# Symptoms:
# - Throughput graph: sharp drop after ~20s
# - Logs: Repeated "SF-10: Cannot allocate page"
# - Page state: Many pages in Sealed state, few in Flushed
```

### Workaround (temporary)
Use **single writer** with linear distribution, or **multiple writers with Zipfian distribution** (more updates to existing keys).

### Permanent Fix (requires FASTER core changes)
One of:
1. **Flush pipeline robustness**: Retry flushing Sealed pages on next flush cycle (don't give up)
2. **Head advancement relaxation**: Allow head to skip over Sealed pages (non-contiguous advancement)
3. **Back-pressure handling**: Block writers on flush queue full, not buffer full (serialize instead of deadlock)

### Related Configuration
- `buffer_size`: Number of pages in circular buffer (e.g., 64 pages = 2MB with 32KB pages)
- `--device sync|uring`: I/O backend choice (both can deadlock, but uring degrades faster under sustained load)

## Anti-Patterns
- **Assuming multi-writer works like single-writer**: The circular buffer deadlock is multi-writer specific.
- **Blaming I/O device**: The deadlock is in the flush/evict coordination logic, not the device.
- **Ignoring SF-10 errors**: These are not transient — they indicate the circular buffer state machine is stuck.

## Confidence
**High** — Root cause identified via stress testing (2026-03-10).

## Learned From
- Disk Throughput Stress Test — page-cache Sample (2026-03-10): Multi-writer permanent stall discovery
- Analysis of faster-core state machine: log_allocator.rs, eviction.rs, flush.rs coordination
