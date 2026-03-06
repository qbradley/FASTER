# Project Context

- **Owner:** qbradley
- **Project:** Rust implementation of Microsoft FASTER — a high-performance durable hash map
- **Stack:** Rust (primary), C++ (reference), C# (reference), C FFI
- **Goal:** Production-grade, no async runtime required, seamless Tokio integration, idiomatic Rust API + C FFI interface. Quality bar: mission-critical cloud services at planetary scale.
- **Created:** 2026-03-05

## Learnings

<!-- Append new learnings below. Each entry is something lasting about the project. -->

---

## 2026-03-05T19:15: Comprehensive Unsafe Audit Completed

**What:** Systematic security audit of all 90 unsafe sites in `rust/crates/faster-core/src/`. Categorized each into Eliminable (3%), Abstractable (24%), or Necessary (72%). Identified 44 missing SAFETY comments (49% gap).

**Artifact Location:** `.squad/decisions/inbox/maul-unsafe-audit.md` (49KB detailed report)

**Key Findings:**
- **Total surface:** 71 unsafe blocks, 10 unsafe fns, 9 unsafe impls
- **Highest-risk modules:** `allocator.rs` (28 sites, lock-free Treiber stack), `page.rs` (21 sites, raw frame manipulation), `record_ops.rs` (16 sites, pointer-based accessors)
- **Critical gaps:** 49% of unsafe code lacks SAFETY justifications (violation of SF-7 best practices)
- **ABA risk:** Allocator's 16-bit tag in Treiber stack could overflow under extreme churn; mitigated by epoch deferral but not runtime-enforced
- **FFI callbacks:** Device completion callbacks assume context pointer validity with no runtime validation

**Category Breakdown:**
1. **Category A (Eliminable, 3 sites):** Checkpoint writer casts `HashBucket` to byte array — replace with `bytemuck::bytes_of`; allocator init dereferences raw pointer — refactor to safe accessor
2. **Category B (Abstractable, 22 sites):** `PageFrame::as_mut_slice`/`::zero` could take `&mut self`; `RecordAccessor::new` could have safe factory; manual Send/Sync impls could be eliminated via Arc wrapper
3. **Category C (Necessary, 65 sites):** FFI boundaries, custom allocators, lock-free CAS loops, pointer arithmetic for records — intrinsically require unsafe

**Critical Recommendations (Phase 0 Blockers):**
1. Add SAFETY comments to all 44 missing sites (Mando responsibility)
2. Run Miri on allocator + epoch drain tests (Jyn responsibility)
3. Fix 3 Category A eliminable sites (30 min effort)

**Non-Blocking Improvements (Phase 1+):**
- Refactor 22 Category B sites to reduce unsafe surface
- Add debug assertions to FFI callbacks for null context checks
- Document ABA tag overflow risk in allocator header
- Consider phantom lifetime for `RecordAccessor` to tie to epoch guard

**What This Means For You:**
- **Mando:** No new unsafe code without SAFETY comments. Fix 3 Category A sites and add 44 missing comments before Phase 1 feature work.
- **Jyn:** Run Miri stress tests on allocator before Mando depends on it. Focus on concurrent alloc/free churn (Treiber stack ABA scenarios).
- **Cassian:** Phase 0 must include unsafe documentation sprint (1-2 days). Block Phase 1 until Miri passes and comments are complete.
- **Chirrut:** Add debug assertions to device callbacks (null context checks). Review FFI boundary safety contracts.
- **Kenobi:** Consider lifetime-parameterized record accessors for Phase 2 API stabilization (optional for Phase 1).
- **Thrawn:** Audit validates your architecture's unsafe usage is appropriate but under-documented. 72% necessary aligns with FASTER's performance goals.

**Risk Profile:** Overall verdict: unsafe usage is **appropriate but under-documented**. No memory safety bugs detected. Lock-free algorithms follow standard patterns. Main risk is ABA tag overflow if epoch integration breaks (currently mitigated but not compile-time enforced).

