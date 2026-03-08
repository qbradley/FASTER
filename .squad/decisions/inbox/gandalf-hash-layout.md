# Decision: Hash Table Layout — Keep Multi-Slot Buckets

**Agent:** Gandalf (Lead Architect)
**Date:** 2026-03-06
**Status:** DECIDED — Research task A8 complete
**Impact:** Hash index architecture — confirmed current approach is correct

## Decision

Keep the current multi-slot bucket design. Do NOT pursue open addressing with inline data.

## Key Finding

The hypothesis that C# uses "open addressing with inline keys" was **incorrect**. Both C# and Rust FASTER use identical hash table architectures: 64-byte cache-line-aligned buckets with 7 data entries, each storing an 8-byte packed word (14-bit tag + 48-bit logical address). Neither stores keys or values inline.

## Rationale

1. True open addressing (inline keys/values) is fundamentally incompatible with FASTER's hybrid log architecture, which requires records to flow through mutable → read-only → disk tiers via address indirection.
2. The ~14% Workload C read gap lives in the record access path (page table translation, physical address resolution, key comparison), NOT in the hash index.
3. Changing the hash index would be high risk for zero gain on the actual bottleneck.

## Next Steps

1. Profile full read path with `perf record` to identify actual cycle distribution
2. Benchmark record access isolation (logical address → value bytes)
3. Evaluate `Ordering::Relaxed` for read-path loads behind `#[cfg(target_arch = "x86_64")]`

## Artifacts

- `.squad/agents/gandalf/hash-table-layout-analysis.md`
- `rust/crates/faster-core/benches/hash_layout_bench.rs`
