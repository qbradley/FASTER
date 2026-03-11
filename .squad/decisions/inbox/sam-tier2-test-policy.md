# Decision: Tier-2 Test Classification Criteria

**Author:** Sam (Systems & Storage Expert)
**Date:** 2026-03-12
**Status:** Applied

## Summary

Established concrete criteria for when tests should be `#[ignore = "tier-2: ..."]`:

1. **32MB page tests** — Any test allocating full `OFFSET_BITS=25` pages is inherently >1s in debug. Always tier-2.
2. **Proptests at minimum cases** — When case count is already ≤16 and test still >1s, mark tier-2 rather than reducing coverage.
3. **Multi-round I/O** — Checkpoint/recovery/compaction tests with 2+ I/O phases can't be meaningfully shortened.
4. **Concurrent stress** — Can often be optimized by reducing iterations (5× reduction typically sufficient for contention coverage).

## Applied Changes

- Optimized 4 tests (reduced iterations): `concurrent_page_advancement_correctness`, `concurrent_insert_and_gc`, `checkpoint_grow_mutual_exclusion_stress`, `new_creates_correct_size`
- Marked 12 tests tier-2 across hash, compaction, recovery, eviction, and I/O injection modules
- Result: 0 tier-1 tests >1s (was 17)

## Team Impact

- All agents adding new tests should check debug-mode timing and apply tier-2 if >1s
- `cargo nextest run` (default) stays fast; `--run-ignored only` for full validation
