# Decision: Loom Shim Patterns for Production Code

**Author:** Aragorn  
**Date:** 2026-07-22  
**Status:** Implemented

## Context

The `RUSTFLAGS="--cfg loom" cargo check --features loom` build had 7 compilation errors across the crate. These are systemic gaps in the loom shim layer that anyone touching sync primitives should know about.

## Decisions

### 1. `thread::sleep` under loom → `yield_now()`

Loom doesn't model wall-clock time. The `crate::sync::thread` module now provides a `sleep` shim that calls `loom::thread::yield_now()`. Any code using `crate::sync::thread::sleep` will automatically get the right behavior.

### 2. `self: &Arc<Self>` is incompatible with loom's Arc

Loom's `Arc` doesn't implement `core::ops::Receiver`, so `self: &Arc<Self>` receiver syntax fails. **Pattern:** cfg-gate the method signature and use a shared `_impl` function. Production call sites should use UFCS (`Type::method(&arc)`) which works under both cfgs.

### 3. `AtomicPtr::get_mut()` unavailable under loom

Use `load(Ordering::Relaxed)` as the portable alternative when `&mut self` guarantees exclusive access. Semantically equivalent.

### 4. Loom atomics aren't const-constructible

Any `static` using loom atomics must be wrapped in `std::sync::LazyLock` under `#[cfg(loom)]`. `LazyLock<T>` derefs to `T`, so call sites need no changes.

## Impact

All team members writing production code with sync primitives should be aware of these four patterns. The loom CI gate will catch violations.
