# Skill: Cache-Line Aware Data Structure Layout

## When to Use

When designing data structures for high-performance concurrent systems where:
- Multiple threads access shared state
- Cache coherence traffic dominates performance
- False sharing can destroy scalability
- Alignment affects atomic operation cost

Per Sam's charter: "Design data structures for cache locality before ergonomics" and "Consider a cache miss a bug."

## Pattern

### Cache Line Fundamentals

- **Size**: 64 bytes on x86-64 (standard)
- **False sharing**: Two threads accessing different fields in same cache line → coherence ping-pong
- **Alignment**: Atomics crossing cache lines can be non-atomic on some architectures

### Padding Pattern

```rust
use std::sync::atomic::AtomicU64;

// ❌ WRONG: Hot fields in same cache line
struct HotPath {
    shared_counter: AtomicU64,  // Thread A writes
    local_state: u64,           // Thread B writes
    // Both in same 64-byte line → false sharing
}

// ✅ CORRECT: Pad to separate cache lines
#[repr(C, align(64))]
struct HotPath {
    shared_counter: AtomicU64,
    _pad: [u8; 56],  // 64 - 8 = 56 bytes padding
}

#[repr(C, align(64))]
struct LocalState {
    local_state: u64,
    _pad: [u8; 56],
}
```

### RecordInfo Example (Real FASTER Pattern)

```rust
// 64-bit atomic with bit-packing
// [63:Fin][62:Tom][61:Inv][60:Seal][59..48:Ver(12)][47..0:PrevAddr(48)]

#[repr(transparent)]
pub struct RecordInfo {
    atomic_info: AtomicU64,  // Single atomic, cache-line friendly
}

impl RecordInfo {
    // Bit operations on single atomic → one cache line touch
    pub fn seal(&self) {
        self.atomic_info.fetch_or(SEALED_BIT_MASK, Release);
    }
    
    pub fn is_sealed(&self) -> bool {
        self.atomic_info.load(Acquire) & SEALED_BIT_MASK != 0
    }
}
```

**Why it works**: All coordination state fits in single u64 → single cache line, single atomic operation.

### Epoch Table Pattern (Counter-Example)

```rust
// From faster-core/src/epoch/table.rs
const MAX_THREADS: usize = 256;

pub struct EpochTable {
    entries: [AtomicU64; MAX_THREADS],  // 256 * 8 = 2KB, 32 cache lines
}

impl EpochTable {
    // ❌ PERFORMANCE COST: Scans all 256 entries on every unprotect()
    pub fn compute_safe_epoch(&self) -> u64 {
        let mut min = u64::MAX;
        for entry in &self.entries {
            let val = entry.load(Acquire);  // 32 cache line loads!
            if val != 0 {
                min = min.min(val);
            }
        }
        min
    }
}
```

**Cost**: ~97-128ns per scan (measured), 16KB cache scan. This is why epoch-amortization skill exists.

### Layout Checklist

- [ ] Hot atomics in separate cache lines (64-byte align + pad)
- [ ] Read-only data grouped together (cache-friendly, no coherence)
- [ ] Write-hot data separated by thread (prevent false sharing)
- [ ] Bit-pack related state into single atomic when possible
- [ ] Measure with `perf stat -e cache-misses` or similar

## Confidence: High

## Learned From

- **Charter**: "Think in cache lines and memory fences"
- **Epoch-amortization skill**: compute_safe_epoch() scans 16KB, costs ~128ns
- **RecordInfo design**: All coordination in single u64 atomic
- **OFFSET_BITS = 25**: 32MB pages mean memory allocation dominates small tests

## Key Patterns by Use Case

| Use Case | Pattern | Example |
|----------|---------|---------|
| Coordination flags | Single packed atomic | RecordInfo (u64 with bit fields) |
| Per-thread counters | Align(64) + pad | Epoch table entries (but oversized) |
| Shared read-only | Dense packing OK | Config structs, constants |
| Multi-writer state | Separate cache lines | Producer/consumer queue heads |

## Measurement

```bash
# Cache miss profiling
perf stat -e cache-references,cache-misses cargo bench

# L1/L2/L3 breakdown
perf stat -e L1-dcache-load-misses,LLC-load-misses cargo bench

# False sharing detection (Intel)
perf c2c record -p <pid>
perf c2c report
```

## Anti-Patterns

❌ **Struct with mixed hot/cold fields** → wastes cache lines
❌ **Atomics sharing cache lines across threads** → false sharing hell
❌ **Array of structs when struct of arrays better** → poor locality
❌ **Ignoring alignment of atomics** → potential non-atomicity
❌ **Premature optimization without measurement** → wrong cache lines optimized

## Key Files

- `rust/crates/faster-core/src/record/record_info.rs` (packed atomic pattern)
- `rust/crates/faster-core/src/epoch/table.rs` (large scan cost example)
- `rust/crates/faster-core/benches/perf_analysis.rs` (epoch cost measurement)

## References

- **Epoch-amortization skill**: Documents the epoch table scan cost in detail
- **C++ FASTER**: Similar RecordInfo packing pattern
- Intel optimization manual: "3.6.11 Contended Access, False Sharing, and Performance"
