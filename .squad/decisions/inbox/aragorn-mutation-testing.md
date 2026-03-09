# Decision: Mutation Testing Configuration (cargo-mutants)

**Author:** Aragorn (Rust Expert)
**Date:** 2025-07-18
**Branch:** `aragorn/mutation-testing`
**Status:** Implemented — ready for review

## Summary

Added `cargo-mutants` v27.0 configuration and documentation for the FASTER Rust
project. Ran pilot on `address.rs` achieving 100% mutation catch rate (55/55
viable mutants caught, 0 timeouts).

## Key Decisions

| Decision | Rationale |
|----------|-----------|
| `timeout_multiplier = 3.0` | 3x baseline (~41s) gives ~120s headroom; eliminates prior timeout issues |
| `test_tool = "nextest"` | Matches CI runner; parallel test execution |
| `exclude_re` for bitwise field packing | `page_index`, `offset_in_page`, `from_page_offset` mutations cause infinite loops in unsafe code |
| `exclude_re` for Display/Debug impls | Surviving mutants are cosmetic, not bugs — noise reduction |
| `examine_globs` for Phase 1 critical modules | Focus on high-value code; full-crate runs deferred to periodic audits |
| Quality target: 80% all / 85% critical | Per team decision; pilot exceeded at 100% |

## Files Changed

- `rust/mutants.toml` — cargo-mutants configuration (already existed, unchanged)
- `rust/docs/mutation-testing.md` — NEW: workflow guide with pilot results
- `rust/.gitignore` — Added `mutants.out/` exclusion

## Team Impact

- **All agents**: Can run `cargo mutants --package faster-core -F 'myfile.rs'` to check mutation coverage on changed files.
- **CI (future)**: Full-scope run as pre-merge gate when runtime is acceptable (< 30 min target).
