# Project Context

- **Owner:** qbradley
- **Project:** Rust implementation of Microsoft FASTER — a high-performance durable hash map
- **Stack:** Rust (primary), C++ (reference), C# (reference), C FFI
- **Goal:** Production-grade, no async runtime required, seamless Tokio integration, idiomatic Rust API + C FFI interface. Quality bar: mission-critical cloud services at planetary scale.
- **Created:** 2026-03-05

## Core Context

- **Role:** Architect, merge coordinator, hardening lead. Owns cross-cutting concerns: merge consolidation, hash table layout decisions, foundation hardening, retrospectives.
- **Merge strategy:** Feature branches per agent → consolidation merges to main integration branch. Resolve conflicts by understanding both sides' intent. See `.squad/skills/merge-consolidation/` for full process.
- **Hash table layout:** Cache-line aligned buckets (64 bytes). 7 entries per bucket + overflow pointer. Tag bits in entry for fast rejection. Prefetch-friendly linear probe within bucket.
- **EPVS coordination:** Epoch Protection Version Scheme spans multiple modules (state, checkpoint, grow). Two-phase intermediate CAS prevents ABA. Version bumps only Prepare→InProgress.
- **Foundation patterns:** `#[deny(unsafe_code)]` on safe modules. Unsafe confined to FFI, atomics, and allocator. All public API types implement Send+Sync where safe. Error types are non-exhaustive for forward compat.
- **Quality gates:** See `.squad/skills/quality-gate-verification/` for 4-tier validation model. Fast gate: `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo nextest run`.
- **Key decisions log:** (1) No async runtime dependency — use `std::thread` + channels. (2) C FFI via `extern "C"` functions, not cbindgen auto-gen. (3) Single-writer session model (no interior mutability on hot path). (4) Epoch-based memory reclamation over hazard pointers.

## Critical Learnings

- **Architecture-first validated:** 12 binding decisions on Day 1 were never revisited. Spec survived 9 parallel agents.
- **Multi-layer verification wins:** 2,102 tests across unit/Miri/Loom/DST/mutation caught bugs no single modality finds.
- **Multi-writer deadlock (P0):** 3-point deadlock in flush/evict pipeline remains structural issue, not tuning.
- **Merge discipline:** Consolidate 5+ branches by dependency order. See merge-consolidation skill.
- **Publishing gotchas:** Path deps need `version = "x.y.z"`, crates.io index syncs in ~30s between publishes.
- **Surgical staging:** `checkin` script runs `git add -A` — stage explicitly when needed.

## Session Log (Compressed)

### Wave 0 Foundation (2026-03-06)
Established workspace layout, CI pipeline (`rust-ci.yml`), coding standards. Key: `rust/` directory structure, `#[deny(unsafe_code)]` default, feature flags for optional deps.

### Iteration 6 Merge (2026-03-06)
Merged 6 feature branches (EPVS implementation, sealed-bit, integration tests, benchmarks, compaction, unsafe context). 14 conflicts across 8 files. Post-merge: 1709 tests passing. See merge-consolidation skill for process.

### Session 2 Retrospective (2026-03-08)
Standardized `git worktree` for agents. Metrics: 8 branches merged, avg 45min merge time, +494 tests.

### A8 Hash Table Layout (2026-03-06)
Validated current 64-byte bucket design against alternatives (128-byte bucket, open addressing, cuckoo). Decision: keep current layout, optimize prefetch pipeline. Rationale documented in `hash-table-layout-analysis.md`.

### TESTING-ARCHITECTURE.md (2026-03-09)
Wrote comprehensive testing strategy (490 lines). Established 4-tier validation model. Inventory: ~1,950 tests across 9 categories. See quality-gate-verification skill.

### Wave 3 Release Automation (2026-03-09)
Branch: `gandalf/release-automation`. Created `release.toml`, `rust-release.yml`, `rust-ci.yml` semver checks, `docs/releasing.md`. Dependency-ordered publish with 30s index sync delays. See rust-release-pipeline skill.

### Wave 3 Integration (2026-03-09)
Integrated Legolas's benchmark baseline infrastructure and Boromir's release gate. Updated releasing.md to reference `bench-record-baseline.sh` and `release-gate` script.
