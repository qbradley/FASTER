# Skill: Integration Test Patterns for FASTER-Rust

## When to Use
When writing integration tests for FASTER that verify multi-component interactions, state machine transitions, or end-to-end correctness.

## FASTER State Machine Testing

FASTER components have complex state transitions. Integration tests verify these paths.

### Page State Lifecycle

**States:** Open → Sealed → Flushing → Flushed → Evicted → Truncated

**Critical transitions to test:**
1. **Open → Sealed** — page fills (exact boundary at `page_size`)
2. **Sealed → Flushing** — flush triggered
3. **Flushing → Flushed** — I/O completion callback
4. **Flushed → Evicted** — memory pressure eviction
5. **Evicted → Reloaded** — read from disk on access

**Test pattern:**
```rust
#[test]
fn page_lifecycle_full_cycle() {
    let store = create_store_with_small_pages();
    
    // 1. Fill to exact boundary (triggers seal)
    fill_page_exactly(&store, PAGE_SIZE);
    assert_page_state(&store, 0, PageState::Sealed);
    
    // 2. Trigger flush
    store.flush_async();
    store.complete_pending(); // Wait for I/O
    assert_page_state(&store, 0, PageState::Flushed);
    
    // 3. Trigger eviction
    trigger_memory_pressure(&store);
    assert_page_state(&store, 0, PageState::Evicted);
    
    // 4. Read forces reload
    let value = store.read(&key);
    assert_eq!(value.unwrap(), expected);
}
```

### Region Boundary Testing

FASTER divides the log into regions (read-only, fuzzy, mutable).

**Pattern: Region-specific operations**
```rust
#[test]
fn operations_respect_region_boundaries() {
    let store = create_store();
    
    // Insert in read-only region (needs RMW)
    let old_key = insert_and_age_to_readonly(&store);
    let outcome = store.upsert(&old_key, &new_value);
    assert_eq!(outcome.status(), OperationStatus::Success);
    
    // Verify value in fuzzy region
    let fuzzy_key = insert_and_age_to_fuzzy(&store);
    assert!(is_in_fuzzy_region(&store, &fuzzy_key));
    
    // Fresh insert in mutable region
    let new_key = generate_key();
    store.upsert(&new_key, &value);
    assert!(is_in_mutable_region(&store, &new_key));
}
```

## Pattern 1: Exact Boundary Testing

Many bugs hide at exact boundaries (page fills, region transitions).

**Key insight:** FASTER uses 32MB pages (1 << 25). Filling to exact boundary requires full-page allocations.

```rust
const PAGE_SIZE: usize = 1 << 25; // 32 MB
const RECORD_SIZE: usize = 64;    // Depends on key/value sizes

#[test]
fn seal_only_at_exact_page_boundary() {
    let store = create_store();
    
    // Fill to exactly N-1 records (should NOT seal)
    let records_per_page = PAGE_SIZE / RECORD_SIZE;
    for i in 0..(records_per_page - 1) {
        store.upsert(&key(i), &value(i));
    }
    assert_page_state(&store, 0, PageState::Open);
    
    // One more record (should seal)
    store.upsert(&key(records_per_page), &value);
    assert_page_state(&store, 0, PageState::Sealed);
}
```

## Pattern 2: Multi-Page Tests

Verify behavior across page boundaries.

```rust
#[test]
fn operations_span_multiple_pages() {
    let store = create_store();
    let keys = fill_multiple_pages(&store, 3); // Fill 3 pages
    
    // Verify all keys readable
    for key in &keys {
        assert!(store.read(key).is_some());
    }
    
    // Verify page states
    assert_page_state(&store, 0, PageState::Sealed);
    assert_page_state(&store, 1, PageState::Sealed);
    assert_page_state(&store, 2, PageState::Open);
}
```

## Pattern 3: Counter Validation

Counter bugs (`+=` → `*=` mutations) break silently. Verify counts > 0.

```rust
#[test]
fn counters_increment_correctly() {
    let store = create_store();
    
    fill_and_flush_pages(&store, 5); // Flush 5 pages
    
    let stats = store.stats();
    assert!(stats.pages_flushed > 0, "Counter stuck at zero");
    assert_eq!(stats.pages_flushed, 5, "Counter calculation wrong");
}
```

## Pattern 4: Arithmetic Verification with Marker Bytes

For offset calculations, use distinct marker values to verify correctness.

```rust
#[test]
fn page_offsets_calculated_correctly() {
    let device = InMemoryDevice::new();
    
    // Write pages with distinct marker bytes
    for page_id in 0..4 {
        let marker = (page_id as u8) * 17; // Distinct per page
        let data = vec![marker; PAGE_SIZE];
        write_page(&device, page_id, &data);
    }
    
    // Read back and verify offsets
    for page_id in 0..4 {
        let data = read_page(&device, page_id);
        let expected_marker = (page_id as u8) * 17;
        assert_eq!(data[0], expected_marker, "Page offset calculation wrong");
    }
}
```

## Pattern 5: Logic Condition Independence

For `||` / `&&` mutations, test each condition separately.

```rust
#[test]
fn recycle_accepts_evicted_or_free() {
    let store = create_store();
    
    // Test Evicted condition
    let evicted_frame = create_evicted_frame(&store);
    assert!(can_recycle(&store, evicted_frame));
    
    // Test Free condition
    let free_frame = create_free_frame(&store);
    assert!(can_recycle(&store, free_frame));
    
    // Test neither (should not recycle)
    let active_frame = create_active_frame(&store);
    assert!(!can_recycle(&store, active_frame));
}
```

## Pattern 6: Error Recovery Paths

Integration tests for error handling across subsystems.

```rust
#[test]
fn recovers_from_io_error_during_flush() {
    let device = FaultInjectingDevice::new(base, |_, _| IoStatus::Error);
    let store = create_store_with_device(device);
    
    store.upsert(&key, &value);
    store.flush_async();
    store.complete_pending(); // I/O fails here
    
    // Verify graceful degradation
    assert_eq!(store.stats().flush_errors, 1);
    assert!(store.read(&key).is_some(), "Data still readable from memory");
}
```

## Test Organization

**File naming:**
- `*_tests.rs` — general integration tests
- `*_mutation_tests.rs` — tests targeting specific mutations
- `*_integration.rs` — cross-component tests
- `*_edge_cases.rs` — boundary/corner case tests

**Module structure:**
```rust
// tests/common/mod.rs — shared test utilities
pub mod devices;  // Test device doubles
pub mod helpers;  // Store creation, filling helpers
pub mod assertions; // Custom assertions for FASTER state

// tests/subsystem_integration.rs
use common::*;

#[test]
fn test_name() { ... }
```

## Confidence
**High** — Patterns extracted from 17 mutation-killing tests, deadlock fix harness, 30+ integration test files.

## Learned From
- Mutation campaign (2024): Boundary testing, counter validation, marker bytes
- Deadlock tests (2026-03-11): State machine testing, multi-device scenarios
- Page size matters: 32MB pages require full-page allocations for sealing tests
- Region boundaries: Many operations require pages in specific regions
