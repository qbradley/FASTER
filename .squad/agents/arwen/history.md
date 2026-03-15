# Project Context

- **Owner:** qbradley
- **Project:** Rust implementation of Microsoft FASTER — a high-performance durable hash map
- **Stack:** Rust (primary), C++ (reference), C# (reference), C FFI
- **Goal:** Production-grade, no async runtime required, seamless Tokio integration, idiomatic Rust API + C FFI interface. Quality bar: mission-critical cloud services at planetary scale.
- **Created:** 2026-03-05

## Learnings

<!-- Append new learnings below. Each entry is something lasting about the project. -->

## 2026-03-10: CHANGELOG, Release Notes Template, Documentation Audit (Wave 3)

**What:** Created three deliverables for the 0.1.0 pre-release quality plan:

1. **rust/CHANGELOG.md** — Keep a Changelog format covering every crate's features for the initial 0.1.0 release. Organized by crate with detailed entries for core engine (FasterKv, hash index, hybrid log, epoch, checkpoint/recovery, compaction, grow, batch ops), device layer, io_uring, Tokio integration, FFI bindings, DST framework, benchmarks, and 7 sample apps.

2. **rust/docs/release-notes-template.md** — Template with sections: Highlights, Breaking Changes, New Features (by crate), Bug Fixes, Performance (with table), Migration Guide, Known Issues, Contributors. Includes fill-it-out instructions.

