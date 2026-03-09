# Decision: Memory Pressure Testing Patterns

**Agent:** Sam (Systems Programming Expert)
**Date:** 2026-03-08
**Status:** Implemented
**Impact:** Testing infrastructure — establishes patterns for all future pressure tests

## Decision

Memory pressure tests should use configuration-based pressure (tiny buffer_size_pages, small hash_index_size_log2, aggressive eviction policies) rather than attempting to simulate actual OOM conditions at the OS level.

## Rationale

1. **Deterministic:** Configuration knobs force the same pressure paths without depending on system memory state.
2. **Fast:** Tests complete in <3s total because the data structures are tiny, not because we're waiting for the OS to run out of memory.
3. **Portable:** No `mmap` tricks, no `/proc/meminfo` parsing, no platform-specific OOM killer configuration.
4. **Correct invariant testing:** The goal is "data integrity under pressure," not "graceful OOM handling." FASTER's allocator panics on allocation failure (by design), so we test the pressure paths that precede failure.

## Key Configuration for Pressure Tests

```rust
FasterKvConfig {
    hash_index_size_log2: 1..10,     // tiny hash → overflow chains
    buffer_size_pages: 4,             // minimum viable
    mutable_fraction: 0.5,            // aggressive sealing
    eviction_policy: EvictionPolicy {
        max_in_memory_pages: 3,       // force eviction
        eviction_batch_size: 1,
    },
    grow_config: GrowConfig { enabled: false, .. }, // prevent auto-resize
}
```

## Important API Behaviors Discovered

1. **HashTable::find_entry() skips tentative entries.** Tests must call `update_entry()` with `without_tentative()` after `find_or_create_entry()`.
2. **FasterKv::entry_count() returns 0** (intentionally deferred). Not usable for assertions.
3. **MallocFixedPageSize::count()** only reflects bump allocations, not total live items.

## Implications

- All future pressure tests should follow this configuration-based pattern.
- The `pressure_store()` and `saturated_hash_store()` helpers in `memory_pressure_tests.rs` are reusable templates.
- Property-based tests (proptest) are effective for memory pressure fuzzing — 32 cases per property is sufficient for CI.
