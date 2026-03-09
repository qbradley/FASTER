# Decision: Loom Tests Re-implement Algorithms (Not Wrapping Production Types)

**Agent:** Éowyn (Deterministic Simulation Testing Expert)
**Date:** 2026-03-10
**Status:** Implemented
**Impact:** Testing strategy — affects how loom tests are maintained

## Decision

Loom tests re-implement FASTER's concurrent algorithms from scratch using `loom::sync::atomic` primitives, rather than wrapping the production types.

## Rationale

1. Production code uses `std::sync::atomic` directly throughout — the `crate::sync` loom shim exists in `sync.rs` but is not wired into any production types.
2. Wiring loom into production code is a large refactor touching ~25 files with `use std::sync::atomic`.
3. The existing 7 loom tests already use this re-implementation pattern, establishing precedent.
4. Re-implementations faithfully mirror exact bit layouts, orderings, and protocols from production.

## Implications

- **When `crate::sync` is wired in:** All D-series loom tests can be updated to use actual production types, eliminating the re-implementation layer. This is the preferred future state.
- **Maintenance burden:** Any change to production atomic orderings or CAS protocols must be manually reflected in loom tests until the shim is wired in.
- **Coverage gap:** Loom tests verify the *algorithm* but not the *exact production code path*. Bugs introduced by typos in production (e.g., wrong constant value) would not be caught.

## Next Steps

- [ ] Wire `crate::sync` re-exports into production code (replaces `use std::sync::atomic` in ~25 files)
- [ ] Update loom tests to use production types directly once shim is active
