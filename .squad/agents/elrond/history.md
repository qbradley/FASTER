# Project Context

- **Owner:** qbradley
- **Project:** Rust implementation of Microsoft FASTER — a high-performance durable hash map
- **Stack:** Rust (primary), C++ (reference), C# (reference), C FFI
- **Goal:** Production-grade, no async runtime required, seamless Tokio integration, idiomatic Rust API + C FFI interface. Quality bar: mission-critical cloud services at planetary scale.
- **Created:** 2026-03-05

## Learnings

<!-- Append new learnings below. Each entry is something lasting about the project. -->

---

## 2026-03-06T20:25: Callback→Future Bridge Implemented (Wave 4 T1)

**What:** Created `faster-tokio/src/bridge.rs` — the foundational async bridge that converts FASTER's completion-callback model into standard Rust Futures.

**Key Types:**
- `PendingFuture<T>` / `CompletionSender<T>` — linked pair via `Arc<Mutex<SharedState<T>>>`
- `MaybePending<T>` — enum for zero-alloc synchronous completions (`Ready(T)`) vs pending I/O (`Pending(PendingFuture<T>)`), implements `IntoFuture`
- `MaybePendingFuture<T>` — the `IntoFuture` output type; `impl Unpin for MaybePendingFuture<T>` is sound because T is never structurally pinned

**Design Decisions:**
- **Runtime-agnostic:** Only `std::future::Future` and `std::task::Waker` — zero Tokio types in the bridge
- **Single allocation:** One `Arc<Mutex<SharedState>>` per pending operation
- **Cancel-safe:** Dropping the future before completion is a no-op for the sender
- **Waker replacement:** Latest waker always wins (correct per Future contract)
- **Abandoned I/O:** Dropping sender without completing leaves future permanently pending (expected)

**Testing:** 9 unit tests + 4 doc-tests, including multi-threaded Tokio integration (100 concurrent futures completed from OS threads), waker replacement, and both drop-safety scenarios.

**Dependencies:** `tokio` is dev-dependency only (for `#[tokio::test]`). The bridge itself compiles with zero external deps beyond `std`.

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

## 2026-03-06: tokio-kv-server Sample Application

**What:** Created `rust/crates/samples/tokio-kv-server/` — a complete async TCP key-value server demonstrating `faster-tokio` integration with Tokio.

**Architecture:**
- `AsyncFasterKv` manages store lifecycle with background maintenance + graceful shutdown
- Each TCP connection spawned via `tokio::spawn` (Send-compatible async task)
- FASTER operations dispatched via `tokio::task::spawn_blocking` with short-lived sessions
- This bridge pattern is necessary because `FasterSession` is `!Send` (thread-affine epoch protection)

**Key Design Decisions:**
- **Session-per-command via `spawn_blocking`:** Simplest correct pattern for interactive use. Production servers would use dedicated worker threads with long-lived sessions via channels.
- **u64 keys/values:** `SimpleFunctions<K,V>` requires `V: Copy`, ruling out `String`/`Vec<u8>`. Custom `Functions` impl needed for variable-length values.
- **Text protocol over TCP:** Line-based SET/GET/DEL/BENCH/STATS/HELP/QUIT. Testable via `nc`/`telnet`.
- **broadcast channel for shutdown:** `tokio::sync::broadcast` propagates Ctrl+C to all connection handlers.

**Test Coverage:** 13 unit tests (command parsing, CRUD logic, bench, stats) + 7 integration tests (full TCP round-trip including multi-client cross-visibility).

**Learned:**
- `FasterKv` handles epoch protection internally in `upsert`/`read`/`delete` — no explicit `begin_unsafe()` guard needed for simple operations.
- `FasterKvConfig` fields are all `pub` — direct struct initialization with `..Default::default()` works.
- The workspace had pre-existing issues (cross-impl-bench missing main.rs, faster-uring clippy/fmt warnings) that block `scripts/precheckin` on the full workspace.


