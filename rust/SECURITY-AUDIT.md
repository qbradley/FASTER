# FASTER Rust — Security Audit Report

**Auditor:** Galadriel (Security Expert)
**Date:** 2026-03-06
**Scope:** All Rust crates — faster-core, faster-ffi, faster-tokio, faster-uring, faster-dst
**Codebase:** ~55,500 lines of Rust across 5 crates
**Unsafe inventory:** 248 unsafe blocks, 37 unsafe fns, 17 unsafe impls (302 total sites)

---

## Executive Summary

**Verdict: CONDITIONAL PASS**

The FASTER Rust implementation demonstrates strong security engineering fundamentals. The codebase enforces `#![forbid(clippy::undocumented_unsafe_blocks)]` — every unsafe block has a SAFETY comment — and `#![deny(unsafe_op_in_unsafe_fn)]` prevents implicit unsafe in unsafe fn bodies. Lock-free algorithms follow standard patterns (Treiber stacks with ABA tags, CAS loops with appropriate orderings). No confirmed memory safety bugs were found during this audit.

**One Critical issue was found and fixed:** All 12 FFI `extern "C"` functions lacked `catch_unwind` protection, meaning any Rust panic would unwind across the FFI boundary — instant undefined behavior. This has been remediated.

**Conditions for production clearance:**
1. ✅ FFI panic safety — FIXED (all functions now protected)
2. ⚠️ Remaining High/Medium findings should be tracked and addressed per schedule below
3. ⚠️ Miri validation of the allocator's lock-free operations should be run before production load
4. ⚠️ Fuzzing targets identified below should be implemented before accepting external input

---

## Findings Summary

| Severity | Count | Status |
|----------|-------|--------|
| **Critical** | 1 | ✅ Fixed |
| **High** | 4 | ⚠️ Open — track for remediation |
| **Medium** | 6 | ⚠️ Open — address before GA |
| **Low** | 5 | ℹ️ Informational |
| **Info** | 3 | ℹ️ Noted |

---

## Per-Crate Findings

### faster-core (101 blocks, 29 fns, 12 impls)

| ID | Location | Severity | Description | Recommendation |
|----|----------|----------|-------------|----------------|
| CORE-01 | `allocator.rs:95-105` | **Medium** | Treiber stack free list uses 16-bit ABA tag (65,536 cycle space). With epoch deferral the tag is sufficient, but if epoch integration is disabled (`free_immediate`), tag wraparound becomes possible under sustained high-throughput free/alloc churn. | Document in `free_immediate` that it is unsafe without epoch protection. Consider widening to 32-bit tag (uses bits 32..63, still fits u64). |
| CORE-02 | `allocator.rs:607` | **Medium** | `free_immediate()` bypasses epoch deferral. SAFETY comment warns about concurrent readers, but the function is `pub` — any internal caller could misuse it. | Restrict visibility to `pub(crate)` and add `debug_assert!` verifying no other threads hold references. |
| CORE-03 | `device.rs:31` | **High** | `IoCompletionCallback` is a raw function pointer type (`unsafe fn(*mut u8, IoStatus, u64)`). Context pointer validity is entirely caller-enforced with no runtime validation. If a completion fires after context deallocation, the callback dereferences freed memory. | Add a debug-mode validation layer: assign unique IDs to pending I/O contexts and verify at completion time. Consider `TypedIoContext<T>` wrapper for type-safe context management. |
| CORE-04 | `store/pending_io.rs` | **High** | `PendingIoContext` stores raw pointers to session state. If the session is disposed while I/O is pending, completion callback dereferences dangling pointer. | Enforce that `dispose_session` drains all pending I/O before releasing session state. Add assertion in debug builds. |
| CORE-05 | `hybrid_log/flush.rs` | **Medium** | Flush callbacks reference the page table via raw pointer. Correct only if `Drop` for `FasterKv` drains all pending flushes before deallocating the page table. | Verify and document drop ordering. Add `debug_assert!` in flush completion that page table is still live. |
| CORE-06 | `store/kv.rs` | **Low** | `unsafe impl Send` and `unsafe impl Sync` on `FasterKv` are sound given the atomic field types, but the safety comment could be more explicit about which fields require it. | Expand SAFETY comment to enumerate the non-auto-Send/Sync fields and why each is safe. |
| CORE-07 | `hybrid_log/page.rs:228` | **Low** | `as_mut_ptr(&self)` returns `*mut u8` from a shared reference. Safe because it's a raw pointer (no aliasing guarantee), but the method name suggests mutability from `&self`. | Consider renaming to `data_ptr()` returning `*mut u8` for clarity, or documenting why `&self` is intentional. |
| CORE-08 | `epoch/drain.rs:218` | **Low** | `pending_count()` reads the free list without serialization. Documented as test-only, but `pub` visibility allows misuse. | Restrict to `#[cfg(test)]` or `pub(crate)`. |

