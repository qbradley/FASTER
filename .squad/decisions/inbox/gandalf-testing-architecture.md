# Decision: Testing Architecture Documentation

**Author:** Gandalf
**Date:** 2025
**Status:** Implemented

## Context

The FASTER Rust project has ~1,950 tests across 9 categories (unit, integration, property, DST, Miri, Loom, fuzz, crash consistency, benchmarks) but lacked a single document explaining the overall testing strategy, tiered validation model, and how to navigate the test infrastructure.

## Decision

Created `rust/TESTING-ARCHITECTURE.md` as the definitive testing strategy document with:

1. **4-tier validation architecture** — Fast Gate (< 60s, every commit), Correctness Gate (< 5 min, every PR), Deep Validation (< 30 min, nightly), Release Gate (< 2 hr, pre-release).
2. **Complete test inventory** — Audited all test files with exact counts and LOC.
3. **Testing gaps** — Documented known gaps: mutation testing, async test suite, cross-platform CI, benchmark automation.
4. **Decision tree** — For determining which test category to use for new code.

## Relationship to Existing Docs

- `TESTING.md` remains the quick-reference for test categories and time budgets.
- `FUZZING.md` remains the fuzzing-specific guide.
- `TESTING-ARCHITECTURE.md` is the comprehensive architectural overview that ties everything together.

## Impact

All team members should reference `TESTING-ARCHITECTURE.md` when:
- Adding new test infrastructure
- Deciding which test category to use
- Understanding the CI tier model
- Planning test coverage improvements
