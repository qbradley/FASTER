# Project Context

- **Owner:** qbradley
- **Project:** Rust implementation of Microsoft FASTER — a high-performance durable hash map
- **Stack:** Rust (primary), C++ (reference), C# (reference), C FFI
- **Goal:** Production-grade, no async runtime required, seamless Tokio integration, idiomatic Rust API + C FFI interface. Quality bar: mission-critical cloud services at planetary scale.
- **Created:** 2026-03-05

## Learnings

<!-- Append new learnings below. Each entry is something lasting about the project. -->

### 2026-03-05: MVP Iteration 1 Plan — Bottom-Up Foundation + Hash Index

**What:** Created detailed bottom-up implementation plan for Phases 1 (Foundation) and 2 (Hash Index), decomposed into 16 concrete work items with dependency graphs, assignments, success criteria, and PAW candidacy assessment.

**Key structural insight:** Bottom-up ordering revealed the true dependency chain: workspace → newtypes/status/hash-traits (parallel) → record format → epoch + allocator (parallel) → hash entry → bucket → hash table → concurrent ops. Maximum parallelism = 4 work streams in Phase 1, 2 in early Phase 2.

**PAW candidates identified:** 5 items (1f Record Format, 1g Epoch, 1h Allocator, 2d Hash Table, 2e Concurrent Ops) — all contain unsafe code, complex concurrency, or both. Simple type definitions (newtypes, enums, thin wrappers) are better as direct implementation.

**Critical dependency chain:** The epoch system (1g) and memory allocator (1h) are the two longest-pole items in Phase 1 at ~1–1.5 weeks each. They can run in parallel (Aragorn on epoch, Sam on allocator) but both block Phase 2's hash table.

**Risk insight:** The tentative-bit CAS protocol in the hash index (2d/2e) is the most subtle concurrency algorithm in Iteration 1. Model-based property testing (shadow HashMap comparison) is the primary correctness verification strategy — not just stress testing.

**Artifact:** `.squad/agents/gandalf/mvp-iteration-1-plan.md`

### 2026-03-05: Architecture Part 3 — Verification, Phasing, Decisions, Risks

**What:** Authored Sections 11-14 of the Rust FASTER architecture document (Part 3 of 3).
- Section 11: Testing Strategy — 7-layer approach (unit, integration, property-based, deterministic simulation, benchmarks, correctness verification, fuzzing). CI pipeline tiered: commit (<5min), PR (<15min), nightly (<4hr), weekly.
- Section 12: Implementation Phases — 10 phases from Foundation to Release. Critical path ~28-37 weeks. Team assignments for all 13 agents. Phase dependency graph with parallelism opportunities.
- Section 13: Key Decisions & Rationale — 12 binding architectural decisions with full alternatives analysis and trade-off documentation. Key: no async in core (user directive), custom epoch, inline varlen records, completion-based Device trait.
- Section 14: Risk Register — 25 risks across 5 categories (correctness, performance, complexity, compatibility, scope). 5 critical risks identified; primary mitigation is deterministic simulation testing.

**Key insight:** The callback/completion model (Decision 1, user directive) is the most architecturally consequential decision — it shapes the Device trait, async adapter pattern, pending operation flow, and testing strategy. Everything flows from this constraint.

**Decisions written to:** `.squad/decisions/inbox/gandalf-rust-architecture.md`
**Artifact:** `.squad/agents/gandalf/architecture-part3.md` (84 KB, 1532 lines)

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

### 2026-03-05: Iteration 3 Retrospective — Facilitated Full Squad Retro

**What:** Facilitated retrospective covering Iterations 1–3 on the `squad` branch. 76 Rust commits, 50K+ lines, 1,280 tests (1,276 passing, 4 failing doctests, 8 ignored).

