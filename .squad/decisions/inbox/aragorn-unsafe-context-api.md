# Decision: UnsafeContext Epoch-Amortized Batch API

**Agent:** Aragorn (Rust Expert)
**Date:** 2026-07-17
**Status:** IMPLEMENTED
**Impact:** Public API addition — affects all crate consumers

## Decision

Add `UnsafeContext<'a, F>` as an epoch-amortized batch API. Users create it via `FasterKv::unsafe_context(&mut session)`, perform multiple operations without per-op epoch enter/exit, and call `refresh()` periodically.

## Rationale

Per-operation epoch protect/unprotect/try_drain costs ~124ns — 43% of a 285ns upsert and >80% of a read or RMW. The `UnsafeContext` pattern (matching C# FASTER's `UnsafeContext`) holds epoch protection across batches, eliminating this overhead.

## Benchmark Evidence

| Operation | Per-op | Batch | Speedup |
|-----------|--------|-------|---------|
| Upsert | 3.56 Mops/s | 10.07 Mops/s | 2.83× |
| Read | 4.85 Mops/s | 47.33 Mops/s | 9.76× |
| Mixed 50/50 | 3.37 Mops/s | 13.55 Mops/s | 4.03× |
| RMW | 4.83 Mops/s | 47.45 Mops/s | 9.83× |

## API Shape

```rust
let mut ctx = store.unsafe_context(&mut session);
for i in 0..batch_size {
    ctx.upsert(&store, &key, &value, context);
    if i % 64 == 0 { ctx.refresh(); }
}
drop(ctx); // releases epoch protection
```

## Implications

- **All agents:** The per-operation API remains unchanged; this is additive.
- **FFI (Sam):** Will need a C FFI wrapper for `unsafe_context` / batch ops.
- **Async (Elrond):** Async adapter should support batch mode via UnsafeContext.
- **Testing (Éowyn):** Batch API needs fuzz testing for refresh interval edge cases.
- **Safety (Galadriel):** No new `unsafe` code — all safety is through Rust's type system.

## Key Design Choices

1. **Methods on `UnsafeContext` take `&FasterKv<F>`** — avoids storing a store reference in the context, keeping lifetimes simple.
2. **`FasterKv` fields `hash_index`, `allocator`, `functions` changed to `pub(crate)`** — needed for session.rs batch methods.
3. **`dispatch_pending_io` changed to `pub(crate)`** — same reason.
4. **Refresh calls try_drain() explicitly** — allows epoch advancement without leaving protection.
