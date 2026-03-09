---
date: 2025-07-12T22:00:00Z
git_commit: 869d3dee
branch: feature/deterministic-simulation-testing
repository: FASTER
topic: "Deterministic Simulation Testing Framework Implementation Map"
tags: [research, codebase, faster-dst, simulation, epoch, checkpoint, compaction, recovery]
status: complete
last_updated: 2025-07-12
---

# Research: Deterministic Simulation Testing Framework

## Research Question

What existing infrastructure, abstraction points, nondeterminism sources, and state machines must the simulation framework interact with? Where precisely in the codebase must changes be made?

## Summary

The FASTER Rust codebase has a strong foundation for simulation testing via the existing `faster-dst` crate (1,882 lines) with `SimulatedDevice`, `FaultConfig`, seeded workload generation, and crash-recovery tests. The codebase has exactly **2 production thread spawn sites** (uring I/O thread, sync file worker pool), **2 mpsc channels**, **~15 production `Instant::now()` calls** (5 critical for timeout enforcement), and **2 `SystemTime::now()` calls** (checkpoint metadata). The `sync.rs` module provides a proven `cfg`-based abstraction pattern for loom that can be extended to simulation. The checkpoint state machine has 6 phase transitions via CAS with a two-phase intermediate protocol. Compaction has a 4-phase pipeline (scan→copy→pointer-swing→truncate) under epoch protection. Recovery has a 3-stage flow with existing CRC32 validation for index checkpoints. Hybrid log pages currently have NO checksums — CRC insertion points identified at `flush.rs:252` (write) and `log_recovery.rs:190` (verify).

## Documentation System

- **Framework**: Plain markdown (no doc generator)
- **Docs Directory**: `docs/` at repo root (project-level docs, not Rust API docs)
- **Navigation Config**: N/A
- **Style Conventions**: Cargo doc comments (`///`) on public APIs; integration test files have module-level `//!` docs
- **Build Command**: `cargo doc --workspace --no-deps`
- **Standard Files**: `README.md` (root), `CONTRIBUTING.md`, `SECURITY.md`, `rust/README.md`

## Verification Commands

- **Test Command**: `cd rust && cargo nextest run --workspace --all-targets` (all tests) or `./scripts/precheckin` (full gate: fmt→clippy→doctests→nextest)
- **Lint Command**: `cargo clippy --workspace --all-targets -- -D warnings`
- **Build Command**: `cargo build --workspace --all-targets`
- **Type Check**: `cargo check --workspace --all-targets`
- **Loom Tests**: `RUSTFLAGS="--cfg loom" cargo test --features loom -p faster-core --test loom_tests`
- **Miri Tests**: `cargo +nightly miri test -p faster-core --test miri_tests`
- **Checkin Script**: `rust/scripts/checkin` (agents must use this instead of `git commit`)

## Detailed Findings

### 1. Existing faster-dst Crate

**Location**: `rust/crates/faster-dst/` (1,882 lines total: 1,304 src + 564 tests + 14 Cargo.toml)

**Dependencies** (`Cargo.toml:10-13`):
- `faster-core` (path), `rand` 0.9 with `small_rng`, `tempfile` 3

**Module Structure** (`lib.rs:72-86`):

| Module | File | Lines | Key Type | Purpose |
|--------|------|-------|----------|---------|
| clock | `clock.rs` | 85 | `SimulatedClock` | Controlled time (AtomicU64 nanos) |
| device | `device.rs` | 477 | `SimulatedDevice`, `SimulatedStorage` | Fault-injectable in-memory I/O |
| fault | `fault.rs` | 135 | `FaultConfig`, `CrashPoint` | Fault injection configuration |
| harness | `harness.rs` | 180 | `SimulationHarness` | Test factory (stores, devices, dirs) |
| runtime | `runtime.rs` | 106 | `SimulationRuntime` | Seeded PRNG workload generation |
| workload | `workload.rs` | 88 | `Workload` trait, `CrudWorkload` | Composable workload definitions |
| invariant | `invariant.rs` | 147 | `Invariant` trait, `AllCommittedRecoverable` | Post-scenario verification |

**SimulatedClock** (`clock.rs:9-14`): `nanos: AtomicU64`. Methods: `new()` :18, `starting_at()` :25, `now()` :32, `advance()` :37, `set()` :43.

