# Mutation Testing with cargo-mutants

FASTER Rust uses [cargo-mutants](https://mutants.rs/) v27.0+ for mutation
testing. Mutation testing verifies that our test suite actually detects bugs
by systematically introducing small changes (mutants) and confirming that
at least one test fails for each.

## Quick Start

```bash
# Install (one-time)
cargo install cargo-mutants

# Run against all configured modules (uses rust/mutants.toml)
cd rust
cargo mutants --package faster-core

# Run against a single file
cargo mutants --package faster-core -F 'address.rs'

# Dry-run: list mutants without executing
cargo mutants --package faster-core --list
```

## Configuration

Configuration lives in `rust/mutants.toml`. Key settings:

| Setting | Value | Rationale |
|---------|-------|-----------|
| `timeout_multiplier` | 3.0 | Test suite baseline ~41s; 3× gives ~120s headroom |
| `minimum_test_timeout` | 60 | Floor for very fast baselines |
| `test_tool` | nextest | Matches CI runner; faster parallel execution |
| `examine_globs` | Critical modules | Phase 1: address, record, hash, store, compaction |
| `exclude_globs` | Bench/sample/FFI | No assertions → mutants always survive (noise) |
| `exclude_re` | Bitwise field packing, Display/Debug | Known timeout-prone or cosmetic-only |

### Examined Modules (Phase 1)

- `address.rs` — Logical address arithmetic
- `record/record_info.rs` — Record metadata bit-packing
- `hash/table.rs`, `hash/index.rs`, `hash/bucket.rs` — Hash index core
- `store/mod.rs`, `store/kv.rs`, `store/operations.rs`, `store/session.rs` — KV store
- `compaction/*.rs` — Compaction scanner and plans
- `error.rs`, `status.rs` — Error/status enums

## Quality Target

| Scope | Kill Rate Target |
|-------|-----------------|
| All modules | ≥ 80% |
| Critical modules (store, compaction, hash) | ≥ 85% |

"Kill rate" = caught / (caught + missed). Unviable mutants (compile errors) are
excluded from the denominator — they indicate the type system is already
preventing that class of bug.

## Pilot Results

**Date:** 2025-07-18
**Tool:** cargo-mutants 27.0.0
**Target:** `address.rs` (also matched `compaction/begin_address.rs`, `grow/splitter.rs` via substring)

### Summary

| File | Caught | Missed | Unviable | Timeouts | Kill Rate |
|------|--------|--------|----------|----------|-----------|
| `address.rs` | 45 | 0 | 8 | 0 | **100%** |
| `compaction/begin_address.rs` | 9 | 0 | 1 | 0 | **100%** |
| `grow/splitter.rs` | 1 | 0 | 0 | 0 | **100%** |
| **Total** | **55** | **0** | **9** | **0** | **100%** |

**Runtime:** ~6 minutes for 64 mutants (55 viable).

### Unviable Mutants

All 9 unviable mutants were in `address.rs` — bitwise constant definitions and
newtype constructors where the mutation produces a type error at compile time:

- 5× replacing `-` with `+` or `<<` with `>>` in const bit-mask definitions
- 3× replacing constructor bodies with `Default::default()` on non-Default types
- 1× replacing `-` with `+` in const expression

These are *good* — the type system prevents the bug class entirely.

### Comparison with Prior Pilot

The prior pilot (mentioned in team decision) saw 8/53 timeouts on `address.rs`.
This run saw **zero timeouts**. The difference:
- `exclude_re` in `mutants.toml` filters out the timeout-prone bitwise field
  packing functions (`page_index`, `offset_in_page`, `from_page_offset`)
- Timeout multiplier of 3× (vs potentially tighter in the prior run)

## Recommended Workflow

### During Development

Run mutation testing on files you've changed:

```bash
cargo mutants --package faster-core -F 'my_changed_file.rs'
```

### Pre-merge Gate (Future)

For CI integration, run the full configured scope:

```bash
cargo mutants --package faster-core
```

Target: complete in < 30 minutes. If it exceeds this, tighten `examine_globs`
to critical modules only.

### Periodic Audit

Monthly or per-milestone, run against the full crate (no `examine_globs` filter):

```bash
cargo mutants --package faster-core --in-place
```

Review `mutants.out/missed.txt` for test gaps.

## Known Limitations

1. **Bitwise operations in unsafe blocks** — Mutations to bit-shifts and masks in
   address/record packing can cause infinite loops or nonsensical memory access.
   These are excluded via `exclude_re` in the config.

2. **Substring matching with `-F`** — The `-F` flag does substring matching on
   the mutant description, so `-F 'address.rs'` also matches
   `begin_address.rs`. Use `-F 'src/address.rs'` for precision.

3. **Long test suite** — With ~1500 tests and a ~41s baseline, each mutant takes
   5-15s. Full-crate mutation testing may take hours. Use `examine_globs` to
   scope down.

4. **Unviable mutant noise** — Newtype wrappers with no `Default` impl produce
   unviable mutants. This is harmless but inflates the total count.

## Output

Results are written to `rust/mutants.out/` (gitignored). Key files:

- `caught.txt` — Mutants killed by tests ✓
- `missed.txt` — Mutants that survived (test gaps!) ✗
- `timeout.txt` — Mutants that exceeded the time limit
- `unviable.txt` — Mutants that didn't compile
- `outcomes.json` — Machine-readable full results
