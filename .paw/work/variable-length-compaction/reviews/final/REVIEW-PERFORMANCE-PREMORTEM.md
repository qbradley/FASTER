# Performance Review — Premortem Perspective

**Perspective**: premortem
**Reviewer**: Performance Specialist
**Date**: 2025-07-15
**Diff scope**: Variable-length record compaction (~3,400 lines, 21 files)

---

## Scenario

Six months after deploying variable-length compaction, performance has degraded significantly. What failure modes would have been obvious in retrospect?

---

### Finding: Scanner heap-allocates on every live record via `K::deserialize`, creating O(N) allocation pressure on the hot scan path

**Severity**: should-fix
**Confidence**: HIGH
**Category**: performance

#### Grounds (Evidence)

In `scanner.rs` (diff line 2924), the scanner's main loop deserializes every non-tombstone, non-invalid record:

```rust
let key: K = K::deserialize(&bytes[KEY_OFFSET..]);
if self.is_current_version(&key, current) {
```

For `Vec<u8>` keys, `K::deserialize()` calls `buf[4..4+len].to_vec()` (traits.rs:316), which heap-allocates. This runs once per record in the scan region.

**Cardinality estimate**: A compaction cycle over a 32 MB page with 256-byte average variable-length records processes ~130,000 records. That's ~130,000 heap allocations just for key deserialization in a single scan pass. Each allocation is `len` bytes (e.g., 32–256 bytes for typical string/blob keys). At 130K records × ~128 bytes average = ~16 MB of transient allocations per compaction cycle, all immediately discarded after the hash lookup.

Additionally, the `is_current_version` chain walk calls `read_header_and_match_key_varlen` (record_ops.rs:550-556), which uses `key.eq_from_bytes()` — this is zero-copy for the chain walk itself. But the initial deserialization at the scan site still allocates, even for records classified as dead (the deserialization happens before the hash lookup result is known).

#### Warrant (Rule)

The scanner is the hottest path in compaction — it touches every record in the compaction region linearly. At production scale with variable-length keys, the allocator pressure from 130K+ transient allocations per cycle (which must be freed shortly after) creates fragmentation and CPU overhead in the global allocator. With `jemalloc` this might be ~20ns per alloc+free = ~2.6ms overhead per cycle. With the system allocator under contention from concurrent operations, this can spike to 50-100ns per alloc+free = ~13ms per cycle.

The implementation already has `eq_from_bytes` for zero-copy comparison. The missing piece is a zero-copy *hash* of the serialized key (a hypothetical `hash_from_bytes`) that would allow the scanner to avoid deserialization entirely for the classify step. The key is deserialized only to compute its hash (line 2925: `is_current_version` calls `key.hash()`), then compared via `eq_from_bytes` during chain walks.

#### Rebuttal Conditions

This is NOT a concern if:
1. Key types are all fixed-size (u64, u32) — deserialization is a stack copy with zero heap allocation.
2. Compaction is an infrequent background operation and ~13ms of allocation overhead is negligible compared to the I/O and epoch coordination costs.
3. An arena allocator is used for the scan phase (not currently implemented).

For fixed-size types, LLVM will inline `deserialize` to a stack copy, so this finding applies **only to variable-length types** (`Vec<u8>`, `String`).

#### Suggested Verification

Add a benchmark that measures scanner throughput for `Vec<u8>` keys at various sizes (32B, 128B, 512B, 2KB) on a 32 MB region. Compare against `u64` baseline. If the gap exceeds 2×, add a `Key::hash_from_bytes(&[u8]) -> KeyHash` trait method to avoid deserialization in the scan loop.

---

### Finding: `record_size_from_bytes` passes `remaining` (up to 32 MB) as the RecordAccessor size, potentially defeating L1 cache locality

**Severity**: should-fix
**Confidence**: MEDIUM
**Category**: performance

#### Grounds (Evidence)

In `scanner.rs` (diff lines 2876-2883):

```rust
let remaining = page_size - offset;
// ...
let accessor = match RecordAccessor::from_log(self.allocator, current, remaining) {
    Some(a) => a,
    None => { ... }
};
let bytes = accessor.as_slice();
```

