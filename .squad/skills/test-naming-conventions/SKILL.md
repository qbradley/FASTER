# Skill: Test Naming Conventions for FASTER-Rust

## When to Use
When naming test files, test functions, or test modules in the FASTER Rust workspace.

## Test File Naming

### Integration Test Files (`rust/crates/*/tests/`)

**Pattern:** `{subject}_{type}.rs`

**Types:**
- `_tests.rs` — general integration tests for a subject
- `_mutation_tests.rs` — tests targeting specific mutation gaps
- `_integration.rs` — cross-component/subsystem tests
- `_edge_cases.rs` — boundary and corner case tests

**Examples:**
```
tests/hybrid_log_tests.rs              # General HybridLog tests
tests/hybrid_log_mutation_tests.rs     # Mutation-killing tests
tests/checkpoint_recovery_tests.rs     # Checkpoint recovery scenarios
tests/io_error_injection.rs            # I/O error handling tests
tests/deadlock_tests.rs                # Deadlock scenario tests
tests/compaction_integration.rs        # Compaction subsystem integration
```

### Special Test Files

**Common test utilities:**
```
tests/common/mod.rs         # Shared test utilities (NOT a test file itself)
tests/common/devices.rs     # Test device doubles
tests/common/helpers.rs     # Store creation, filling helpers
```

**Single-purpose test files:**
```
tests/loom_tests.rs         # Loom concurrency tests (--cfg loom)
tests/miri_tests.rs         # Miri UB detection tests
tests/property_tests.rs     # Property-based tests (proptest)
```

## Test Function Naming

### Pattern: `{action}_{scenario}_{expectation}`

**Components:**
1. **Action:** What operation is being tested
2. **Scenario:** Under what conditions
3. **Expectation:** What should happen

**Examples:**
```rust
#[test]
fn upsert_to_sealed_page_advances_to_next_page() { }

#[test]
fn flush_during_queuefull_retries_until_success() { }

#[test]
fn read_from_evicted_page_reloads_from_disk() { }

#[test]
fn checkpoint_with_concurrent_writes_captures_consistent_snapshot() { }

#[test]
fn seal_boundary_exact_triggers_at_page_size() { }
```

### Mutation-Killing Test Names

Be explicit about what mutation is being killed.

```rust
#[test]
fn seal_boundary_mutation_gt_vs_gte() {
    // Kills: needs_eviction `>` → `>=`
}

#[test]
fn offset_calculation_multiply_vs_add() {
    // Kills: offset = page_id * PAGE_SIZE → page_id + PAGE_SIZE
}

#[test]
fn flush_counter_increment_vs_multiply() {
    // Kills: flushed += 1 → flushed *= 1
}
```

### Property-Based Test Names

Start with `prop_` prefix.

```rust
#[test]
fn prop_upsert_then_read_returns_latest_value() { }

#[test]
fn prop_checkpoint_recovery_restores_all_keys() { }

#[test]
fn prop_concurrent_operations_maintain_invariants() { }
```

### Loom/Miri Test Names

Include the testing framework in the name.

```rust
#[test]
#[cfg(loom)]
fn loom_concurrent_increment_no_race() { }

#[test]
fn miri_aligned_read_no_ub() { }
```

## Test Module Organization

### In library code (`src/`)

Use `#[cfg(test)]` modules for unit tests.

```rust
// src/allocator.rs
pub struct Allocator { ... }

impl Allocator { ... }

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocate_returns_valid_offset() { }
    
    #[test]
    fn free_then_allocate_reuses_space() { }
}
```

### In integration tests (`tests/`)

No `#[cfg(test)]` needed (entire file is test-only).

```rust
// tests/checkpoint_recovery_tests.rs
use faster_core::*;
mod common; // Import common test utilities

#[test]
fn full_checkpoint_recovery() { }

#[test]
fn incremental_checkpoint_recovery() { }
```

## Test Categorization Comments

For large test files, use section comments.

```rust
// ══════════════════════════════════════════════════════════════
// Basic Operations
// ══════════════════════════════════════════════════════════════

#[test]
fn upsert_new_key() { }

#[test]
fn read_existing_key() { }

// ══════════════════════════════════════════════════════════════
// Boundary Conditions
// ══════════════════════════════════════════════════════════════

#[test]
fn seal_at_exact_page_boundary() { }

#[test]
fn eviction_at_memory_limit() { }

// ══════════════════════════════════════════════════════════════
// Error Recovery
// ══════════════════════════════════════════════════════════════

#[test]
fn recovers_from_io_error() { }
```

## Anti-Patterns

**❌ Avoid:**
```rust
#[test]
fn test1() { }  // No context

#[test]
fn test_function() { }  // Too vague

#[test]
fn it_works() { }  // Meaningless

#[test]
fn test_upsert() { }  // No scenario or expectation
```

**✅ Prefer:**
```rust
#[test]
fn upsert_to_full_page_seals_and_advances() { }

#[test]
fn concurrent_reads_during_flush_return_consistent_data() { }

#[test]
fn boundary_mutation_needs_eviction_gte() { }
```

## Confidence
**High** — Derived from 30+ test files in faster-core, consistent patterns across codebase.

## Learned From
- Test file naming patterns in `rust/crates/faster-core/tests/`
- Mutation test naming from 17-gap campaign
- Integration test organization from deadlock tests, checkpoint recovery, I/O injection
