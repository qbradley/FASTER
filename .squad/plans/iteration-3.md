# MVP Iteration 3 — Checkpoint/Recovery, Performance Hardening, Hash Grow

**Architect:** Grand Admiral Thrawn
**Status:** 📋 Planned
**Baseline:** 656 tests, 22K+ lines, zero clippy warnings
**Predecessor:** Iteration 2 (Hybrid Log + CRUD + Storage)

---

## 1. Strategic Overview

### Iteration Goal

Transform FASTER from a fast in-memory KV store into a **durable, production-grade database** with checkpoint/recovery, while hardening the hot path to **10M+ single-threaded ops/sec** and enabling dynamic hash table resizing.

### What Iteration 3 Delivers

1. **Performance hardening** — 3× single-threaded throughput gain via SeqCst elimination, snapshot-based chain traversal, and zero-copy in-place upsert
2. **Checkpoint & recovery** — Fuzzy checkpoints (fold-over and snapshot modes), metadata persistence, full recovery protocol with session continuity
3. **Hash table grow** — Online doubling with chunk-based parallel splitting, zero-downtime resizing
4. **Pending I/O completion** — Wired into FasterKv operations, `complete_pending()` API, timeout/cancellation
5. **Ordered shutdown** — `FasterKv::Drop` drains in-flight I/O before teardown
6. **Log scan/iterator** — Forward iteration over the hybrid log for analytics and compaction prep
7. **Public API polish** — Builder pattern, ergonomic session API, comprehensive error types

### What Is Deferred to Iteration 4+

- C FFI layer
- Async runtime adapters (tokio, async-std)
- io_uring device / DirectIO device
- Read cache
- Log compaction / GC
- Incremental snapshots (delta log)
- C-2: `snapshot()` non-atomicity (benign on x86 — revisit for ARM)
- C-4: `drain_up_to` Vec allocation (profile-guided optimization)

### Quality Bar

- Zero clippy warnings, `cargo fmt` clean
- All tests pass via `cargo nextest run` in <10s total
- Individual tests <1s unless justified
- All new concurrent code has loom tests
- All new unsafe code has miri tests
- >90% line coverage on new code
- Performance validated: ≥10M single-threaded upsert ops/sec
- Checkpoint round-trip: write → crash → recover → verify all data intact

---

## 2. Architecture

### New Module Layout

```
src/
├── (existing modules unchanged)
├── checkpoint/                    # [ALL NEW]
│   ├── mod.rs                     # Public API, CheckpointType enum
│   ├── metadata.rs                # IndexRecoveryInfo, LogRecoveryInfo, tokens
│   ├── manager.rs                 # CheckpointManager trait + FileCheckpointManager
│   ├── state_machine.rs           # CheckpointStateMachine, phases, transitions
│   ├── index_checkpoint.rs        # Hash index serialization to device
│   ├── log_checkpoint.rs          # HybridLog fold-over and snapshot modes
│   └── recovery.rs                # Full recovery protocol
├── grow/                          # [ALL NEW]
│   ├── mod.rs                     # GrowState, public API
│   ├── state_machine.rs           # PREPARE → IN_PROGRESS → REST
│   └── split.rs                   # Chunk-based parallel bucket splitting
└── scan/                          # [ALL NEW]
    └── mod.rs                     # LogIterator, ScanCursor
```

### Key Design Decisions

| Decision | Rationale |
|----------|-----------|
| Fold-over checkpoint first, snapshot second | Fold-over is simpler (no separate file), validates protocol before adding snapshot complexity |
| CheckpointManager trait | Decouples persistence strategy from checkpoint protocol; enables testing with InMemoryCheckpointManager |
| Epoch-coordinated phase transitions | Matches C++/C# protocol; threads advance independently, epoch ensures visibility |
| Chunk-based parallel grow (16K buckets/chunk) | Proven C++/C# design; enables lock-free concurrent splitting |
| SeqCst→Acquire downgrade with benchmark proof | Must measure before/after; ~150 cycles/op on x86 per C++ measurements |
| AddressInfo snapshot passed through chain traversal | Eliminates 4-6 redundant atomic loads per chain hop; single snapshot at entry point |
| Raw pointer upsert for Copy types | Eliminates deserialize→clone→serialize triple-copy; single `ptr::write` for u64 |

---

## 3. Work Items

