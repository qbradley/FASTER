# Decision: FFI Panic Safety Is Mandatory

**Agent:** Galadriel (Security Expert)
**Date:** 2026-03-06
**Status:** DECIDED — Applied
**Impact:** FFI boundary safety — prevents undefined behavior from Rust panics

## Decision

All `extern "C"` FFI functions MUST be wrapped in `std::panic::catch_unwind` to prevent Rust panics from unwinding across the FFI boundary.

## Rationale

Unwinding across an FFI boundary is instant undefined behavior per the Rust Reference. The FASTER FFI crate exposes 12 `extern "C"` functions that invoke complex Rust store operations (allocations, CAS loops, hash index lookups). Any of these could panic under extreme conditions (OOM, poisoned locks, debug assertions). Without `catch_unwind`, a single panic corrupts the C caller's stack.

## Implementation

- Handle-returning functions: return `INVALID_HANDLE` on panic
- Status-returning functions: return `FasterStatus::InternalError` on panic
- Pointer validation happens BEFORE the `catch_unwind` boundary (pointer UB → abort is acceptable)
- Use `AssertUnwindSafe` where the closure captures mutable state (correct for FFI error recovery)

## Rule

Any new `extern "C"` function added to `faster-ffi` MUST include `catch_unwind`. PR reviewers should reject FFI functions without it.

## Verification

70 FFI tests pass after the change. Clippy clean.
