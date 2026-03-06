# Project Context

- **Owner:** qbradley
- **Project:** Rust implementation of Microsoft FASTER — a high-performance durable hash map
- **Stack:** Rust (primary), C++ (reference), C# (reference), C FFI
- **Goal:** Production-grade, no async runtime required, seamless Tokio integration, idiomatic Rust API + C FFI interface. Quality bar: mission-critical cloud services at planetary scale.
- **Created:** 2026-03-05

## Learnings

<!-- Append new learnings below. Each entry is something lasting about the project. -->

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

## 2026-03-06: AI-4 Performance Analysis — Epoch Amortization Deep Dive

**What:** Comprehensive profiling of FASTER Rust to investigate the 10M ops/sec target gap. Built custom benchmark (`perf_analysis.rs`) and collected detailed component cost breakdown.

**Key Findings:**
- Per-op epoch protect/unprotect costs **~97–128 ns** (33–44% of upsert cost)
- Root cause: `compute_safe_epoch()` scans all 256 MAX_THREADS entries with Acquire loads on every unprotect (16KB cache scan)
- `UnsafeContext` (epoch amortization) already exists and provides **1.8× upsert speedup, 3.5× read speedup**
- L1 dcache miss rate: 43% (memory-bound workload, IPC only 0.73)
- Hash index cache misses are the dominant remaining bottleneck after epoch amortization (~80ns per lookup)

**Performance Numbers (local 20-CPU machine):**
- Single-thread amortized read: **12.7M ops/sec** (exceeds 10M target)
- Single-thread amortized upsert: **6.1–6.6M ops/sec**
- 4-thread amortized upsert: **10.0M total ops/sec** (achieves 10M target)
- 10M single-thread upsert requires hash prefetching (not achievable with current hash index)

**Artifacts:**
- Custom benchmark: `rust/crates/faster-core/benches/perf_analysis.rs`
- Analysis report: `.squad/decisions/inbox/ahsoka-perf-analysis-ai4.md`

**Architecture Insights:**
- `UnsafeContext` in `store/session.rs:593` is the fast-path API (matches C# `UnsafeContext`)
- `compute_safe_epoch()` in `epoch/table.rs:320` is the hot function in the epoch path
- Refresh sweet spot: every 64–128 operations
- Multi-thread scaling capped at ~54% efficiency due to hash index + log tail contention

**Optimization Priority Order:**
1. Promote `UnsafeContext` as primary API (+80–250%, Low effort)
2. Bound epoch scan to registered threads only (+5–10%, Low effort)
3. Hash index prefetching (+20–40%, Medium effort)
4. Per-thread log tail allocation (+5–10%, Medium effort)
5. Hash table layout optimization (+10–15%, High effort)