### Wave 0: Performance Hardening (SF-4, SF-5, SF-6)

*Unlocks the 10M ops/sec target. Independent of all other waves. Execute first.*

| Item | Title | Description | Deps | Size | Test Strategy |
|------|-------|-------------|------|------|---------------|
| P1 | Benchmark baseline snapshot | Run existing YCSB benchmarks, record baseline numbers (sequential upsert, read, RMW, 8-thread, latency). Save results as JSON for before/after comparison. | — | S | Verify benchmark runs clean |
| P2 | SeqCst→Acquire in log allocator | Downgrade `log_allocator.rs` atomic orderings: `tail_address` CAS → AcqRel, all boundary getters → Acquire, `snapshot()` loads → Acquire. Preserve SeqCst only for cross-boundary ordering where required. ~15 atomic operations to audit. | P1 | M | Loom test: concurrent allocate+shift_read_only. All existing tests pass. Benchmark: measure improvement. |
| P3 | AddressInfo snapshot in chain traversal | Modify `find_record_for_key()` in `operations.rs` to accept `AddressInfo` parameter. Replace `allocator.is_in_memory(addr)` calls with `info.classify(addr)`. Thread `AddressInfo` from `internal_read`/`internal_upsert`/`internal_rmw`/`internal_delete`. | — | M | All CRUD tests pass. Benchmark read+upsert throughput. |
| P4 | Zero-copy in-place upsert | Add `Functions::upsert_in_place_raw()` with `*mut u8` pointer path. For `Copy` types, replace deserialize→clone→serialize with single `ptr::write`. Add `SimpleFunctions` specialization. Gate behind `is_copy: bool` associated const on Functions trait. | P3 | M | Miri test: raw pointer write safety. Round-trip correctness test. Benchmark upsert throughput. |
| P5 | Performance validation | Re-run YCSB benchmarks. Validate ≥10M single-threaded upsert ops/sec. Generate comparison report against P1 baseline. If target not met, profile with `perf` and iterate. | P2, P3, P4 | S | Benchmark comparison passes threshold |

### Wave 1: FasterKv Lifecycle & Pending I/O

*Foundation for checkpoint. Must complete before checkpoint waves.*

| Item | Title | Description | Deps | Size | Test Strategy |
|------|-------|-------------|------|------|---------------|
| L1 | FasterKv::Drop ordered shutdown | Implement `Drop for FasterKv<F>`: (1) signal shutdown to flusher, (2) drain all pending flush callbacks with timeout, (3) drop device last. Resolves SF-10 by ensuring PageTable outlives all flush callbacks. | — | M | Test: drop during active flushes doesn't panic. Test: all flushed pages committed before drop completes. Miri test for lifetime safety. |
| L2 | Wire pending I/O into operations | Connect `PendingIoManager` to `internal_read` and `internal_rmw` for on-disk records. When record is in OnDisk region, enqueue pending read via `PendingIoManager::submit()`. Store `PendingContext` in session's pending queue. | L1 | L | Integration test: insert records, evict pages, read back via pending I/O path. Verify correct values returned. |
| L3 | Session complete_pending() API | Add `FasterSession::complete_pending()` that polls pending ops queue, checks completion flags, invokes `Functions::read_completion_callback` / `rmw_completion_callback`. Add `complete_pending_with_drain(wait: bool)` variant that spins until all pending ops complete. | L2 | M | Test: submit 100 pending reads, complete_pending() returns all. Test: timeout path. Test: callback invocation order. |
| L4 | Pending I/O timeout & cancellation | Add configurable timeout to `PendingIoContext` (default: 5s). Add `cancel()` method. On timeout, return `OperationStatus::Error` with `FasterError::IoTimeout`. Add metrics counter for timed-out operations. Resolves SF-14. | L3 | M | Test: mock slow device, verify timeout fires. Test: cancel mid-flight. Test: metrics increment. |

### Wave 2: Checkpoint Infrastructure

*Metadata types, persistence interface, state machine. Parallel with Wave 1 items L2-L4.*

