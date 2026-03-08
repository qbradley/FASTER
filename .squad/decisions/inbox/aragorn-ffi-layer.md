# Decision: C FFI Layer — Thread-Affinity & Missing Functions

**Agent:** Aragorn (Rust Expert)
**Date:** 2026-03-07
**Status:** Implemented

## Context

The FFI layer (`faster-ffi`) had `unsafe impl Send + Sync` on `SessionCell` 
with no runtime enforcement — the safety contract was purely documentary 
(C caller must not cross threads). Three FFI functions were also missing 
from the spec.

## Decision

### Thread-Affinity (FFI-02)
Record the creating thread's `ThreadId` in `SessionCell` and check it on 
every CRUD operation via `with_store_session()`. Return `ThreadMismatch = 105` 
on violation instead of silently invoking UB.

**Trade-off:** One `std::thread::current().id()` call per FFI operation 
(~5ns overhead). This is negligible compared to the epoch refresh and I/O 
that follows.

### Missing Functions
- `faster_destroy()` — alias for `faster_close()` (C API convention)
- `faster_session_refresh()` — epoch refresh + pending completion
- `faster_wait_for_all_pending()` — synchronous drain of all pending I/O

### Unwrap Audit (Part 3)
Audited `store/functions.rs` — all unwraps are in `#[cfg(test)]` code only. 
No production unwraps found. Audit finding was a false positive.

## Alternatives Considered

1. **Compile-time enforcement only:** Not possible across FFI boundary; 
   C has no type system to prevent cross-thread handle passing.
2. **`thread_local!` session storage:** Would change the API contract 
   (sessions become implicit). Rejected for compatibility with C patterns.
3. **`log` crate for thread-mismatch warnings:** Considered logging instead 
   of returning an error. Rejected because silent UB is worse than a 
   deterministic error code.

## Impact

- `FasterStatus` enum gains `ThreadMismatch = 105` (ABI addition, not breaking)
- `faster.h` header regenerated with new functions and enum value
- 83 tests pass (13 new), clippy clean with `--all-targets -D warnings`

## Follow-Up

- Config handle API (`faster_config_*`) not yet implemented — store works 
  with `FasterKvConfig::default()`. Can be added when C callers need 
  custom configuration.
