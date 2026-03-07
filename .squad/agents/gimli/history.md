# Project Context

- **Owner:** qbradley
- **Project:** Rust implementation of Microsoft FASTER — a high-performance durable hash map
- **Stack:** Rust (primary), C++ (reference), C# (reference), C FFI
- **Goal:** Production-grade, no async runtime required, seamless Tokio integration, idiomatic Rust API + C FFI interface. Quality bar: mission-critical cloud services at planetary scale.
- **Created:** 2026-03-05

## Learnings

<!-- Append new learnings below. Each entry is something lasting about the project. -->

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

## 2026-03-07: Wave 5 U1 — faster-uring Crate (io_uring Ring Wrapper)

**What:** Created `rust/crates/faster-uring/` — a safe Rust wrapper around Linux io_uring for high-performance async I/O.

**Key Design Decisions:**
- All unsafe io_uring syscalls encapsulated inside `Ring`; public API is fully safe
- Completion-based model (submit → reap) aligns with FASTER's callback architecture (Decision #2)
- `Drop` impl drains in-flight I/O to prevent kernel use-after-free of caller buffers
- Buffer lifetime is caller's responsibility in U1; documented for U2 registered-buffer upgrade
- `io-uring` crate v0.7.11 used (well-maintained, safe Rust bindings)
- Linux-only via `#[cfg(target_os = "linux")]` on the module

**Crash Analysis:**
- Write completion alone does NOT mean durable — fsync required
- Fsync completion guarantees all prior writes to that fd are on stable storage
- Drop during in-flight I/O: drain() blocks until kernel finishes, preventing buffer UAF

**Tests:** 9 tests covering ring creation, read/write round-trip, multi-I/O (10 ops), queue-full backpressure, fsync, clean shutdown with inflight I/O, drain, direct_io config.

**Pre-existing issue noted:** faster-core has an uncommitted compilation error (`MutableRecordAccessor` wrong path in `allocate_with_retry`) from another agent's work. Does not affect faster-uring.

**Next:** U2 will add registered buffer pool for zero-copy I/O. Integration with `Device` trait in faster-device comes in a later wave.

