# Decision: FFI Opaque Handle Design

**Agent:** Chirrut (Systems Programming Expert)
**Date:** 2026-07-25
**Status:** Implemented (Wave 3, F1)
**Impact:** FFI boundary design — affects all C/language bindings

## Decision

Use a `RwLock<HashMap<u64, Box<dyn Any + Send + Sync>>>` handle table with monotonically increasing `AtomicU64` handles and callback-based access (`with`/`with_mut` closures).

## Rationale

1. **Monotonic handles (never reuse)** — Prevents ABA bugs from C callers caching stale handles. The 64-bit counter won't wrap in any realistic workload (would take 584 years at 1 billion handles/sec).
2. **`RwLock<HashMap>` over lock-free** — FFI calls are session-setup/teardown level, not hot-path. The simplicity of `RwLock` eliminates entire classes of lock-free bugs while allowing concurrent read access.
3. **Callback-based `with`/`with_mut`** — The `RwLock` read-guard must be held while the reference is live. A closure API scopes guard lifetime without leaking lock internals to callers. Alternative (returning a guard wrapper) would complicate the FFI layer.
4. **Poisoned-lock recovery** — At the FFI boundary, unwinding through C frames is UB. We use `unwrap_or_else(|e| e.into_inner())` to recover poisoned locks.
5. **Type-mismatch preserves entries** — `remove<WrongType>()` returns `None` without destroying the stored object. The type-check + remove is atomic under the write lock.
6. **`Relaxed` ordering on handle counter** — Only uniqueness is required (monotonic increment). The `RwLock` write-lock provides the happens-before for HashMap visibility.

## Implications

- F2-F5 will use `HandleTable::insert` to wrap `FasterKv`, sessions, etc. behind opaque `u64` handles.
- The `with`/`with_mut` closure pattern should be used consistently across all FFI functions that access stored objects.
- `FasterStatus` discriminant values are ABI-stable — new variants may be added but existing values must never change.
- The `faster-ffi` crate already has `cdylib` + `staticlib` crate types configured for shared/static library output.

## Alternatives Considered

- **Slab allocator with generation counters** — More complex, slight advantage for use-after-free detection, but monotonic handles already prevent ABA and the generation counter adds no benefit when handles are never reused.
- **Lock-free concurrent HashMap (dashmap, etc.)** — Overkill for FFI call frequency. Adds a dependency and complexity for no measurable gain.
- **Returning raw pointers directly** — Unsafe, no type safety, no thread safety, invites UB from C callers.
