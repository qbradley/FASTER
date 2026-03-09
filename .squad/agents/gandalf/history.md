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
- For crates.io publishing, path dependencies must include `version = "x.y.z"` alongside `path = "..."` — omitting it causes publish failures.
- The `checkin` script runs `git add -A`, so untracked files from other branches will get swept in. Stage explicitly and use `git commit` directly when surgical staging is needed.
- cargo-release `consolidate-commits = true` is essential for workspace releases — one commit covers all crate version bumps.
- Semver-checks should be `continue-on-error: true` pre-1.0 since minor versions can break APIs per convention.
- Crates.io index takes ~30s to sync after publish — dependent crate publishes must wait or they'll fail resolution.
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

---

### TESTING-ARCHITECTURE.md — Authoritative Testing Strategy (2026-03-09)
**Branch:** `squad` (direct commit)

Wrote `rust/TESTING-ARCHITECTURE.md` — a 490-line comprehensive testing strategy document. Audited all test infrastructure and established the canonical 4-tier validation model for the project.

**4-tier validation model:**
- Fast Gate (<60s, every commit): `cargo nextest run` — unit + integration
- Correctness Gate (<5 min, every PR): + property tests + Miri subset
- Deep Validation (<30 min, nightly): + full Miri + Loom + DST smoke
- Release Gate (<2 hr, pre-release): + fuzz + crash consistency + full mutation

**Complete test inventory:** ~1,950 tests across 9 categories (unit, integration, property, DST, Miri, Loom, fuzz, crash consistency, benchmarks).

**Known gaps documented:** Mutation testing, async test suite, cross-platform CI, benchmark automation.

**Relationship to existing docs:** `TESTING.md` (quick-reference, unchanged), `FUZZING.md` (fuzzing guide, unchanged), `TESTING-ARCHITECTURE.md` (architectural overview, new).

**Cross-agent context:**
- Aragorn resolved the mutation testing gap in this same session — `rust/docs/mutation-testing.md` + `mutants.toml`, 100% catch rate on `address.rs` pilot.
- Legolas fixed `hash_layout_bench` in this same session — the Release Gate tier's benchmark smoke test now runs in 0.15s.

---

### Wave 3 Release Automation (2026-03-09)
**Branch:** `gandalf/release-automation`

Set up complete release automation infrastructure for the Rust workspace.

**Deliverables:**
1. `rust/release.toml` — cargo-release config with dependency-ordered publish (core → device → tokio → uring → ffi), tag format `rust-v{version}`, consolidated commits, pre-release hooks.
2. `.github/workflows/rust-ci.yml` — added semver-checks job (PR-only, `continue-on-error: true` pre-1.0) for all 5 publishable crates.
3. `.github/workflows/rust-release.yml` — full release pipeline triggered by `rust-v*` tags: validate → test (nextest) → semver-checks → publish (dependency order with 30s index sync delays) → GitHub Release. Includes dry-run via `workflow_dispatch`.
4. `rust/docs/releasing.md` — human-readable release guide: version strategy, dry-run, publish, rollback procedures.
5. Cargo.toml updates — removed `publish = false` from 5 publishable crates, added `version` constraints to path deps for crates.io compatibility, added `publish = false` to `faster-dst`.

**Key architectural decisions:**
- Actual dependency graph: `faster-core` (root) → `faster-device`, `faster-tokio`, `faster-uring` → `faster-ffi` (depends on core + device).
- Non-publishable: `faster-bench` (benchmarks), `faster-dst` (test framework), all samples.
- Semver checks are advisory pre-1.0, will become mandatory post-1.0.
- Release tag format `rust-v*` avoids collision with existing `squad-release.yml` (Node.js, triggered on `main` push).

---

### Wave 3 — Benchmark Baseline Infrastructure (2026-03-09T1846Z)
**Branch:** `legolas/bench-baseline-infra` — merged to `squad`
**Agent ID (Legolas):** agent-149

**What this means for you:**
- `rust/scripts/bench-record-baseline.sh` — record named baselines tied to git version before each release.
- `rust/scripts/bench-release-compare.sh` — compare against stored baseline; >5% regression gates release. Exit code 1 on failure.
- `rust/scripts/bench-ci-smoke.sh` — fast CI smoke test for benchmark sanity.
- `rust/baselines/README.md` — explains storage, usage, and release workflow.
- Release process should record a baseline before tagging: `bench-record-baseline.sh v0.1.0`.

**Your `rust/docs/releasing.md` should reference bench-record-baseline.sh in the release checklist.**

### Wave 3 — Release Gate Validation (2026-03-09T1846Z)
**Branch:** `boromir/release-gate` — merged to `squad`
**Agent ID (Boromir):** agent-150

**What this means for you:**
- `rust/scripts/release-gate` integrates all 4 validation tiers into a single script.
- Tier 3 runs `bench-release-compare.sh` — your benchmark baseline work feeds directly into the release gate.
- Release workflow: run `release-gate all` before tagging; use `--report` flag to capture output as release artifact.
- `rust/docs/release-gate.md` is now the companion doc to your `rust/docs/releasing.md`.