| Item | Title | Description | Deps | Size | Test Strategy |
|------|-------|-------------|------|------|---------------|
| C1 | Checkpoint metadata types | Create `checkpoint/metadata.rs`: `IndexRecoveryInfo` (table_size, num_buckets, num_ht_bytes, num_ofb_bytes, begin_address, checkpoint_start_address), `LogRecoveryInfo` (version, flushed_address, final_address, snapshot_start, use_snapshot_file, session_commit_points), `CheckpointToken` (Uuid), `CheckpointType` enum (FoldOver, Snapshot). Serde serialization. | — | M | Unit: round-trip serialize/deserialize. Property: arbitrary metadata → serialize → deserialize = identity. |
| C2 | CheckpointManager trait | Create `checkpoint/manager.rs`: trait with `commit_index_checkpoint()`, `commit_log_checkpoint()`, `get_index_device()`, `get_log_device()`, `get_snapshot_device()`, `get_latest_checkpoint()`, `get_recovery_info()`. Implement `FileCheckpointManager` (directory-based) and `InMemoryCheckpointManager` (for tests). | C1 | M | Unit: FileCheckpointManager writes/reads metadata files. Integration: multiple checkpoints, get_latest returns newest. |
| C3 | Checkpoint state machine | Create `checkpoint/state_machine.rs`: phases `REST → PREP_INDEX → INDEX_CHECKPOINT → PREPARE → IN_PROGRESS → WAIT_PENDING → WAIT_FLUSH → PERSISTENCE_CALLBACK → REST`. Each phase transition gated by epoch (all threads must reach phase before advancing). System version tracking. | C1 | L | Loom test: 4 threads advancing through phases. Unit: phase ordering invariants. Test: version monotonically increases. |
| C4 | Session checkpoint participation | Extend `FasterSession` with: `checkpoint_version` tracking, `AtomicSwitch()` for version transition, `CommitPoint` (serial_no + excluded_serials) capture. Sessions report completion per phase. Add `CHECKPOINT_PHASES` epoch slots (already reserved in epoch table). | C3 | M | Test: session version switch during IN_PROGRESS. Test: CommitPoint capture is accurate. Loom: concurrent sessions switching versions. |

### Wave 3: Checkpoint Execution

*Actually writing checkpoint data. Requires Wave 2 complete.*

| Item | Title | Description | Deps | Size | Test Strategy |
|------|-------|-------------|------|------|---------------|
| C5 | Index checkpoint writer | Create `checkpoint/index_checkpoint.rs`: serialize hash table main array + overflow buckets to device via chunked async writes (chunk size = min(2GB, table_size)). Track completion via atomic counter + notification. Delete tentative entries before write. | C2, C3 | L | Test: checkpoint 1M-entry index, verify file size matches. Test: overflow buckets included. Test: async completion callback fires. |
| C6 | HybridLog fold-over checkpoint | Create `checkpoint/log_checkpoint.rs` fold-over path: shift read_only to tail, flush all pages, record flushed_address and final_address in metadata. No separate snapshot file needed. | C4, L1 | L | Integration: write 10K records, fold-over checkpoint, verify all data flushed. Test: metadata addresses are correct. |
| C7 | HybridLog snapshot checkpoint | Add snapshot path to `log_checkpoint.rs`: capture mutable region (safe_read_only..tail) to separate snapshot device. Write snapshot file via `CheckpointManager::get_snapshot_device()`. Record snapshot addresses in metadata. | C6 | L | Integration: write records, snapshot checkpoint, verify snapshot file contains mutable portion. Test: main log unchanged. |
| C8 | Checkpoint metadata persistence | Wire metadata writing into state machine's PERSISTENCE_CALLBACK phase: collect session CommitPoints, compute checksums, write `IndexRecoveryInfo` + `LogRecoveryInfo` via CheckpointManager. Atomic commit (write temp → rename). | C5, C6 | M | Test: metadata file exists after checkpoint. Test: checksum validates. Test: corrupt metadata → error on recovery. |
| C9 | Full checkpoint orchestration | Add `FasterKv::checkpoint(type: CheckpointType) → CheckpointToken` API. Orchestrate: start state machine, drive phases via `maintenance()` loop, return token on completion. Support both FoldOver and Snapshot types. | C8, C7 | L | Integration: full checkpoint end-to-end. Test: concurrent operations continue during checkpoint. Test: token is valid UUID. |

### Wave 4: Recovery

*Reading checkpoint data back. Requires Wave 3 complete.*

