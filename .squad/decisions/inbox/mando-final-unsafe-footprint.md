# Final Unsafe Footprint — faster-core

**Agent:** Mando (Rust Expert)
**Date:** 2025-07-18
**Phase:** 5 — Module-Level Safety Gates (Final)

## Summary

After 5 phases of the unsafe audit, the `faster-core` crate has:

| Metric | Count |
|--------|-------|
| **Unsafe blocks** | 100 |
| **Unsafe fns** | 30 |
| **Unsafe impls** | 15 |
| **Total unsafe constructs** | 145 |
| **Files with unsafe** | 18 of 60 (.rs files) |
| **Files gated with `#[deny(unsafe_code)]`** | 42 modules |

## Safety-Gated Modules (compiler-enforced zero unsafe)

These modules have `#[deny(unsafe_code)]` applied. Any future `unsafe` addition triggers a compile error.

### Top-level (gated in lib.rs)

| Module | Scope |
|--------|-------|
| `address` | Logical/physical address types |
| `error` | Error types |
| `grow` | Online hash table resize (manager, splitter, state_machine) |
| `metrics` | Counters and instrumentation |
| `record` | Record layout, RecordInfo, traits |
| `status` | Operation status enums |
| `sync` | Synchronization primitives (pub(crate)) |
| `instrument` | Tracing macros (private) |

### Sub-modules (gated in parent mod.rs)

| Parent | Gated Sub-modules | Ungated (has unsafe) |
|--------|--------------------|----------------------|
| `checkpoint` | log_writer, manager, metadata, metadata_store, orchestrator, participant, session_state, snapshot_writer, state_machine (9/10) | index_writer |
| `epoch` | entry, guard, table (3/4) | drain |
| `hash` | bucket, hash, overflow (3/5) | index, table |
| `hybrid_log` | eviction, regions (2/7) | flush, log_allocator, page, record_ops, scan |
| `recovery` | log_recovery, session_recovery (2/3) | index_recovery |
| `store` | builder, session (2/6) | functions, kv, operations, pending_io |

## Per-File Unsafe Breakdown

| File | Blocks | Fns | Impls | Notes |
|------|--------|-----|-------|-------|
| `allocator.rs` | 18 | 3 | 3 | Lock-free Treiber stack, page allocation |
| `buffer_pool.rs` | 4 | 0 | 2 | Raw memory pool with Send/Sync |
| `checkpoint/index_writer.rs` | 2 | 0 | 0 | Index serialization slice |
| `device.rs` | 13 | 12 | 0 | I/O device trait + FFI callbacks |
| `epoch/drain.rs` | 8 | 2 | 2 | Drain list raw pointer callbacks |
| `hash/index.rs` | 1 | 0 | 0 | Bucket serialization slice |
| `hash/table.rs` | 2 | 0 | 0 | Unchecked indexing (hot path) |
| `hybrid_log/flush.rs` | 4 | 1 | 1 | I/O completion callback |
| `hybrid_log/log_allocator.rs` | 3 | 0 | 0 | Raw pointer arithmetic |
| `hybrid_log/page.rs` | 15 | 2 | 4 | PageFrame raw memory, Send/Sync |
| `hybrid_log/record_ops.rs` | 8 | 2 | 0 | Record accessor raw pointers |
| `hybrid_log/scan.rs` | 1 | 0 | 0 | Scan iterator slice |
| `recovery/index_recovery.rs` | 1 | 0 | 0 | Index deserialization slice |
| `store/functions.rs` | 6 | 4 | 0 | Functions trait callback dispatch |
| `store/kv.rs` | 0 | 0 | 2 | unsafe impl Send/Sync for FasterKv |
| `store/operations.rs` | 5 | 0 | 0 | MutableRecordAccessor construction |
| `store/pending_io.rs` | 3 | 1 | 0 | I/O completion callback |
| `sync_file_device.rs` | 6 | 3 | 1 | File I/O thread pool |

## Crate-Level Lint Configuration

```rust
// lib.rs
#![deny(unsafe_op_in_unsafe_fn)]        // Require explicit unsafe blocks inside unsafe fns
#![forbid(clippy::undocumented_unsafe_blocks)] // Every unsafe block must have a SAFETY comment
```

```toml
# Cargo.toml
[lints.clippy]
undocumented_unsafe_blocks = "deny"  # Also covers tests and benchmarks
```

## What Remains Unsafe and Why

All remaining unsafe falls into 5 categories:

1. **Raw pointer construction** — `RecordAccessor`, `MutableRecordAccessor`, `PageFrame` own raw pointers to caller-provided memory. The constructors are inherently unsafe.
2. **Send/Sync impls** — `FasterKv`, `PageFrame`, `BufferPool`, `DrainList` need manual Send/Sync because they contain raw pointers with lifetime contracts that the compiler can't verify.
3. **I/O callbacks** — Device completion callbacks receive `*mut u8` from the OS; reconstruction into typed pointers is inherently unsafe. `TypedIoContext<T>` centralizes this.
4. **Lock-free data structures** — The allocator's Treiber stack and epoch drain list use CAS on raw pointers for lock-free operation.
5. **Hot-path indexing** — `HashTable::bucket()` uses `get_unchecked()` where the index is guaranteed valid by construction (mask-based).

## Test Results

- **1161 tests passed, 3 skipped**
- **Clippy clean** (`-D warnings`, all targets)
- **Zero regressions** from safety gate additions
