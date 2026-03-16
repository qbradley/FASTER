# Project Context

- **Owner:** qbradley
- **Project:** Rust implementation of Microsoft FASTER — a high-performance durable hash map
- **Stack:** Rust (primary), C++ (reference), C# (reference), C FFI
- **Goal:** Production-grade, no async runtime required, seamless Tokio integration, idiomatic Rust API + C FFI interface. Quality bar: mission-critical cloud services at planetary scale.
- **Created:** 2026-03-05

## Core Context: Developer Advocate Playbook

Arwen's accumulated knowledge distilled into reusable patterns:

### Documentation Infrastructure
- **Keep a Changelog format** for release notes (crate-organized, detailed entries, feature tables)
- **Release notes template:** Highlights, Breaking Changes, New Features, Bug Fixes, Performance table, Migration Guide, Known Issues
- **Documentation audit scoring:** 60 points possible (core API 15–20, samples 10, README 10, features 10, internals 5–10). Score pre-release to find P0 gaps.
- **Sample README template:** Title, one-liner, What It Demonstrates, Key Concepts, Usage (with cargo run examples), CLI options table, Example Output, Notes, See Also
- **Doctest validation:** `cargo test --doc` must pass in CI. `cargo nextest` does NOT run doctests—explicit step required. Doc examples must be real, tested code.

### Blog Post Methodology (4 posts shipped)
- **Title formula:** [Compelling stat] [Summary], and [Hook]. Example: "1,525 Tests, 6 Crates, and a Vec<u8> That Humbled Us All"
- **Structure:** Callback to previous blog → problem → what we built → why it was hard → By The Numbers table → What Went Wrong → Lessons for Users → fun coda → closing links
- **Code snippets:** Real, tested code only. Choose for illumination (CAS loop, Waker bridge, TreiberStack). 5–10 lines max, include 1-line explanation.
- **Tone:** Technical depth for systems programmers. Specific over vague. Numbers matter. Honest about bugs.

### Developer Experience (DX) Audit
- **First 5 minutes test:** Can new user follow README → quickstart → example → run code without reading source?
- **API discoverability:** Public APIs in rustdoc, self-documenting names, iterator/getter/setter conventions, composable traits
- **Error message quality:** Human-readable, include problem + fix suggestion, panic messages clear
- **Documentation structure:** README (what/why/quick example), Quickstart, API docs, Samples, Architecture guide
- **Adoption friction:** Zero major cloud SDK deps, clear sync/async guidance, documented Cargo features, migration paths, troubleshooting

### Sample Crate Documentation Patterns
- 8 samples documented 2026-03-14: cross-impl-bench, disk-io-bench, event-counter-tokio, page-cache, page-store, read-cache-sim, read-cache-sim-tokio, torture-stress
- Tone: Direct, action-oriented, 10-minute decision-maker focus
- Layout: ASCII architecture diagrams for async samples; realistic CLI examples with actual output
- Common antipatterns: Never explain FASTER internals in sample README (link to docs instead); avoid pseudocode; each README must stand alone

## Learnings (Detailed History)

<!-- Append new learnings below. Each entry is something lasting about the project. -->

## 2026-03-10: Wave 3 Release Infrastructure (CHANGELOG, Release Notes Template, Documentation Audit)

**Deliverables:** (1) `rust/CHANGELOG.md` — Keep a Changelog format, crate-organized for 0.1.0. (2) `rust/docs/release-notes-template.md` — Reusable template. (3) `rust/docs/documentation-audit.md` — Rated 7.5/10, found ~697 doc examples, identified P0 gaps (5 sample READMEs missing, sparse crate docs).

**Key caveat:** `rust/scripts/checkin` stages ALL workspace changes. For docs-only commits, use `git commit` directly.

**Documentation inventory:** 7 top-level markdown docs, 2 in docs/, 4 crate READMEs. Missing: 5 sample crate READMEs (remediated 2026-03-14).

---

## 2026-03-07: Iteration 4 Blog Post (blog-iteration-4.md)

**Coverage:** Compaction, C FFI, Tokio async bridge, io_uring, DST framework, alignment bug story, test audit evolution.

**Architecture learned:** 6-crate structure (core, ffi, tokio, uring, dst, read-cache-sim), epoch lifecycle (K1–K3 protected, K4 outside), FFI HandleTable with poisoned-lock recovery, Waker bridge (zero tokio deps in core), io_uring thread pool with TreiberStack buffer pool, SimulatedDevice with seed-controlled fault injection.

---

## 2026-03-06: Quality Gate & Doc Conventions (rust/CONTRIBUTING.md)

**Key insight:** `cargo nextest` does NOT run doctests. Explicit `cargo test --doc` step required in CI. Established convention: inline docs during development, user-facing docs after API stabilization.

---

## 2026-03-05: Architecture Finalized (Gandalf)

Gandalf delivered 288 KB comprehensive architecture spec (6101 lines, 14 sections) covering all crates, concurrency, storage, checkpoint, FFI, async integration, testing, and 25 identified risks.