**Next Steps:** Phase 0 cleanup sprint (SAFETY comments + Category A fixes + Miri validation) before any Phase 1 implementation work begins.

---

## 2026-03-05T18:33: Thrawn Rust FASTER Architecture Finalized

**What:** Thrawn completed 288 KB comprehensive Rust FASTER architecture specification (6101 lines, 14 sections). 3-part parallel document (Thrawn-A/B/C) due to massive context requirements.

**Artifact Location:** `.squad/agents/thrawn/rust-faster-architecture.md`

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
- **Kenobi:** Async adapter design bridges callback core to Future/async/await ecosystem (Decision #3 path).
- **Mando:** Core code stays fully sync, zero async runtime deps. No tokio in core.
- **Chirrut:** Device trait is callback-based (Box<dyn FnOnce>), not Future-based.
- **Jyn:** Simulation framework must model callback completion ordering for testing.
- **Maul:** Unsafe audit scope: hash index atomics (AcqRel/Acquire), record pointer arithmetic, FFI boundary.
- **Cassian:** Phase roadmap and implementation sequence encoded in Part 3.
- **Grievous:** Your async/await recommendation overridden by user directive; async ergonomics achieved via adapter layer.

**Risk Profile:** 25 identified risks (5 critical ≥15, 8 high 10-14). Primary mitigations: deterministic simulation, Miri verification, security audit, cross-implementation comparison.

**Next Steps:** Risk mitigation task force (Jyn lead), unsafe audit planning (Maul lead), Phase 1 sprint (Cassian lead), async adapter spike (Kenobi lead).


---

## 2026-03-05T20:XX: Phase 4 — SAFETY Documentation Sweep (No-Op)

**What:** Tasked with adding SAFETY comments to all unsafe blocks missing them (~44 sites from audit). Upon systematic review, found that **all SAFETY comments are already in place** across every file in scope. Additionally, `lib.rs` already enforces three critical lints:

- `#![deny(unsafe_op_in_unsafe_fn)]` — forces explicit `unsafe {}` blocks inside unsafe fn bodies
- `#![forbid(clippy::undocumented_unsafe_blocks)]` — compilation fails if any unsafe block lacks a `// SAFETY:` comment
- `#![warn(missing_docs)]` — warns on undocumented public items

**Files Audited (no changes needed):**
- `allocator.rs` — 18 unsafe blocks, 3 unsafe fns, 3 unsafe impls — all documented
- `buffer_pool.rs` — 4 unsafe blocks, 2 unsafe impls — all documented
- `checkpoint/index_writer.rs` — 2 unsafe blocks — all documented
- `epoch/drain.rs` — 8 unsafe blocks, 2 unsafe fns, 2 unsafe impls — all documented
- `hash/index.rs` — 1 unsafe block — documented
- `hash/table.rs` — 2 unsafe blocks — all documented
- `hybrid_log/log_allocator.rs` — 3 unsafe blocks — all documented
- `hybrid_log/page.rs` — 15 unsafe blocks, 2 unsafe fns, 4 unsafe impls — all documented
- `hybrid_log/record_ops.rs` — 10 unsafe blocks, 2 unsafe fns — all documented
- `hybrid_log/scan.rs` — 1 unsafe block — documented
- `recovery/index_recovery.rs` — 1 unsafe block — documented
- `store/functions.rs` — 6 unsafe blocks, 4 unsafe fns — all documented
- `store/operations.rs` — 6 unsafe blocks — all documented
- `store/kv.rs` — 2 unsafe impls — all documented

**Skipped (Mando's territory):** `device.rs`, `sync_file_device.rs`, `flush.rs`, `pending_io.rs`

**SAFETY comments added/fixed: 0** — prior work (likely Phase 2/3) already completed this.
**Tests:** 1161 passed, 3 skipped. **Clippy:** clean (0 warnings).

**Key Insight:** The `#![forbid(clippy::undocumented_unsafe_blocks)]` lint makes this task self-enforcing — the codebase cannot compile with undocumented unsafe blocks. This is the strongest possible guarantee. The 44-site gap identified in the original audit has been fully closed.
