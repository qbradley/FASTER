# MVP Iteration 4 — Log Compaction, C FFI, Async Adapters, io_uring

**Architect:** Grand Admiral Gandalf
**Status:** 📋 Planned
**Baseline:** 1,158 tests, ~30K lines, zero clippy warnings, miri/loom clean
**Predecessor:** Iteration 3 (Checkpoint/Recovery, Grow, Performance Hardening)

---

## 1. Strategic Overview

### Iteration Goal

Transform FASTER-Rust from a feature-complete but isolated Rust crate into a **production-deployable, multi-language, high-performance storage engine**. Iteration 3 delivered durability (checkpoint/recovery) and an 8M ops/sec hot path. Iteration 4 closes the remaining gaps between our implementation and the mature C++/C# siblings: log compaction for bounded storage growth, a C FFI for cross-language consumption, async runtime integration for modern Rust services, and an io_uring device for kernel-bypass I/O on Linux.

### What Iteration 4 Delivers

1. **Log compaction** — Online garbage collection that reclaims dead/tombstoned records, compacts the active dataset into a contiguous tail region, and advances the log's begin-address. Matches C++/C# `compact()` semantics.
2. **Read cache** — In-memory cache for hot on-disk records, eliminating repeated device I/O for read-heavy workloads. Mirrors the C++ `ReadCache` with per-entry hash-chaining.
3. **C FFI layer** — Opaque-handle C ABI (~20 functions) with `cbindgen`-generated header, enabling consumption from C, Python, Go, Java/JNI, and any FFI-capable language.
4. **Async/Tokio adapter** — `AsyncFasterKv` wrapper that bridges the callback-based core to Tokio's `Future`/`async fn` model, plus a generic `AsyncDevice` trait for non-blocking I/O.
5. **io_uring device** — Linux `io_uring` implementation of the `Device` trait with direct I/O, submission queue batching, and zero-copy completions.
6. **Performance push** — Close the gap to 10M ops/sec: epoch batching, per-session allocation arena, and reduced CAS contention on the tail.
7. **Hardening** — Address all deferred TODOs, ARM memory ordering audit, and copy-to-tail retry for allocation failures.

### What Is Deferred to Iteration 5+

- Multi-key transactions / serializable isolation
- Incremental snapshots (delta log)
- Remote/distributed FASTER (gRPC replication)
- Windows IOCP device
- WAL-based durability (alternative to checkpoint)
- Tiered storage (NVMe → SSD → object store)

### Quality Bar

- Zero clippy warnings (`--all-targets`), `cargo fmt` clean
- All tests pass via `cargo nextest run` in <30s total
- All new concurrent code has loom tests
- All new unsafe code has miri tests
- io_uring device benchmarked against SyncFileDevice (≥2× improvement expected)
- Log compaction: verified via round-trip (write → compact → verify all live keys present, dead keys gone)
- C FFI: tested from C via integration test binary, valgrind clean
- Async adapter: tokio test with `#[tokio::test]` for all CRUD paths
- Performance: ≥10M single-threaded upsert ops/sec on dedicated hardware (or documented path to it)

---

## 2. Architecture

### New Module Layout

```
rust/crates/
├── faster-core/src/
│   ├── (existing modules unchanged)
│   ├── compaction/                     # [ALL NEW]
│   │   ├── mod.rs                      # Public API: CompactionConfig, compact()
│   │   ├── scanner.rs                  # Live-record scanner using LogScanIterator
│   │   ├── copier.rs                   # Record copier: read→filter→append-to-tail
│   │   ├── address_update.rs           # Hash index pointer swing after compaction
│   │   └── policy.rs                   # Compaction triggers (space-amp, tombstone%)
│   └── read_cache/                     # [ALL NEW]
│       ├── mod.rs                      # ReadCache, CacheEntry, eviction
│       ├── chain.rs                    # Hash-chained cache lookup
│       └── metrics.rs                  # Hit/miss/eviction counters
├── faster-ffi/src/                     # [IMPLEMENT — stub exists]
│   ├── lib.rs                          # extern "C" functions
│   ├── handle.rs                       # Opaque handle management
│   └── error.rs                        # C-compatible error codes
├── faster-tokio/src/                   # [IMPLEMENT — stub exists]
│   ├── lib.rs                          # AsyncFasterKv, AsyncSession
│   ├── bridge.rs                       # Callback→Future bridge (Waker registration)
│   └── device.rs                       # AsyncDevice trait + TokioFileDevice
└── faster-uring/                       # [ALL NEW]
    ├── Cargo.toml
    └── src/
        ├── lib.rs                      # UringDevice impl
        ├── ring.rs                     # io_uring submission/completion queue wrapper
        └── buffer.rs                   # Registered buffer management
```

