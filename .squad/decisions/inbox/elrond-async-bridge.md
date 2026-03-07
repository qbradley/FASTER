# Decision: Callback→Future Bridge Pattern

**Agent:** Elrond (Tokio/Async Expert)
**Date:** 2026-03-06
**Status:** IMPLEMENTED
**Impact:** Foundation for all async FASTER operations

## Decision

Implement the callback→Future bridge using `Arc<Mutex<SharedState<T>>>` shared between a `PendingFuture<T>` and a `CompletionSender<T>`, using only `std` types.

## Rationale

1. **Runtime-agnostic:** Uses only `std::future::Future` and `std::task::Waker` — no Tokio, no channels, no runtime-specific types. Any executor can drive these futures.
2. **Minimal allocation:** Single `Arc<Mutex<SharedState>>` per pending operation. No boxing of futures, no channel overhead.
3. **Cancel-safe by construction:** If the future is dropped, `CompletionSender::complete()` silently discards the result. No panics, no leaks.
4. **`MaybePending<T>` for zero-cost sync path:** When FASTER operations complete synchronously (hot path — in-memory reads), `MaybePending::Ready(T)` avoids the `Arc<Mutex>` allocation entirely.

## Alternatives Considered

- **tokio::sync::oneshot:** Adds Tokio dependency to the bridge; violates runtime-agnostic requirement.
- **futures::channel::oneshot:** External dependency for a 30-line abstraction; unnecessary.
- **Custom lock-free channel:** Over-engineering for a single-producer-single-consumer one-shot pattern.
- **Mutex-free approach (AtomicPtr + UnsafeCell):** Possible but `Mutex` contention is zero (only two participants, at-most-once wake). Not worth the unsafe.

## Implications

- T2 (AsyncSession) wraps `Session` operations: sync result → `MaybePending::Ready`, pending → `MaybePending::Pending` with sender given to callback.
- T3/T4 (TokioFileDevice) will need `tokio` as a real dependency, but the bridge layer remains runtime-agnostic.
- Future performance optimization: if profiling shows `Mutex` overhead matters, can swap to lock-free with same public API.

## Files

- `rust/crates/faster-tokio/src/bridge.rs` — Implementation + 9 unit tests + 4 doc-tests
- `rust/crates/faster-tokio/src/lib.rs` — Module declaration
- `rust/crates/faster-tokio/Cargo.toml` — tokio in dev-dependencies only