| Item | Title | Description | Deps | Size | Test Strategy |
|------|-------|-------------|------|------|---------------|
| R1 | Recovery metadata loading | Create `checkpoint/recovery.rs`: `find_recovery_info()` locates latest valid checkpoint via CheckpointManager. Validate index/log compatibility (index.final_address ≤ log.final_address). Return `RecoveryPlan` with devices and metadata. | C8 | M | Test: find latest among 3 checkpoints. Test: incompatible index/log → error. Test: missing metadata → error. |
| R2 | Index recovery | Read hash table from index device in chunked reads. Rebuild overflow bucket allocator state. Delete tentative entries. Verify table_size matches or reinitialize to new size. | R1, C5 | L | Test: checkpoint → recover → index lookups return same results. Test: overflow chains survive round-trip. Miri: chunked read safety. |
| R3 | Log recovery — fold-over | Read hybrid log from main device. Scan from start_address to final_address. For each record in fuzzy region (checkpoint_version ≤ checkpoint version), update hash index pointers. Rebuild address boundaries. | R2, C6 | L | Integration: write 10K records, checkpoint, simulate crash (drop without flush), recover, verify all reads correct. |
| R4 | Log recovery — snapshot | Read from snapshot device for mutable portion. Merge with main log for read-only/on-disk portions. Apply same record scanning as fold-over but from snapshot file. | R3, C7 | L | Integration: snapshot checkpoint → recover → verify. Test: snapshot + main log merge produces correct state. |
| R5 | Session recovery | Restore session CommitPoints from metadata. Resume sessions with correct serial numbers and excluded serials. Provide `FasterKv::recover() → RecoveryResult` with session info. | R3 | M | Test: 4 sessions checkpoint, recover, sessions resume at correct serial numbers. Test: excluded serials are skipped on replay. |
| R6 | Checkpoint/recovery integration tests | End-to-end scenarios: (1) write→checkpoint→recover→verify, (2) concurrent writes during checkpoint→recover, (3) multiple checkpoints→recover latest, (4) fold-over vs snapshot produce same recovered state, (5) crash during checkpoint→recover from previous. | R4, R5 | L | 10+ integration test scenarios. Stress: 100K records, multi-threaded writes during checkpoint. |

### Wave 5: Hash Table Grow

*Independent of checkpoint. Can execute in parallel with Waves 3-4.*

| Item | Title | Description | Deps | Size | Test Strategy |
|------|-------|-------------|------|------|---------------|
| G1 | GrowState and grow metadata | Create `grow/mod.rs`: `GrowState` (old_version, new_version, num_chunks, num_pending_chunks, next_chunk), chunk_size = 16384 buckets. Extend `HashIndex` with version tracking and dual-table support (table_[0], table_[1]). | — | M | Unit: GrowState initialization. Unit: version flip. Test: chunk count calculation for various table sizes. |
| G2 | Grow state machine | Create `grow/state_machine.rs`: `REST → PREPARE_GROW → IN_PROGRESS_GROW → REST`. PREPARE_GROW: epoch barrier, allocate new table at 2× size. IN_PROGRESS_GROW: threads grab chunks and split. Completion: free old table, flip version. | G1 | L | Loom test: 4 threads through grow phases. Test: state transitions are monotonic. Test: epoch barrier ensures all threads observe new state. |
| G3 | Chunk-based bucket splitting | Create `grow/split.rs`: for each chunk, iterate old buckets. For each entry: hash → check high bit → place in left bucket (i) or right bucket (old_size + i). Handle overflow chains. Allocate new overflow buckets as needed. CAS on `splitStatus[chunk]` to claim work. | G2 | XL | Test: split 1M entries, verify all findable in new table. Test: overflow chains preserved. Property: for all keys, lookup(key) returns same value before and after grow. Loom: concurrent split + lookup. |
| G4 | Wire grow into FasterKv | Add `FasterKv::grow_index()` API. Add load-factor monitoring to `maintenance()`: if load_factor > 0.75, trigger grow. Add `entry_count` atomic counter (resolves existing TODO). Coordinate with checkpoint state machine (no grow during checkpoint). | G3, C3 | L | Integration: insert until load factor > 0.75, verify auto-grow triggers. Test: grow + concurrent CRUD. Test: grow blocked during checkpoint. |

### Wave 6: Log Scan, API Polish, Documentation

*Dependent on Waves 0-5 for complete API surface. Some items can start earlier.*

