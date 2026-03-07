# Decision: faster-uring Crate — Safe io_uring Ring Wrapper

**Agent:** Gimli (Database/Storage Expert)
**Date:** 2026-03-07
**Status:** Implemented (Wave 5, U1)
**Impact:** New crate — foundation for io_uring-based device I/O

## Decision

Created `rust/crates/faster-uring/` with a safe `Ring` wrapper around Linux io_uring. The public API exposes zero unsafe code; all io_uring syscalls are encapsulated internally.

## API Surface

- `UringConfig` — queue depth (power of 2), SQPOLL mode, direct I/O flag
- `Ring::new(config)` — create io_uring instance
- `Ring::submit_read/write/fsync` — queue I/O operations
- `Ring::submit()` — flush SQ to kernel
- `Ring::reap_completions()` — wait and collect CQEs as `(user_data, result)` pairs
- `Ring::drain()` — block until all in-flight ops complete

## Crash Safety Properties

1. **Write ≠ durable.** A completed write only means data reached the kernel. Only a completed fsync guarantees persistence.
2. **Fsync semantics.** After fsync completion, all prior writes to that fd are on stable storage.
3. **Drop safety.** `Ring::drop()` calls `drain()` to wait for in-flight I/O, preventing kernel use-after-free of caller buffers.

## Buffer Lifetime Contract (U1)

The kernel holds raw pointers to caller buffers between submit and completion. In U1, buffer lifetime is the **caller's responsibility** — documented in both module-level and method-level docs. U2 will add a registered buffer pool to enforce this at the type level.

## Rationale

- **Completion-based model** aligns with FASTER's callback architecture (Decision #2: no async/await in core)
- **io-uring crate v0.7.11** is the best-maintained Rust binding; provides safe SQE construction
- **Linux-only** via `#[cfg(target_os = "linux")]` — io_uring is a Linux kernel feature
- **Direct I/O support** is essential for FASTER's page-aligned, unbuffered workloads

## Dependencies

- `io-uring = "0.7"` (new external dependency)
- `faster-core` (path dependency, for future type integration)
- `tempfile = "3"` (dev-dependency for tests)

## Implications

- **Sam (Device):** The `Ring` API is designed to plug into the `Device` trait's completion-callback model. A `UringDevice` adapter will wrap `Ring` in a future wave.
- **Galadriel (Safety):** The `unsafe` blocks in `ring.rs` (3 total: read, write, fsync SQE pushes) all have `// SAFETY:` comments. The buffer-lifetime unsafety is documented but not yet type-enforced.
- **All agents:** `faster-uring` is auto-discovered by the workspace (`members = ["crates/*"]`).

## Next Steps

- [ ] U2: Registered buffer pool (type-safe kernel buffer management)
- [ ] Integration with `Device` trait as `UringDevice`
- [ ] SQPOLL benchmarking (requires `CAP_SYS_ADMIN` or `IORING_SETUP_SQPOLL` sysctl)