### Key Design Decisions

| Decision | Rationale |
|----------|-----------|
| Compaction as copy-forward (not in-place) | Matches C++/C# design; simpler correctness, no page-level locking, works with immutable log invariant |
| Read cache as separate hash chain | Avoids polluting main hash index with cache metadata; cache misses are free (single pointer check) |
| FFI via opaque handles + error codes | Standard C ABI pattern; no memory ownership ambiguity; `cbindgen` generates header automatically |
| Callback→Future via `Waker` registration | Zero-cost bridge: pending ops register a `Waker`, completion callback wakes the `Future`; no extra allocation |
| io_uring with registered buffers | Avoids per-I/O kernel buffer copy; batch submission amortizes syscall overhead |
| Compaction before read cache | Read cache benefits from compaction (smaller working set to cache); compaction is higher priority for production |

---

## 3. Work Items

### Track Overview

| Track | Prefix | Items | Description |
|-------|--------|-------|-------------|
| Compaction | K | K1–K6 | Log compaction / garbage collection |
| Read Cache | RC | RC1–RC4 | In-memory cache for on-disk records |
| C FFI | F | F1–F5 | C-compatible foreign function interface |
| Async/Tokio | T | T1–T4 | Async runtime adapter |
| io_uring | U | U1–U4 | Linux kernel-bypass I/O device |
| Performance | P | P1–P3 | 10M ops/sec push |
| Hardening | H | H1–H3 | Deferred TODOs and edge cases |

### Wave 0: Foundation & Hardening

*Independent fixes and infrastructure. No cross-track dependencies. Execute first to clean the slate.*

| Item | Title | Description | Deps | Size | Test Strategy |
|------|-------|-------------|------|------|---------------|
| H1 | Address deferred TODOs | Fix all remaining TODOs/FIXMEs: (1) copy-to-tail retry on allocation failure in `process_completed_io`, (2) `sync_file_device` alignment checks → release assertions, (3) `flush.rs` bytes_transferred validation, (4) `drain_up_to` Vec pre-allocation based on profiling. | — | M | Existing tests pass. New test: allocation-failure retry path. |
| H2 | ARM memory ordering audit | Audit all `Acquire`/`Release`/`AcqRel` orderings for ARM weak-memory correctness. The `snapshot()` non-atomicity (C-2 from Iteration 3) is benign on x86 TSO but may need `SeqCst` fence on ARM. Add `cfg(target_arch)` conditional fences where needed. | — | M | Cross-compile check `--target aarch64-unknown-linux-gnu`. Loom tests verify ordering. |
| H3 | Metrics & observability polish | Wire `tracing` spans into all I/O paths, checkpoint phases, and compaction. Add `metrics` feature counters for: cache hit/miss, compaction bytes reclaimed, pending I/O in-flight. Ensure zero overhead when features disabled. | — | S | Feature-gate test: compile with/without `metrics`/`tracing`, verify no binary size diff when off. |

### Wave 1: Log Compaction

*The highest-priority production feature. Enables bounded storage growth.*

