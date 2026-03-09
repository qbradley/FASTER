# Project Context

- **Owner:** qbradley
- **Project:** Rust implementation of Microsoft FASTER — a high-performance durable hash map
- **Stack:** Rust (primary), C++ (reference), C# (reference), C FFI
- **Goal:** Production-grade, no async runtime required, seamless Tokio integration, idiomatic Rust API + C FFI interface. Quality bar: mission-critical cloud services at planetary scale.
- **Created:** 2026-03-05

## Core Context

- **Role:** Architect, merge coordinator, hardening lead. Owns cross-cutting concerns: merge consolidation, hash table layout decisions, foundation hardening, retrospectives.
- **Merge strategy:** Feature branches per agent → consolidation merges to main integration branch. Resolve conflicts by understanding both sides' intent. Prefer rebase for linear history when possible.
- **Hash table layout:** Cache-line aligned buckets (64 bytes). 7 entries per bucket + overflow pointer. Tag bits in entry for fast rejection. Prefetch-friendly linear probe within bucket.
- **EPVS coordination:** Epoch Protection Version Scheme spans multiple modules (state, checkpoint, grow). Two-phase intermediate CAS prevents ABA. Version bumps only Prepare→InProgress.
- **Foundation patterns:** `#[deny(unsafe_code)]` on safe modules. Unsafe confined to FFI, atomics, and allocator. All public API types implement Send+Sync where safe. Error types are non-exhaustive for forward compat.
- **Quality gates:** `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo nextest run` (no `--all-targets`). All PRs must pass before merge.
- **Key decisions log:** (1) No async runtime dependency — use `std::thread` + channels. (2) C FFI via `extern "C"` functions, not cbindgen auto-gen. (3) Single-writer session model (no interior mutability on hot path). (4) Epoch-based memory reclamation over hazard pointers.

## Learnings
<!-- Append new learnings -->
- Merge consolidation across 5+ agent branches requires careful ordering — merge dependency chains first.
- Hash table bucket size is a critical perf knob — 64-byte cache line alignment is non-negotiable.
- Always verify `cargo nextest run` passes after merge, not just `cargo build`.
- Multi-agent workspace requires explicit coordination protocol for shared files (Cargo.toml, mod.rs).
- `git worktree` is the safest multi-agent pattern — each agent gets isolated working tree.

## Session Log

### Iteration 6 Merge Consolidation (2026-03-06)
Merged 6 feature branches into integration branch: sam/epvs-implementation, sam/sealed-bit-p03, boromir/epvs-integration-tests, legolas/wave3-benchmarks, aragorn/compaction-scanner, aragorn/unsafe-context.

**Conflict resolution:** 14 merge conflicts across 8 files. Primary conflicts in `store/mod.rs` (re-exports), `status.rs` (new variants from multiple branches), `Cargo.toml` (dependency additions).

**Post-merge validation:** 1709 tests passing, clippy clean, fmt clean.

---

### Session 2 Retrospective (2026-03-08)
**Action items:** (1) Standardize on `git worktree` for all agents. (2) Add pre-merge checklist to squad protocol. (3) Prioritize compaction end-to-end over new features. (4) Reduce scope of individual tasks — smaller PRs merge cleaner.

**Metrics:** 8 branches merged, 2 conflict cycles, avg merge time 45min. Test count 1215→1709 (+494).

---

### A8 Hash Table Layout Investigation (2026-03-06)
**Findings:** Current 64-byte bucket with 7 entries + overflow pointer is optimal for L1 cache. Alternatives evaluated: (1) 128-byte bucket (2 cache lines, worse for sparse tables), (2) Open addressing (poor for variable-length keys), (3) Cuckoo hashing (complex resize, no clear win).

**Tag bits:** 8-bit tag per entry from hash MSBs. Reduces false-positive chain walks by ~99%. Already implemented in `hash/bucket.rs`.

**Recommendation:** Keep current layout. Focus optimization on prefetch pipeline (legolas/A4) and per-thread log tail.

---

### Wave 0 Foundation & Hardening (2026-03-06)
Established project scaffolding: workspace layout, CI pipeline, coding standards, module structure.

**Key decisions:** (1) `rust/` directory for all Rust code. (2) `src/` = library crate, `tests/` = integration tests, `benches/` = criterion benchmarks. (3) Feature flags for optional deps (e.g., `tokio`). (4) `#[deny(unsafe_code)]` default, explicit `#[allow(unsafe_code)]` on modules that need it.

**CI:** `cargo fmt --check && cargo clippy -- -D warnings && cargo nextest run`. No `--all-targets` flag.
