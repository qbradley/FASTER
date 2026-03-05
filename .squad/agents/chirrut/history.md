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

## 2026-03-06: Task 1d — Hash Utility Traits Completed (Chirrut)

**What:** Implemented `KeyHash`, `Hashable` trait, and FASTER hash functions in `rust/crates/faster-core/src/hash.rs`. Added hash benchmarks in `benches/core_benchmarks.rs`.

**Key design decisions:**
- Ported the exact C++ `FasterHash` algorithm (magic constant 40343, rotr64) for behavioral compatibility. Two variants: `faster_hash_u64` for integer keys (rotr 43) and `faster_hash_bytes` for byte slices (rotr 6).
- 14-bit tag extracted from bits 48..61 of the hash, matching architecture spec §3.1.5 and C++ `HotLogIndexBucketEntryDef::kTagBits = 14`.
- Bucket index via `hash & (table_size - 1)`, power-of-two table size enforced by debug_assert.
- No unsafe code. No external hash dependencies (ahash deferred behind feature flag per plan).
- Blanket `Hashable` impls for: `u32`, `u64`, `i32`, `i64`, `&[u8]`, `[u8; N]`, `&str`, `String`.

**Quality:** 28 hash-specific unit tests + 6 property tests (proptest) + chi-squared distribution test (1M keys). All 64 crate tests pass. Clippy clean.

**Artifacts:**
- `rust/crates/faster-core/src/hash.rs` — full implementation
- `rust/crates/faster-core/benches/core_benchmarks.rs` — 6 criterion benchmarks (u64 hash, byte hash 12B/128B, Hashable u64/str, tag extraction)

---

## 2026-03-05T19:20: Wave 1 Sprint Complete — 1d Done

**Context:** All 4 Wave 1 tasks executed in parallel by full team.

**Your Deliverable (1d):**
- Task 1d: Hash traits + FasterHash algorithm + quality validation — 64 tests passing

**Decision Merged:**
- Decision #12: Hash Function and Tag Layout — C++ FasterHash algorithm, 14-bit tags (bits 48..61), chi-squared validated uniform distribution

**Orchestration Log:** `.squad/orchestration-log/2026-03-05T1920-chirrut.md`

**What's Next:**
- Task 2a (Hash Bucket Entries) — ready to start, depends on 1c/1d ✅
- 2a will directly use `KeyHash`, `tag_from_hash`, `bucket_index` from your hash module
- Your second-pass focus: hash bucket entry layout, tag collision handling, probe sequence

**Team Status:**
- Mando (1b/1c): Address + status/error complete, 127 tests passing
- Rex (1e): CI skeleton complete, 5-job GitHub Actions pipeline validated locally
- All 127 tests passing in workspace. Zero regressions.

