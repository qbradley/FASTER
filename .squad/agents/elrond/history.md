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



---

## 2026-03-07: read-cache-sim-tokio Async Benchmark

**What:** Created `rust/crates/samples/read-cache-sim-tokio/` — an async/Tokio version of the existing sync `read-cache-sim` sample for performance comparison.

**Architecture:**
- `TokioFileDevice` for async I/O dispatch (Tokio's blocking pool instead of dedicated thread pool)
- `AsyncFasterKv` for background maintenance (epoch drain, compaction)
- `tokio::task::spawn_blocking` for workers (since `FasterSession` is `!Send`)
- Control plane as Tokio tasks: `tokio::time::interval` stats, `tokio::signal::ctrl_c()` shutdown, duration timer

**Key Design Decisions:**
- **Workers use spawn_blocking, not async sessions:** `AsyncSession` is `!Send` due to thread-affine epoch protection. Workers run on Tokio's blocking pool with direct `FasterKv` sessions, making data-plane identical to sync version.
- **refresh() instead of maintenance():** Workers call `store.refresh(&mut session)` every 256 ops; `AsyncFasterKv` handles global maintenance in the background.
- **TokioFileDevice takes 4 args:** No `num_threads` param since Tokio manages the thread pool.

**Pre-existing Bug Fixes (to unblock workspace build):**
- `device.rs`: Removed `#[cfg(debug_assertions)]` block with reserved `gen` keyword (Rust 2024)
- `drain.rs`: Fixed extra closing brace, restored `drained` counter
- `table.rs`: Added `scan_limit: CachePadded<AtomicUsize>` field
- `session.rs`: Fixed `ctx.status.is_completed()` → `ctx.is_completed()`
- `faster-ffi`: Added `ThreadMismatch` variant, safety comments on test unsafe blocks
- `disk-io-bench`: Suppressed dead code warnings

**Performance:** Smoke test ~513K ops/s (3-second run, single thread, NullDevice).

---

## 2026-03-07T19:20: Event Counter (Tokio) Sample — Wave 2 Complete

**What:** Created `rust/crates/samples/event-counter-tokio/` — a complete async ad-click aggregator demonstrating FASTER + Tokio integration.

**Files:** Cargo.toml, src/{main,functions,workload,checkpoint,verify}.rs (6 files, ~1400 lines)

**Key Patterns:**
- `spawn_blocking` for workers (FasterSession is !Send)
- Partitioned key space per worker (avoids concurrent RMW race)
- `TokioFileDevice` with "log." prefix for checkpoint-compatible segments
- Data + checkpoint in same directory for recovery compatibility
- Value/FixedSizeValue impl for CampaignStats (24-byte LE)

**Learnings:**
1. Concurrent RMW on the same key loses updates — partition keys among workers
2. Recovery expects `log.{n}` segment names — device prefix must be `"log."`
3. Checkpoint dir must contain the log segments (same as data dir)
4. `git show commit:path > file` can fail silently when file is already open by cargo
5. During merge state, `git add` can reset file contents — always backup to /tmp first

**Commit:** 659b29b1 on squad branch

---

## 2026-03-10: faster-tokio Integration Test Suite

**What:** Created `rust/crates/faster-tokio/tests/integration.rs` — 29 integration tests exercising the async layer end-to-end.

**Test Categories (29 tests, ~1.2s total):**
1. **Full lifecycle** — open → upsert → checkpoint → read → shutdown
2. **Concurrent multi-session** — 8 spawn_blocking tasks, 1600 keys, cross-task visibility
3. **Checkpoint + recovery** — TokioFileDevice write → checkpoint → reopen → recover → verify (200 keys)
4. **Async checkpoint** — AsyncFasterKv.checkpoint() with TokioFileDevice
5. **Mixed sync/async** — sync writes on blocking pool, async reads via AsyncFasterKv; interleaved even/odd keys
6. **Shutdown under load** — 4 writer tasks in flight during shutdown; maintenance loop stops, writers complete
7. **Drop safety** — drop without explicit shutdown, maintenance task aborted cleanly
8. **Error propagation** — NotFound reads/deletes, double-delete, checkpoint to bad path
9. **Timeout behaviour** — tokio::time::timeout on checkpoint, maintenance, shutdown
10. **Bridge futures** — 50 pending futures completed from OS threads + 50 MaybePending::Ready
11. **Session churn** — 50 rapid create/drop cycles testing epoch slot recycling
12. **Large batch** — 10K key upsert/read-back
13. **RMW, overwrite, from_store, multiple sessions, stats**

**Key Learnings:**
- `FasterSession.stats().operations_completed` tracks `serial_number`, NOT operations done through `FasterKv::upsert/read`. Serial is only incremented via `next_serial()`. For in-memory ops dispatched through the store, the counter stays at 0.
- The `scripts/checkin` script stages ALL dirty files (`git add -A`), not just the ones you intend. Always commit manually when the workspace has other agents' unstaged work.
- `FasterKv::recover(&mut self, ...)` takes `&mut self`, so it cannot be called through `AsyncFasterKv` (which wraps `Arc<FasterKv>`). Recovery must be done on a fresh `FasterKv` instance before wrapping.

**Branch:** `elrond/tokio-integration-tests`
