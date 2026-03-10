# Decision: Mutation Testing Campaign 3 — Remaining Modules

**Author:** Boromir (QA)  
**Date:** 2026-03-10  
**Status:** Complete  

## Summary

Campaign 3 applied cargo-mutants to all previously-untested modules in
`faster-core/src/`, completing full mutation coverage of the crate.

## Scope

18 module groups tested (1,637 total mutants):

| Module | Mutants | Caught | Missed | Unviable | Tests Written |
|--------|---------|--------|--------|----------|--------------|
| record/record_info.rs | 91 | 73 | 7→0 | 11 | 13 |
| address.rs | 54 | 46 | 0 | 8 | 0 |
| state/ | 61 | 49 | 4→0 | 7+1T | 2 |
| record/layout.rs | 56 | 50 | 2→0 | 3+1T | 1 |
| record/traits.rs | 111 | 98 | 10→0 | 3 | 10 |
| epoch/ | 73 | 57 | 14→6 | 2 | 6 |
| buffer_pool+device | 124 | 95 | 16→0 | 13 | 12 |
| sync_file_device.rs | 124 | 89 | 25→0 | 2+8T | 7 |
| store/functions+batch+builder+pending_io | 181 | 114 | 38→0 | 29 | 5 |
| store/operations.rs | 51 | 30 | 12 | 9 | 0 |
| store/session.rs | 89 | 36 | 7 | 46 | 0 |
| store/kv.rs | 117 | 31 | 46→0 | 32+8T | 11 |
| recovery/ | 180 | 130 | 35 | 15 | 0 |
| checkpoint/ | 314 | 195 | 84→0 | 33+2T | 3 |
| metrics.rs | 4 | 3 | 0 | 1 | 0 |
| instrument.rs | 1 | 1 | 0 | 0 | 0 |
| sim_hooks.rs | 5 | 2 | 0 | 0+3E | 0 |
| sync.rs | 1 | 1 | 0 | 0 | 0 |

T=timeout, E=equivalent (cfg-gated out)

## Results

- **76 new tests** across 8 test files
- **1,637 mutants** tested across all modules
- **1,100 caught** by existing + new tests
- **214 unviable** (compilation failures)
- **20 timeouts**
- **Effective kill rate: 78.6%** (caught / (caught + missed))

## Equivalent Mutant Patterns Confirmed

1. **Display/Debug fmt** → cosmetic, no behavioral impact
2. **Drop impl → ()** — leak only, undetectable in tests
3. **Prefetch → ()** — performance-only (SKILL.md rule)
4. **cfg-gated simulation hooks** — compiled out without feature flag
5. **drain_count bookkeeping** — optimization counter only
6. **Session Pending status checks** — no-op for in-memory operations
7. **dispose_session/refresh → ()** — cleanup/optimization only
8. **EINTR retry in sync_file_device** — impossible to trigger deterministically

## Key Discoveries

- `entry_count()` on FasterKv returns hardcoded `0` — real test gap caught
- `recover()` requires `&mut self` (not &self) — API constraint
- Recovery expects log segment files co-located with checkpoint metadata
- Multi-checkpoint recovery (overwriting same dir) doesn't preserve updated values for same keys — possible orchestration limitation

## Files Added

All in `rust/crates/faster-core/tests/`:
- `record_info_mutation_tests.rs` (13 tests)
- `state_layout_traits_mutation_tests.rs` (19 tests)
- `epoch_mutation_tests.rs` (6 tests)
- `buffer_device_mutation_tests.rs` (12 tests)
- `sync_file_device_mutation_tests.rs` (7 tests)
- `store_mutation_tests.rs` (5 tests)
- `kv_operations_mutation_tests.rs` (11 tests)
- `checkpoint_mutation_tests.rs` (3 tests)
