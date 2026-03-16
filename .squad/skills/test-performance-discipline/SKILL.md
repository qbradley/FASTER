# Skill: Test Performance Discipline

## When to Use

When writing or modifying tests that involve proptest, large datasets, or I/O operations.

## Pattern

### Two-Tier Test Strategy

**Tier 1 (default):** Fast feedback gate, runs on every commit
- **Target:** Every test <1s in isolation
- **Enforcement:** CI precheckin check
- **How to verify:** `cargo nextest run` (without `--run-ignored`)

**Tier 2 (`#[ignore]`):** Exhaustive coverage, runs on-demand
- **Tests that:** Fill pages (40s+), stress test concurrency, use SyncFileDevice I/O
- **How to run:** `cargo nextest run --run-ignored`

### Property Test Sizing

```rust
#[cfg(test)]
proptest! {
    #![proptest_config(ProptestConfig {
        cases: 2,  // Tier 1: minimal cases for <1s
        ..Default::default()
    })]
    
    fn property_test(data: Vec<u8>) {
        // Test logic
    }
}
```

For tier-2 exhaustive tests:
```rust
#[ignore]  // Tier 2
#[cfg(test)]
proptest! {
    #![proptest_config(ProptestConfig {
        cases: 256,  // Full coverage
        ..Default::default()
    })]
    
    fn exhaustive_property_test(data: Vec<u8>) {
        // Same test, more cases
    }
}
```

### Record Count Calculations

When writing tests that fill pages:
- **Page size:** 32 MiB (2^25 bytes)
- **Record size:** 24 bytes (u64 key + u64 value + 8-byte header)
- **Records per page:** ~1.4M
- **Eviction threshold:** Need 6+ pages to trigger eviction in 4-page buffer

```rust
// ✅ Tier 1 — Quick smoke test
const RECORD_COUNT: usize = 50_000;  // ~1 page, completes in 0.5s

// ❌ Moved to tier 2 — Fills 6 pages
#[ignore]
#[test]
fn test_eviction() {
    const RECORD_COUNT: usize = 9_000_000;  // ~6 pages, takes 40s+
    // ...
}
```

### Nextest Best Practices

1. **Never use `--all-targets`** — Criterion bench binaries trigger full benchmark runs
2. **Parallel contention is real** — A test taking 0.1s in isolation may show 1.7s under load
3. **Individual time is the true metric** — Use `cargo nextest run --test-threads=1` to measure baseline

### Property Test Case Cost

```rust
// HashIndex proptest cases cost ~240ms each (epoch + allocator setup)
// For <1s tier-1: use 2 cases
// For tier-2: use 64-256 cases
```

## Checklist for New Tests

- [ ] Does this test fill >1 page (1.4M records)? → `#[ignore]`
- [ ] Does this test use SyncFileDevice or disk I/O? → `#[ignore]`
- [ ] Does this test use proptest? → Set `cases: 2` for tier-1, or `#[ignore]` + high count for tier-2
- [ ] Does this test stress-test concurrency (>30s)? → `#[ignore]`
- [ ] Verify: `cargo nextest run --test-threads=1` shows <1s for tier-1 tests

## Examples

### Tier 1 → Tier 2 Migration
```rust
// Before (tier-1 violation: 40s)
#[test]
fn test_lossy_eviction() {
    const RECORDS: usize = 9_000_000;  // Fills 6 pages
    // ...
}

// After (tier-2)
#[ignore]  // Tier 2: fills 6 pages, ~40s
#[test]
fn test_lossy_eviction() {
    const RECORDS: usize = 9_000_000;
    // ...
}
```

### Reducing Iteration Count
```rust
// Before (1.2s)
#[test]
fn test_concurrent_ops() {
    const OPS_PER_THREAD: usize = 1_000_000;
    // ...
}

// After (0.4s)
#[test]
fn test_concurrent_ops() {
    const OPS_PER_THREAD: usize = 10_000;  // Still exercises concurrency
    // ...
}
```

## Confidence

High

## Learned From

- Code Quality Cleanup (2026-03-10): 17 tests moved to tier-2, 9 sped up
- History.md: "Property test case cost," "Page-fill tests fundamentally slow," "Nextest parallel contention"
- History.md: "Precheckin Fix — Nextest hung due to Criterion bench binaries with --all-targets"
