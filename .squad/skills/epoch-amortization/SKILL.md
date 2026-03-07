# Skill: Epoch-Amortized Batch Operations

## Pattern

When a per-operation RAII guard (enter/exit) dominates the operation cost, create a batch context that holds the guard across multiple operations with a periodic `refresh()` to allow progress.

## Structure

```rust
pub struct BatchContext<'a> {
    session: &'a mut Session,
    // guard is held for lifetime of context
}

impl BatchContext<'_> {
    fn new(session: &mut Session) -> Self { /* enter protection */ }
    fn refresh(&self) { /* update epoch without leaving protection, drain pending work */ }
    fn operation(&mut self, ...) { /* skip enter/exit, do work directly */ }
}

impl Drop for BatchContext<'_> {
    fn drop(&mut self) { /* exit protection */ }
}
```

## When to Apply

- Per-operation overhead is >20% of total operation cost
- Operations are naturally batched (loops, bulk loads, YCSB workloads)
- The protection mechanism supports reentrance or prolonged holding with refresh

## Key Considerations

1. **Refresh frequency**: Optimal is 64–128 ops (measured). Too rare → stalls GC/epoch advancement; too frequent → negates the benefit.
2. **`!Send` enforcement**: Batch context must not cross thread boundaries.
3. **Existing API preserved**: Batch API is additive, not a replacement.

## Measured Impact (FASTER Rust, 2026-03-06)

| Operation | Per-Op Epoch | Amortized (UnsafeContext) | Speedup |
|-----------|-------------|--------------------------|---------|
| Upsert insert | 293 ns | 165 ns | 1.8× |
| Upsert update | 277 ns | 151 ns | 1.8× |
| Point read | 276 ns | 79 ns | 3.5× |
| 4-thread upsert | 6.3M total | 10.0M total | 1.6× |

## Root Cause of Epoch Cost

`compute_safe_epoch()` (`epoch/table.rs:320`) scans all 256 `MAX_THREADS` entries with `Acquire` loads on every `unprotect()`. This 16KB cache scan costs ~97–128ns and is the dominant factor.

## Key Files

- Benchmark: `rust/crates/faster-core/benches/perf_analysis.rs`
- UnsafeContext: `rust/crates/faster-core/src/store/session.rs:593`
- Epoch table: `rust/crates/faster-core/src/epoch/table.rs:296` (try_drain)
- Analysis: `.squad/decisions/inbox/legolas-perf-analysis-ai4.md`

## Profiling Command

```bash
cd rust && cargo bench --bench perf_analysis -p faster-core -- --nocapture
```