| Item | Title | Description | Deps | Size | Test Strategy |
|------|-------|-------------|------|------|---------------|
| S1 | Log scan / iterator API | Create `scan/mod.rs`: `LogIterator` implementing `Iterator<Item = LogRecord>`. Forward scan from begin_address to tail. Handle page boundaries, skip tombstoned records, yield key-value pairs. Support both in-memory and on-disk scanning. | L2 | L | Test: insert 1K records, scan returns all in address order. Test: deleted records skipped. Test: scan across page boundaries. Test: scan on-disk records via device reads. |
| A1 | FasterKv builder pattern | Add `FasterKvBuilder<F>` with fluent API: `.hash_index_size(n)`, `.buffer_pages(n)`, `.mutable_fraction(f)`, `.device(d)`, `.checkpoint_manager(m)`, `.build() → Result<FasterKv<F>>`. Validate config at build time. Deprecate direct `new()`. | C2 | M | Test: builder produces valid store. Test: invalid config → descriptive error. Test: default builder works. |
| A2 | Ergonomic session API | Add `FasterKv::session_scope(f: impl FnOnce(&mut Session))` for RAII session management. Add `Session::read_value(key) → Option<Value>` convenience method. Ensure sessions auto-dispose on drop. Improve error messages for common mistakes. | L3 | M | Test: session_scope auto-disposes. Test: read_value returns None for missing key. Test: double-dispose is safe. |
| A3 | Error handling audit | Audit all `unwrap()`, `expect()`, and error paths. Replace panics with `Result` returns where appropriate. Add `FasterError::CheckpointError`, `RecoveryError`, `GrowError` variants. Ensure all public APIs return `Result`. | C9, G4 | M | Test: every error variant is constructable and displayable. Test: no panics on invalid input (fuzz-like property tests). |
| A4 | Documentation and examples | Rustdoc for all public types and methods. README update with checkpoint/recovery usage. Example programs: basic_kv, checkpoint_recovery, concurrent_sessions. Doc-tests for all public APIs. | All | L | `cargo doc --no-deps` clean. All doc-tests pass. Examples compile and run. |

---

## 4. Execution Graph

```
Wave 0 (Performance):    P1 ─────────────────────────────────────────────┐
                          ├─→ P2 (SeqCst)  ───────────────────────┐      │
                          ├─→ P3 (AddressInfo) ──→ P4 (zero-copy) ┤      │
                          │                                        ├─→ P5 (validate)
                          │                                        │
Wave 1 (Lifecycle):      L1 (Drop) ──→ L2 (pending wire) ──→ L3 (complete_pending)
                                                                   │
                                                              L4 (timeout) ──┐
                                                                              │
Wave 2 (Ckpt Infra):    C1 (metadata) ──→ C2 (manager trait) ─┐              │
                                           │                    │              │
                                           C3 (state machine) ─┤              │
                                                                │              │
                                           C4 (session ckpt) ──┘              │
                                                                │              │
Wave 3 (Ckpt Execute):                    C5 (index write) ────┤              │
                                           C6 (fold-over) ──────┤              │
                                           C7 (snapshot) ────────┤              │
                                           C8 (persist meta) ────┤              │
                                           C9 (orchestrate) ─────┘              │
                                                                │               │
Wave 4 (Recovery):       R1 (find meta) ──→ R2 (index) ──→ R3 (fold-over) ──┐ │
                                                             R4 (snapshot) ──┤ │
                                                             R5 (sessions) ──┤ │
                                                             R6 (integ) ─────┘ │
                                                                                │
Wave 5 (Grow):           G1 (state) ──→ G2 (machine) ──→ G3 (split) ──→ G4 ───┘
                         (parallel with Waves 2-4)                              │
                                                                                │
Wave 6 (Polish):         S1 (scan) ───────────────────────────────────────┐     │
                         A1 (builder) ────────────────────────────────────┤     │
                         A2 (session ergonomics) ─────────────────────────┤     │
                         A3 (errors) ─────────────────────────────────────┤     │
                         A4 (docs) ───────────────────────────────────────┘─────┘
```

### Critical Path

```
P1 → P2 ─┐
P3 → P4 ─┤→ P5 → L1 → L2 → L3 → C4 → C6 → C8 → C9 → R1 → R2 → R3 → R6
          │
C1 → C2 → C3 ───┘
```

