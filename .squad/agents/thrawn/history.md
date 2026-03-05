# Project Context

- **Owner:** qbradley
- **Project:** Rust implementation of Microsoft FASTER — a high-performance durable hash map
- **Stack:** Rust (primary), C++ (reference), C# (reference), C FFI
- **Goal:** Production-grade, no async runtime required, seamless Tokio integration, idiomatic Rust API + C FFI interface. Quality bar: mission-critical cloud services at planetary scale.
- **Created:** 2026-03-05

## Learnings

<!-- Append new learnings below. Each entry is something lasting about the project. -->

### 2026-03-05: Architecture Part 3 — Verification, Phasing, Decisions, Risks

**What:** Authored Sections 11-14 of the Rust FASTER architecture document (Part 3 of 3).
- Section 11: Testing Strategy — 7-layer approach (unit, integration, property-based, deterministic simulation, benchmarks, correctness verification, fuzzing). CI pipeline tiered: commit (<5min), PR (<15min), nightly (<4hr), weekly.
- Section 12: Implementation Phases — 10 phases from Foundation to Release. Critical path ~28-37 weeks. Team assignments for all 13 agents. Phase dependency graph with parallelism opportunities.
- Section 13: Key Decisions & Rationale — 12 binding architectural decisions with full alternatives analysis and trade-off documentation. Key: no async in core (user directive), custom epoch, inline varlen records, completion-based Device trait.
- Section 14: Risk Register — 25 risks across 5 categories (correctness, performance, complexity, compatibility, scope). 5 critical risks identified; primary mitigation is deterministic simulation testing.

**Key insight:** The callback/completion model (Decision 1, user directive) is the most architecturally consequential decision — it shapes the Device trait, async adapter pattern, pending operation flow, and testing strategy. Everything flows from this constraint.

**Decisions written to:** `.squad/decisions/inbox/thrawn-rust-architecture.md`
**Artifact:** `.squad/agents/thrawn/architecture-part3.md` (84 KB, 1532 lines)

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

