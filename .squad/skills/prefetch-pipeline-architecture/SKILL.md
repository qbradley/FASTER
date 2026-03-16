# Prefetch Pipeline Architecture Skill — FASTER

## When to Use
Optimizing FASTER batch operations for cache-aware performance. Understanding the two-level prefetch pipeline.

## Pattern

### Two-Level Prefetch Architecture
```
L1 Prefetch: Hash bucket (8 ops ahead)
    ↓
find() / find_or_create()
    ↓
L2 Prefetch: Record data (4 ops ahead)
    ↓
Operation execution (read/upsert/RMW/delete)
```

### L1: Hash Bucket Prefetch
```rust
// In hash/mod.rs
pub fn prefetch(key_hash: u64) {
    let bucket_addr = /* compute bucket address from hash */;
    unsafe { _mm_prefetch(bucket_addr as *const i8, _MM_HINT_T0) };
}
```

**When**: Before calling `find()` or `find_or_create()`
**Target**: Hash table bucket entry (~64 bytes)
**Distance**: 8 operations ahead in batch pipeline

### L2: Record Data Prefetch
```rust
// In store/operations.rs
pub fn prefetch_record(allocator: &LogAllocator, addr: u64, is_write: bool) {
    if is_write {
        prefetch_write(addr);  // _MM_HINT_ET0 (exclusive ownership)
    } else {
        prefetch_read(addr);   // _MM_HINT_T0 (shared)
    }
}
```

**When**: After `find()` returns address, before read/modify operation
**Target**: Full record (key + value, variable size)
**Distance**: 4 operations ahead in batch pipeline

### PrefetchPipeline Implementation
Located in `store/batch.rs`:
```rust
pub struct PrefetchPipeline<F> {
    ops: Vec<BatchOp<F>>,
    l1_distance: usize,  // 8
    l2_distance: usize,  // 4
}

impl<F> PrefetchPipeline<F> {
    pub fn execute(&mut self, context: &mut UnsafeContext<F>) -> Vec<BatchResult> {
        for i in 0..self.ops.len() {
            // L1 prefetch (8 ahead)
            if i + self.l1_distance < self.ops.len() {
                context.hash_index.prefetch(self.ops[i + 8].key_hash);
            }
            
            // Find/create
            let addr = context.find_or_create(self.ops[i].key);
            
            // L2 prefetch (4 ahead)
            if i + self.l2_distance < self.ops.len() {
                prefetch_record(context.allocator, addr, self.ops[i + 4].is_write);
            }
            
            // Execute operation
            results.push(self.ops[i].execute(context, addr));
            
            // Epoch refresh every 256 ops
            if i % 256 == 0 {
                context.refresh_epoch();
            }
        }
    }
}
```

### Batch Methods on UnsafeContext
All in `store/session.rs`:
- `batch_read(keys: &[K]) -> Vec<Option<V>>`
- `batch_upsert(entries: &[(K, V)]) -> Vec<()>`
- `batch_rmw(entries: &[(K, fn(&V) -> V)]) -> Vec<Status>`
- `batch_delete(keys: &[K]) -> Vec<bool>`
- `batch_execute(ops: Vec<BatchOp>) -> Vec<BatchResult>`

### Convenience Methods on FasterKv
In `store/kv.rs`:
- `read_batch(&self, keys: &[K]) -> Vec<Option<V>>`
- `upsert_batch(&self, entries: &[(K, V)])`
- `rmw_batch(&self, entries: &[(K, fn(&V) -> V)])`
- `delete_batch(&self, keys: &[K]) -> Vec<bool>`

### Performance Impact (Measured)
- **Throughput**: +6-7% on YCSB Workload A/C/F
- **P99.9 latency**: +39% improvement (tail latency reduction)
- **Best speedup**: Read-heavy workloads (Workload C)

### Distance Tuning Rationale
- **L1=8**: Modern CPUs have ~10-cycle L1 miss, ~40-cycle L2 miss. 8 ops gives ~320 cycles of prefetch latency hiding.
- **L2=4**: Record data is larger (avg 128 bytes), needs fewer operations ahead. 4 ops = ~160 cycles.

## Anti-Patterns
- **Using `_MM_HINT_T0` for writes**: Wrong! Writes need `_MM_HINT_ET0` (exclusive ownership) to avoid cache coherence traffic.
- **Prefetching too close**: L1/L2 distance <2 doesn't hide latency. Must be far enough ahead.
- **Prefetching too far**: Distance >16 wastes cache capacity, evicts useful data.
- **Forgetting epoch refresh**: Every 256 ops to prevent memory leak in lock-free epoch GC.

## Key Files
- `rust/faster-core/src/hash/prefetch.rs` — L1 hash bucket prefetch intrinsics
- `rust/faster-core/src/store/operations.rs` — L2 record data prefetch
- `rust/faster-core/src/store/batch.rs` — PrefetchPipeline, BatchResult, BatchOp
- `rust/faster-core/src/store/session.rs` — 6 batch methods on UnsafeContext
- `rust/faster-core/src/store/kv.rs` — 4 convenience methods on FasterKv

## Confidence
**High** — Production implementation validated in Wave 3 benchmarks (2026-03-07).

## Learned From
- Two-Level Prefetch Pipeline (Task A4, 2026-03-08): Architecture design and implementation
- Wave 3 Comprehensive Benchmarking (2026-03-07): Performance impact validation (+6-7% throughput, +39% P99.9)
