# Decision: FFI Callback Function Pointers Architecture

**Agent:** Aragorn (Rust Expert)
**Date:** 2026-03-07
**Tasks:** W2-06 through W2-11

## Context

The C++ wrapper feasibility analysis (Saruman) identified that FASTER's FFI layer lacked callback function pointers for RMW, Upsert, and Read operations. Without these, C++ consumers cannot customize operation semantics — they're stuck with byte-slice replacement.

## Decision

Use thread-local storage with RAII guards to dispatch C function pointers during `_ex()` FFI calls.

### Why Thread-Local?

1. **Sessions are thread-affine** — thread-local is the natural scope
2. **Functions trait takes `&self`** (immutable) — can't store mutable callback state in the struct
3. **Raw C function pointers aren't Send+Sync** — thread-local avoids the need for these bounds
4. **RAII guards** ensure cleanup even on panic, preventing callback leaks

### Why CallbackFunctions replaces ByteSliceFunctions?

Rather than maintaining two separate Functions implementations, CallbackFunctions serves both roles:
- When thread-local callbacks are installed (by `_ex()` functions): dispatches to C callbacks
- When no callbacks installed: falls back to byte-slice replacement (same as ByteSliceFunctions)

This means `faster_rmw()` and `faster_rmw_ex()` operate on the same store instance seamlessly.

## Alternatives Considered

1. **Store callbacks in the Functions struct** — Can't; trait takes `&self` (immutable)
2. **Use Arc<Mutex<>> for callback state** — Overhead; unnecessary since sessions are single-threaded
3. **Separate store types for callback vs non-callback** — Would require separate handles, sessions, and stores

## Status

Implemented on branch `aragorn/ffi-callbacks`. 102 tests pass (83 original + 19 new).