The accessor is constructed with `remaining` bytes (up to 32 MB for the first record on a page, given `OFFSET_BITS = 25` → `page_size = 33,554,432`). `RecordAccessor::as_slice()` creates a slice of `record_size` bytes via `from_raw_parts(self.ptr, self.record_size as usize)` (record_ops.rs:146). This means `bytes` is a 32 MB slice for the first record on a page.

The `record_size_from_bytes` function then indexes into this slice at small offsets (bytes 8-12, etc.) — the actual memory touched is <64 bytes. However, depending on LLVM's bounds-check elimination, the slice length may cause unnecessary range-check branches that pollute the branch predictor in the tight loop.

More critically, the previously fixed-size code requested exactly `record_size` bytes from `from_log`, which meant the allocator's page bounds check was tight. Now the accessor always covers the entire remaining page. If `from_log` performs any validation proportional to `remaining`, this regresses.

#### Warrant (Rule)

In the old code, `RecordAccessor::from_log(allocator, current, record_size as u32)` requested exactly the known record size. The new code must request `remaining` because the record size isn't known yet — a necessary trade-off. However, this changes the RecordAccessor's `record_size` field from ~24 bytes (for u64/u64) to potentially 33 MB, which may affect any downstream code that uses `record_size()` for iteration or validation.

The performance impact depends on how `from_log` validates the size parameter. If it simply checks `offset + size <= page_size` and returns a pointer, the overhead is negligible. But the changed semantics could cause subtle issues if any code path iterates over `as_slice()`.

#### Rebuttal Conditions

This is NOT a concern if:
1. `RecordAccessor::from_log` does O(1) work regardless of the `size` parameter (likely — it just computes a pointer).
2. No downstream code iterates over the full `as_slice()` — only indexed access at known offsets is used (confirmed by the diff).
3. LLVM eliminates bounds checks on the known-small index accesses.

#### Suggested Verification

Check the generated assembly for `record_size_from_bytes::<Vec<u8>, Vec<u8>>` to confirm bounds checks are elided. The existing `bench_compaction_record_size_variable` benchmark should show this is a non-issue if the variable-length benchmark is within 2× of the fixed-size benchmark.

---

### Finding: Version chain walks perform one `RecordAccessor::from_log` call per hop with page-remaining size, creating O(chain_depth) page lookups

**Severity**: consider
**Confidence**: MEDIUM
**Category**: performance

#### Grounds (Evidence)

In `scanner.rs` (diff line 2973), the `is_current_version` chain walk calls:

```rust
match reader.read_header_and_match_key_varlen::<K>(current, key) {
```

Which in `record_ops.rs:550` does:

```rust
let record_size = safe_record_size(addr, RECORD_HEADER_SIZE as u32);
let accessor = RecordAccessor::from_log(self.allocator, addr, record_size)?;
```

`safe_record_size` returns `max(remaining, RECORD_HEADER_SIZE)` — which for most records is `remaining` (page-size minus offset). Each hop in the chain creates a new `RecordAccessor` and performs `key.eq_from_bytes()`.

**Cardinality**: With MAX_CHAIN_DEPTH=4096 and a hash table of 1024 buckets (as in the test stores), 130K records → ~127 records per bucket average. Version chain walks in the scan's classify step traverse an average of ~1-3 hops for the common case (chain head matches), but worst-case up to 127 hops per record in a highly loaded bucket.

Per record: 1-3 hops × (from_log + eq_from_bytes). For 130K records: 130K-390K accessor constructions for the scan phase. This is the same cost as the old fixed-size code per-hop, but the old code used the fixed `RecordLayout` to request a tight size from `from_log`.

#### Warrant (Rule)

The `eq_from_bytes` optimization is excellent — it avoids heap allocation during chain walks. The zero-copy comparison (`buf[4..4+len] == self[..]`) is the right approach. However, each hop still constructs a RecordAccessor with `safe_record_size(addr, 8)` which returns the page-remaining bytes. This is functionally correct but semantically wasteful — we only need ~8 + key_size bytes for the comparison.

At production scale with hot hash buckets (e.g., Zipfian distribution), some keys may have chain depths of 10-50 during compaction. The per-hop cost is dominated by the memory access to read the record header + key bytes, not the accessor construction, so this is likely fine.

#### Rebuttal Conditions