### faster-ffi (78 blocks, 0 fns, 2 impls)

| ID | Location | Severity | Description | Recommendation |
|----|----------|----------|-------------|----------------|
| FFI-01 | `lib.rs:159-737` (all extern "C" fns) | **Critical** | ~~No `catch_unwind` on any FFI function. A Rust panic from store operations would unwind across the C boundary — instant UB.~~ | ✅ **FIXED** — All 12 functions now wrapped in `catch_unwind`. On panic, handle functions return `INVALID_HANDLE`, status functions return `InternalError`. |
| FFI-02 | `session.rs:40-42` | **High** | `SessionCell` implements `Send + Sync` via `unsafe impl` to allow storage in the handle table. The invariant (single-threaded access per session) is enforced only by documentation. A C caller passing a session handle across threads causes a data race. | Add optional runtime thread-ID validation: record creating thread ID on `session_start`, verify on each operation. Compile-time enforcement is impossible across FFI. |
| FFI-03 | `lib.rs:438` | **Medium** | `data.len() as u32` truncation in the read path. If a value exceeds 4GB (theoretical on 64-bit), the truncated length would pass the `data_len > val_buf_len` check but `copy_nonoverlapping` receives the full `usize` length — potential buffer overflow. | Use `usize` comparisons throughout, or add `if data.len() > u32::MAX as usize { return InternalError; }` guard. |
| FFI-04 | `handle.rs:63` | **Low** | Handle counter uses `debug_assert_ne!(handle, 0)` for wraparound detection. In release builds, after 2^64 allocations, handle 0 (INVALID_HANDLE) could be issued. | Replace with runtime check. Practically unreachable but theoretically unsound. |
| FFI-05 | `lib.rs` (all pointer params) | **Info** | Pointer validation pattern `if len > 0 && ptr.is_null()` accepts garbage pointers when `len == 0`. Pointers are not dereferenced in this case, so it's safe but the API contract is ambiguous. | Document explicitly: "When `len == 0`, the pointer value is ignored." |

### faster-tokio (7 blocks, 3 fns, 0 impls)

| ID | Location | Severity | Description | Recommendation |
|----|----------|----------|-------------|----------------|
| TOKIO-01 | `device.rs:357-439` | **High** | `read_async`/`write_async` cast raw pointers to `usize` to send them into `spawn_blocking` closures (working around `Send` bounds on raw pointers). This creates a temporal window: if the caller frees the buffer after submitting I/O but before the Tokio task executes, the reconstructed pointer dereferences freed memory. | This is inherent to the unsafe Device trait contract — the caller guarantees buffer lifetime. Document the temporal assumption explicitly. Consider adding a `debug_assert!` with a generation counter. |
| TOKIO-02 | `device.rs:537-545` | **Low** | Test callback reconstructs `Arc` from raw pointer. If invoked twice (double-completion bug), second call operates on freed memory. | Safe in tests (controlled invocation). Add `debug_assert!` for single-invocation in production callback paths. |

### faster-uring (56 blocks, 3 fns, 3 impls)

