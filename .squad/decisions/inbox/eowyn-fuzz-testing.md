# Decision: Fuzz Testing Infrastructure Design

**Agent:** Éowyn (Deterministic Simulation Testing Expert)  
**Date:** 2026-03-08  
**Status:** IMPLEMENTED  
**Impact:** Testing infrastructure — affects CI pipeline and quality assurance

## Decision

Implemented cargo-fuzz testing framework at `rust/fuzz/` with 5 fuzz targets, a CI runner script, and documentation.

## Key Design Choices

### 1. Standalone Workspace for Fuzz Crate

The fuzz crate declares its own `[workspace]` in `Cargo.toml` rather than relying on `exclude = ["fuzz"]` in the parent workspace. This provides build isolation and avoids conflicts with concurrent workspace modifications.

### 2. Fuzz Target Selection

Five high-value attack surfaces were selected:
- **Record parsing** — catches deserialization panics on malformed input
- **Record layout** — validates arithmetic safety in size/offset computations  
- **Compaction record size** — ensures graceful corruption handling (SF-8 related)
- **Hash functions** — verifies no-panic guarantee for all Hashable types
- **Store operations** — random CRUD sequences catch stateful bugs

### 3. String Deserialization Excluded from Arbitrary Fuzzing

`String::deserialize` panics by design on invalid UTF-8. The fuzz target validates UTF-8 before calling it, focusing fuzzer effort on discovering *unexpected* panics rather than exercising known-panic paths.

### 4. Store Ops Resource Limits

The store operations target constrains hash table size (4-14 log2) and operation count (≤2048) to avoid fuzzer timeouts while still achieving meaningful coverage.

## Implications

- **CI Integration:** Add `./scripts/fuzz` to CI pipeline with appropriate duration (60s default for fast CI, 300s+ for nightly)
- **New Targets:** Anyone can add targets by creating `fuzz_targets/fuzz_<name>.rs` and registering in Cargo.toml
- **Nightly Required:** Fuzz builds require nightly Rust toolchain
- **No Precheckin Impact:** Fuzz crate is fully excluded from `./scripts/precheckin`