**SimulatedStorage** (`device.rs:26-29`): `data: RwLock<Vec<u8>>`. Wrapped in `Arc` for persistence across device drops (crash simulation). Methods: `new()` :33 returns `Arc<Self>`, `snapshot()` :40, `restore()` :45, `len()` :50.

**SimulatedDevice** (`device.rs:72-80`): Fields: `storage: Arc<SimulatedStorage>`, `fault_config: FaultConfig`, `rng: Mutex<SmallRng>`, `write_count: AtomicU64`, `read_count: AtomicU64`, `sector_size: u32`, `segment_size: u64`. Implements `Device` trait (lines 198-356). Async I/O methods call callback synchronously (`IoRequestResult::CompletedSync`). Partial writes: writes `actual_len < full_len` but reports `full_len` to caller (torn page simulation, device.rs:294-295).

**FaultConfig** (`fault.rs:23-35`): Fields: `write_error_rate: f64`, `read_error_rate: f64`, `partial_write_rate: f64`, `fail_after_n_writes: Option<u64>`, `fail_after_n_reads: Option<u64>`. Builder pattern via `with_*()` methods (lines 45-73).

**CrashPoint** enum (`fault.rs:7-17`): `DuringFlush`, `DuringCheckpoint`, `DuringCompaction`, `DuringRecovery`. Currently documentation markers — not hooked into faster-core state machines.

**SimulationHarness** (`harness.rs:21-26`): Fields: `seed`, `storage: Arc<SimulatedStorage>`, `checkpoint_dir: TempDir`, `device_seed_counter: AtomicU64`. Factory methods: `create_store()` :92, `create_device()` :64, `create_file_store()` :119. Child seed derivation uses LCG multiplier (line 59).

**SimulationRuntime** (`runtime.rs:11-14`): Fields: `seed: u64`, `rng: SmallRng`. Seeded via `SmallRng::seed_from_u64(seed)` (line 21). Methods: `child_seed()` :36, `random_keys()` :44, `random_kv_pairs()` :49, `sequential_kv_pairs()` :57.

**Workload trait** (`workload.rs:10-22`): `execute(store, session)` and `expected_state() -> HashMap<u64, u64>`. `CrudWorkload` implementation upserts all records (lines 50-68).

**Invariant trait** (`invariant.rs:8-16`): `check(store, session) -> Result<(), String>`. `AllCommittedRecoverable` reads back all expected keys and verifies values match.

**Tests**: `crash_recovery.rs` (357 lines) — write→checkpoint→drop(crash)→recover→verify. `seed_exploration.rs` (207 lines) — multi-seed sweeps with failure reporting.

### 2. Thread Spawning (Nondeterminism Source #1)

**Production thread spawns (2 sites)**:

| File:Line | Thread Name | Pattern | Purpose |
|-----------|-------------|---------|---------|
| `faster-uring/src/device.rs:964-969` | `"uring-io"` | `thread::Builder::new().name().spawn()` | Single I/O thread owning io_uring Ring, receives `IoCommand` via mpsc |
| `faster-core/src/sync_file_device.rs:360-363` | `"sync-io-{i}"` | `thread::Builder::new().name().spawn()` | N worker threads, each with dedicated `IoRequest` channel |

**Production channel usage (2 sites)**:

| File:Line | Type | Direction | Purpose |
|-----------|------|-----------|---------|
| `faster-uring/src/device.rs:962` | `mpsc::channel::<IoCommand>` | Caller → uring-io thread | Async I/O command queue |
| `faster-core/src/sync_file_device.rs:~350` | `mpsc::channel::<IoRequest>` | Caller → sync-io-N thread | Per-worker sync I/O queue |

**Production thread::sleep (3 sites)**:

| File:Line | Duration | Purpose |
|-----------|----------|---------|
| `store/kv.rs:284` | `POLL_INTERVAL` (1ms) | Busy-wait in blocking poll loop |
| `checkpoint/orchestrator.rs:283` | 1ms | Checkpoint flush completion wait |
| `faster-tokio/src/bridge.rs:388` | 1ms | Bridge polling (test only) |

**No Condvar usage** in the entire codebase. Coordination via atomics, barriers (tests only), and channels.

### 3. Time Usage (Nondeterminism Source #2)