**Length:** 18 items on critical path
**Parallelism opportunities:**
- P2 ‖ P3 (independent hot-path optimizations)
- Wave 5 (Grow) runs entirely parallel to Waves 2-4
- C1/C2 start parallel with L2/L3/L4
- S1, A1, A2 can start once their minimal deps are met
- R4, R5 parallel with R3

---

## 5. Dependency Matrix

| Item | Hard Dependencies | Soft Dependencies | Can Parallelize With |
|------|-------------------|-------------------|----------------------|
| P1 | — | — | Everything |
| P2 | P1 | — | P3, P4 |
| P3 | — | P1 (for measurement) | P2, C1 |
| P4 | P3 | — | P2, C1, C2 |
| P5 | P2, P3, P4 | — | L1, C1 |
| L1 | — | — | P2, P3, P4, C1 |
| L2 | L1 | — | C1, C2, C3 |
| L3 | L2 | — | C2, C3, G1 |
| L4 | L3 | — | C3, C4, G1, G2 |
| C1 | — | — | All Wave 0, L1, L2 |
| C2 | C1 | — | P-all, L-all, G1 |
| C3 | C1 | — | L-all, G1, G2 |
| C4 | C3 | L3 (session model) | G1, G2 |
| C5 | C2, C3 | — | C6, G2, G3 |
| C6 | C4, L1 | — | C5, G2, G3 |
| C7 | C6 | — | G3 |
| C8 | C5, C6 | C7 | G3, G4 |
| C9 | C8, C7 | — | G4 |
| R1 | C8 | — | G3, G4 |
| R2 | R1, C5 | — | G4 |
| R3 | R2, C6 | — | R4, R5 |
| R4 | R3, C7 | — | R5 |
| R5 | R3 | — | R4 |
| R6 | R4, R5 | — | G4 |
| G1 | — | — | All Waves 0-2 |
| G2 | G1 | — | C-all, R-all |
| G3 | G2 | — | C5-C9, R-all |
| G4 | G3, C3 | — | R-all |
| S1 | L2 | — | C-all, G-all |
| A1 | C2 | — | Everything after C2 |
| A2 | L3 | — | C-all, G-all |
| A3 | C9, G4 | — | R6, A4 |
| A4 | All | — | — (last) |

---

## 6. Complexity Budget

| Size | Estimate | Items | Total |
|------|----------|-------|-------|
| S (1-2h) | 2 items | P1, P5 | ~4h |
| M (2-4h) | 15 items | P2, P3, P4, L1, L3, L4, C1, C2, C4, C8, R1, R5, A1, A2, A3 | ~45h |
| L (4-8h) | 11 items | L2, C3, C5, C6, C7, C9, R2, R3, R4, R6, S1, A4 | ~66h |
| XL (8-12h) | 1 item | G3 | ~10h |
| **Total** | **29 items** | | **~125h agent-time** |

With 3-4 parallel agents: **~35-40 hours wall-clock**

---

## 7. Testing Strategy

### Per-Item Test Matrix

| Item | Unit | Property | Loom | Miri | Integration | Benchmark |
|------|------|----------|------|------|-------------|-----------|
| P1 | — | — | — | — | — | ✓ (baseline) |
| P2 | ✓ | — | ✓ (ordering) | — | ✓ | ✓ |
| P3 | ✓ | — | — | — | ✓ | ✓ |
| P4 | ✓ | ✓ (round-trip) | — | ✓ (raw ptr) | ✓ | ✓ |
| P5 | — | — | — | — | — | ✓ (validate) |
| L1 | ✓ | — | — | ✓ (lifetime) | ✓ | — |
| L2 | ✓ | — | — | — | ✓ | — |
| L3 | ✓ | — | — | — | ✓ | — |
| L4 | ✓ | — | — | — | ✓ | — |
| C1 | ✓ | ✓ (serde) | — | — | — | — |
| C2 | ✓ | — | — | — | ✓ (file I/O) | — |
| C3 | ✓ | ✓ (ordering) | ✓ (phases) | — | — | — |
| C4 | ✓ | — | ✓ (version) | — | ✓ | — |
| C5 | ✓ | — | — | ✓ (chunks) | ✓ | — |
| C6 | — | — | — | — | ✓ | — |
| C7 | — | — | — | — | ✓ | — |
| C8 | ✓ | ✓ (checksum) | — | — | ✓ | — |
| C9 | — | — | — | — | ✓ | — |
| R1 | ✓ | — | — | — | ✓ | — |
| R2 | ✓ | — | — | ✓ (chunks) | ✓ | — |
| R3 | — | — | — | — | ✓ | — |
| R4 | — | — | — | — | ✓ | — |
| R5 | ✓ | — | — | — | ✓ | — |
| R6 | — | — | — | — | ✓ (10 scenarios) | — |
| G1 | ✓ | — | — | — | — | — |
| G2 | ✓ | ✓ (phases) | ✓ (4 threads) | — | — | — |
| G3 | ✓ | ✓ (all keys found) | ✓ (concurrent) | — | ✓ | ✓ |
| G4 | — | — | — | — | ✓ | ✓ |
| S1 | ✓ | ✓ (all records) | — | — | ✓ | — |
| A1 | ✓ | — | — | — | ✓ | — |
| A2 | ✓ | — | — | — | ✓ | — |
| A3 | ✓ | ✓ (no panics) | — | — | — | — |
| A4 | — | — | — | — | doc-tests | — |