| ID | Location | Severity | Description | Recommendation |
|----|----------|----------|-------------|----------------|
| URING-01 | `ring.rs:126,165,296` | **Medium** | io_uring submission holds kernel references to userspace buffers until completion. Buffer lifetime is enforced by documentation only — the type system cannot prevent a caller from dropping a buffer while the kernel holds a reference to it. | Consider a `PhantomData<&'a [u8]>` lifetime tie between submitted buffers and the Ring, or a "pending I/O" guard that borrows the buffer. |
| URING-02 | `device.rs:285` | **Medium** | `unsafe impl Send for IoCommand` — sound if buffer pointers are valid for the I/O thread's lifetime, but relies entirely on the Device trait contract. | SAFETY comment is adequate. No change needed beyond documentation. |
| URING-03 | `device.rs:1073-1119` | **Low** | `read_async`/`write_async` do not validate buffer alignment for O_DIRECT. Misaligned buffers cause kernel `EINVAL`, reported via callback. | Add `debug_assert!` for alignment when `direct_io` is enabled. |
| URING-04 | `buffer.rs:181,186` | **Info** | `Send`/`Sync` for `BufferPool` rely on the Treiber stack for exclusive ownership. 32-bit generation counter provides ABA protection. | Sound as implemented. Generation counter space (4 billion) is sufficient. |

### faster-dst (6 blocks, 2 fns, 0 impls)

| ID | Location | Severity | Description | Recommendation |
|----|----------|----------|-------------|----------------|
| DST-01 | `device.rs:211-301` | **Info** | `SimulatedDevice` relies on the same unsafe Device trait contract as production devices. Since this is a test-only device with controlled callers, the risk is acceptable. | No action needed. This is test infrastructure. |

---

## Unsafe Block Inventory

| Crate | Blocks | Fns | Impls | Total | SAFETY Comments | Lint Enforced |
|-------|--------|-----|-------|-------|-----------------|---------------|
| faster-core | 101 | 29 | 12 | 142 | 142/142 (100%) | `#![forbid(clippy::undocumented_unsafe_blocks)]` |
| faster-ffi | 78 | 0 | 2 | 80 | 80/80 (100%) | `#![forbid(clippy::undocumented_unsafe_blocks)]` |
| faster-tokio | 7 | 3 | 0 | 10 | 10/10 (100%) | Yes |
| faster-uring | 56 | 3 | 3 | 62 | 62/62 (100%) | Yes |
| faster-dst | 6 | 2 | 0 | 8 | 8/8 (100%) | Yes |
| **Total** | **248** | **37** | **17** | **302** | **302/302 (100%)** | **All crates** |

The `#![forbid(clippy::undocumented_unsafe_blocks)]` lint in `lib.rs` makes undocumented unsafe blocks a compilation error. This is the strongest possible guarantee — the 100% SAFETY comment coverage is compiler-enforced, not just convention.

---

## Atomic Ordering Assessment

| Pattern | Location | Ordering Used | Assessment |
|---------|----------|--------------|------------|
| Free list CAS | `allocator.rs:760` | AcqRel/Acquire | ✅ Correct — standard Treiber stack ordering |
| Free list load | `allocator.rs:746,775` | Acquire | ✅ Correct — synchronizes with Release store in CAS |
| Epoch counter | `epoch/table.rs` | Acquire/Release | ✅ Correct — publish-subscribe pattern |
| Page state | `page.rs` (AtomicPageState) | AcqRel | ✅ Correct — state transitions need acquire-release |
| Handle counter | `handle.rs:63` | Relaxed | ✅ Correct — uniqueness only, ordering from RwLock |
| Bump allocator | `allocator.rs` (count) | Relaxed | ✅ Correct — monotonic counter, no ordering needed |
| Hash bucket CAS | `hash/table.rs` | AcqRel/Acquire | ✅ Correct — lock-free hash bucket updates |

No ordering issues found. No unnecessary SeqCst usage (good — SeqCst is appropriately avoided).

---

## FFI Boundary Assessment

**Post-fix status: ACCEPTABLE for production.**

| Check | Status | Notes |
|-------|--------|-------|
| Null pointer validation | ✅ Pass | All pointer params checked before dereference |
| Panic safety | ✅ Pass | All 12 extern "C" fns wrapped in `catch_unwind` |
| Handle validation | ✅ Pass | Handle table returns `None` for invalid/stale handles |
| Double-free protection | ✅ Pass | `remove()` returns `None` on second call |
| Type confusion | ✅ Pass | `downcast_ref` fails gracefully for wrong types |
| Lock poisoning | ✅ Pass | `unwrap_or_else(\|e\| e.into_inner())` recovers poisoned locks |
| Memory ownership | ✅ Pass | Immediate `to_vec()` copies sever aliasing with C buffers |
| Thread safety docs | ⚠️ Partial | Session thread-affinity documented but not runtime-enforced |
| Buffer overflow | ⚠️ Note | `data.len() as u32` truncation (FFI-03) — low practical risk |

