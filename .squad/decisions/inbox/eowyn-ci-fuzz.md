# Decision: CI Fuzz Integration (Iteration 6 A7)

**Agent:** Éowyn (Deterministic Simulation Testing Expert)
**Date:** 2026-03-09
**Status:** DECIDED
**Impact:** CI pipeline — PR validation and nightly testing

## Decision

Fuzz testing is now CI-ready via two scripts (`fuzz-ci`, `fuzz-minimize`) with a checked-in seed corpus and comprehensive documentation.

## Key Choices

### 1. Smoke mode duration: 10 seconds per target

**Rationale:** 10s gives libFuzzer enough time to load the corpus and execute millions of iterations for simple targets (record parsing: ~7M runs, record layout: ~8.5M runs) and hundreds for complex targets (store ops: ~300 runs). Total wall-clock for all 5 targets is ~60s, acceptable for PR validation.

**Alternative considered:** 5s per target was too short for store ops to exercise meaningful code paths.

### 2. Seed corpus tracked in Git

**Rationale:** Reproducibility is paramount for fuzz testing. Checked-in corpus means every CI run starts from the same baseline. Fuzzer-discovered inputs from smoke runs are also included for richer initial coverage.

**Trade-off:** Adds ~100 small binary files to the repo. Corpus can be minimized with `fuzz-minimize` to keep size manageable.

### 3. Artifact directory separate from corpus

**Rationale:** Crash artifacts (`artifacts/`) are runtime-only investigation aids, not reproducible seeds. Keeping them gitignored prevents accidental commit of crash files that may contain sensitive memory contents.

### 4. Scripts support shorthand target names

**Rationale:** `--target hash` is easier to type than `--target fuzz_hash`. The `fuzz_` prefix is auto-prepended when missing. This reduces friction for developers investigating specific targets.

## Implications

- **Frodo:** Wire `./scripts/fuzz-ci --smoke` into PR validation (e.g., Azure Pipelines `pr:` trigger or GitHub Actions `pull_request` event). Wire `FUZZ_DURATION=300 ./scripts/fuzz-ci` into nightly schedule.
- **All agents:** When adding a new fuzz target, update `ALL_TARGETS` in both `fuzz-ci` and `fuzz-minimize`, add corpus seeds, and update `rust/fuzz/README.md`.
- **Galadriel:** Fuzz targets exercise unsafe-adjacent code paths. Any new unsafe code should have a corresponding fuzz target.

## Pre-existing Issues Noted

- `cargo clippy` fails on `faster-ffi` (29 errors: `undocumented_unsafe_blocks`) and `faster-core` (`clone_on_copy`). These pre-date this work and blocked `precheckin` from passing. Only fuzz-related files were committed.