**Critical production `Instant::now()` (5 timeout-enforcement sites)**:

| File:Line | Purpose | Impact |
|-----------|---------|--------|
| `store/kv.rs:266` | Blocking poll loop deadline (`TIMEOUT` = 5s) | Controls max wait for pending I/O |
| `store/kv.rs:279` | Deadline check in busy-wait | Determines loop termination |
| `checkpoint/orchestrator.rs:273` | Max flush wait deadline (`max_wait_flush` = 30s) | Controls checkpoint timeout |
| `checkpoint/orchestrator.rs:278` | Deadline enforcement | Determines flush timeout |
| `faster-uring/src/device.rs:456,478` | Batching deadline (microsecond precision) | Controls io_uring batch window |

**Non-critical `Instant::now()` (10+ sites)**: Latency measurement/logging in `checkpoint/log_writer.rs:129,597,599`, `checkpoint/snapshot_writer.rs:89,369,371`, `sync_file_device.rs:692`, `faster-tokio/src/device.rs:538,742,751`. These record elapsed time for diagnostics — do not affect control flow.

**`SystemTime::now()` (2 sites)**:

| File:Line | Purpose |
|-----------|---------|
| `checkpoint/orchestrator.rs:296` | Checkpoint metadata timestamp |
| `checkpoint/metadata_store.rs:355` | Metadata store timestamp |

**Timeout constants**:

| File:Line | Constant | Value |
|-----------|----------|-------|
| `store/pending_io.rs:41` | `DEFAULT_IO_TIMEOUT` | 30 seconds |
| `store/kv.rs:265` | `TIMEOUT` | 5 seconds |
| `store/kv.rs:267` | `POLL_INTERVAL` | 1 millisecond |
| `checkpoint/orchestrator.rs:184` | `max_wait_flush` | 30 seconds |

### 4. Random Number Generation (Nondeterminism Source #3)

**Production randomness (1 site)**:

| File:Line | Usage | Impact |
|-----------|-------|--------|
| `checkpoint/orchestrator.rs:296` | `SystemTime::now()` used to derive checkpoint token | Nondeterministic token generation |

**Simulation randomness (fully seeded)**:

| File | Pattern | Seed Source |
|------|---------|-------------|
| `faster-dst/src/runtime.rs` | `SmallRng::seed_from_u64(seed)` | Caller-provided master seed |
| `faster-dst/src/device.rs` | `SmallRng::seed_from_u64(seed)` | Harness-derived child seed |
| `faster-dst/src/harness.rs:59` | `seed.wrapping_add(counter * LCG)` | LCG from master seed |

### 5. cfg-Based Abstraction Model (sync.rs)

**File**: `faster-core/src/sync.rs` (44 lines)

Provides compile-time swap of synchronization primitives:
```
#[cfg(loom)] → loom::sync::atomic::{AtomicPtr, AtomicU64, AtomicUsize, Ordering, fence}
#[cfg(loom)] → loom::sync::{Arc, Mutex}
#[cfg(loom)] → loom::thread

#[cfg(not(loom))] → std::sync::atomic::{...}
#[cfg(not(loom))] → std::sync::{Arc, Mutex}
#[cfg(not(loom))] → std::thread
```

Note at line 8: "These re-exports are not yet used by production code (planned for a follow-up refactor)." Production code currently imports directly from `std`.

ARM memory ordering audit documented in comments (lines 10-28): All `Relaxed` uses are advisory counters only; synchronization paths use `Acquire`/`Release`/`AcqRel`/`SeqCst`.

### 6. Checkpoint State Machine

**Directory**: `faster-core/src/checkpoint/` (11 files, ~5,400 lines total)

| File | Lines | Purpose |
|------|-------|---------|
| `state_machine.rs` | 575 | Phase transitions via CAS |
| `orchestrator.rs` | 707 | Coordinates full checkpoint flow |
| `manager.rs` | 671 | Public checkpoint API |
| `metadata_store.rs` | 870 | Metadata persistence |
| `index_writer.rs` | 745 | Hash index serialization |
| `log_writer.rs` | 774 | Log checkpoint (fold-over) |
| `snapshot_writer.rs` | 794 | Log checkpoint (snapshot) |
| `participant.rs` | 166 | Per-session participation |
| `session_state.rs` | 381 | Session checkpoint state |
| `metadata.rs` | 456 | Recovery metadata types |
| `mod.rs` | 63 | Module declarations |

