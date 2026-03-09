# Decision: Mutation Testing Strategy for FASTER Rust

**Author:** Aragorn (Rust Expert)
**Date:** 2026-03-06
**Status:** Proposed
**Impact:** Testing quality, release readiness

---

## Summary

Use `cargo-mutants` (v27.0.0) for mutation testing across the FASTER Rust workspace. Run a 2–3 week phased campaign before public release to identify test suite gaps, targeting ≥80% kill rate overall and ≥85% for critical modules (store, compaction, hash).

## Key Decision Points

### 1. Tool: `cargo-mutants` ✅

- Only viable option. mutagen is unmaintained and requires nightly + source annotations.
- Works with our stable toolchain, edition 2024, MSRV 1.85.0.
- Zero source modifications required.
- Installed and validated: v27.0.0 runs correctly on our workspace.

### 2. Scope: 3,307 mutants (excluding samples)

- 4,454 total workspace mutants; 1,147 are in sample crates (skip).
- faster-core alone has 2,598 mutants across 10 modules.
- Prioritize by risk: store (416) → compaction (200) → hash (250) → hybrid_log (369) → epoch (72).

### 3. Unsafe Code Policy: Accept Timeouts as "Tested"

- Mutating arithmetic/bitwise ops inside unsafe blocks creates UB → segfaults/hangs → timeouts.
- Observed: 8/53 mutations in address.rs timed out (all unsafe-adjacent bitwise ops).
- These are NOT test gaps. Classify timeouts in unsafe code as acceptable.
- Atomic ordering correctness must be verified separately (loom/Miri, not mutation testing).

### 4. Kill Rate Target: 80% overall, 85% critical modules

- 90%+ is diminishing returns for a systems crate with significant unsafe code.
- Equivalent mutants (Display, logging, capacity hints) inflate survivors without indicating real gaps.
- Focus effort on zero missed mutants in safety-critical paths.

### 5. CI Integration: Weekly + Per-PR (Advisory Only)

- Weekly full run sharded across 4 CI workers (~30–60 min wall-clock).
- Per-PR: `--in-diff` on changed files only (~5–15 min).
- Advisory, not blocking. Don't gate merges on mutation testing.

### 6. Timeline: 2–3 Weeks Pre-Release

- ~30–50 hours total compute time across all phases.
- Human effort: ~50% running/triaging, ~50% writing targeted tests.

## Decisions Needed from Team

1. **Approve timeline:** Can we allocate 2–3 weeks before release for this campaign?
2. **CI budget:** Do we want weekly mutation testing in CI? Requires 16-core runners.
3. **Kill rate target:** Is 80%/85% the right bar, or should we aim higher for a v1.0?
4. **Ownership:** Should this be Aragorn-led or distributed across the team?

## Full Plan

See `.squad/plans/mutation-testing-plan.md` for the complete plan with phased execution, operational playbook, and experimental results.
