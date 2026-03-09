# Changelog

All notable changes to the FASTER Rust workspace will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.0] — 2026-03-10

Initial release of the Rust implementation of Microsoft FASTER — a ground-up
rewrite targeting mission-critical cloud services at planetary scale.

### Added

#### Core Engine (`faster-core`)

- **FasterKv** — Thread-safe (`Send + Sync`) concurrent key-value store with
  lock-free hash index and hybrid log, supporting millions of operations per
  second per thread.
- **FasterKvBuilder** — Fluent builder API for store construction with
  validation.
- **FasterKvConfig** — Full configuration surface: hash index size, buffer
  pages, mutable fraction, sector size, eviction policy, grow config, and
  auto-compaction toggle.
- **FasterSession** — Thread-affine (`!Send`) session handles with compile-time
  enforcement via `PhantomData<*const ()>`. Sessions manage epoch protection and
  pending I/O operations.
- **CRUD operations** — `read`, `upsert`, `rmw` (read-modify-write), and
  `delete` with pluggable user callbacks via the `Functions` trait.
- **Batch operations** — `batch_read`, `batch_upsert`, `batch_rmw`,
  `batch_delete`, and mixed `batch_execute` with single epoch entry, hash bucket
  prefetch, and automatic epoch refresh every 256 ops.
- **Functions trait** — User-defined callback interface for customizing
  read/upsert/rmw/delete behavior. Ships with `SimpleFunctions` (full-value
  replacement) and `CounterFunctions` (accumulator pattern).
- **Key and Value traits** — Serialization interface for fixed-size and
  variable-length records. Built-in implementations for `u32`, `u64`, `u128`,
  `Vec<u8>`, and `String`.
- **OperationStatus enum** — Rich status reporting: `Ok`, `Created`,
  `InPlaceUpdated`, `CopyUpdated`, `Revivified`, `NotFound`, `Pending`,
  `Deleted`, `Aborted`.
- **Hash index** — Latch-free concurrent hash table with 7-entry buckets
  (`HashBucket`, 64-byte cache-line aligned), atomic CAS-based inserts, and
  overflow chain support. Includes cross-platform software prefetch on bucket
  lookups for reduced cache-miss stalls.
- **LogicalAddress** — 48-bit address encoding (23-bit page | 25-bit offset)
  supporting 256 TB address space. Read-cache bit (47) reserved.
- **Hybrid log** — Circular buffer allocator with mutable, read-only, and
  on-disk regions. `HybridLogAllocator` manages page lifecycle through
  `Free → Allocated → Sealed → Flushing → Flushed → Evicted` state machine.
- **Page flusher** — Async page writes to device with completion callbacks.
- **Page evictor** — Configurable eviction policies: `LRU` (default),
  `Segmented`, and `Fixed`.
- **LogScanIterator** — Sequential log scan with optional tombstone filtering
  and key prefix filters.
- **Record accessors** — `RecordAccessor` (read-only), `MutableRecordAccessor`
  (in-place update), `LogRecordWriter`, `LogRecordReader`, and
  `VersionChainIterator` for walking version chains.
- **Epoch-based memory reclamation** — Lock-free coordination via `EpochTable`
  with per-thread entries (cache-line padded, max 256 threads), RAII
  `EpochGuard`, and drain callbacks for safe deferred cleanup.
- **Checkpoint system** — Two strategies: `FoldOver` (flush in-memory pages to
  main log) and `Snapshot` (write mutable region to separate file). Multi-phase
  state machine (7 phases), JSON metadata on disk, 128-bit UUID tokens.
- **Recovery manager** — Restore to consistent state from checkpoint with
  configurable validation: `Strict` (verify all checksums), `Lenient` (skip
  corrupted records), or `None`.
- **Online hash table grow** — Dynamic resize via `GrowManager` with load
  factor threshold (default: 0.75), chunk-based parallel splitting (16,384
  buckets per chunk), and two-version scheme allowing reads during resize.
