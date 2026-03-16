# Project Context

- **Owner:** qbradley
- **Project:** Rust implementation of Microsoft FASTER — a high-performance durable hash map
- **Stack:** Rust (primary), C++ (reference), C# (reference), C FFI
- **Goal:** Production-grade, no async runtime required, seamless Tokio integration, idiomatic Rust API + C FFI interface. Quality bar: mission-critical cloud services at planetary scale.
- **Created:** 2026-03-05

## Core Security Context

### Unsafe Inventory (302 total across 5 crates)
- faster-core: 142 (101 blocks, 29 fns, 12 impls)
- faster-ffi: 80 (78 blocks, 0 fns, 2 impls)  
- faster-uring: 62 (56 blocks, 3 fns, 3 impls)
- faster-tokio: 10 (7 blocks, 3 fns, 0 impls)
- faster-dst: 8 (6 blocks, 2 fns, 0 impls)

**Enforcement:** `#![forbid(clippy::undocumented_unsafe_blocks)]` + `#![deny(unsafe_op_in_unsafe_fn)]` in lib.rs

### Critical Security Findings (2026-03-06 Audit)

**CRITICAL (FIXED):**
- FFI panic safety — All 12 `extern "C"` functions now wrapped in `catch_unwind` (commit: security(audit): add catch_unwind panic protection)

**HIGH (OPEN, tracked):**
1. Device callback context pointer lifetime — not enforceable at compile time
2. PendingIoContext raw pointers to session state — UAF risk if session disposed during pending I/O
3. FFI SessionCell Send+Sync relies on C caller thread-affinity contract
4. Tokio device raw pointer→usize cast — temporal UAF window

**MEDIUM (documented):**
- Allocator 16-bit ABA tag sufficient with epoch, fragile without
- u32 truncation in FFI read buffer size
- Various lifetime assumptions in async I/O paths

### Threat Model
Storage engines have unique attack surface:
- **Untrusted disk data** (checkpoint files, log segments) — P0 fuzz targets
- **FFI boundary violations** (null pointers, thread races, panics) — catch_unwind + validation
- **Concurrent UAF** (epoch bugs, ABA) — Miri + Loom coverage
- **Resource exhaustion** (hash collisions, unbounded chains) — monitoring
- **Information disclosure** (tombstone leaks) — documented policies

### Testing Coverage
- **Miri:** 78 tests, 100% of testable unsafe (excludes file I/O)
- **Fuzz:** 8 targets (record parsing, checkpoint recovery, log recovery, page trailer, hash, store ops)
- **Loom:** Concurrency primitives (epoch, drain, allocator)
- **Atomic orderings:** All verified correct (no SeqCst overuse, no Relaxed underuse)

### Enforcement Policy
- All new unsafe code MUST include Miri test (code review gate)
- All `extern "C"` functions MUST have catch_unwind
- All FFI pointers MUST be null-checked before dereference
- Fuzz target required for any new deserialization path

---

## Learnings

<!-- Append new learnings below. Each entry is something lasting about the project. -->

## 2026-03-11: 100% Miri Coverage Achieved
- **78 miri tests** across all testable unsafe modules in faster-core
- Key technical insights: MutableRecordAccessor needs 8-byte aligned backing (Vec<u64>), KeyHash::new(0) is empty sentinel, DrainList tested via EpochTable public API
- File I/O modules excluded (documented): recovery/index_recovery.rs, flush.rs, pending_io.rs, sync_file_device.rs
- CI integration: ~45s runtime, acceptable for pre-merge

## 2026-03-11: Fuzz Targets for Recovery Paths
- **3 new targets:** fuzz_page_trailer, fuzz_checkpoint_recovery, fuzz_log_recovery
- File-based fuzzing uses tempfile crate for disk-based APIs
- CRC validation coverage includes version-gated paths (format_version >= 3)
- Dependencies added to fuzz crate: serde_json, serde, tempfile, crc32fast