### Key Integration Test Scenarios

1. **Checkpoint round-trip**: Write 10K records → checkpoint → drop store → recover → read all 10K → verify values
2. **Concurrent checkpoint**: 4 threads writing while checkpoint executes → recover → no data loss
3. **Multiple checkpoints**: Take 3 checkpoints → recover from latest → verify
4. **Fold-over vs snapshot equivalence**: Same data, both modes → recover → identical results
5. **Crash during checkpoint**: Simulate crash mid-flush → recover from previous valid checkpoint
6. **Grow under load**: Insert until load factor > 0.75 → grow triggers → concurrent reads return correct values
7. **Grow + checkpoint interaction**: Grow completes → checkpoint → recover → table at new size
8. **Pending I/O completion**: Evict pages → read evicted records → complete_pending() → all values correct
9. **Scan correctness**: Insert 1K, delete 200 → scan → exactly 800 records
10. **Full lifecycle**: Build → populate → checkpoint → grow → more writes → checkpoint → recover → verify everything

### Stress Tests

1. **Checkpoint storm**: 100 rapid checkpoints with concurrent writes — no corruption
2. **Grow cascade**: Insert enough to trigger 3 consecutive grows — all lookups correct
3. **Pending I/O saturation**: 10K concurrent pending reads — all complete within timeout
4. **Session churn during checkpoint**: Create/destroy sessions while checkpoint in progress

---

## 8. Risk Register

| ID | Risk | Severity | Probability | Mitigation |
|----|------|----------|-------------|------------|
| R-1 | SeqCst downgrade introduces ordering bugs on ARM | High | Low | Loom tests cover all orderings. CI includes ARM cross-check. Conservative: only downgrade to Acquire (never Relaxed) for address boundaries. |
| R-2 | Checkpoint state machine deadlock (epoch barrier + phase transition) | High | Medium | Model state machine with loom. Add timeout to epoch barriers. Instrument phase transitions with tracing. Fallback: abandon checkpoint on timeout. |
| R-3 | Recovery fails on corrupted checkpoint metadata | Medium | Medium | Checksum validation on all metadata. Fall back to previous checkpoint on corruption. Test with intentionally corrupted files. |
| R-4 | Hash grow interleaves with checkpoint causing inconsistent state | High | Medium | Mutex: no grow during checkpoint (checked in G4). State machine ensures mutual exclusion. Test both orderings: grow-then-checkpoint and checkpoint-then-grow. |
| R-5 | Snapshot checkpoint correctness (mutable region captured while writes continue) | High | Medium | Epoch-gated version switch ensures all threads see snapshot boundary. Copy-on-write for mutable pages during snapshot. Verify via hash comparison of snapshot vs recovery. |
| R-6 | Performance regression from checkpoint hooks in hot path | Medium | Low | Checkpoint hooks are in maintenance() path, not per-operation. Version check is single atomic load (cheap). Benchmark with checkpoint enabled vs disabled. |
| R-7 | Log iterator encountering partially-written records | Medium | Medium | Use record_info.is_valid() + tentative bit checks. Skip invalid records. Add tombstone detection. Test with intentionally truncated pages. |
| R-8 | Chunk-based grow correctness with overflow chains | High | Medium | Property test: for all keys, lookup(grow(index, key)) == lookup(index, key). Test deep chains (depth 10+). Test boundary conditions (bucket exactly full). |

