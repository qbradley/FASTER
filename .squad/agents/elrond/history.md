# Project Context

- **Owner:** qbradley
- **Project:** Rust implementation of Microsoft FASTER — a high-performance durable hash map
- **Stack:** Rust (primary), C++ (reference), C# (reference), C FFI
- **Goal:** Production-grade, no async runtime required, seamless Tokio integration, idiomatic Rust API + C FFI interface. Quality bar: mission-critical cloud services at planetary scale.
- **Created:** 2026-03-05

## Core Context

### Architectural Foundation (Gandalf, 2026-03-05)

12 binding decisions govern FASTER-Rust:
- **No async in core:** Sync + threaded design, no async runtime dependency
- **Custom epoch:** Thread-affine sessions (`!Send`) for epoch protection
- **Inline VL records, lock-free hash, 32MB pages**
- **Completion-based Device:** Callback model for I/O
- **Opaque FFI handles, own checkpoint format**
- **Result<Status,Error> API, Key/Value traits, epoch-grow protocol**

Elrond's role: Bridge callback core to Future/async ecosystem (Decision #3 path). Full spec in `.squad/agents/gandalf/rust-faster-architecture.md`.

### Async Integration Patterns

**Crate Structure:**
- `faster-core`: Pure sync + threaded, zero async runtime deps
- `faster-tokio`: Thin async wrapper with `TokioFileDevice` + `AsyncFasterKv`
- Bridge lives in `faster-tokio/src/bridge.rs` using ONLY `std::future` + `std::task`

**Key Types (bridge.rs):**
- `MaybePending<T>`: Zero-alloc `Ready(T)` path + `Pending(PendingFuture<T>)` for I/O
- `PendingFuture<T>` / `CompletionSender<T>`: Arc-linked pair for callback→Future
- Runtime-agnostic: Single allocation, waker replacement, cancel-safe

**Session Integration (!Send):**
- `FasterSession` is thread-affine (epoch protection) → NOT `Send`
- Pattern: `tokio::task::spawn_blocking` with short-lived sessions
- Production: Dedicated worker threads with long-lived sessions + channels
- Call `refresh()` every ~256 ops for long-lived sessions

**Device Compatibility:**
- `TokioFileDevice::new(path, size, max_io, "log.")` — prefix MUST be `"log."` for recovery
- Checkpoint metadata + log segments must be in compatible directories
- Recovery regex expects `log.{n}` file names

**Concurrent RMW:**
- Lock-free but NOT atomic across sessions → partition key space per worker
- Worker N handles keys in range `[N*keys_per_worker, (N+1)*keys_per_worker)`

### Test Coverage

- **Bridge tests:** 9 unit + 4 doc-tests (multi-thread completion, waker replacement, drop-safety)
- **Integration tests:** 29 tests in `faster-tokio/tests/integration.rs` (lifecycle, concurrency, checkpoint/recovery, shutdown, timeouts, RMW, stats)
- **Sample apps:** `tokio-kv-server`, `read-cache-sim-tokio`, `event-counter-tokio`

### Hard-Won Lessons

1. **Concurrent RMW loses updates** without key partitioning (event-counter-tokio)
2. **Recovery needs "log." prefix** — `TokioFileDevice`/`SyncFileDevice` coupling (Boromir decision)
3. **`FasterKv::recover(&mut self)`** requires exclusive access → cannot call through `Arc<FasterKv>`, must recover before wrapping
4. **`operations_completed` counter** tracks serial numbers, NOT in-memory ops
5. **`scripts/checkin` stages ALL files** — commit manually when sharing workspace

## Learnings

<!-- Append new learnings below. Each entry is something lasting about the project. -->
