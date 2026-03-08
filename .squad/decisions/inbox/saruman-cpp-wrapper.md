# Decision: C++ Wrapper FFI Surface Uses Self-Contained Declarations

**Author:** Saruman
**Date:** 2026-03-07
**Task:** W3-04 — C++ Wrapper Header over Rust FFI

## Decision

The C++ wrapper header (`faster_cpp.h`) redeclares the entire FFI surface inline instead of including the cbindgen-generated `faster.h`.

## Rationale

cbindgen renders Rust's `Option<extern "C" fn(...)>` as opaque `struct Option_*Fn` forward declarations. These cannot be constructed or passed by value from C/C++ code, making the `_ex()` callback functions uncallable. Since the Rust ABI guarantees `Option<fn>` is layout-identical to a nullable function pointer, we declare the `_ex()` functions with proper pointer types.

This also makes the header self-contained with no dependency on the cbindgen output path.

## Impact

- **Aragorn:** If the FFI surface changes (new functions, changed signatures), both `faster.h` and `faster_cpp.h` must be updated. Consider adding a CI check that verifies ABI compatibility.
- **All:** The cbindgen `Option<fn>` limitation should be tracked — if cbindgen gains proper nullable function pointer support, we could switch back to including `faster.h`.

## Alternatives Considered

1. **Include faster.h + cast tricks** — Brittle, relies on compiler-specific behavior for casting to/from opaque structs.
2. **Modify cbindgen config** — cbindgen doesn't support rendering `Option<fn>` as nullable pointers in C mode.
3. **Wrap _ex() in C shims** — Adds an unnecessary indirection layer.
