# Mutation Testing Skill — FASTER Rust

## Tool
`cargo-mutants` v27.0 — configured in `rust/mutants.toml`.

## Quick Start
```bash
cd rust
# Full run on a module:
cargo mutants --package faster-core -F "src/hash/" --timeout 120

# Single file:
cargo mutants --package faster-core -F "src/allocator.rs" --timeout 120

# Results in mutants.out/{caught,missed,timeout,unviable}.txt
```

## Configuration (`rust/mutants.toml`)
- `test_tool = "nextest"` — uses nextest, matches CI
- `timeout_multiplier = 3.0`, `minimum_test_timeout = 60`
- `examine_globs` — scoped to critical modules only
- `exclude_re` — skips bitwise field-packing (timeouts), Display/Debug impls (cosmetic), test helpers

## Interpreting Results

### Kill Rate Formula
```
kill_rate = caught / (caught + missed)
```
Exclude timeouts and unviable from the denominator.

### Equivalent Mutations (expected survivors)
These mutations produce identical behavior — they are NOT test gaps:
1. **`| vs ^` when bits don't overlap**: In Treiber stack tag+address packing, tag occupies bits 48-63 and address occupies bits 0-47. `addr | tag == addr ^ tag` when no bits overlap.
2. **`> 0` vs `>= 0` on unsigned**: `if count > 0` and `if count >= 0` are equivalent for `u64` since it's always ≥ 0.
3. **Const bit shifts**: `1u64 << 48` vs `1u64 >> 48` produces 0 vs a huge number — but these are compile-time constants that affect behavior in complex ways. If the const is only used in combinations where its exact value doesn't matter for correctness, it's equivalent.

### Performance-Only Mutations (acceptable survivors)
- **Prefetch noop**: Replacing `prefetch_read/write` with `()` has no correctness impact, only throughput.
- **Eager page allocation**: The allocator's `get_or_add_page(next_page)` optimization prevents contention but doesn't affect correctness since pages are created on-demand.

### True Test Gaps (action required)
- Mutations in hash function internals (shift directions)
- Mutations in boundary conditions (`>=` vs `>` in range checks)
- Free-list corruption (if not caught by existing tests)
- Overflow chain traversal (returning None instead of following chain)

## Writing Mutation-Killing Tests

### Pattern 1: Pinned Reference Vectors
For hash functions, compute expected outputs offline and hardcode them:
```rust
assert_eq!(faster_hash_u64(0x0000_0000_0000_0001), 0x8E88_45B9_5B21_09C1);
```
Catches any internal computation change.

### Pattern 2: Boundary Precision
For range operations `[begin, end)`, insert entries at:
- Before begin (should survive)
- At begin (should be affected)
- In middle (should be affected)
- At end (should survive — exclusive)
- After end (should survive)

### Pattern 3: Free-List Roundtrip
Allocate → free → reallocate. Assert same address returned and allocation count unchanged.

### Pattern 4: Overflow Chain
Use a small hash table (2^3 = 8 buckets) and insert enough entries targeting the same bucket to force overflow. Verify all entries findable.

## Timing
- allocator.rs (82 mutants): ~50 minutes
- hash/ (251 mutants): ~2 hours
- Full `examine_globs` scope: ~6-8 hours estimated