**Sections:** Vision, Crates, Data Structures, Concurrency, Storage, Operations, Checkpoint, API, FFI, Async Integration, Testing, Phases, 12 Key Decisions, Risk Register (25 risks, 5 critical)

**12 Core Architectural Decisions:**
1. No async/await in core (user directive) — callback/completion model
2. Custom epoch (not crossbeam-epoch) — FASTER-specific semantics
3. Inline variable-length records (C++ model)
4. Rust atomic patterns for hash index — lock-free
5. 32 MB default page size — 512-byte sector alignment
6. Completion-based Device trait — runtime-agnostic
7. Opaque FFI handles (~15 function API surface)
8. Own checkpoint format — forward-compatible binary
9. Result<Status, Error> taxonomy with bitflag Status
10. Thread-affine sessions (!Send compile-time) — mono-threaded safety
11. Unified Key/Value traits for fixed+variable-length
12. Epoch-coordinated grow protocol (doubles, no shrink)

**What This Means For You:**
- **All:** You are now operating under this architecture. Decisions binding for implementation phases.
- **Elrond:** Async adapter design bridges callback core to Future/async/await ecosystem (Decision #3 path).
- **Aragorn:** Core code stays fully sync, zero async runtime deps. No tokio in core.
- **Sam:** Device trait is callback-based (Box<dyn FnOnce>), not Future-based.
- **Éowyn:** Simulation framework must model callback completion ordering for testing.
- **Galadriel:** Unsafe audit scope: hash index atomics (AcqRel/Acquire), record pointer arithmetic, FFI boundary.
- **Frodo:** Phase roadmap and implementation sequence encoded in Part 3.
- **Saruman:** Your async/await recommendation overridden by user directive; async ergonomics achieved via adapter layer.

**Risk Profile:** 25 identified risks (5 critical ≥15, 8 high 10-14). Primary mitigations: deterministic simulation, Miri verification, security audit, cross-implementation comparison.

**Next Steps:** Risk mitigation task force (Éowyn lead), unsafe audit planning (Galadriel lead), Phase 1 sprint (Frodo lead), async adapter spike (Elrond lead).


---

## 2026-03-09T1846Z: CHANGELOG, Release Notes Template, Documentation Audit (Wave 3 Release Infrastructure)

**Branch:** `arwen/changelog-docs-audit` (from `squad`, 1 commit, fast-forward merged)
**Agent ID:** agent-147

**Deliverables:**
1. `rust/CHANGELOG.md` — Keep a Changelog format. Covers all 7 crates for the initial 0.1.0 release. Organized by crate: core engine (FasterKv, hash index, hybrid log, epoch, checkpoint/recovery, compaction, grow, batch ops), device layer (SyncFileDevice, NullDevice, Device trait), io_uring, Tokio integration, FFI bindings, DST framework, benchmarks, and 7 sample apps.

2. `rust/docs/release-notes-template.md` — Reusable release notes template with sections: Highlights, Breaking Changes, New Features (by crate), Bug Fixes, Performance (table), Migration Guide, Known Issues, Contributors. Includes fill-it-out instructions for the release manager.

3. `rust/docs/documentation-audit.md` — Rated project 7.5/10. Core API well-covered (~697 doc examples, 291 lines of //! docs across 6 crates). Gap table:
   - **P0:** 5 sample crate READMEs missing (`read-cache-sim`, `kv-server`, `uring-stress`, `tokio-kv-server`, `bench`), `faster-device/src/lib.rs` too sparse
   - **P1:** `faster-tokio` and `faster-uring` need usage examples in crate-level docs
   - **P2:** Public unsafe code needs more safety docs

**Test suite:** 2,067 passed, 11 skipped, 67s (post-merge, no regressions)

**Wave 3 team context:**
- **Gandalf (agent-148):** Added semver-checks to CI — new public API on publishable crates will be flagged on PRs.
- **Legolas (agent-149):** `bench-record-baseline.sh`, `bench-release-compare.sh`, `bench-ci-smoke.sh` now available for release benchmarking.
- **Boromir (agent-150):** `rust/scripts/release-gate` with `--tier 1` is a superset of `precheckin`; use it as a pre-commit check.

---

## 2026-03-14: Sample Crate READMEs (8 crates, 1052 lines, ~30 KB)

**Completed:** cross-impl-bench, disk-io-bench, event-counter-tokio, page-cache, page-store, read-cache-sim, read-cache-sim-tokio, torture-stress.

**Pattern used:** Title, one-liner, What It Demonstrates (2–3 sentences), Key Concepts (bullet list), Usage (cargo run examples), CLI Options (reference table), Example Output (realistic run), Notes (platform requirements, caveats), architecture diagram where helpful.

**Key design choices:** YCSB emphasis for cross-impl-bench, memory budgeting explanation for disk-io-bench, ASCII async architecture for event-counter-tokio, side-by-side comparison for sync/tokio variants, detailed oracle mechanics for torture-stress.

**Branch:** `arwen/sample-readmes` (Commit 1880da1a). Solves Wave 3 documentation audit gap.