- **Compaction pipeline** — Four-phase log garbage collection:
  scan → copy → pointer-swing → begin-address-advance. Coordinates with epoch
  system for safety.
- **Compaction policies** — `SpaceAmplificationPolicy` (dead% threshold),
  `TombstonePercentPolicy`, `ManualPolicy`, plus `AnyPolicy`/`AllPolicy`
  combinators.
- **Error types** — Unified `FasterError` enum covering I/O, checkpoint,
  recovery, invalid operation, grow, config, internal, and session errors.
- **Metrics** — Optional (`metrics` feature) advisory counters via relaxed
  atomics: hash lookups/inserts, overflow allocations, epoch bumps/drains,
  allocator operations, pending I/O, checkpoint count, total operations, flush
  count. `MetricsSnapshot` for point-in-time capture.
- **Feature flags** — `paranoid` (extra runtime assertions), `metrics`
  (operation counters), `loom` (deterministic concurrency testing), `tracing`
  (observability spans), `simulation` (cooperative scheduler for DST).

#### Device Layer (`faster-device`)

- **Device trait** — Completion-callback-based async I/O interface, fully
  runtime-agnostic. No async runtime dependency.
- **NullDevice** — Discards writes, returns zeros on reads. For testing and
  benchmarking.
- **InMemoryDevice** — RAM-backed storage via `RwLock<Vec<u8>>` for integration
  testing without filesystem access.
- **SyncFileDevice** — Synchronous file I/O with configurable sector size and
  max file size. Thread pool for async completion callbacks.
- `#![warn(missing_docs)]` and `#![deny(unsafe_op_in_unsafe_fn)]` enforced
  crate-wide.

#### io_uring Backend (`faster-uring`)

- **UringDevice** — Linux io_uring-based async I/O device implementation with
  adaptive batching via `BatchPolicy` (aggressive, conservative, adaptive).
- **Ring** — Safe Rust wrapper around raw io_uring syscalls with configurable
  queue depth, sqpoll, and iopoll options.
- **BufferPool** — Lock-free pool of sector-aligned buffers for fixed-buffer
  I/O. `RegisteredBuffer` RAII guards enforce exclusive access and automatic
  return on drop.
- Platform-gated: all public types behind `#[cfg(target_os = "linux")]`.

#### Tokio Integration (`faster-tokio`)

- **AsyncFasterKv** — Managed async store with automatic background
  maintenance, graceful shutdown, and async checkpoint/compaction methods.
- **AsyncSession** — Async CRUD interface wrapping `FasterSession` with
  `complete_pending()`, `complete_pending_with_results()`, and `refresh()`.
- **Bridge module** — `PendingFuture<T>` / `CompletionSender<T>` pair for
  zero-runtime-dependency callback-to-Future conversion. `MaybePending<T>` for
  ergonomic sync-or-async branching.
- **TokioFileDevice** — File I/O device using Tokio `spawn_blocking` with
  completion callbacks via the Tokio runtime.

#### C FFI Bindings (`faster-ffi`)

- **Opaque handle API** — `FasterHandle` (u64) with type-safe `HandleTable`
  using monotonic IDs. No transmute; poisoned-lock recovery.
- **Store lifecycle** — `faster_open()`, `faster_open_with_path()`,
  `faster_close()`.
- **Session lifecycle** — `faster_session_start()`, `faster_session_end()`.
  Session handles are NOT thread-safe (must be used from creating thread).
- **CRUD** — `faster_upsert()`, `faster_read()`, `faster_delete()`,
  `faster_rmw()` with raw byte pointers and length parameters.
- **Maintenance** — `faster_complete_pending()`.
- **Persistence** — `faster_checkpoint()` (returns 128-bit token via
  `FasterCheckpointResult`), `faster_recover()`.
- **C-compatible types** — `#[repr(C)]` enums: `FasterStatus` (12 variants),
  `FasterCheckpointType` (FoldOver, Snapshot).
- Builds as both `cdylib` and `staticlib`.