**CheckpointPhase enum** (`state_machine.rs:34-47`): `Rest(0)`, `Prepare(1)`, `InProgress(2)`, `WaitFlush(3)`, `WaitCompletion(4)`, `Completed(5)`.

**Legal transitions** (`state_machine.rs:73-78`): Rest→Prepare→InProgress→WaitFlush→WaitCompletion→Completed→Rest.

**SystemState packing** (`state/system_state.rs:24-34`): 64-bit packed word. Phase in bits 56-63 (`PHASE_SHIFT=56`, `PHASE_MASK=0xFF00_0000_0000_0000`), version in bits 0-55 (`VERSION_MASK=0x00FF_FFFF_FFFF_FFFF`). `INTERMEDIATE_BIT=0x80` marks mid-transition.

**Two-phase CAS protocol** (`system_state.rs:157-190`):
1. CAS expected → intermediate (set 0x80 bit) with `AcqRel` ordering (line 174)
2. Execute hooks while holding intermediate state (line 182)
3. CAS intermediate → final state with `AcqRel` ordering (line 186)

**Version bump**: Occurs on Prepare→InProgress transition only (`state_machine.rs:219-225`).

**Crash-point injection sites** (6 phase transitions):

| Transition | Location |
|------------|----------|
| Rest → Prepare | `state_machine.rs:236` |
| Prepare → InProgress | `state_machine.rs:381` (version bumps here) |
| InProgress → WaitFlush | `state_machine.rs:411` |
| WaitFlush → WaitCompletion | `state_machine.rs:415` |
| WaitCompletion → Completed | `state_machine.rs:419` |
| Completed → Rest | `state_machine.rs:424` |

**Orchestrator flow** (`orchestrator.rs`):
1. `take_checkpoint()` :103 — public entry point
2. `run_checkpoint()` :145 — private coordination
3. `advance(from, to)` :193 — wraps CAS with error handling
4. `write_index_checkpoint()` :226 — serializes hash buckets
5. `write_log_checkpoint()` :243 — fold-over or snapshot
6. `wait_for_flush()` :265 — spin-wait with 30s deadline
7. `collect_session_infos()` :288 — gather per-session state

### 7. Compaction 4-Phase Pipeline

**Directory**: `faster-core/src/compaction/` (7 files, ~4,059 lines total)

| File | Lines | Purpose |
|------|-------|---------|
| `orchestrator.rs` | 495 | Coordinates K1-K4 pipeline |
| `scanner.rs` | 1,198 | K1: Record classification |
| `copier.rs` | 563 | K2: Live record copying |
| `address_update.rs` | 780 | K3: Hash index CAS updates |
| `begin_address.rs` | 359 | K4: Begin-address advance |
| `policy.rs` | 558 | Compaction trigger policies |
| `mod.rs` | 106 | Module declarations |

**CompactionOrchestrator** (`orchestrator.rs:120-141`): Fields: `allocator`, `hash_index`, `device`, `epoch_table`.

**Pipeline execution** (`orchestrator.rs:154-232`):
- K1-K3 under epoch protection: `let _guard = epoch_thread.protect()` (line 172)
- K1: `CompactionScanner::scan::<K,V>()` at `scanner.rs:135`
- K2: `RecordCopier::copy_records()` at `copier.rs:156`
- K3: `AddressUpdater::swing::<K,V>()` at `address_update.rs:111`
  - Hash index CAS at `address_update.rs:194` → delegates to `hash/index.rs:349-361`
  - SwingStats tracking (`address_update.rs:45-56`): `swung`, `cas_failed`, `not_found`, `tombstones_removed`
- Epoch drain: `unregister()` at line 209, `drain_epoch()` at line 211
  - Drain implementation (`orchestrator.rs:240-276`): bump epoch (line 244), spin-wait for threads (lines 250-264)
- K4: `BeginAddressAdvancer::advance()` at `begin_address.rs` (after epoch drain, no protection needed)

**Crash-point injection sites**:

| Phase Boundary | Location |
|----------------|----------|
| Before K1 (scan) | `scanner.rs:135` |
| K1→K2 (after scan, before copy) | `orchestrator.rs:~180` |
| K2→K3 (after copy, before swing) | `orchestrator.rs:~190` |
| Mid-K3 (during CAS) | `address_update.rs:194` |
| K3→K4 (after swing, before truncate) | `orchestrator.rs:~216` |
| After K4 | `begin_address.rs` completion |

### 8. Recovery 3-Stage Flow

**Directory**: `faster-core/src/recovery/` (3+ files)

| File | Purpose |
|------|---------|
| `mod.rs` | RecoveryManager, discovery, planning |
| `index_recovery.rs` | Index deserialization + CRC check |
| `log_recovery.rs` | Log replay (fold-over and snapshot) |

**Stage 1 — Discovery & Planning**:
- `discover_checkpoints()` at `mod.rs:245` — enumerate on-disk checkpoints
- `select_checkpoint()` at `mod.rs:267` — pick by token or latest
- `validate_checkpoint()` at `mod.rs:294` — verify artifacts

**Stage 2 — Index Recovery**:
- `recover_index()` at `index_recovery.rs:131`
- CRC32 integrity check at `index_recovery.rs:146` (existing pattern for checksums)
- Bucket loading at `index_recovery.rs:151-157`
- Version restoration at `index_recovery.rs:158-161`

**Stage 3 — Log Recovery**:
- Fold-over: `recover_fold_over()` at `log_recovery.rs:153`
  - Address restoration: begin, head, tail, read_only, flushed_until
  - Optional chain verification at `log_recovery.rs:190-209`
- Snapshot: `recover_snapshot()` at `log_recovery.rs:215`
  - Merges main log + snapshot file

**Crash-point injection sites**:

| Stage Boundary | Location |
|----------------|----------|
| Before discovery | `mod.rs:245` |
| After discovery, before index recovery | `index_recovery.rs:131` |
| After CRC check, before bucket load | `index_recovery.rs:149` |
| After index recovery, before log recovery | `log_recovery.rs:153` |
| During chain verification | `log_recovery.rs:190` |
| After log recovery | Completion |

### 9. Hybrid Log Page Format & Flush Path

**Directory**: `faster-core/src/hybrid_log/`

**PageState enum** (`page.rs:43-56`): `Free(0)→Open(1)→Sealed(2)→Flushing(3)→Flushed(4)→Evicted(5)`. Transitions via atomic CAS in `AtomicPageState` (`page.rs:116-124`).

**PageFrame** (`page.rs:151-163`): Fields: `data: NonNull<u8>` (sector-aligned, 512-byte), `size: usize` (default 32MB), `layout: Layout`, `state: AtomicPageState`, `flushed_until: AtomicU32`.

**Current page format**: NO explicit page header. Records have `RecordInfo` (8 bytes) headers. Variable-size records follow sequentially. No checksums on pages.

**Flush path (async)**: `PageFlusher::flush_page()` at `flush.rs:206`. Device write issued at `flush.rs:252` via `device.write_async()`. Completion callback at `flush.rs:138-175` updates `flushed_until` and transitions state Sealed→Flushing→Flushed.

**Flush path (sync)**: `flush_page_sync()` at `flush.rs:291`. Direct `device.write_sync()` at `flush.rs:322`.

**CRC checksum insertion points**:
- **Write**: Before `device.write_async()` at `flush.rs:252` — compute CRC over page data, append to page footer or store in metadata
- **Verify**: During recovery at `log_recovery.rs:190-209` — after page load, before using page data
- **Existing pattern**: Index recovery already has CRC32 check at `index_recovery.rs:146`

### 10. Epoch Framework

**Directory**: `faster-core/src/epoch/` (5 files, ~1,900 lines)

| File | Lines | Purpose |
|------|-------|---------|
| `table.rs` | 408 | EpochTable — global coordinator |
| `entry.rs` | 94 | EpochEntry — per-thread state |
| `guard.rs` | 221 | EpochGuard, EpochThread — RAII wrappers |
| `drain.rs` | 350 | DrainList — lock-free Treiber stack |
| `mod.rs` | ~50 | Module declarations |

**EpochTable** (`table.rs:47-82`): Fields: `current_epoch: AtomicU64`, `safe_to_reclaim_epoch: AtomicU64`, per-thread `entries` table, `drain_list: DrainList`.

**Critical atomic operations**:

