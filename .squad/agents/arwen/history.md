# Project Context

- **Owner:** qbradley
- **Project:** Rust implementation of Microsoft FASTER — a high-performance durable hash map
- **Stack:** Rust (primary), C++ (reference), C# (reference), C FFI
- **Goal:** Production-grade, no async runtime required, seamless Tokio integration, idiomatic Rust API + C FFI interface. Quality bar: mission-critical cloud services at planetary scale.
- **Created:** 2026-03-05

## Learnings

<!-- Append new learnings below. Each entry is something lasting about the project. -->

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