**Key findings:**
1. **Doctests are a blind spot.** 4 builder.rs doctests reference `TestFunctions` (internal type) instead of `SimpleFunctions` (public API). Went undetected because our test flow runs `cargo test --lib` + integration tests, not `cargo test --doc`. Quality gate must include all test types.
2. **Documentation-before-stabilization causes rework.** QUICKSTART.md was written during Iteration 3, then had to be patched (`6b94ce3c`) to be "honest about pending I/O semantics" after write-path pending was implemented. Lesson: batch docs as a trailing phase.
3. **Sleep-based test synchronization creates fragile, slow tests.** `write_pending_completion` tests took 18.5s before 20× speedup. Convention needed: no `thread::sleep` in tests without justification.
4. **Performance target gap.** 8.11M ops/sec vs 10M target — shared infrastructure may explain ~20% gap but needs dedicated-hardware validation.
5. **Unsafe footprint (23 files, ~191 occurrences) needs SAFETY documentation audit** before Iteration 4 adds io_uring and C FFI.

**Action items written to:** `.squad/decisions/inbox/gandalf-retrospective-actions.md` (7 action items, 3 decision proposals)

### 2026-03-06: Wave 0 — Foundation & Hardening (H1+H2+H3)

**What:** Implemented all three Wave 0 hardening items for Iteration 4 in a single commit.

**H1 — Deferred TODOs resolved:**
- `allocate_with_retry` method added to `FasterKv` — flushes sealed pages on allocation failure then retries once, preventing silent data loss in write-pending completion.
- `SyncFileDevice` alignment checks promoted from `debug_assert` to runtime error returns in all four I/O methods (`read_async`, `write_async`, `read_sync`, `write_sync`).
- `flush_completion_callback` now validates `bytes_transferred >= expected` before transitioning to Flushed — short writes stay in Flushing for retry.
- `DrainList::claim_chain` pre-allocates `Vec::with_capacity(16)` to reduce per-drain allocation overhead.
- Three items documented as intentionally deferred: `entry_count` (contention concern), SF-3 merge-on-upsert (future feature), SF-9 page alias check (perf concern).
- 6 new alignment validation tests for `SyncFileDevice`.

**H2 — ARM memory ordering audit:**
- Audited all ~60 `Ordering::Relaxed` sites. All are advisory counters (metrics, live_entry_count, high_water) — safe on ARM.
- All synchronization-critical paths already use Acquire/Release/AcqRel correctly.
- No `#[cfg(target_arch = "aarch64")]` fences needed — documented rationale in `sync.rs`.
- `cargo check --target aarch64-unknown-linux-gnu` passes clean.

**H3 — Metrics & observability:**
- Added 4 new counters: `pending_io_inflight`, `checkpoint_count`, `total_operations`, `flush_count`.
- Created `metrics_inc!` macro — compiles to nothing when `metrics` feature is disabled.
- Added `trace_span!` to 5 key paths: `flush_page`, `flush_sealed_pages`, `take_checkpoint`, `read_completion`, `store_flush`.
- Wired counters into all 4 CRUD operations, flush, checkpoint, and pending I/O dispatch.
- Added `FasterKv::metrics()` accessor (feature-gated).
- Verified zero overhead: compiles identically with features off.

**Key decision:** ARM doesn't need conditional fences because Rust's `Ordering` enum provides sufficient abstraction — the compiler emits correct barrier instructions per-target. `Relaxed` on advisory-only data is safe even on weakly-ordered architectures.

**Test results:** All 1173 tests pass. Clippy clean. Fmt clean. Cross-compile for aarch64 passes.

### 2026-03-06: A8 — Hash Table Layout Investigation (Open Addressing Prototype)

**What:** Investigated whether open addressing with inline data could close the ~14% Workload C read gap vs C#.

**Critical finding:** The hypothesis was wrong. C# and Rust FASTER use **identical** hash table architectures — 64-byte multi-slot buckets with 8-byte logical address entries. Neither stores keys or values inline. The gap lives in the record access path, not the hash index.

**Deliverables:**
- Analysis document: `.squad/agents/gandalf/hash-table-layout-analysis.md`
- Benchmark prototype: `rust/crates/faster-core/benches/hash_layout_bench.rs`
- Decision: `.squad/decisions/inbox/gandalf-hash-layout.md`

**Architectural insight:** True open addressing is fundamentally incompatible with FASTER's hybrid log. Records must be addressable by logical address so they can flow through mutable → read-only → disk tiers without touching the hash index. Inlining data would break grow, compaction, and the entire tiered storage model.

**Next steps:** Profile the record access path (logical address → physical pointer → key comparison → value) to find the actual bottleneck.