This is fine because:
1. The dominant cost per hop is the memory access (L1/L2 cache miss at ~4-10ns), not the accessor construction (~1ns).
2. `eq_from_bytes` avoids the heap allocation that would have occurred with `read_header_and_match_key` (the old method that called `K::deserialize`).
3. Average chain depth is 1-3 hops for uniform distributions.

**Verdict: This is fine.** The chain walk is optimized correctly with `eq_from_bytes`. The accessor construction overhead is negligible compared to memory access latency.

#### Suggested Verification

The existing chain depth limit (MAX_CHAIN_DEPTH=4096) prevents runaway traversals. No additional action needed unless Zipfian benchmarks show unexpected regression.

---

### Finding: `tombstone_records` Vec grows unbounded during scan, proportional to tombstone count in the region

**Severity**: consider
**Confidence**: MEDIUM
**Category**: performance

#### Grounds (Evidence)

In `scanner.rs` (diff line 2913-2916) and `mod.rs` (diff line 2635):

```rust
plan.tombstone_records.push(LiveRecord {
    address: current,
    record_size,
});
```

`CompactionPlan` now has `tombstone_records: Vec<LiveRecord>` in addition to `live_records: Vec<LiveRecord>`. Each `LiveRecord` is 16 bytes (LogicalAddress: 8 bytes + record_size: 8 bytes).

**Cardinality**: In a deletion-heavy workload, a 32 MB page could have up to ~130K tombstones (if every record was deleted). That's 130K × 16 bytes = ~2 MB of Vec allocation for `tombstone_records` alone, plus ~2 MB for `live_records`. The Vec growth involves log₂(130K) ≈ 17 reallocations.

Previously, the orchestrator passed an empty `&[]` for tombstone_addresses (diff line 2712: `updater.swing::<K>(&copy_result, &[], key_size, value_size)`), meaning tombstones were never actually processed. The new code collects them properly, which is a correctness improvement but adds memory proportional to tombstone count.

#### Warrant (Rule)

For a database with heavy delete workloads (e.g., TTL-based expiration), the tombstone count in a compaction region could be 50-90% of all records. The ~2 MB memory overhead is modest for a background compaction operation, but the Vec reallocation pattern (17 doublings) could cause transient memory spikes. Pre-allocating with `Vec::with_capacity` based on an estimate (e.g., `estimated_records / 4` for the tombstone case) would eliminate reallocation overhead.

However, `live_records` also has the same growth pattern and is not pre-allocated in the existing code, so this is consistent with the existing pattern.

#### Rebuttal Conditions

This is NOT a concern if:
1. Compaction regions are small enough that Vec reallocation is negligible (likely for most workloads).
2. The system has sufficient memory headroom for background compaction.
3. This is consistent with existing `live_records` allocation patterns (it is).

**Verdict: This is fine.** The memory overhead is proportional and modest. Pre-allocation would be a minor optimization but not a performance-critical issue.

#### Suggested Verification

No immediate action needed. If profiling shows allocator contention during compaction, add `Vec::with_capacity` estimates to both `live_records` and `tombstone_records` based on `(until_address - begin_address) / estimated_record_size`.

---

### Finding: Absence of software prefetch leaves a data-dependent stride bottleneck as an acknowledged known issue

**Severity**: consider
**Confidence**: HIGH
**Category**: performance

#### Grounds (Evidence)

In `scanner.rs` (diff lines 2932-2937):