| Operation | File:Line | Ordering | Purpose |
|-----------|-----------|----------|---------|
| Epoch bump | `table.rs:268` | SeqCst | Advance global epoch counter |
| Thread protect | `table.rs:198` | Release | Announce thread's current epoch |
| Thread unprotect | `table.rs:226` | Release | Leave epoch protection |
| Safe-epoch scan | `table.rs:355,361` | Acquire | Read all thread epochs, compute min |
| Drain claim | `drain.rs:181` | AcqRel swap | Atomically claim callback list |
| Drain CAS | `drain.rs:102-103` | AcqRel | Push callback onto Treiber stack |

**EpochThread** (`guard.rs`): RAII wrapper. `protect()` returns `EpochGuard`. Guard's `Drop` calls `unprotect()`.

### 11. Hash Index & CAS Operations

**Directory**: `faster-core/src/hash/` (6 files, ~6,300 lines)

**HashBucketEntry** (`bucket.rs`): 64-bit packed: `[tentative:1|reserved:1|tag:14|address:48]`.

**AtomicHashBucketEntry CAS** (`bucket.rs:411,431`): Two-phase insert protocol:
1. Insert with tentative bit set (CAS at line 411, AcqRel)
2. Commit by clearing tentative bit (CAS at line 431, AcqRel)

**HashBucket**: 7 entries + 1 overflow pointer, 64-byte cache-line aligned.

**HashIndex API** (`index.rs`): `find()`, `find_or_create()`, `update()` at `index.rs:349-361`.

### 12. Store Entry Points

**File**: `store/kv.rs` (2,877 lines)

**FasterKv struct** (kv.rs:~180): Fields: `hash_index`, `allocator: HybridLogAllocator`, `functions: F`, `device: Box<dyn Device>`, `epoch_table: Arc<EpochTable>`, `session_pool`, `flusher`, `evictor`, `pending_io_mgr`, `grow_manager`, `config`, `compaction_lock`, `compaction_policy`.

**Key operation entry points** (all enter epoch protection):

| Operation | File:Line |
|-----------|-----------|
| `read()` | `kv.rs:855` |
| `upsert()` | `kv.rs:899` |
| `rmw()` | `kv.rs:941` |
| `delete()` | `kv.rs:982` |
| `new_session()` | `kv.rs:395` |
| `dispose_session()` | `kv.rs:403` |
| `checkpoint()` | Via checkpoint manager |
| `recover()` | Via recovery manager |
| `compact()` | Via compaction orchestrator |

### 13. Device Trait & Callback Mechanism

**Device trait** (`device.rs:222-280`): `sector_size()`, `segment_size()`, `max_outstanding_io()`, `read_async()`, `write_async()`, `read_sync()`, `write_sync()`, `truncate_until()`, `size()`, `close()`.

**IoCompletionCallback** (`device.rs:33-34`): `unsafe fn(context: *mut u8, status: IoStatus, bytes_transferred: u32)`.

**TypedIoContext<T>** (`device.rs:100-195`): Type-safe context wrapper with use-after-free detection (generation counter in debug builds).

**Callback invocation in SyncFileDevice** (`sync_file_device.rs:298`): Worker thread calls callback after completing I/O.

**Device implementations**:

| Type | File | Threading | Deterministic? |
|------|------|-----------|----------------|
| `NullDevice` | `device.rs:292-384` | None | Yes |
| `InMemoryDevice` | `device.rs:395-550` | None (RwLock) | Yes |
| `SyncFileDevice` | `sync_file_device.rs:307` | Thread pool | No |
| `UringDevice` | `faster-uring/src/device.rs` | Single I/O thread | No |
| `TokioFileDevice` | `faster-tokio/src/device.rs` | Tokio runtime | No |
| `SimulatedDevice` | `faster-dst/src/device.rs:72` | None (sync callbacks) | Yes (seeded) |

## Code References

