# Skill: Lossy Eviction Race Handling

## When to Use

When implementing read/write/delete operations in a log-structured store with concurrent lossy eviction, where:
- Epoch protection guards record addresses during chain walks
- Separate maintenance thread can evict pages between operations
- `get_record()` can return `None` for addresses that were valid during `find_record_for_key()`

This is the **epoch-doesn't-prevent-eviction** pattern.

## Pattern

### The Race Condition

```
Thread A (reader):
  1. epoch.protect()
  2. chain_walk() → finds RecordAddress(page=5, offset=100)
     [epoch guard means page won't be freed/reused]
  3. --> RACE: Thread B evicts page 5 (lossy mode) <--
  4. get_record(RecordAddress(5, 100)) → None  [❌ panic if .expect()]

Thread B (maintenance):
  1. evict_page(5) → page content deallocated
     [epoch guard prevents freeing page struct, not content]
```

### The Misconception

```rust
// ❌ WRONG ASSUMPTION (from old comment):
// "Epoch protection ensures the page won't be evicted
//  during the operation, so get_record() must succeed"

let addr = self.find_record_for_key(key);
let record = self.get_record(addr).expect("epoch protected");
//                                  ^^^^^^ PANIC in lossy mode!
```

**Reality**: Epoch guards prevent page struct reuse (ABA prevention), NOT content eviction. In lossy mode, maintenance thread can evict pages while readers hold epoch guards.

### The Fix Pattern

Always handle `None` gracefully in hot paths:

```rust
// ✅ CORRECT: Handle eviction race
let addr = self.find_record_for_key(key);

match self.get_record(addr) {
    Some(record) => {
        // Process record
        OperationStatus::Success(record.value)
    }
    None => {
        // Record was evicted between find and get
        match region {
            Mutable => OperationStatus::NotFound,      // truly doesn't exist
            FuzzyRegion | ReadOnly => OperationStatus::Aborted,  // might exist on disk
        }
    }
}
```

### Affected Operation Paths

All three operation types have this race:

1. **Read**: `internal_read()`
   - Mutable miss → `NotFound` (correct, doesn't exist)
   - FuzzyRegion/ReadOnly miss → `Aborted` (might be on disk, retry with disk I/O)

2. **RMW**: `internal_rmw()`
   - Mutable miss → create new record
   - FuzzyRegion/ReadOnly miss → `Aborted` (need disk read for initial value)

3. **Delete**: `internal_delete()`
   - Mutable miss → `NotFound` (already deleted or never existed)
   - FuzzyRegion/ReadOnly miss → `NotFound` (we tried)

### Classification Helper

```rust
fn classify_region(&self, addr: RecordAddress) -> Region {
    let page = addr.page();
    let head = self.log.head_address();
    let read_only = self.log.read_only_address();
    
    if page >= read_only {
        Region::Mutable
    } else if page >= head {
        Region::FuzzyRegion  // on disk, might be in memory
    } else {
        Region::ReadOnly     // definitely on disk only
    }
}
```

## Confidence: High

## Learned From

- **Lossy Eviction Race Fix (2026-03-11)**: Discovered during concurrent maintenance thread testing
- **Root cause**: Misleading comment claimed epoch guards prevent eviction
- **Impact**: Three operation paths affected (read, rmw, delete)
- **EvictionPolicy::default()** has `max_in_memory_pages=256` — never triggers for small buffers, making this race rare in tests

## Key Insights

1. **Epoch protection != eviction prevention** — guards prevent ABA, not content loss
2. **Classification is advisory** — snapshot-based region classification can be stale
3. **Lossy mode needs explicit config** — default policy hides the race in testing
4. **None is not an error** — it's a valid outcome in concurrent systems

## Testing

```rust
#[test]
fn lossy_eviction_concurrent_reads() {
    let policy = EvictionPolicy {
        max_in_memory_pages: 3,  // ✅ Force eviction
        eviction_trigger_ratio: 0.8,
    };
    
    let store = FasterKv::with_eviction(config, device, policy);
    
    // Writer thread: fill buffer, trigger eviction
    // Reader threads: concurrent reads
    // Should see Aborted, not panics
}
```

## Anti-Patterns

❌ **`.expect()` on `get_record()`** in hot paths → panic on eviction race
❌ **Assume epoch guard prevents eviction** → wrong mental model
❌ **Test only with default EvictionPolicy** → hides eviction races
❌ **Treat `None` as impossible** → violates concurrent systems reality

## Key Files

- `rust/crates/faster-core/src/hybrid_log/operations.rs` (internal_read, internal_rmw, internal_delete)
- `rust/crates/faster-core/src/hybrid_log/eviction.rs` (lossy eviction logic)
- `rust/crates/faster-core/src/epoch/` (epoch protection, NOT eviction prevention)
- `crates/samples/page-cache/src/main.rs` (lossy cache with eviction testing)