| Item | Title | Description | Deps | Size | Test Strategy |
|------|-------|-------------|------|------|---------------|
| K1 | Compaction scanner | Create `compaction/scanner.rs`: wrap `LogScanIterator` to classify records as Live (in hash index and points to this address), Dead (superseded by newer record), or Tombstoned (deleted). Emit `CompactionPlan` with live-record addresses and total bytes. | — | M | Unit: scan log with known dead/live mix, verify classification. Property: scanner never classifies a live record as dead. |
| K2 | Record copier | Create `compaction/copier.rs`: read live records from the scan, append them sequentially to the log tail (reusing the hybrid log's `try_allocate` path). Handle variable-length records. Return `CopyResult` mapping old→new addresses. | K1 | L | Integration: copy 1K live records, verify all readable at new addresses. Test: variable-length records copied correctly. |
| K3 | Hash index pointer swing | Create `compaction/address_update.rs`: for each copied record, CAS the hash index entry from old address to new address. Handle concurrent readers (they may still hold old address — epoch protection ensures safety). Tombstoned entries are removed from the hash index. | K2 | L | Loom: concurrent read + pointer swing. Test: after swing, all reads return correct values. Test: tombstones removed from index. |
| K4 | Begin-address advance | After pointer swing completes and epoch drains, advance `begin_address` past the compacted region. Truncate device segments below new begin-address via `Device::truncate_until()`. Update metadata for next checkpoint. | K3 | M | Integration: compact, verify begin_address advanced. Test: old addresses are no longer accessible. Test: device segments truncated. |
| K5 | Compaction policy engine | Create `compaction/policy.rs`: `CompactionPolicy` trait with implementations: (1) `SpaceAmplificationPolicy` — trigger when log_size / live_data_size > threshold (default 2×), (2) `TombstonePercentPolicy` — trigger when tombstone% > threshold (default 25%), (3) `ManualPolicy` — explicit `compact()` call only. | K1 | S | Unit: policy triggers at correct thresholds. |
| K6 | Compaction orchestration | Wire K1–K5 into `FasterKv::compact()` public API. Add `CompactionConfig` to `FasterKvConfig`. Compaction runs on a background thread, coordinated via epoch system. Multiple concurrent compactions are serialized via `Mutex`. Add compaction to `maintenance()` when auto-policy triggers. | K4, K5 | L | Integration: write 100K records, delete 50K, compact, verify: (a) 50K live records intact, (b) storage size reduced, (c) tombstones gone. Stress: compact during concurrent writes. |

### Wave 2: Read Cache

*Accelerates read-heavy workloads. Can execute in parallel with Wave 1 K5–K6.*

| Item | Title | Description | Deps | Size | Test Strategy |
|------|-------|-------------|------|------|---------------|
| RC1 | Read cache data structure | Create `read_cache/mod.rs`: fixed-size in-memory page pool (separate from hybrid log pages). Cache entries are hash-chained from `HashBucket.read_cache_address` field (new field, or use tag bits in existing entries). LRU eviction when pool is full. | — | L | Unit: insert/lookup/evict. Property: cache never returns stale data. Test: LRU eviction order correct. |
| RC2 | Cache-aware read path | Modify `internal_read` in `operations.rs`: before issuing device I/O for on-disk records, check read cache. On cache hit, return directly (no I/O). On cache miss after device read completes, insert into cache via `try_copy_to_read_cache`. | RC1 | M | Integration: read on-disk record twice — first read triggers I/O, second hits cache. Benchmark: cache hit latency vs device read latency. |
| RC3 | Cache invalidation | Invalidate cache entries on: (1) upsert/RMW that creates a new version, (2) delete, (3) compaction address swing. Use epoch-safe invalidation (mark entry as invalid, reclaim after epoch drain). | RC1, RC2, K3 | M | Test: upsert invalidates cached read. Test: compaction doesn't serve stale cached data. Loom: concurrent read + invalidate. |
| RC4 | Cache configuration & metrics | Add `ReadCacheConfig` to `FasterKvBuilder`: `enable_read_cache(bool)`, `read_cache_size_pages(usize)`, `read_cache_eviction_policy(LRU|FIFO)`. Wire hit/miss/eviction counters into metrics system. | RC2, RC3, H3 | S | Test: disabled cache has zero overhead. Test: metrics counters accurate. |

### Wave 3: C FFI Layer

*Cross-language interface. Independent of Waves 1–2. Can execute in parallel.*

| Item | Title | Description | Deps | Size | Test Strategy |
|------|-------|-------------|------|------|---------------|
| F1 | Opaque handle system | Create `faster-ffi/src/handle.rs`: type-erased handle table mapping `u64` handles to `Box<dyn Any>`. Thread-safe (RwLock). `faster_store_open()` → handle, `faster_store_close(handle)` → drop. Handle validation on every call. | — | M | Unit: open/close lifecycle. Test: invalid handle returns error code. Test: double-close is safe. Miri: no UB in handle table. |
| F2 | Core CRUD FFI functions | Implement in `faster-ffi/src/lib.rs`: `faster_upsert(store, key_ptr, key_len, val_ptr, val_len)`, `faster_read(store, key_ptr, key_len, val_buf, val_buf_len, val_out_len)`, `faster_delete(store, key_ptr, key_len)`, `faster_rmw(store, key_ptr, key_len, input_ptr, input_len)`. All return `FasterStatus` enum (C int). Use `Vec<u8>` key/value internally. | F1 | L | Integration test from C: upsert→read→delete round-trip. Valgrind: zero leaks. Test: buffer-too-small returns BUFFER_TOO_SMALL status. |
| F3 | Session FFI | `faster_session_open(store)` → session handle, `faster_session_close(store, session)`, `faster_complete_pending(store, session)`. Sessions are per-thread — document that sharing across threads is UB. | F1, F2 | M | C test: multi-threaded sessions. Test: session from wrong store returns error. |
| F4 | cbindgen header generation | Add `cbindgen.toml`, configure for C11 output. Generate `faster.h` automatically in build.rs or as cargo xtask. Header includes all public functions, error codes enum, opaque handle typedef. | F2, F3 | S | CI: header generation doesn't fail. Compile C test against generated header. |
| F5 | Checkpoint/recovery FFI | `faster_checkpoint(store, type)` → token, `faster_recover(store, token)` → status. Expose `FasterCheckpointType` enum (FoldOver=0, Snapshot=1). | F3 | M | C test: checkpoint→close→reopen→recover→verify data. |

### Wave 4: Async/Tokio Adapter

*Bridges core to async Rust. Requires stable core APIs (Waves 0–1).*

| Item | Title | Description | Deps | Size | Test Strategy |
|------|-------|-------------|------|------|---------------|
| T1 | Callback→Future bridge | Create `faster-tokio/src/bridge.rs`: `PendingFuture<T>` that wraps a pending operation. On creation, register a `Waker` into the pending-op's context. Completion callback calls `waker.wake()`. `poll()` checks completion flag. Zero-allocation for the bridge itself (Waker is stored in-line). | — | L | Unit: mock completion wakes future. Test: future resolves with correct value. Tokio test: spawn 100 concurrent pending ops. |
| T2 | AsyncSession | Create `faster-tokio/src/lib.rs`: `AsyncSession` wrapping `FasterSession`. Methods: `async fn read()`, `async fn upsert()`, `async fn rmw()`, `async fn delete()`. For in-memory operations (non-Pending), return immediately. For Pending, return `PendingFuture`. | T1 | M | `#[tokio::test]`: async CRUD round-trip. Test: mixed sync/async returns. Test: `!Send` enforced on `AsyncSession`. |
| T3 | AsyncFasterKv | Create wrapper: `AsyncFasterKv` that owns `FasterKv` + manages a Tokio task for `maintenance()` polling. `async fn new_session()`, `async fn compact()`, `async fn checkpoint()`. Background maintenance task runs on `tokio::task::spawn_blocking`. | T2 | M | Tokio test: full lifecycle (open, CRUD, checkpoint, close). Test: maintenance task shuts down cleanly on drop. |
| T4 | TokioFileDevice | Create `faster-tokio/src/device.rs`: `Device` implementation that uses `tokio::fs` for async file I/O. Submission goes through `spawn_blocking` to maintain the completion-callback contract. Benchmark against `SyncFileDevice`. | T1 | L | Tokio test: read/write round-trip. Benchmark: throughput comparison vs SyncFileDevice. |

### Wave 5: io_uring Device

*Linux-specific high-performance I/O. Independent of Waves 3–4.*

| Item | Title | Description | Deps | Size | Test Strategy |
|------|-------|-------------|------|------|---------------|
| U1 | io_uring ring wrapper | Create `faster-uring/src/ring.rs`: safe wrapper around `io_uring` syscalls (via `io-uring` crate). Manages SQ/CQ ring pair, pre-allocated SQEs, and registered file descriptors. Configurable queue depth (default 256). | — | L | Unit: submit read+write, verify completion. Test: queue-full backpressure. Test: registered FD lifecycle. |
| U2 | Buffer registration | Create `faster-uring/src/buffer.rs`: pre-registered I/O buffers via `io_uring_register_buffers`. Aligned to sector size. Buffer pool with checkout/return semantics (lock-free Treiber stack). | U1 | M | Unit: register/unregister buffers. Test: alignment invariants. Miri: no UB in buffer pool. |
| U3 | UringDevice implementation | Create `faster-uring/src/lib.rs`: implement `Device` trait. `read_async`/`write_async` submit SQEs and stash the completion callback. A reaper thread polls the CQ and invokes callbacks. Batch submission via `io_uring_submit()`. Direct I/O via `O_DIRECT` flag. | U1, U2 | XL | Integration: write+read 1000 pages, verify correctness. Benchmark: vs SyncFileDevice on sequential and random I/O. Stress: 64 concurrent I/O ops. |
| U4 | Adaptive batching | Implement submission coalescing: accumulate SQEs for a configurable window (default 1µs or 32 SQEs, whichever comes first) before calling `io_uring_submit()`. Reduces syscall overhead under high load. Bypass batching for sync operations. | U3 | M | Benchmark: batching vs immediate submit under varying loads. Test: latency doesn't degrade under low load (batch window expires). |

### Wave 6: Performance Push

*Final optimization pass targeting 10M ops/sec. Requires Waves 0–1.*

| Item | Title | Description | Deps | Size | Test Strategy |
|------|-------|-------------|------|------|---------------|
| P1 | Epoch batching | Amortize epoch protect/unprotect across multiple operations. Add `session.batch_begin(n)` / `batch_end()` that enters epoch once for n operations. Reduces epoch table CAS from 1-per-op to 1-per-batch. | H1 | M | Benchmark: batch-of-100 vs individual. Loom: batched epoch still drains correctly. Test: drain callback fires between batches. |
| P2 | Per-session allocation arena | Give each session a thread-local pre-allocated page slab for tail appends. Eliminates CAS contention on the global tail address for the common case (slab has space). Fall back to global CAS when slab exhausted. | P1 | L | Benchmark: 8-thread upsert throughput. Loom: arena→global fallback is correct. Test: session drop returns slab pages. |
| P3 | Performance validation (10M gate) | Re-run YCSB benchmarks on dedicated hardware (or best-available). Target: ≥10M single-threaded upsert ops/sec, ≥40M 8-thread upsert ops/sec. Generate comparison report against Iteration 3 baseline. Document remaining bottlenecks if target not met. | P1, P2, K6 | S | Benchmark suite runs clean. Results documented. |

---

## 4. Execution Graph

```
Wave 0 (Foundation):        H1 ─────┐    H2    H3
                                     │
Wave 1 (Compaction):   K1 → K2 → K3 → K4 ──→ K6
                       K1 ──────────→ K5 ──→ K6
                                               │
Wave 2 (Read Cache):  RC1 → RC2 → RC3 ──→ RC4 │
                                    ↑           │
                              (K3 feeds RC3)    │
                                                │
Wave 3 (C FFI):       F1 → F2 → F3 → F4       │
                            F2 → F5             │
                                                │
Wave 4 (Async):       T1 → T2 → T3             │
                       T1 → T4                  │
                                                │
Wave 5 (io_uring):    U1 → U2 → U3 → U4       │
                                                │
Wave 6 (Perf):        H1 → P1 → P2 → P3 ←─────┘
```

### Parallelism Strategy

| Phase | Agents | What's Running |
|-------|--------|----------------|
| Phase A | 5 | H1, H2, H3, K1, F1 (all independent) |
| Phase B | 5 | K2, RC1, F2, T1, U1 (all independent, different crates/modules) |
| Phase C | 5 | K3, RC2, F3, T2, U2 |
| Phase D | 4 | K4+K5, RC3, F4+F5, U3 |
| Phase E | 4 | K6, RC4, T3+T4, U4 |
| Phase F | 2 | P1→P2→P3 (sequential, needs compaction done) |

**Estimated agent-hours:** ~160h across all items
**With 5 parallel agents:** ~35–40h wall-clock
**Critical path:** K1→K2→K3→K4→K6→P3 (compaction→performance validation)

---

## 5. Dependency Matrix

| Item | Hard Dependencies | Soft Dependencies | Safe to Parallelize With |
|------|-------------------|-------------------|--------------------------|
| H1–H3 | — | — | Everything (Wave 0) |
| K1 | — | — | F1, RC1, T1, U1, H* |
| K2 | K1 | — | F2, RC1, T1, U1 |
| K3 | K2 | — | RC2, F3, T2, U2 |
| K4 | K3 | — | RC3, F4, U3 |
| K5 | K1 | — | K2, K3, K4, anything |
| K6 | K4, K5 | — | RC4, T3, U4 |
| RC1 | — | — | K1, K2, F1, T1, U1 |
| RC2 | RC1 | — | K3, F3, T2, U2 |
| RC3 | RC1, RC2, K3 | — | F4, U3 |
| RC4 | RC2, RC3, H3 | — | K6, T3, U4 |
| F1 | — | — | K1, RC1, T1, U1, H* |
| F2 | F1 | — | K2, RC1, T1, U1 |
| F3 | F1, F2 | — | K3, RC2, T2, U2 |
| F4 | F2, F3 | — | K4, RC3, U3 |
| F5 | F3 | — | K4, RC3, U3 |
| T1 | — | — | K1, F1, RC1, U1, H* |
| T2 | T1 | — | K3, F3, RC2, U2 |
| T3 | T2 | — | K4, F4, RC3, U3 |
| T4 | T1 | — | K3, F3, RC2, U2 |
| U1 | — | — | K1, F1, RC1, T1, H* |
| U2 | U1 | — | K3, F3, T2, RC2 |
| U3 | U1, U2 | — | K4, F4, T3, RC3 |
| U4 | U3 | — | K6, F5, T3, RC4 |
| P1 | H1 | — | K4, K5, RC3, F4 |
| P2 | P1 | — | K6, RC4, F5 |
| P3 | P1, P2, K6 | — | — (final gate) |

---

## 6. Testing Strategy

### Per-Track Test Requirements

| Track | Unit | Integration | Property | Loom | Miri | Benchmark | C/FFI |
|-------|------|-------------|----------|------|------|-----------|-------|
| Compaction (K) | ✓ | ✓ (write→compact→verify) | ✓ (no live record lost) | ✓ (K3 pointer swing) | ✓ (K2 record copy) | ✓ (K6 throughput) | — |
| Read Cache (RC) | ✓ | ✓ (cache hit path) | ✓ (no stale reads) | ✓ (RC3 invalidation) | — | ✓ (hit vs miss latency) | — |
| C FFI (F) | ✓ | ✓ (C test binary) | — | — | ✓ (handle table) | — | ✓ (valgrind) |
| Async (T) | ✓ | ✓ (`#[tokio::test]`) | — | — | — | ✓ (vs sync baseline) | — |
| io_uring (U) | ✓ | ✓ (read/write round-trip) | — | — | ✓ (buffer pool) | ✓ (vs SyncFileDevice) | — |
| Performance (P) | — | — | — | ✓ (P1 epoch batch) | — | ✓ (10M gate) | — |
| Hardening (H) | ✓ | ✓ | — | ✓ (H2) | ✓ (H1) | — | — |

### Key Correctness Invariants to Test

1. **Compaction safety:** No live record is ever lost. After compaction, every key that was readable before is still readable with the same value.
2. **Cache coherence:** A read never returns a value older than the most recent completed upsert/RMW for that key.
3. **FFI memory safety:** No leaked handles, no use-after-free, no buffer overflows. Valgrind-clean.
4. **Async correctness:** Every `Pending` operation eventually resolves. No dropped wakers.
5. **io_uring correctness:** Every submitted I/O either completes successfully or returns an error. No silent data corruption.

---

## 7. Risk Register

| ID | Risk | Severity | Probability | Mitigation |
|----|------|----------|-------------|------------|
| R1 | Compaction + concurrent writes race condition | High | Medium | Epoch-gated pointer swing; exhaustive loom tests; property test (no live record lost) |
| R2 | Read cache introduces stale reads | High | Low | Invalidation on every write path; loom test for cache+write interleaving |
| R3 | io_uring crate API instability | Medium | Low | Pin `io-uring` crate version; wrap in our own safe abstraction layer |
| R4 | FFI handle table becomes bottleneck | Low | Low | RwLock for reads (hot path), exclusive lock only for open/close (cold path) |
| R5 | Callback→Future bridge drops waker | High | Low | Unit test: every pending op resolves. Stress test: 10K concurrent futures. |
| R6 | 10M ops/sec still not reached | Medium | Medium | Profile-guided optimization. Document architectural ceiling if hit. |
| R7 | ARM cross-compile reveals ordering bugs | Medium | Medium | H2 audit before other work. CI cross-compile gate. |
| R8 | Compaction during checkpoint creates inconsistency | High | Medium | Serialize compaction with checkpoint (compaction waits for checkpoint-in-progress to complete and vice versa) |

---

## 8. Definition of Done

- [ ] All 37 work items implemented and code-reviewed
- [ ] Zero clippy warnings (`cargo clippy --all-targets -p faster-core -p faster-ffi -p faster-tokio -p faster-uring -- -D warnings`)
- [ ] `cargo fmt` clean across all crates
- [ ] All existing tests still pass (regression-free)
- [ ] New test count: ≥1,400 total (estimated ~250 new tests)
- [ ] Loom tests pass for all new concurrent code
- [ ] Miri tests pass for all new unsafe code
- [ ] C FFI integration test passes (compile C test, run, valgrind-clean)
- [ ] `#[tokio::test]` suite passes for async adapter
- [ ] io_uring device benchmark shows ≥2× throughput vs SyncFileDevice
- [ ] Compaction round-trip verified: write 100K → delete 50K → compact → all 50K live keys intact
- [ ] Performance benchmark: ≥10M single-threaded ops/sec (or documented path)
- [ ] QUICKSTART.md updated with compaction and async examples
- [ ] Blog post documenting Iteration 4

---

## 9. Execution Notes

### Agent Assignment Strategy

| Track | Primary Agent(s) | Rationale |
|-------|-------------------|-----------|
| Compaction (K) | Boba Fett (reverse eng) + Sabine (Rust) | Compaction requires deep understanding of log internals + record layout |
| Read Cache (RC) | Gimli (storage) + Sabine (Rust) | Storage-layer expertise + read path optimization |
| C FFI (F) | Legolas (C++) + Sabine (Rust) | Cross-language FFI requires both C and Rust expertise |
| Async/Tokio (T) | Hera (Tokio) + Sabine (Rust) | Tokio-specific expertise critical for correct bridge design |
| io_uring (U) | Gimli (storage) + Vader (systems) | Low-level kernel I/O + systems programming |
| Performance (P) | Galadriel (perf) + Vader (systems) | Profiling expertise + systems-level optimization |
| Hardening (H) | Boromir (QA) + Saruman (security) | QA thoroughness + security audit for ARM/FFI |

### Conflict-Free Module Ownership

To enable maximum parallelism, each track operates in isolated modules:
- **K track:** `src/compaction/` — no overlap with other tracks
- **RC track:** `src/read_cache/` — touches `operations.rs` read path (coordinate with K3)
- **F track:** `crates/faster-ffi/` — separate crate, no source conflicts
- **T track:** `crates/faster-tokio/` — separate crate, no source conflicts
- **U track:** `crates/faster-uring/` — separate crate, no source conflicts
- **P track:** `src/epoch/` + `src/allocator.rs` — coordinate with K3/K4 (both touch epoch)

**Shared file bottleneck:** `store/kv.rs` will need modifications from K6 (compact API), RC2 (read path), and P1 (epoch batching). Serialize these items or use feature branches.