---

## Threat Model Summary

### Attack Surface
1. **FFI boundary** — C callers can pass arbitrary data. Mitigated by null checks, handle validation, immediate buffer copies, and panic protection.
2. **Device callbacks** — Completion callbacks use raw function pointers with raw context pointers. Mitigated by the unsafe Device trait contract.
3. **Checkpoint/Recovery** — Reads data from disk into memory. Potential for malformed data to cause issues.
4. **Concurrent access** — Lock-free data structures have subtle correctness requirements. Mitigated by epoch protection and standard algorithms.

### Trust Boundaries
| Boundary | Trust Level | Mitigation |
|----------|-------------|------------|
| C FFI → Rust | Untrusted | Pointer validation, catch_unwind, to_vec copies |
| Disk → Memory (recovery) | Semi-trusted | Checksum verification (if implemented) |
| Thread → Thread (lock-free) | Trusted (same process) | Epoch protection, atomic orderings |
| Kernel → Userspace (io_uring) | Trusted (OS) | io_uring completion queue |

### Residual Risks
1. **Session thread-affinity violation** (FFI-02) — C caller can create a data race by sharing a session handle across threads. Mitigation: documentation + optional runtime check.
2. **Device callback context lifetime** (CORE-03/04) — Async I/O completion fires after context deallocation. Mitigation: dispose_session must drain pending I/O.
3. **Malformed checkpoint data** — If checkpoint files are tampered with, recovery may produce corrupt state. Mitigation: implement checksum validation for checkpoint metadata.

---

## Recommended Fuzzing Targets

| Priority | Target | Input | Rationale |
|----------|--------|-------|-----------|
| **P0** | FFI CRUD operations | Random key/val bytes, lengths, null pointers, invalid handles | FFI boundary is the primary external attack surface |
| **P0** | Checkpoint recovery | Mutated checkpoint files (bit flips, truncation, reordering) | Disk data is semi-trusted input |
| **P1** | Concurrent allocator stress | Multi-threaded alloc/free with epoch advancement | Treiber stack ABA under extreme contention |
| **P1** | Variable-length record access | Random record sizes, offsets near page boundaries | Pointer arithmetic edge cases |
| **P2** | Hash index grow | Concurrent operations during index doubling | Lock-free grow protocol correctness |
| **P2** | io_uring buffer lifecycle | Rapid submit/complete/cancel cycles | Kernel buffer reference lifetime |

**Tool recommendations:**
- `cargo-fuzz` with `libfuzzer` for single-threaded FFI fuzzing
- `loom` for concurrency model checking (already partially used)
- `miri` for detecting UB in allocator and epoch drain operations

---

## Remediation Schedule

| Priority | Finding | Owner | Timeline |
|----------|---------|-------|----------|
| ✅ Done | FFI-01: Panic protection | Galadriel | Complete |
| P0 | FFI-02: Session thread-ID validation | Elrond | Before GA |
| P0 | CORE-03: Callback context validation | Sam | Before GA |
| P0 | CORE-04: dispose_session drains pending I/O | Aragorn | Before GA |
| P1 | FFI-03: u32 truncation guard | Elrond | Before GA |
| P1 | CORE-01: ABA tag documentation | Aragorn | Next sprint |
| P1 | CORE-05: Flush drop ordering verification | Aragorn | Next sprint |
| P2 | All fuzzing targets | Éowyn | Ongoing |
| P2 | URING-01: Buffer lifetime documentation | Sam | Next sprint |

---

## Conclusion

The FASTER Rust codebase demonstrates mature security engineering practices:
- 100% SAFETY comment coverage, compiler-enforced
- Appropriate atomic orderings throughout (no SeqCst overuse, no Relaxed underuse)
- Well-structured FFI boundary with handle table, type safety, and lock-poison recovery
- Epoch-based memory reclamation correctly protects lock-free data structures

The one Critical finding (FFI panic safety) has been fixed. The remaining High findings are inherent to the unsafe Device trait contract and FFI trust boundary — they require defense-in-depth measures (runtime validation, debug assertions) rather than architectural changes.

**This codebase is ready for production deployment with the conditions noted above.**