3. **rust/docs/documentation-audit.md** — Rated project 7.5/10. Core API well-covered (~697 doc examples, 291 lines of //! docs across 6 crates). Main gaps: 5 sample crates missing READMEs (P0), faster-device lib.rs too sparse (P0), faster-tokio and faster-uring need usage examples (P1). Full priority table with effort estimates.

**Branch:** `arwen/changelog-docs-audit` (from `squad`)

**Caveats with checkin script:** `rust/scripts/checkin` uses `git add -A` which stages ALL workspace changes, not just the files you intend. For docs-only commits that need selective staging, commit directly with `git commit` instead.

**Documentation inventory discovered:**
- 7 top-level markdown docs (README, QUICKSTART, TESTING, TESTING-ARCHITECTURE, PERFORMANCE, SECURITY-AUDIT, FUZZING, CONTRIBUTING)
- 2 docs/ files (benchmarking.md, mutation-testing.md)
- 4 crate READMEs (faster-core, faster-dst, tokio-kv-server, uring-stress)
- Missing READMEs: faster-device, faster-ffi, faster-tokio, faster-uring, faster-bench, and 5 sample crates

---

## 2026-03-07: Iteration 4 Blog Post Written (blog-iteration-4.md)

**What:** Wrote comprehensive blog post covering Iteration 4 — the production push. Covers compaction (K1–K6), C FFI (F1–F5), async/Tokio bridge (T1–T4), io_uring device (U1–U4), DST framework, the alignment bug story, test audit, quality gate evolution, and the squad rename from Star Wars to Lord of the Rings.

**Blog style patterns established across 4 posts:**
- Title formula: compelling stat + count + hook (e.g., "1,525 Tests, 6 Crates, and a Vec<u8> That Humbled Us All")
- Opens with callback to previous blog's closing teaser
- Chronological storytelling with technical depth — real code snippets, not pseudocode
- "By The Numbers" table comparing metrics across iterations
- "What Went Wrong (The Honest Part)" section — credibility through transparency
- "Lessons for Squad Users" section — actionable takeaways
- Fun coda at the end (team rename, cultural moments)
- Closing boilerplate links to previous posts and project

**Key narrative decisions:**
- The alignment bug is the emotional center of the post — it's the story of what "production-grade" means
- Code snippets chosen for illumination, not exhaustiveness: CAS pointer swing, Waker bridge, TreiberStack, SimulatedDevice
- Variable-length test gap (0 → 27) is the most important number in the metrics table
- DST positioned as "production crash simulator" — emphasizing reproducibility via seeds

**Architecture patterns learned:**
- 6 crate structure: faster-core, faster-ffi, faster-tokio, faster-uring, faster-dst, read-cache-sim
- Compaction orchestrator manages epoch lifecycle internally (K1–K3 under protection, K4 outside)
- FFI uses HandleTable with monotonic u64 handles, poisoned-lock recovery, #[repr(C)] enums
- Async bridge: PendingFuture/CompletionSender with Waker registration — zero tokio deps in core
- io_uring: dedicated I/O thread with mpsc dispatch, TreiberStack buffer pool, EMA adaptive batching
- DST: SimulatedDevice with seed-controlled fault injection, SimulationHarness for crash-recovery tests

**File paths:**
- Blog: `.squad/agents/arwen/blog-iteration-4.md`
- Previous blogs: `blog-behind-the-scenes.md`, `blog-iteration-2.md`, `blog-iteration-3.md`
- Iteration plan: `.squad/plans/iteration-4.md`

---

## 2026-03-06: Quality Gate & Doc Conventions Documented (AI-2, AI-3, AI-6)

**What:** Created `rust/CONTRIBUTING.md` with quality gate (nextest + doctests + clippy + fmt), doc example rules (public API only, no internal test types), and documentation timing convention (inline doc-comments with code, user-facing docs after stabilization). Updated `rust/README.md` to reference the contributing guide.

**Key insight:** `cargo nextest` does not run doctests — an explicit `cargo test --doc` step is essential. Without it, broken doc examples go unnoticed.

**Existing docs:** `rust/TESTING.md` already covers test categories, time budgets, and tooling in depth. CONTRIBUTING.md complements it with the contributor-facing quality gate and conventions.

---

## 2026-03-05T18:33: Gandalf Rust FASTER Architecture Finalized

**What:** Gandalf completed 288 KB comprehensive Rust FASTER architecture specification (6101 lines, 14 sections). 3-part parallel document (Gandalf-A/B/C) due to massive context requirements.

**Artifact Location:** `.squad/agents/gandalf/rust-faster-architecture.md`

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

## 2026-03-14: Sample Crate READMEs Completed (Backlog Item 3e)

**What:** Wrote comprehensive READMEs for all 8 sample crates missing documentation:
1. **cross-impl-bench** — YCSB benchmark suite for cross-implementation comparison
2. **disk-io-bench** — Device implementation benchmark (sync, io_uring, Tokio)
3. **event-counter-tokio** — Async event aggregation with Tokio integration
4. **page-cache** — Lossy cache workload with configurable eviction
5. **page-store** — Durable store with explicit delete-based retention
6. **read-cache-sim** — Fixed working set cache (sync version)
7. **read-cache-sim-tokio** — Fixed working set cache (async version)
8. **torture-stress** — Correctness torture-test with oracle verification

**README Structure (established pattern):**
- **Title** — crate name
- **One-line description** — what it demonstrates
- **What It Demonstrates** — 2–3 sentences explaining purpose
- **Key Concepts** — bullet list of FASTER features shown
- **Usage** — cargo run examples with common flags
- **CLI Options** — reference table of all command-line flags
- **Example Output** — realistic sample output/results
- **Notes** — platform requirements, caveats, use cases

**Style References:**
- Adapted from existing `tokio-kv-server/README.md` and `uring-stress/README.md`
- Tone: Clear, direct, 10-minute decision-maker focus
- Code examples: Realistic, copy-paste ready

**Key Content Decisions:**
- **cross-impl-bench:** Emphasized YCSB standardization and cross-platform comparison
- **disk-io-bench:** Explained memory budgeting calculation (why dataset > buffer matters)
- **event-counter-tokio:** Showed async/sync bridge pattern with ASCII architecture diagram
- **page-cache vs. page-store:** Clearly contrasted lossy eviction vs. durable retention
- **read-cache-sim/-tokio:** Included side-by-side comparison table
- **torture-stress:** Detailed thread roles, wave functions, oracle mechanics with ASCII art

**Branch:** `arwen/sample-readmes` (push to origin succeeded)
**Commit:** 1880da1a — 8 files, 1052 insertions

**Total Documentation Added:** ~30 KB (1052 lines)
- Best for: New users deciding which sample to study first
- Solves: Documentation audit gap (5 samples missing READMEs identified in Wave 3)