```rust
// C-1: Software prefetch hint for the next record header.
// NOTE: Omitted because the crate uses `#[deny(unsafe_code)]`.
// When unsafe blocks are permitted in a future release, add
// `_mm_prefetch` here targeting `bytes[record_size..]` with
// `_MM_HINT_T0` on x86_64 to compensate for data-dependent stride.
```

The diff explicitly acknowledges the absence of prefetch as a trade-off against `deny(unsafe_code)`.

**Impact estimate**: Variable-length records have a data-dependent stride — each record's size must be computed before the next record's address is known. For fixed-size records, the stride is constant and the CPU's hardware prefetcher can predict the pattern. For variable-length records, the prefetcher sees irregular strides and its effectiveness drops.

On the test hardware (Intel i9-10900K, 32 KB L1d), scanning a 32 MB page with variable-length records at ~256 bytes average: the next record header is 256 bytes away on average. At an L1d hit rate of ~90% for sequential access (hardware prefetcher still works somewhat for semi-sequential patterns), the miss rate may increase to ~30-40% for variable strides. At ~4ns per L1d hit vs ~10ns per L2 hit, the per-record penalty is ~2-3ns, or ~0.3ms per 130K-record scan. This is modest.

#### Warrant (Rule)

For a fixed-size scanner, the CPU's hardware prefetcher handles the constant-stride pattern efficiently. The variable-stride change introduces cache miss unpredictability that software prefetch would mitigate. However, the crate's `deny(unsafe_code)` policy makes this impossible without a policy change. The comment correctly identifies this as future work.

At 32 MB pages with ~256-byte records, the overhead is ~0.3ms per scan — negligible compared to the total compaction cycle time (which includes copy, pointer swing, and epoch drain, typically 10-100ms).

**Verdict: This is fine for now.** The acknowledged trade-off is reasonable given the safety policy. The performance impact is modest.

#### Rebuttal Conditions

This WOULD become a concern if:
1. Variable-length records are very small (< 64 bytes), making the scan loop CPU-bound rather than memory-bound.
2. Compaction latency becomes a critical SLA (e.g., real-time compaction with strict latency budgets).
3. The `deny(unsafe_code)` policy is relaxed for performance-critical modules.

#### Suggested Verification

Compare `bench_compaction_record_size_variable` at various payload sizes against the `u64` baseline. If the variable-length scan is >3× slower per record than fixed-size at small payloads (16-64 bytes), the prefetch omission is significant and should be prioritized.

---

### Finding: `K::deserialize` in `address_update.rs::swing_one` allocates for every copied record during pointer swing

**Severity**: should-fix
**Confidence**: HIGH
**Category**: performance

#### Grounds (Evidence)

In `address_update.rs` (diff line 2010):

```rust
let key: K = K::deserialize(&accessor.as_slice()[key_offset..]);
let hash = key.hash();
```

This runs once per record in `copy_result.mappings` during Phase 3 (pointer swing). For variable-length keys, `K::deserialize` heap-allocates a `Vec<u8>` or `String` to compute the hash.

Similarly, in `remove_tombstone` (diff line 2042):

```rust
let key: K = K::deserialize(&accessor.as_slice()[key_offset..]);
let hash = key.hash();
```

**Cardinality**: If the scan found 50K live records and 20K tombstones, Phase 3 performs 70K deserializations, each allocating and freeing a variable-length key.

#### Warrant (Rule)

This is the same pattern as Finding 1 — deserialization purely to compute a hash. A `hash_from_bytes` trait method would allow both the scanner and the address updater to compute hashes without deserialization, eliminating the majority of heap allocations in the compaction pipeline.

The combined allocation overhead across scanner (130K records) + address updater (70K records) = ~200K transient heap allocations per compaction cycle for variable-length types. At ~30ns per alloc+free, that's ~6ms of pure allocator overhead.

#### Rebuttal Conditions

Same as Finding 1 — not a concern for fixed-size types where deserialization is zero-alloc.

#### Suggested Verification

Same as Finding 1 — a `Key::hash_from_bytes` trait method is the correct fix for both the scanner and address updater hot paths.

---

## Summary

| # | Finding | Severity | Confidence | Impact at Scale |
|---|---------|----------|------------|-----------------|
| 1 | Scanner heap-allocates per record via `K::deserialize` | should-fix | HIGH | ~130K allocs/cycle for varlen types |
| 2 | RecordAccessor constructed with page-remaining size | should-fix | MEDIUM | Likely negligible; verify with asm check |
| 3 | Chain walk accessor construction per hop | consider | MEDIUM | Fine — dominated by memory access latency |
| 4 | Tombstone Vec grows unbounded | consider | MEDIUM | Fine — consistent with existing patterns |
| 5 | No software prefetch for variable stride | consider | HIGH | ~0.3ms/cycle — acknowledged trade-off |
| 6 | Address updater deserializes keys to compute hashes | should-fix | HIGH | ~70K allocs/cycle for varlen types |

**Top priority**: Findings 1 and 6 share a root cause — deserialization for hash computation. Adding `Key::hash_from_bytes(&[u8]) -> KeyHash` would eliminate ~200K heap allocations per compaction cycle for variable-length types, which is the dominant performance regression risk in this change.