---

## 9. SoT Finding Resolution Map

| Finding | Item | Resolution Strategy |
|---------|------|---------------------|
| SF-4: SeqCst overuse in allocator | P2 | Audit all 15 atomics in log_allocator.rs. Downgrade to Acquire/AcqRel. Validate with loom + benchmarks. |
| SF-5: Redundant atomic loads in chain traversal | P3 | Pass AddressInfo snapshot through find_record_for_key(). Eliminate 4-6 loads per chain hop. |
| SF-6: Triple-copy value path | P4 | Add upsert_in_place_raw() with raw pointer path. Specialize for Copy types. |
| SF-10: FlushCallbackContext raw pointer lifetime | L1 | FasterKv::Drop drains pending flush callbacks before dropping PageTable. |
| SF-14: No timeout/cancellation for pending I/O | L4 | Add configurable timeout + cancel() to PendingIoContext. |
| C-2: Snapshot non-atomicity | Deferred | Benign on x86; revisit for ARM in iteration 4. |
| C-4: drain_up_to Vec allocation | Deferred | Profile-guided; optimize only if profiling shows hot. |

---

## 10. Definition of Done

MVP Iteration 3 is complete when ALL of the following are true:

- [ ] All 29 work items implemented and committed
- [ ] `cargo nextest run` passes all tests (target: 800+)
- [ ] `cargo clippy --all-targets` zero warnings
- [ ] `cargo fmt --check` clean
- [ ] Sequential upsert ≥ 10M ops/sec (3× improvement from 3.3M baseline)
- [ ] Checkpoint round-trip: write → checkpoint → crash → recover → verify ✓
- [ ] Both fold-over and snapshot checkpoint modes working
- [ ] Hash table grow doubles table under concurrent load without data loss
- [ ] Pending I/O completion wired end-to-end with timeout/cancellation
- [ ] FasterKv::Drop cleanly shuts down with no resource leaks
- [ ] Log iterator correctly scans full hybrid log
- [ ] Builder pattern and ergonomic session API available
- [ ] All integration test scenarios (10+) pass
- [ ] SoT review of complete iteration with findings addressed

---

## 11. Wave Execution Summary

| Wave | Items | Est. Agent-Hours | Parallelism | Critical Path? |
|------|-------|-------------------|-------------|----------------|
| 0: Performance | P1-P5 | ~15h | P2 ‖ P3; P4 after P3 | ✓ (start) |
| 1: Lifecycle | L1-L4 | ~14h | Sequential chain | ✓ (middle) |
| 2: Ckpt Infra | C1-C4 | ~14h | C1‖L1; C2‖C3 | ✓ (C3→C4) |
| 3: Ckpt Execute | C5-C9 | ~24h | C5 ‖ C6; C7 after C6 | ✓ (C6→C8→C9) |
| 4: Recovery | R1-R6 | ~28h | R4 ‖ R5 after R3 | ✓ (R1→R3→R6) |
| 5: Grow | G1-G4 | ~22h | Entire wave ‖ Waves 2-4 | No (parallel track) |
| 6: Polish | S1,A1-A4 | ~18h | Most items ‖ | No (tail) |

### Recommended Execution Order

**Sprint 1 (Performance + Foundation):**
- Agent A: P1 → P2 → P5 (SeqCst track)
- Agent B: P3 → P4 (chain traversal track)
- Agent C: L1 → L2 (lifecycle track)
- Agent D: C1 → C2 (metadata track)

**Sprint 2 (Checkpoint + Grow):**
- Agent A: C3 → C4 → C6 (state machine → fold-over)
- Agent B: C5 → C8 (index checkpoint → metadata persist)
- Agent C: L3 → L4 (pending I/O completion)
- Agent D: G1 → G2 → G3 (grow track, fully parallel)

**Sprint 3 (Recovery + Polish):**
- Agent A: C7 → C9 (snapshot → orchestration)
- Agent B: R1 → R2 → R3 → R5 (recovery chain)
- Agent C: R4 → R6 (snapshot recovery + integration)
- Agent D: G4 → S1 (grow wire-up + scan)

**Sprint 4 (API + Documentation):**
- Agent A: A1 → A2 (builder + session ergonomics)
- Agent B: A3 → A4 (errors + docs)
- Agent C: Final integration testing pass
- Agent D: SoT review preparation