### Simulation Framework (faster-dst)
- `rust/crates/faster-dst/src/lib.rs:72-86` — Module exports and re-exports
- `rust/crates/faster-dst/src/clock.rs:9-14` — SimulatedClock struct
- `rust/crates/faster-dst/src/device.rs:72-80` — SimulatedDevice struct
- `rust/crates/faster-dst/src/device.rs:26-29` — SimulatedStorage struct
- `rust/crates/faster-dst/src/device.rs:198-356` — Device trait implementation
- `rust/crates/faster-dst/src/fault.rs:7-17` — CrashPoint enum
- `rust/crates/faster-dst/src/fault.rs:23-35` — FaultConfig struct
- `rust/crates/faster-dst/src/harness.rs:21-26` — SimulationHarness struct
- `rust/crates/faster-dst/src/runtime.rs:11-14` — SimulationRuntime struct
- `rust/crates/faster-dst/src/workload.rs:10-22` — Workload trait
- `rust/crates/faster-dst/src/invariant.rs:8-16` — Invariant trait

### Nondeterminism Sources
- `rust/crates/faster-core/src/sync.rs` — cfg-based loom abstraction (model for simulation)
- `rust/crates/faster-uring/src/device.rs:964-969` — uring I/O thread spawn
- `rust/crates/faster-core/src/sync_file_device.rs:360-363` — Sync file worker spawn
- `rust/crates/faster-uring/src/device.rs:962` — mpsc channel (uring commands)
- `rust/crates/faster-core/src/store/kv.rs:266-279` — Timeout-enforced poll loop
- `rust/crates/faster-core/src/checkpoint/orchestrator.rs:273-278` — Flush deadline
- `rust/crates/faster-core/src/checkpoint/orchestrator.rs:296` — SystemTime for token
- `rust/crates/faster-core/src/checkpoint/metadata_store.rs:355` — SystemTime for metadata

### State Machine Transition Points
- `rust/crates/faster-core/src/state/system_state.rs:157-190` — Two-phase CAS protocol
- `rust/crates/faster-core/src/checkpoint/state_machine.rs:204-232` — Phase transition CAS
- `rust/crates/faster-core/src/compaction/orchestrator.rs:154-232` — K1-K4 pipeline
- `rust/crates/faster-core/src/compaction/address_update.rs:194` — Hash index CAS in K3
- `rust/crates/faster-core/src/recovery/mod.rs:245-294` — Recovery discovery and planning

### Checksum Insertion Points
- `rust/crates/faster-core/src/hybrid_log/flush.rs:252` — Write path (insert CRC before device write)
- `rust/crates/faster-core/src/recovery/log_recovery.rs:190-209` — Read path (verify CRC after page load)
- `rust/crates/faster-core/src/recovery/index_recovery.rs:146` — Existing CRC32 pattern for index

### Epoch Framework
- `rust/crates/faster-core/src/epoch/table.rs:268` — bump_current_epoch (SeqCst)
- `rust/crates/faster-core/src/epoch/table.rs:198` — protect (Release)
- `rust/crates/faster-core/src/epoch/table.rs:226` — unprotect (Release)
- `rust/crates/faster-core/src/epoch/drain.rs:102-103` — DrainList push (AcqRel CAS)
- `rust/crates/faster-core/src/epoch/drain.rs:181` — DrainList claim (AcqRel swap)

## Architecture Documentation

**Concurrency model**: Epoch-protected concurrent access. Threads register with EpochTable, enter protection via `protect()`, perform operations, exit via guard drop. Deferred callbacks (e.g., page reclamation) execute when all threads advance past the callback's epoch.

**I/O model**: Callback-based async I/O (NOT Rust async/await). Callers provide `IoCompletionCallback` + context pointer. Device invokes callback on completion (on I/O thread for real devices, synchronously for simulated). `TypedIoContext<T>` provides type-safe lifecycle management.

**State machine pattern**: CAS-based transitions with intermediate state. Two-phase protocol prevents torn reads during phase changes. Intermediate bit (0x80) marks transient state visible only to the transitioning thread.

**Key invariant**: All address boundaries (begin, head, read_only, safe_read_only, tail) move monotonically forward. Never backward.

## Open Questions

- Should the `simulation` feature flag also gate the page checksum code, or should checksums be always-on once added? (Format versioning implications)
- The `sync.rs` re-exports are "not yet used by production code" — should the simulation work also complete the refactor to route production code through `sync.rs`? This would make the cfg-based swap actually effective for production paths.
- `SimulatedDevice` currently calls callbacks synchronously (`CompletedSync`). For simulation of callback ordering nondeterminism, should it queue callbacks and let the scheduler deliver them?
