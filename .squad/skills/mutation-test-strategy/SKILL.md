# Skill: Mutation Testing Strategy for FASTER-Rust

## When to Use
When analyzing mutation test results and deciding which mutations need tests vs which are acceptable survivors.

## Mutation Classification Framework

Not all surviving mutations are test gaps. Use this decision tree:

### 1. Equivalent Mutations (ACCEPT)

Mutations that produce identical behavior — no test can distinguish them.

**Examples:**
- **`| vs ^` when bits don't overlap:** In Treiber stack tag+address packing, tag occupies bits 48-63 and address occupies bits 0-47. `addr | tag == addr ^ tag` when no bits overlap.
- **`> 0` vs `>= 0` on unsigned types:** `if count > 0` and `if count >= 0` are equivalent for `u64` since it's always ≥ 0.
- **Const bit shifts in packing:** `1u64 << 48` vs `1u64 >> 48` may be compile-time constants where exact value doesn't affect correctness.

**Action:** Document as equivalent, no test needed.

### 2. Performance-Only Mutations (ACCEPT)

Mutations that affect throughput but not correctness.

**Examples:**
- **Prefetch noop:** Replacing `prefetch_read/write` with `()` has no correctness impact, only throughput.
- **Eager page allocation:** Optimizations that prevent contention but don't affect correctness.
- **Cache hints:** Memory ordering optimizations that are functionally equivalent.

**Action:** Document as performance-only, no test needed.

### 3. Private Method Mutations (CONDITIONAL)

Mutations in private methods that are fully covered by public API tests.

**Decision criteria:**
- Is the private method tested via all its public callers?
- Would a test be tautological (duplicating public API tests)?
- Is the mutation caught by higher-level integration tests?

**Action:** If yes to all, document coverage. Otherwise, write targeted unit test.

### 4. True Test Gaps (REQUIRE TESTS)

Mutations that represent real bugs the test suite missed.

**Examples:**
- Boundary conditions: `>=` vs `>` in range checks
- Arithmetic: `*` vs `+` or `/` in offset calculations
- Counters: `+=` vs `*=` (breaks all counters: 0 * 1 = 0)
- Logic: `||` vs `&&` where conditions are independent
- Hash function internals: shift directions, mixing steps

**Action:** Write mutation-killing test (see patterns below).

## Mutation-Killing Test Patterns

### Pattern 1: Pinned Reference Vectors

For hash functions, compute expected outputs offline and hardcode them.

```rust
#[test]
fn hash_function_consistency() {
    // Catches any internal computation change
    assert_eq!(faster_hash_u64(0x0000_0000_0000_0001), 0x8E88_45B9_5B21_09C1);
    assert_eq!(faster_hash_u64(0xFFFF_FFFF_FFFF_FFFF), 0x1234_5678_9ABC_DEF0);
    assert_eq!(faster_hash_u64(0), 0xDEAD_BEEF_CAFE_BABE);
}
```

**Why it works:** Any mutation in shift directions, XOR vs AND, etc. changes the output.

### Pattern 2: Exact Boundary Testing

For boundary mutations (`>` → `>=`, `==` → `!=`).

```rust
#[test]
fn seal_boundary_exact() {
    let store = create_store();
    
    // Test boundary-1 (should NOT seal)
    fill_to_bytes(&store, PAGE_SIZE - RECORD_SIZE);
    assert_eq!(page_state(&store, 0), PageState::Open);
    
    // Test exact boundary (SHOULD seal)
    fill_to_bytes(&store, PAGE_SIZE);
    assert_eq!(page_state(&store, 0), PageState::Sealed);
    
    // Test boundary+1 (new page)
    fill_to_bytes(&store, PAGE_SIZE + RECORD_SIZE);
    assert_eq!(page_state(&store, 1), PageState::Open);
}
```

### Pattern 3: Counter Validation

For counter mutations (`+=` → `*=`), verify counts > 0 and exact values.

```rust
#[test]
fn flush_counter_accurate() {
    let store = create_store();
    
    fill_and_flush_pages(&store, 3);
    
    let stats = store.stats();
    assert!(stats.pages_flushed > 0, "Counter stuck at zero (0 * 1 = 0)");
    assert_eq!(stats.pages_flushed, 3, "Counter calculation wrong");
}
```

### Pattern 4: Arithmetic Verification (Marker Bytes)

For offset mutations (`*` → `+` or `/`), use distinct marker values.

```rust
#[test]
fn page_offset_calculation() {
    let device = InMemoryDevice::new();
    
    // Write pages with distinct marker bytes
    for page_id in 0..4 {
        let marker = (page_id as u8) * 17;
        write_page(&device, page_id, &vec![marker; PAGE_SIZE]);
    }
    
    // Verify correct offsets (catches * → + or /)
    for page_id in 0..4 {
        let data = read_page(&device, page_id);
        let expected = (page_id as u8) * 17;
        assert_eq!(data[0], expected, "Offset = page_id * PAGE_SIZE broken");
    }
}
```

### Pattern 5: Logic Independence

For `||` → `&&` or `&&` → `||`, test each condition independently.

```rust
#[test]
fn recycle_condition_or_logic() {
    let store = create_store();
    
    // Evicted ALONE should allow recycle
    let evicted = create_frame_state(&store, FrameState::Evicted);
    assert!(can_recycle(evicted));
    
    // Free ALONE should allow recycle
    let free = create_frame_state(&store, FrameState::Free);
    assert!(can_recycle(free));
    
    // Neither should NOT recycle
    let active = create_frame_state(&store, FrameState::Active);
    assert!(!can_recycle(active));
}
```

## Test Placement Strategy

1. **Unit tests** (`src/*/tests.rs` or `tests/mutation_tests.rs`)
   - Simple arithmetic/logic mutations
   - Pure functions without complex setup
   - Fast, isolated

2. **Integration tests** (`tests/*_integration.rs`)
   - State machine transitions
   - Multi-component interactions
   - Private method mutations covered by public APIs

3. **Documentation** (code comments or `mutants.toml` exclude)
   - Equivalent mutations
   - Performance-only mutations
   - Higher-level coverage notes

## Interpreting cargo-mutants Output

**Kill rate formula:**
```
kill_rate = caught / (caught + missed)
```
⚠️ Exclude `timeouts` and `unviable` from the denominator.

**Files:**
- `mutants.out/caught.txt` — tests successfully killed these
- `mutants.out/missed.txt` — **ACTION REQUIRED** (or justify)
- `mutants.out/timeout.txt` — mutations cause infinite loops (exclude from scope)
- `mutants.out/unviable.txt` — mutations don't compile (expected)

## Configuration Patterns

**In `rust/mutants.toml`:**

```toml
examine_globs = [
    "src/hash/**/*.rs",
    "src/allocator.rs",
    # Focus on critical paths only
]

exclude_re = [
    "::tests?::",           # Test code
    "impl.*Debug",          # Display/Debug impls (cosmetic)
    "impl.*Display",
    "prefetch_",            # Performance hints (no correctness)
    "TAG_MASK|ADDR_MASK",   # Bit-packing constants (timeouts)
]

timeout_multiplier = 3.0
minimum_test_timeout = 60
```

## Confidence
**High** — Refined through 17-gap mutation campaign, covers all common mutation types.

## Learned From
- Mutation Testing Campaign (2024): 17 gaps killed across 5 modules
- cargo-mutants v27.0 configuration tuning
- Equivalent mutation patterns discovered in hash packing, unsigned comparisons
- Performance-only mutations identified via profiling impact analysis