#### Deterministic Simulation Testing (`faster-dst`)

- **SimulationHarness** — Seed-controlled test fixture creating stores with
  simulated devices and temp checkpoint directories.
- **SimulatedDevice** — Fault-injecting device backed by `SimulatedStorage`
  that persists across drops for crash-recovery testing.
- **DeterministicScheduler** — Cooperative single-threaded task scheduler with
  seed-controlled execution ordering. Same seed = same execution path.
- **CrashSchedule** — 18 crash-point injection sites across checkpoint,
  compaction, and recovery phases. `crash_point!()` macro compiles to no-op
  without `simulation` feature.
- **ScenarioTemplate** — Declarative builder for reusable test configurations
  with workload factories, fault configs, crash schedules, and invariant checks.
- **SeedCampaign** — Parallel seed-sweep engine running thousands of
  scenario/seed pairs with aggregated `CampaignReport`.
- **Invariant trait** — Post-recovery assertions with `And`, `Or`, `All`, `Not`
  combinators.
- **SimulationTrace** — Append-only event log for debugging scheduler decisions,
  task ordering, and non-determinism.
- **SimulatedClock** — Deterministic time source.
- **Pre-built scenarios** — 14 standard test templates: `crud_stress`,
  `checkpoint_crash`, `compaction_crash`, `concurrent_crash`,
  `dual_subsystem_crash`, `expansion`, `high_density_crash`,
  `large_record_recovery`, `mixed_crash_timing`, `overwrite_recovery`,
  `recovery_stress`, `torn_write`, `torn_write_varied`,
  `write_error_recovery`.

#### Benchmark Suite (`faster-bench`)

- **YCSB workload generator** — Configurable operation mix (read%, insert%,
  update%, scan%) with zipfian and uniform key distributions.
- **Criterion harness** — Benchmarks via `criterion` 0.5 with HTML reports.
- Release profile: thin LTO, single codegen unit, opt-level 3. Bench profile
  inherits release with debug symbols for profiling.

#### Sample Applications

- **tokio-kv-server** — Async HTTP key-value server with REST API (GET, POST,
  DELETE) demonstrating `faster-tokio` integration.
- **event-counter-tokio** — Async event aggregation (ad click counter) with
  checkpoint/recovery.
- **read-cache-sim** — Read-cache simulation exploring bit-47 address encoding.
- **read-cache-sim-tokio** — Async variant of read-cache-sim.
- **disk-io-bench** — Disk I/O benchmarking across device implementations.
- **cross-impl-bench** — Cross-device comparison (NullDevice vs InMemoryDevice
  vs SyncFileDevice vs UringDevice).
- **uring-stress** — io_uring high-volume stress test and verification.

#### Project Infrastructure

- **Workspace** — 7 core crates + 7 sample crates under Cargo workspace with
  resolver v3, Rust 2024 edition, MSRV 1.85.0.
- **Quality gate** — `cargo fmt` → `cargo clippy` → `cargo test --doc` →
  `cargo nextest run`. Enforced via `scripts/checkin` and `scripts/precheckin`.
- **Documentation** — QUICKSTART.md, TESTING.md, TESTING-ARCHITECTURE.md,
  PERFORMANCE.md, SECURITY-AUDIT.md, FUZZING.md, CONTRIBUTING.md.
- **Fuzzing** — 5 fuzz targets via `cargo-fuzz` with corpus management.
- **Mutation testing** — `cargo-mutants` configuration in `mutants.toml`.
- **Safety policy** — `#![deny(unsafe_op_in_unsafe_fn)]`,
  `#![forbid(clippy::undocumented_unsafe_blocks)]`, 100% SAFETY comment
  coverage per security audit.

### Security

- Completed security audit: 1 critical finding (fixed), 4 high, 6 medium.
  See `SECURITY-AUDIT.md` for full report.
- All unsafe blocks documented with `// SAFETY:` comments.

[0.1.0]: https://github.com/microsoft/FASTER/releases/tag/rust-v0.1.0
