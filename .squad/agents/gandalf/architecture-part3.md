# Rust FASTER Architecture — Part 3: Verification, Phasing, Decisions, and Risks

**Author:** Gandalf (Lead / System Architect)
**Date:** 2026-03-05
**Scope:** Sections 11–14 of the Rust FASTER architecture document
**Status:** North-star reference — all implementation work derives from this document

---

## 11. Testing Strategy

The testing strategy is layered: fast unit tests gate every commit, integration and property tests run on every PR, deterministic simulation and fuzzing run nightly, and benchmarks run on dedicated hardware weekly. The goal is not merely "coverage" but **confidence in correctness under adversarial conditions at planetary scale**.

### 11.1 Unit Testing (Per-Module)

Every module gets a `#[cfg(test)] mod tests` block co-located with the implementation. Unit tests are pure, deterministic, and fast (< 1 second each).

| Module | Key Unit Tests |
|--------|---------------|
| **Epoch** | Acquire/release semantics; safe-to-reclaim advances correctly; drain list callbacks fire at correct epoch; reentrant acquire works; thread entry allocation/deallocation; max thread limit enforcement |
| **Hash Index** | Bucket entry packing/unpacking (48-bit address + 14-bit tag + 2 status bits round-trip); overflow bucket allocation and linking; tag extraction from 64-bit hash; `FindEntry` / `FindOrCreateEntry` on single-threaded index; tentative bit CAS semantics |
| **Record Format** | `RecordInfo` bitfield packing (previous_address, checkpoint_version, invalid, tombstone, final bits); key/value alignment padding calculation; serialization round-trip for fixed-size and variable-length records; record size computation |
| **Address** | `Address` newtype: page/offset extraction, special address constants (`kInvalidAddress`, `kMaxAddress`); read-cache bit manipulation; address comparison and ordering |
| **Hybrid Log (Allocator)** | Page allocation and tail-address bumping; mutable/read-only/head address boundary tracking; page-to-physical-address translation; buffer pool acquire/release |
| **Device Trait** | `NullDevice` passes all trait methods; `FileDevice` basic read/write round-trip; alignment enforcement (512-byte sector); segment file creation and naming |
| **Status/Error** | `Status` flag composition (Found, Pending, InPlaceUpdated, etc.); `Error` variant construction; `Result<Status, Error>` ergonomics |
| **Checkpoint Metadata** | `info.dat` serialization/deserialization round-trip; version field forward compatibility; session commit point encoding |
| **MallocFixedPageSize** | Block allocation/deallocation; free list correctness; page growth; alignment guarantees |
| **C FFI** | Opaque handle creation and destruction; status code translation; null pointer safety checks |

**Conventions:**
- Use `#[test]` for synchronous unit tests. No async runtime in core tests.
- Use `assert_eq!` with descriptive messages. Prefer structured assertions over raw `assert!`.
- Test both the happy path and every documented error condition.
- Bitfield tests must cover boundary values: 0, 1, max, max-1, and random mid-range values.

### 11.2 Integration Testing (End-to-End)

Integration tests live in `tests/` at the crate root and exercise the full `FasterKv` stack: hash index → hybrid log → device → checkpoint/recovery.

#### Core CRUD Integration

```
test_upsert_read_single_key
test_upsert_overwrite_returns_latest
test_rmw_counter_increment
test_rmw_initial_value_on_missing_key
test_delete_then_read_returns_not_found
test_delete_then_upsert_resurrects_key
test_read_on_disk_returns_pending_then_completes
test_upsert_never_goes_pending
test_mixed_operations_1000_keys
```

#### Multi-Threaded Concurrent Operations

```
test_concurrent_upsert_disjoint_keys_{2,4,8,16}_threads
test_concurrent_upsert_overlapping_keys
test_concurrent_read_while_upsert
test_concurrent_rmw_same_key_{2,4,8,16}_threads
test_concurrent_delete_while_read
test_concurrent_grow_index_during_operations
test_concurrent_checkpoint_during_operations
test_session_isolation_no_cross_contamination
```

#### Checkpoint/Recovery Integration

```
test_foldover_checkpoint_and_recover
test_snapshot_checkpoint_and_recover
test_incremental_snapshot_checkpoint_and_recover
test_recover_with_session_resume
test_recover_commit_point_exclusions
test_multiple_checkpoints_recover_latest
test_index_checkpoint_avoids_log_replay
test_checkpoint_during_active_operations
```

#### Storage Integration

```
test_operations_with_file_device
test_operations_with_null_device
test_page_flush_and_readback
test_segmented_file_growth
test_io_completion_callbacks_fire
test_pending_operations_complete_after_io
```

**Conventions:**
- Each integration test creates a fresh `FasterKv` instance in a `tempdir`.
- Multi-threaded tests use `std::thread::scope` (no async runtime).
- Tests that exercise disk I/O clean up temp directories on success; leave them on failure for debugging.
- Timeout: integration tests must complete within 30 seconds. Tests that need longer are moved to the nightly suite.

### 11.3 Property-Based Testing (proptest / quickcheck)

Property tests express invariants that must hold for **all** valid inputs, not just hand-picked examples. We use `proptest` for its shrinking capabilities.

#### Hash Index Invariants

```rust
proptest! {
    // Every inserted key is findable
    #[test]
    fn insert_then_find(keys in prop::collection::vec(any::<u64>(), 1..10000)) {
        let index = HashIndex::new(1 << 20);
        for &k in &keys {
            index.find_or_create_entry(hash(k), tag(k));
        }
        for &k in &keys {
            assert!(index.find_entry(hash(k), tag(k)).is_some());
        }
    }

    // Concurrent inserts: no entry is lost
    #[test]
    fn concurrent_insert_no_loss(
        keys in prop::collection::vec(any::<u64>(), 1..5000),
        num_threads in 2..=8usize,
    ) {
        // Partition keys across threads, insert concurrently, verify all findable
    }

    // Overflow chains: length bounded by insert count for distinct tags
    #[test]
    fn overflow_chain_bounded(keys in prop::collection::vec(any::<u64>(), 1..1000)) {
        // Insert keys, verify no overflow chain exceeds expected length
    }
}
```

#### Hybrid Log Address Space Consistency

```rust
proptest! {
    // tail_address >= readonly_address >= head_address >= begin_address (always)
    #[test]
    fn address_ordering_invariant(ops in vec(arbitrary_operation(), 1..10000)) {
        let store = FasterKv::new(config);
        for op in ops {
            apply_operation(&store, op);
            assert!(store.tail_address() >= store.readonly_address());
            assert!(store.readonly_address() >= store.head_address());
            assert!(store.head_address() >= store.begin_address());
        }
    }

    // Every allocated address is within [begin_address, tail_address)
    #[test]
    fn allocated_address_in_range(ops in vec(arbitrary_upsert(), 1..5000)) {
        // Track all allocated addresses, verify bounds
    }
}
```

#### Serialization Round-Trip

```rust
proptest! {
    // RecordInfo: pack → unpack = identity
    #[test]
    fn record_info_roundtrip(
        prev_addr in 0u64..((1 << 48) - 1),
        version in 0u16..((1 << 13) - 1),
        invalid in any::<bool>(),
        tombstone in any::<bool>(),
    ) {
        let info = RecordInfo::new(prev_addr, version, invalid, tombstone);
        let packed = info.to_u64();
        let unpacked = RecordInfo::from_u64(packed);
        assert_eq!(info, unpacked);
    }

    // Address: page/offset extraction round-trip
    #[test]
    fn address_roundtrip(page in 0u32..((1 << 23) - 1), offset in 0u32..((1 << 25) - 1)) {
        let addr = Address::new(page, offset);
        assert_eq!(addr.page(), page);
        assert_eq!(addr.offset(), offset);
    }

    // Variable-length record: serialize → deserialize = identity
    #[test]
    fn varlen_record_roundtrip(
        key in prop::collection::vec(any::<u8>(), 0..1024),
        value in prop::collection::vec(any::<u8>(), 0..4096),
    ) {
        let mut buf = vec![0u8; record_size(&key, &value)];
        serialize_record(&mut buf, &key, &value);
        let (k, v) = deserialize_record(&buf);
        assert_eq!(k, &key[..]);
        assert_eq!(v, &value[..]);
    }
}
```

#### Epoch Safety Properties

```rust
proptest! {
    // No callback fires before all threads have advanced past its epoch
    #[test]
    fn epoch_drain_safety(
        thread_count in 2..=16usize,
        ops in vec(arbitrary_epoch_op(), 1..1000),
    ) {
        // Model: track which epochs each thread protects
        // Verify: no drain callback fires while any thread holds an earlier epoch
    }
}
```

### 11.4 Deterministic Simulation Testing

This is the crown jewel of our testing strategy. Led by **Éowyn** (Deterministic Simulation Testing Expert), the simulation framework provides reproducible exploration of concurrent interleavings, fault injection, and crash recovery scenarios.

#### Architecture

```
┌─────────────────────────────────────────────┐
│         Deterministic Test Harness           │
├─────────────────────────────────────────────┤
│  DeterministicScheduler                     │
│    - Seed-based PRNG for thread scheduling  │
│    - Single-threaded executor with virtual  │
│      thread contexts                        │
│    - Controllable: round-robin, random,     │
│      adversarial (priority inversion, etc.) │
├─────────────────────────────────────────────┤
│  SimulatedDevice                            │
│    - Implements Device trait                 │
│    - Controllable latency, bandwidth        │
│    - Fault injection points:                │
│      · Read errors (EIO)                    │
│      · Write errors (ENOSPC, EIO)           │
│      · Partial writes (torn pages)          │
│      · Latency spikes                       │
│      · Reordered completions                │
├─────────────────────────────────────────────┤
│  CrashSimulator                             │
│    - Captures full store state at any point │
│    - Simulates power loss (drop all         │
│      in-flight I/O, truncate partial writes)│
│    - Restores from checkpoint, verifies     │
│      consistency                            │
├─────────────────────────────────────────────┤
│  LinearizabilityChecker                     │
│    - Records operation history              │
│    - Verifies linearizable ordering exists  │
│    - Reports witness (linearization order)  │
│      or counterexample on failure           │
└─────────────────────────────────────────────┘
```

#### Custom Deterministic Scheduler

The scheduler replaces real thread scheduling with a seed-controlled PRNG. All "threads" execute cooperatively in a single OS thread, yielding at explicit yield points (atomic operations, I/O submissions, epoch acquire/release).

```rust
struct DeterministicScheduler {
    rng: StdRng,                          // Seeded PRNG
    threads: Vec<SimThread>,              // Virtual thread contexts
    schedule: SchedulePolicy,             // Random, round-robin, adversarial
    yield_points_executed: u64,           // For progress tracking
    history: Vec<ScheduleEvent>,          // For replay on failure
}

enum SchedulePolicy {
    Random,                               // Uniform random next-thread
    RoundRobin,                           // Deterministic ordering
    Adversarial(AdversarialConfig),       // Maximize contention
    Replay(Vec<usize>),                   // Replay exact schedule
}
```

**Yield point injection:** The core library's atomic operations and I/O calls are parameterized over a `Scheduler` trait. In production, `NoopScheduler` compiles away. In simulation, `DeterministicScheduler` intercepts every CAS, load, store, and I/O call.

#### Fault Injection Scenarios

| Fault | Injection Point | Verification |
|-------|----------------|--------------|
| **Disk read error** | `SimulatedDevice::read_async` returns `Err(IoError)` | Operation returns error; store remains consistent; retry succeeds |
| **Disk write error** | `SimulatedDevice::write_async` returns `Err(IoError)` | Checkpoint fails gracefully; no data corruption; retry succeeds |
| **Partial write (torn page)** | `SimulatedDevice::write_async` writes only first N bytes | Recovery detects torn page via checksum; falls back to prior checkpoint |
| **Power loss during checkpoint** | `CrashSimulator` kills store mid-WAIT_FLUSH phase | Recovery from last complete checkpoint succeeds; no operations lost beyond commit point |
| **Power loss during normal operation** | `CrashSimulator` kills store at random point | Recovery succeeds; all checkpointed data intact; pending ops may be lost (documented) |
| **I/O latency spike** | `SimulatedDevice` adds 100ms+ delay | Operations complete correctly; no timeout-induced corruption |
| **I/O completion reordering** | `SimulatedDevice` delivers completions out of submission order | Correct behavior (FASTER must tolerate this) |
| **Out-of-disk-space** | `SimulatedDevice::write_async` returns `ENOSPC` | Graceful failure; store can resume after space freed |

#### Crash Recovery Verification Protocol

```
For each seed S in {0..10000}:
  1. Create FasterKv with SimulatedDevice(seed=S)
  2. Execute N random operations (upsert, read, rmw, delete)
  3. Take checkpoint
  4. Execute M more random operations (post-checkpoint)
  5. Record expected state: all ops through checkpoint commit point
  6. CrashSimulator.crash()  // Kill store, discard in-flight I/O
  7. Recover from checkpoint
  8. For each key: verify value matches expected state at commit point
  9. Post-checkpoint operations: verify either present (if completed before crash)
     or absent (if in-flight) — no partial or corrupted state
```

#### Linearizability Checking

For concurrent operation histories, we verify **linearizability**: every concurrent execution is equivalent to some sequential execution that respects real-time ordering.

```rust
struct LinearizabilityChecker {
    history: Vec<Operation>,  // (thread_id, op_type, key, invoke_time, response_time, result)
}

impl LinearizabilityChecker {
    /// Returns Ok(linearization) or Err(counterexample)
    fn check(&self) -> Result<Vec<Operation>, CounterExample> {
        // Wing & Gong (WGL) algorithm or Lowe's algorithm
        // Exponential worst-case but fast in practice for FASTER-style workloads
    }
}
```

We check linearizability for:
- Concurrent reads and upserts to overlapping key sets
- Concurrent RMW operations on the same key
- Operations spanning mutable/read-only/disk boundaries
- Operations during checkpoint phase transitions
- Operations during hash table grow

### 11.5 Benchmark Suite

Led by **Legolas** (Performance Guru). Benchmarks are not tests — they measure, they don't assert. But benchmark regressions block releases.

#### YCSB-Style Workloads

We implement the six standard YCSB (Yahoo! Cloud Serving Benchmark) workloads:

| Workload | Mix | Description | Key Distribution |
|----------|-----|-------------|-----------------|
| **A** | 50% read, 50% update | Update-heavy | Zipfian |
| **B** | 95% read, 5% update | Read-mostly | Zipfian |
| **C** | 100% read | Read-only | Zipfian |
| **D** | 95% read, 5% insert | Read-latest | Latest |
| **E** | 95% scan, 5% insert | Short ranges | Zipfian |
| **F** | 50% read, 50% RMW | Read-modify-write | Zipfian |

**Configuration matrix:**
- Key size: 8 bytes (fixed), 16 bytes, variable (16–256 bytes)
- Value size: 8 bytes, 100 bytes, 1 KB, variable (100 bytes–10 KB)
- Record count: 10M, 100M, 1B
- Thread count: 1, 2, 4, 8, 16, 32, 64
- In-memory fraction: 100% (all in DRAM), 50%, 10% (mostly on disk)

#### Comparison Benchmarks

Run identical workloads on:
1. **Rust FASTER** (this implementation)
2. **C++ FASTER** (`cc/` in this repo)
3. **C# FASTER** (`cs/` in this repo)

**Output:** Side-by-side throughput (ops/sec) and latency distribution. Target: Rust within 10% of C++ throughput, lower tail latency.

#### Latency Distribution Tracking

Every benchmark captures the full latency distribution using HdrHistogram:

```
P50:    typical operation latency
P90:    start of tail
P99:    tail latency (SLA-relevant)
P99.9:  deep tail (incident-relevant)
P99.99: extreme tail (capacity planning)
Max:    worst-case single operation
```

**Tracked per operation type:** read (in-memory), read (pending/disk), upsert, rmw (in-place), rmw (copy-update), delete.

#### Throughput Scaling

Measure throughput vs. thread count to identify:
- **Linear scaling range** (expected: 1–8 threads)
- **Plateau point** (expected: 16–32 threads, depending on NUMA topology)
- **Contention cliff** (if any — indicates design flaw)
- **NUMA effects** (cross-socket penalty, if applicable)

Plot: throughput (Mops/sec) on Y-axis, thread count on X-axis, one line per workload.

#### Micro-Benchmarks

| Benchmark | What It Measures |
|-----------|-----------------|
| `bench_hash_function` | Hash computation throughput (keys/sec) |
| `bench_epoch_acquire_release` | Epoch overhead per operation |
| `bench_hash_index_lookup` | Index lookup latency (cache-hot vs. cache-cold) |
| `bench_record_serialize` | Record serialization throughput (bytes/sec) |
| `bench_page_allocate` | Hybrid log allocation throughput |
| `bench_cas_contention` | CAS retry rate under contention (2–64 threads) |
| `bench_device_read_write` | Raw device I/O throughput |
| `bench_checkpoint_latency` | Time to complete a full checkpoint |
| `bench_recovery_time` | Time to recover from checkpoint |

### 11.6 Correctness Verification

#### Cross-Implementation Comparison

Run an identical operation sequence on Rust, C++, and C# FASTER. Compare:
- Final state of every key (value equality)
- Operation return codes (status parity)
- Checkpoint file contents (metadata equivalence, not byte-for-byte)
- Recovery behavior (same keys recoverable, same commit points)

**Test harness:** A shared test specification in JSON or protobuf defines the operation sequence. Each implementation reads the spec, executes operations, and writes results. A comparator checks equivalence.

```
test_spec.json:
  { "ops": [
    {"type": "upsert", "key": 42, "value": 100},
    {"type": "read", "key": 42, "expected_status": "found"},
    {"type": "rmw", "key": 42, "input": 5},
    {"type": "checkpoint", "type": "snapshot"},
    {"type": "delete", "key": 42},
    {"type": "recover"},
    {"type": "read", "key": 42, "expected_status": "found", "expected_value": 105}
  ]}
```

#### Fuzzing

Use `cargo-fuzz` (libFuzzer) and `afl` for coverage-guided fuzzing of:

| Fuzz Target | Input | Looking For |
|-------------|-------|-------------|
| `fuzz_record_deserialize` | Arbitrary byte slices | Panics, buffer overruns in record parsing |
| `fuzz_address_from_u64` | Arbitrary `u64` values | Invalid address construction, overflow |
| `fuzz_checkpoint_metadata_parse` | Arbitrary byte slices | Panics in metadata deserialization |
| `fuzz_operation_sequence` | Sequence of (op_type, key, value) | Assertion failures, panics, deadlocks |
| `fuzz_hash_bucket_entry` | Arbitrary `u64` values | Bitfield extraction errors |
| `fuzz_varlen_record` | Arbitrary (key_len, value_len, bytes) | Buffer overflow in variable-length layout |
| `fuzz_concurrent_ops` | Seed + operation sequence | Data races (under Miri or ThreadSanitizer) |

**Miri integration:** Run unit tests under Miri (`cargo +nightly miri test`) to detect undefined behavior in unsafe code. Not all tests will pass under Miri (I/O operations won't work), but all pure-logic tests must.

### 11.7 CI Pipeline

#### On Every Commit (< 5 minutes)

```yaml
- cargo fmt --check                    # Formatting
- cargo clippy -- -D warnings          # Linting
- cargo build                          # Compile check
- cargo test --lib                     # Unit tests only
- cargo test --test integration_basic  # Smoke test (small subset)
```

#### On Every PR (< 15 minutes)

```yaml
- cargo fmt --check
- cargo clippy -- -D warnings
- cargo build --release                # Release build (catches optimization-related issues)
- cargo test                           # All unit + integration tests
- cargo test --features proptest       # Property-based tests (reduced iteration count)
- cargo +nightly miri test --lib       # Miri on unit tests (unsafe verification)
```

#### Nightly (< 4 hours)

```yaml
- cargo test --features proptest -- --proptest-cases 100000   # Deep property testing
- cargo test --test deterministic_sim                         # Full simulation suite (10K seeds)
- cargo fuzz run fuzz_operation_sequence -- -max_total_time=3600  # 1 hour fuzzing
- cargo fuzz run fuzz_record_deserialize -- -max_total_time=1800
- cargo bench                                                 # Full benchmark suite
- cross-implementation comparison tests
```

#### Weekly (dedicated hardware)

```yaml
- Full YCSB benchmark suite (all workloads × all configurations)
- Throughput scaling tests (1–64 threads)
- Comparison benchmarks (Rust vs C++ vs C#)
- Extended fuzzing campaign (24 hours)
- Deterministic simulation (100K seeds)
- Valgrind/AddressSanitizer run on C FFI examples
```

---


## 12. Implementation Phases

Each phase has explicit deliverables, success criteria, team assignments, dependencies, and parallelism opportunities. Phases are designed so that each produces a testable, demonstrable artifact — no phase is "infrastructure only" without a visible outcome.

**Team Key:**
- **Aragorn** — Rust Expert (core implementation)
- **Sam** — Systems Programming Expert (low-level memory, I/O, alignment)
- **Gimli** — Database/Storage Expert (hybrid log, checkpoint, recovery)
- **Elrond** — Tokio/Async Expert (async adapters, runtime integration)
- **Éowyn** — Deterministic Simulation Testing Expert (simulation framework, crash testing)
- **Boromir** — QA Engineer (test infrastructure, integration tests, CI)
- **Legolas** — Performance Guru (benchmarks, profiling, optimization)
- **Arwen** — Developer Advocate (API review, documentation, examples)
- **Galadriel** — Security Expert (unsafe audit, security review)
- **Saruman** — C++ Expert (reference implementation consultation)
- **Faramir** — C# Expert (reference implementation consultation)
- **Frodo** — Reverse Engineer (behavioral specification, cross-impl comparison)
- **Gandalf** — Lead / System Architect (architecture oversight, review authority)

### Phase 1: Foundation

**Duration:** 3–4 weeks
**Dependencies:** None (this is the root)
**Parallelism:** Runs before all other phases. Some sub-tasks can run in parallel internally.

**Deliverables:**
1. **Crate workspace structure:**
   ```
   faster-rs/
   ├── Cargo.toml              (workspace root)
   ├── faster-core/            (core library — zero async dependencies)
   │   ├── Cargo.toml
   │   └── src/
   ├── faster-ffi/             (C FFI bindings)
   │   ├── Cargo.toml
   │   └── src/
   ├── faster-tokio/           (Tokio async adapter)
   │   ├── Cargo.toml
   │   └── src/
   ├── faster-bench/           (benchmarks)
   │   ├── Cargo.toml
   │   └── src/
   └── tests/                  (integration tests)
   ```
2. **Build system:** `cargo build`, `cargo test`, `cargo bench` all work. CI pipeline (GitHub Actions) running on every commit.
3. **Epoch-based reclamation:** Full `LightEpoch` implementation with per-thread epoch tracking, drain list, safe-to-reclaim computation, and scope guard (`EpochGuard`). No crossbeam dependency in v1 — custom implementation for full control over drain semantics and checkpoint integration.
4. **Basic memory allocator:** `MallocFixedPageSize<T>` equivalent — fixed-size block allocator for overflow buckets and internal structures. Page-based growth, free list, alignment guarantees.
5. **Record format and serialization:** `RecordInfo` (8-byte header with bitfields), fixed-size record layout with alignment padding, `Address` newtype (48-bit, page/offset extraction). Variable-length record format defined (size-prefixed key + value, contiguous inline storage — C++ model).
6. **Core type newtypes:** `Address`, `PageOffset`, `Epoch`, `HashTag`, `BucketIndex` — all with `From`/`Into` conversions and `Debug`/`Display` implementations.

**Success Criteria:**
- `cargo test` passes all epoch unit tests including concurrent acquire/release with 16 threads
- `MallocFixedPageSize` allocates and frees 1M blocks without leak (checked by drop count)
- `RecordInfo` round-trip property test passes 100K iterations
- CI pipeline green on Linux (x86_64) and macOS (aarch64)
- `cargo clippy` zero warnings; `cargo fmt` clean

**Primary:** Aragorn (Rust Expert), Sam (Systems Programming)
**Supporting:** Gandalf (architecture review), Boromir (CI setup)
**Consulting:** Saruman (C++ epoch/allocator reference)

---

### Phase 2: Hash Index

**Duration:** 3–4 weeks
**Dependencies:** Phase 1 (epoch, allocator, address types)
**Parallelism:** Can overlap with Phase 1's final week (address types finalized early)

**Deliverables:**
1. **Hash bucket structure:** 64-byte cache-line-aligned bucket with 7 entries (8 bytes each: 48-bit address + 14-bit tag + 2 status bits) + 1 overflow pointer. Packed into `[AtomicU64; 8]`.
2. **Hash table:** Power-of-2 bucket array. `FindEntry(hash, tag) → Option<Address>` and `FindOrCreateEntry(hash, tag) → &AtomicU64` with CAS-based insertion.
3. **Overflow bucket management:** Overflow buckets allocated from `MallocFixedPageSize`. Linked list from bucket[7]. Overflow chains walkable without locks.
4. **Concurrent correctness:** All operations are latch-free. Insertions use CAS with tentative bit protocol. Deletions invalidate entries atomically.
5. **Hash function integration:** Pluggable hash function trait. Default: high-quality 64-bit hash (e.g., `ahash` or `xxhash`). Tag extraction: bits [48..61] of hash (matching C++ convention).
6. **GC integration hooks:** Interface for invalidating entries pointing to reclaimed log regions (called during epoch drain).

**Success Criteria:**
- Single-threaded: 100M insert + lookup in < 10 seconds
- Concurrent: 8 threads × 10M inserts, zero lost entries (verified by `FindEntry` on all keys)
- Property test: `insert_then_find` passes 100K iterations with shrinking
- Property test: concurrent insert under `proptest` passes 10K iterations
- Overflow chain length bounded: average < 2 for load factor < 0.7
- No `unsafe` beyond the atomic CAS operations and bucket layout

**Primary:** Aragorn (Rust Expert)
**Supporting:** Sam (cache-line alignment, atomics), Gandalf (API review)
**Consulting:** Saruman (C++ hash index reference), Frodo (behavioral spec)

---

### Phase 3: Hybrid Log (In-Memory)

**Duration:** 4–5 weeks
**Dependencies:** Phase 1 (epoch, allocator), Phase 2 (hash index)
**Parallelism:** Can begin design work during Phase 2; implementation starts after Phase 2 core is stable

**Deliverables:**
1. **Page-based allocator** (`PersistentMemoryMalloc` equivalent): Circular buffer of pages (default: 32 MB per page). Atomic tail-address bump for concurrent allocation. Buffer pool with page reuse.
2. **Address regions:** Four address pointers maintained atomically:
   - `begin_address`: oldest valid address (monotonically increasing)
   - `head_address`: boundary between disk-only and in-memory
   - `readonly_address`: boundary between immutable and mutable regions
   - `tail_address`: next allocation point (monotonically increasing)
   - **Invariant:** `begin_address ≤ head_address ≤ readonly_address ≤ tail_address` (always)
3. **Record operations:**
   - **Allocate:** Bump tail atomically, return address
   - **Read:** Translate address to page + offset, return reference
   - **In-place update:** Write through mutable reference (mutable region only)
   - **Seal:** Set invalid bit on old record when superseded
4. **CRUD implementation (in-memory only):**
   - `Read`: hash lookup → chain walk → return value (or `NotFound`)
   - `Upsert`: hash lookup → in-place update (mutable) or append new record
   - `RMW`: hash lookup → in-place modify (mutable) or copy-update or initial-insert
   - `Delete`: append tombstone record, update hash chain
5. **Pending operation infrastructure:** `PendingContext` struct for operations that require I/O. Completion callback registration. `CompletePending(wait: bool)` method.
6. **Session model:** `Session<K, V, F>` struct marked `!Send` (compile-time thread-affinity enforcement). Session-local pending queue. Epoch protection via `EpochGuard` on every operation. Serial number tracking per session.
7. **Functions trait:** Core callback interface:
   ```rust
   pub trait Functions<K, V> {
       type Input;
       type Output;
       type Context;

       fn read(&self, key: &K, value: &V, input: &Self::Input) -> Self::Output;
       fn upsert(&self, key: &K, old: Option<&V>, new: &V, input: &Self::Input) -> V;
       fn rmw(&self, key: &K, value: &mut V, input: &Self::Input) -> RmwAction;
       fn rmw_initial(&self, key: &K, input: &Self::Input) -> V;
       fn delete(&self, key: &K, old_value: &V) -> bool;
   }
   ```

**Success Criteria:**
- Single-threaded CRUD: 10M operations correct (insert all, read all, update all, delete subset, verify)
- Concurrent CRUD: 8 threads × 1M operations, linearizability verified for overlapping keys
- Address ordering invariant holds across all operations (property test, 100K iterations)
- `!Send` on `Session` — compile error if moved across threads
- In-memory throughput: > 10M ops/sec single-threaded (comparable to C++ in-memory path)

**Primary:** Aragorn (Rust Expert), Gimli (Database/Storage)
**Supporting:** Sam (memory management, alignment), Gandalf (API design review)
**Consulting:** Saruman (C++ hybrid log), Faramir (C# allocator variants), Frodo (behavioral contracts)

---

### Phase 4: Storage Layer

**Duration:** 4–5 weeks
**Dependencies:** Phase 3 (hybrid log, pending operations)
**Parallelism:** Device trait design can begin during Phase 3. Éowyn can begin simulation framework design in parallel.

**Deliverables:**
1. **Device trait:** Completion-based (callback model, not Future-based — per user directive). Runtime-agnostic:
   ```rust
   pub trait Device: Send + Sync {
       fn read_async(
           &self,
           offset: u64,
           buf: &mut [u8],
           callback: Box<dyn FnOnce(Result<usize, IoError>) + Send>,
       );
       fn write_async(
           &self,
           offset: u64,
           buf: &[u8],
           callback: Box<dyn FnOnce(Result<usize, IoError>) + Send>,
       );
       fn flush(&self);
       fn truncate(&self, new_len: u64) -> Result<(), IoError>;
       fn sector_size(&self) -> usize;  // Typically 512
   }
   ```
2. **NullDevice:** In-memory only (no persistence). For testing and benchmarking pure in-memory performance.
3. **FileDevice:** Platform-specific file I/O with O_DIRECT (Linux) / unbuffered (Windows). Segmented file management (1 GB segments by default). Threaded I/O completion: dedicated I/O thread(s) poll for completions and invoke callbacks.
4. **Aligned buffer management:** `AlignedBuffer` type ensuring 512-byte alignment for O_DIRECT I/O. Buffer pool to avoid allocation on the I/O hot path.
5. **Page flush pipeline:** When `readonly_address` advances, pages in [old_readonly, new_readonly) are flushed to device. Flush tracking via `LastFlushedUntilAddress`. Flush callback updates head address when safe.
6. **Page read-back:** When a read targets address < `head_address`, issue async read from device. On completion, invoke pending operation callback.
7. **Pending operation completion flow:**
   - Operation encounters record on disk → create `PendingContext` → issue device read
   - Device callback fires → `InternalContinuePending{Read,Rmw,Delete}`
   - Complete user callback or store result for `CompletePending`
8. **I/O completion thread model:** Separate thread (or thread pool) polls device for completions. Completions enqueued to per-session completion queues. `CompletePending` drains the session's queue.

**Success Criteria:**
- File device: write 1 GB → read back → byte-for-byte identical
- Segmented files: log grows through 5+ segments correctly
- Operations with 50% of data on disk: correct results, no data corruption
- Pending operation flow: 10K read operations on disk-resident data complete correctly
- Benchmark: device throughput within 5% of raw `pread`/`pwrite`
- Cross-platform: tests pass on Linux (x86_64) and macOS (aarch64). Windows support documented as future work.

**Primary:** Sam (Systems Programming), Gimli (Storage)
**Supporting:** Aragorn (Rust trait design), Gandalf (device trait review)
**Consulting:** Saruman (C++ device layer reference), Elrond (future async adapter requirements — ensure trait is adaptable)

---

### Phase 5: Checkpoint & Recovery

**Duration:** 5–6 weeks
**Dependencies:** Phase 4 (storage layer, device trait, page flush)
**Parallelism:** State machine design can begin during Phase 4. Éowyn begins crash recovery simulation tests.

**Deliverables:**
1. **Checkpoint state machine:** Phase transitions: `REST → PREP_INDEX_CHECKPOINT → PREPARE → IN_PROGRESS → WAIT_FLUSH → PERSISTENCE_CALLBACK → REST`. Coordinated via epoch system (threads observe phase transition at epoch boundary).
2. **Fold-over checkpoint:** Flush mutable log to disk, record addresses and version in `info.dat`. Cheapest checkpoint type.
3. **Snapshot checkpoint:** Copy in-memory log pages to separate snapshot file. Main log continues uninterrupted.
4. **Incremental snapshot:** Delta log captures changes since last snapshot. Recovery applies base + delta.
5. **Index checkpoint:** Persist entire hash table to disk. Avoids full log replay on recovery.
6. **Checkpoint metadata (`info.dat`):** Binary format with version field for forward compatibility. Contains: all address pointers, checkpoint version, session commit points (session_id, serial_number, exclusion list), checkpoint type, timestamp.
7. **Recovery flow:**
   - Scan for latest valid checkpoint token pair (index + log)
   - Load index from disk (or rebuild from log if index checkpoint missing)
   - Restore log address pointers from metadata
   - Fold-over: replay log from `checkpoint_start_address` to `final_address`
   - Snapshot: load snapshot file into memory; apply delta if incremental
   - Restore sessions with commit points
8. **Version management:** CPR (Concurrent Prefix Recovery) protocol. Version bumps at checkpoint boundary. Threads creating records in v+1 cannot modify v records. Dual execution contexts (prev/cur) per session.
9. **Checkpoint token management:** UUID-based tokens. Token directory structure for multi-checkpoint retention.

**Success Criteria:**
- Fold-over checkpoint + recovery: 100% data integrity for 1M records
- Snapshot checkpoint + recovery: identical behavior
- Incremental snapshot: delta correctly captures only post-base changes
- Session resume: recovered session has correct serial number and exclusions
- Crash recovery (deterministic simulation): 1000 seeds × crash at random point during checkpoint → recovery succeeds
- Crash recovery: crash during normal operation (no checkpoint in progress) → last checkpoint fully recoverable
- Cross-implementation: checkpoint metadata readable by comparison tool (not binary-compatible with C++/C#, but semantically equivalent)

**Primary:** Gimli (Database/Storage), Aragorn (Rust Expert)
**Supporting:** Éowyn (crash recovery simulation tests), Sam (I/O correctness)
**Consulting:** Frodo (cross-impl checkpoint semantics), Saruman (C++ CPR), Faramir (C# incremental snapshots)

---

### Phase 6: Public API & Ergonomics

**Duration:** 3–4 weeks
**Dependencies:** Phase 5 (full functionality available for API surface)
**Parallelism:** API design sketches can begin during Phase 3. Arwen begins documentation planning during Phase 4.

**Deliverables:**
1. **`FasterKvBuilder`:** Builder pattern for all configuration:
   ```rust
   let store = FasterKv::builder()
       .hash_index_size(1 << 20)
       .log_page_size(1 << 25)        // 32 MB
       .log_memory_size(1 << 30)      // 1 GB
       .log_mutable_fraction(0.9)
       .log_segment_size(1 << 30)     // 1 GB
       .device(FileDevice::new("/path/to/data")?)
       .build()?;
   ```
2. **Session API:** Ergonomic, zero-boilerplate session usage:
   ```rust
   let session = store.new_session(MyFunctions::default());
   session.upsert(&key, &value)?;
   let output = session.read(&key, &input)?;
   session.complete_pending(true)?;
   drop(session);  // RAII: flushes pending, releases epoch
   ```
3. **Functions trait with defaults:** `SimpleFunctions<K, V>` for the common case (direct value read, direct value write, no RMW). Users only implement what they need.
4. **Error handling polish:** Clear error messages with context. `thiserror`-derived error types. Status codes documented with "when does this occur" and "what should I do."
5. **Documentation:** `#[doc]` on every public type, method, and trait. Module-level docs explaining the subsystem. `README.md` with quick-start, installation, and feature overview.
6. **Examples:**
   - `examples/basic.rs` — simple key-value CRUD
   - `examples/counter.rs` — RMW counter pattern
   - `examples/checkpoint.rs` — checkpoint and recovery
   - `examples/concurrent.rs` — multi-threaded concurrent access
   - `examples/variable_length.rs` — variable-length keys/values
7. **API review:** Arwen conducts API review with focus on discoverability, naming, and Rust idiom compliance.

**Success Criteria:**
- All examples compile and run without modification
- `cargo doc --no-deps` generates clean documentation with no broken links
- API review by Arwen: score ≥ 8/10 on ergonomics rubric
- New user (simulated): can write a working CRUD program in < 15 minutes using only docs
- Zero `pub unsafe fn` in the public API (all unsafety internal)

**Primary:** Arwen (Developer Advocate), Aragorn (Rust Expert)
**Supporting:** Gandalf (API design authority), Boromir (example verification)
**Consulting:** Faramir (C# API ergonomics insights)

---

### Phase 7: C FFI

**Duration:** 2–3 weeks
**Dependencies:** Phase 6 (stable public API)
**Parallelism:** Can run in parallel with Phase 8 (async adapters)

**Deliverables:**
1. **`faster-ffi` crate:** Separate crate with `crate-type = ["cdylib", "staticlib"]`.
2. **C header generation:** Auto-generated `faster.h` via `cbindgen`. Manually curated if `cbindgen` output is suboptimal.
3. **Opaque handle API:**
   ```c
   typedef struct FasterKv FasterKv;
   typedef struct FasterSession FasterSession;

   FasterKv* faster_kv_open(const FasterKvConfig* config);
   void faster_kv_close(FasterKv* kv);

   FasterSession* faster_session_new(FasterKv* kv, const FasterFunctions* funcs);
   void faster_session_destroy(FasterSession* session);

   FasterStatus faster_read(FasterSession* s, const void* key, size_t key_len,
                            void* value_out, size_t* value_len_out);
   FasterStatus faster_upsert(FasterSession* s, const void* key, size_t key_len,
                              const void* value, size_t value_len);
   FasterStatus faster_rmw(FasterSession* s, const void* key, size_t key_len,
                           const void* input, size_t input_len);
   FasterStatus faster_delete(FasterSession* s, const void* key, size_t key_len);
   FasterStatus faster_complete_pending(FasterSession* s, int wait);

   FasterStatus faster_checkpoint(FasterKv* kv, FasterCheckpointType type,
                                  FasterGuid* token_out);
   FasterStatus faster_recover(FasterKv* kv, const FasterGuid* token);
   ```
4. **Memory ownership documentation:** Clear documentation in header comments: who allocates, who frees, what happens on error.
5. **C example programs:**
   - `examples/c/basic.c` — CRUD operations
   - `examples/c/checkpoint.c` — checkpoint and recovery
   - `examples/c/Makefile` — build instructions
6. **Error handling:** All C functions return `FasterStatus`. Errors retrievable via `faster_last_error()` (thread-local error string).

**Success Criteria:**
- C examples compile with `gcc` and `clang`, link against the Rust library, and run correctly
- Valgrind: zero memory leaks in C example programs
- AddressSanitizer: zero errors
- Header: compiles cleanly under `-Wall -Wextra -pedantic` in C11 mode
- Double-free protection: calling `faster_kv_close` twice doesn't crash (sets handle to NULL)

**Primary:** Sam (Systems Programming — FFI expertise)
**Supporting:** Aragorn (Rust FFI patterns), Galadriel (safety review)
**Consulting:** Saruman (C API design patterns from C++ FASTER)

---

### Phase 8: Async Adapters

**Duration:** 3–4 weeks
**Dependencies:** Phase 6 (stable public API), Phase 4 (Device trait)
**Parallelism:** Runs in parallel with Phase 7 (C FFI). Elrond begins design during Phase 4.

**Deliverables:**
1. **`faster-tokio` crate:** Adapter wrapping `faster-core` for Tokio runtime integration.
   - Implements `Device` trait using Tokio's threaded runtime for I/O
   - Exposes async session API:
     ```rust
     let result = session.read_async(&key, &input).await?;
     let status = session.upsert_async(&key, &value).await?;
     ```
   - Bridges callback-based core to `Future`-based API via `oneshot` channels or `Waker` integration
   - Zero overhead for operations that complete synchronously (in-memory hits return `Poll::Ready` immediately)
2. **Second runtime adapter (compio or monoio):** At least one additional runtime adapter to validate the multi-runtime design.
   - Demonstrates that `Device` trait and callback model genuinely support alternative runtimes
   - Documents the adapter authoring pattern for third-party runtime support
3. **Async device implementations:**
   - `TokioFileDevice`: Uses Tokio's `spawn_blocking` or `tokio-uring` for async file I/O
   - `CompioFileDevice` or `MonoioFileDevice`: Native async I/O for the second runtime
4. **Adapter pattern documentation:** How to write a new runtime adapter in < 200 lines.

**Success Criteria:**
- Tokio adapter: all integration tests pass under `#[tokio::test]`
- Second adapter: basic CRUD + checkpoint/recovery tests pass
- Benchmark: async path adds < 5% overhead vs. sync path for in-memory operations
- Benchmark: async I/O throughput matches or exceeds sync threaded I/O
- Compile-time: `faster-core` builds with zero async runtime dependencies
- Feature isolation: `faster-tokio` depends on `tokio`; `faster-core` does not

**Primary:** Elrond (Tokio/Async Expert)
**Supporting:** Aragorn (trait design), Sam (I/O path), Gandalf (architecture validation)
**Consulting:** Saruman (C++ async I/O model)

---

### Phase 9: Hardening

**Duration:** 4–6 weeks
**Dependencies:** All prior phases complete
**Parallelism:** Security audit (Galadriel) and simulation testing (Éowyn) can run in parallel with performance optimization (Legolas)

**Deliverables:**
1. **Full deterministic simulation test suite (Éowyn):**
   - 10,000+ seed configurations
   - All fault injection scenarios from §11.4
   - Crash at every checkpoint phase transition
   - Concurrent operations during checkpoint, grow, GC
   - 48-hour continuous simulation run with no failures
2. **Security audit of all `unsafe` code (Galadriel):**
   - Catalog every `unsafe` block with justification comment
   - Verify safety invariants are documented and tested
   - Check for: use-after-free, double-free, data races, buffer overflows, uninitialized memory
   - Audit C FFI boundary for memory safety
   - Report: list of findings, severity, remediation
3. **Performance optimization pass (Legolas):**
   - Profile under YCSB workloads with `perf`, `flamegraph`, `cachegrind`
   - Identify and eliminate: cache misses in hot path, unnecessary allocations, contention points
   - Optimize: hash function, epoch acquire/release, CAS retry loops, page allocation
   - Target: match or exceed C++ FASTER throughput
4. **Fuzzing campaign:**
   - 1 week continuous fuzzing on all fuzz targets
   - Fix all findings
   - Integrate fuzz corpus into CI (regression tests)
5. **ThreadSanitizer run:** All concurrent tests under ThreadSanitizer to detect data races in unsafe code.
6. **Miri verification:** All pure-logic unit tests pass under Miri.

**Success Criteria:**
- Zero correctness bugs found by simulation (after fixes)
- Zero safety issues in `unsafe` audit (after remediation)
- Performance: Rust FASTER ≥ 90% of C++ FASTER throughput on YCSB-A (update-heavy)
- Performance: Rust FASTER ≥ 95% of C++ FASTER throughput on YCSB-C (read-only)
- Fuzzing: no crashes after 1 week of continuous fuzzing
- ThreadSanitizer: zero data race reports
- Miri: all targeted tests pass

**Primary (parallel tracks):**
- Éowyn (simulation testing)
- Galadriel (security audit)
- Legolas (performance optimization)
**Supporting:** Aragorn (fix implementation issues), Boromir (regression test integration)
**Consulting:** Saruman (C++ performance reference), Sam (systems-level optimization)

---

### Phase 10: Release Preparation

**Duration:** 2–3 weeks
**Dependencies:** Phase 9 (hardening complete, all tests green)
**Parallelism:** Documentation and benchmark publication can overlap

**Deliverables:**
1. **Documentation complete (Arwen):**
   - `README.md`: installation, quick-start, feature overview, comparison with C++/C#
   - `ARCHITECTURE.md`: this document, finalized
   - `API.md`: generated from `cargo doc`, with supplementary guides
   - `MIGRATION.md`: guide for users migrating from C++ or C# FASTER
   - `UNSAFE.md`: catalog of all unsafe usage with safety arguments
   - `CHANGELOG.md`: version history
2. **Benchmark publication (Legolas):**
   - YCSB results on standardized hardware (document specs)
   - Comparison charts: Rust vs C++ vs C#
   - Throughput scaling graphs
   - Latency distribution plots (P50, P99, P99.9)
3. **Migration guides:**
   - From C++ FASTER: API mapping table, behavioral differences, checkpoint incompatibilities
   - From C# FASTER: API mapping table, async pattern differences
4. **crates.io preparation:**
   - `Cargo.toml` metadata: description, license (MIT), repository, keywords, categories
   - Crate naming: `faster-kv` (core), `faster-kv-tokio` (async adapter), `faster-kv-ffi` (C bindings)
   - Version: `0.1.0` (initial public release — semver pre-1.0 signals API may evolve)
   - `cargo publish --dry-run` succeeds
5. **Release checklist:**
   - [ ] All CI pipelines green
   - [ ] All benchmarks documented
   - [ ] All examples verified on Linux and macOS
   - [ ] C FFI examples verified with gcc and clang
   - [ ] Security audit findings resolved
   - [ ] CHANGELOG up to date
   - [ ] License file present
   - [ ] `cargo publish` succeeds

**Success Criteria:**
- Clean `cargo publish --dry-run` for all publishable crates
- Documentation review by Arwen: complete and accurate
- All migration guide examples compile and run
- No open P0/P1 issues
- Team sign-off: Gandalf (architecture), Aragorn (implementation), Galadriel (security), Arwen (documentation)

**Primary:** Arwen (Developer Advocate), Gandalf (release authority)
**Supporting:** Legolas (benchmarks), Aragorn (final fixes), Boromir (release verification)

---

### Phase Dependency Graph

```
Phase 1: Foundation ─────────────────────────────────┐
    │                                                 │
    ▼                                                 │
Phase 2: Hash Index ──────────────┐                   │
    │                              │                   │
    ▼                              ▼                   │
Phase 3: Hybrid Log (In-Memory) ──┤                   │
    │                              │                   │
    ▼                              │                   │
Phase 4: Storage Layer ────────────┤                   │
    │                              │                   │
    ▼                              │                   │
Phase 5: Checkpoint & Recovery ────┤                   │
    │                              │                   │
    ├──────────────┐               │                   │
    ▼              ▼               │   (Éowyn: simulation│
Phase 6: API    Phase 7: C FFI    │    framework dev   │
    │              │               │    starts Phase 4) │
    ├──────────────┤               │                   │
    ▼              ▼               │                   │
Phase 8: Async  (parallel w/ 7)   │                   │
    │                              │                   │
    ▼                              │                   │
Phase 9: Hardening ◄──────────────┘                   │
    │                                                  │
    ▼                                                  │
Phase 10: Release ◄───────────────────────────────────┘
```

**Critical path:** 1 → 2 → 3 → 4 → 5 → 6 → 9 → 10 (~28–37 weeks)
**Parallelism savings:** Phases 7 & 8 parallel with each other and with Phase 6's later stages. Éowyn's simulation framework built during Phases 4–5. Legolas's benchmark suite built during Phases 3–5. ~4–6 weeks saved.
**Estimated total:** 6–9 months to v0.1.0 release.



---

## 13. Key Decisions & Rationale

Every major architectural decision is documented here with the decision, alternatives considered, rationale, and trade-offs accepted. These are binding unless revisited through a formal Design Review.

### Decision 1: No async/await in Core — Callback/Completion Model

**Status:** DECIDED — User directive (non-negotiable)
**Date:** 2026-03-05
**Authority:** qbradley (project owner)

**Decision:** The `faster-core` crate must not depend on any async runtime (tokio, async-std, smol, etc.) and must not use `async`/`await` syntax. All asynchronous operations use a callback/completion model. Async runtime integration is provided via separate adapter crates (`faster-tokio`, `faster-compio`, etc.).

**Rationale:**
1. **Runtime freedom:** Tokio is dominant but not universal. Emerging runtimes (kimojio, compio, monoio) offer io_uring-native and thread-per-core models that may outperform Tokio for storage workloads. The core must not pick a winner.
2. **Scaling limitations:** Tokio's work-stealing scheduler adds overhead for latency-sensitive, CPU-bound operations like CAS loops and epoch management. A callback model lets the application control thread placement.
3. **C FFI compatibility:** Callbacks are the natural async model for C consumers. Futures are meaningless across the FFI boundary.
4. **Deterministic testing:** Callback-based code is easier to test deterministically — no need to mock an async runtime.

**Alternatives rejected:**
- **async/await in core (Saruman's recommendation):** Simpler Rust code, better ergonomics, but locks the implementation to a specific runtime model. Overridden by user directive.
- **Poll-only model (no callbacks):** Too low-level for most consumers. Callbacks provide a usable sync API.

**Trade-offs accepted:**
- Slightly more verbose core code (explicit callback threading vs. `.await` chains)
- Adapter crates must bridge callback-to-Future, adding a thin translation layer
- State machines for multi-step operations (pending-to-IO-to-complete) are explicit, not compiler-generated

**Implementation pattern:**
```rust
// Core: callback-based
fn read(&self, key: &K, callback: impl FnOnce(ReadResult<V>) + Send + 'static) {
    // ... synchronous path or register callback for I/O completion
}

// Adapter: bridges to Future
async fn read_async(&self, key: &K) -> Result<V, Error> {
    let (tx, rx) = oneshot::channel();
    self.inner.read(key, move |result| { let _ = tx.send(result); });
    rx.await.map_err(|_| Error::SessionDropped)?
}
```

---

### Decision 2: Custom Epoch Implementation (Not crossbeam-epoch)

**Status:** DECIDED
**Date:** 2026-03-05
**Authority:** Gandalf (architecture), Aragorn (implementation)

**Decision:** Implement a custom `LightEpoch` modeled on C++/C# FASTER's epoch system rather than using `crossbeam-epoch`.

**Rationale:**
1. **Drain list semantics:** FASTER's epoch uses a drain list for deferred actions (GC, checkpoint phase transitions, grow-index). `crossbeam-epoch` provides `defer()` but without the epoch-tagged scheduling that FASTER requires.
2. **Checkpoint integration:** The epoch system is tightly coupled with the checkpoint state machine — phase transitions are triggered via epoch callbacks. This coupling is fundamental to CPR (Concurrent Prefix Recovery).
3. **Thread entry management:** FASTER tracks per-thread epoch entries with explicit registration/deregistration and `phase_finished` flags. `crossbeam-epoch` has a different thread management model.
4. **Behavioral equivalence:** A custom implementation ensures identical behavior to C++/C# for cross-implementation testing.
5. **Code size:** FASTER's epoch is ~500 lines. The custom implementation is comparable. Adding crossbeam as a dependency saves no complexity.

**Alternatives considered:**
- **crossbeam-epoch:** Mature, well-tested, but semantically different. Would require a wrapper that reimplements most of FASTER's epoch logic anyway.
- **seize:** Lighter weight than crossbeam, but same semantic mismatch.

**Trade-offs accepted:**
- Must write and maintain our own epoch implementation (~500 lines)
- Must write thorough tests (unit, property, simulation) to match crossbeam's maturity
- No free ride on crossbeam's fuzz testing and battle-hardened codebase

**Mitigation:** Extensive property-based testing and deterministic simulation to achieve equivalent confidence.

---

### Decision 3: Record Format — Inline Variable-Length (C++ Model)

**Status:** DECIDED
**Date:** 2026-03-05
**Authority:** Gandalf (architecture), Frodo (cross-impl analysis)

**Decision:** Use inline variable-length records where key and value are stored contiguously in a single allocation within the hybrid log. This follows the C++ model, not the C# dual-log model.

**Record layout:**
```
[ RecordInfo (8B) | padding | Key (variable) | padding | Value (variable) ]
```

For variable-length types, keys and values are prefixed with a 4-byte length:
```
[ RecordInfo (8B) | key_len (4B) | key_data[key_len] | pad | value_len (4B) | value_data[value_len] | pad ]
```

**Rationale:**
1. **Cache locality:** Single contiguous allocation means key and value are likely in the same cache line (for small records). The C# dual-log approach splits fixed-size pointers from variable-length data, causing an extra cache miss per access.
2. **Simplicity:** No secondary "object log" device. One log, one set of address pointers, one flush pipeline.
3. **Rust alignment:** Rust's `&[u8]` and `Box<[u8]>` work naturally with inline byte sequences. No GC or separate heap needed.
4. **I/O efficiency:** Entire records read/written in a single I/O operation.

**Alternatives rejected:**
- **C# dual-log (GenericAllocator):** Needed in C# for managed objects (classes on the GC heap). Rust doesn't have this constraint. The extra indirection adds latency and complexity.
- **Fixed-size only:** Too restrictive. Many real-world workloads have variable-length keys or values.

**Trade-offs accepted:**
- In-place updates of variable-length values require the new value to fit in the same space, or fall back to copy-update (append new record). This is acceptable — it matches C++ behavior.
- Record size computation requires knowing key and value sizes upfront.
- Slightly more complex serialization logic than fixed-size records.

---

### Decision 4: Hash Index — Rust Atomic Patterns for Latch-Free Operations

**Status:** DECIDED
**Date:** 2026-03-05
**Authority:** Gandalf (architecture), Aragorn (implementation)

**Decision:** Implement the hash index using Rust's `std::sync::atomic` types with compare-and-swap (CAS) loops for all mutations. The hash bucket is represented as `[AtomicU64; 8]` (64 bytes = 1 cache line).

**Atomic patterns:**
- **Insert:** CAS on the target entry slot. If tentative entry exists from another thread, retry. Use tentative bit (bit 0 of the entry) to reserve a slot before writing the full entry.
- **Delete:** Atomic store of `Address::kInvalidAddress` into the entry (or set invalid bit).
- **Lookup:** Relaxed load of entry, then verify via tag comparison. If match, follow address into hybrid log.
- **Overflow:** Allocate new overflow bucket from `MallocFixedPageSize`. CAS the overflow pointer (bucket[7]) to link the new bucket.

**Memory ordering:**
- **CAS operations:** `Ordering::AcqRel` (acquire on success for visibility of the stored address; release for publishing the new entry to other threads)
- **Lookups:** `Ordering::Acquire` (need to see the address that was stored)
- **Tag-only checks:** `Ordering::Relaxed` is sufficient (worst case: false miss, retry after epoch refresh)
- **Epoch-protected operations:** Epoch acquire provides the necessary memory fence; operations within an epoch can use `Relaxed` where the epoch fence provides ordering

**Rationale:**
1. **Direct mapping:** C++'s `compare_exchange_strong` maps directly to Rust's `AtomicU64::compare_exchange`.
2. **Cache-line alignment:** `#[repr(align(64))]` on the bucket struct ensures no false sharing.
3. **Bitfield packing:** 48-bit address + 14-bit tag + 2 status bits packed into `u64` via shift/mask operations in a `HashBucketEntry` newtype.
4. **No locks:** The entire hash index is latch-free. No `Mutex`, no `RwLock`, no spinlocks.

**Trade-offs accepted:**
- CAS loops can spin under high contention (same key, same bucket). Mitigation: exponential backoff hint (`std::hint::spin_loop`).
- `unsafe` required for the `#[repr(align(64))]` bucket array and pointer arithmetic. Mitigation: encapsulated in the hash index module with extensive testing.

---

### Decision 5: Hybrid Log Page Size and Alignment

**Status:** DECIDED
**Date:** 2026-03-05
**Authority:** Gandalf (architecture), Gimli (storage)

**Decision:** Default page size of 2^25 = 32 MB. Configurable via builder. All pages aligned to sector size (512 bytes) for O_DIRECT I/O compatibility.

**Parameters (matching C++ defaults):**
- `PageSizeBits`: 25 (32 MB pages)
- `MemorySizeBits`: configurable (default: enough for 4-16 pages = 128 MB - 512 MB in-memory)
- `MutableFraction`: 0.9 (90% of in-memory pages are mutable)
- `SegmentSizeBits`: 30 (1 GB disk segments)

**Rationale:**
1. **32 MB pages:** Large enough to amortize page management overhead. Small enough to avoid excessive memory waste on partial pages. Matches C++ and C# defaults.
2. **512-byte sector alignment:** Required for O_DIRECT on Linux. Ensures no read-modify-write cycles at the storage layer.
3. **0.9 mutable fraction:** Most records are in the mutable region for in-place updates. The 10% read-only region provides a buffer for records aging out before flush.
4. **1 GB segments:** Large enough to reduce file count. Small enough to allow independent management (deletion of old segments during compaction).

**Alternatives considered:**
- **Smaller pages (4 KB - 4 MB):** Higher page management overhead, more frequent page transitions. Viable for memory-constrained environments; offered as a configuration option but not the default.
- **Larger pages (64 MB - 256 MB):** Higher memory waste, longer flush times. Not recommended but not prevented.

**Trade-offs accepted:**
- 32 MB minimum memory footprint per in-memory page. For small datasets, this wastes memory. Mitigation: configurable page size.
- Large pages mean longer flush durations (32 MB I/O per page). Mitigation: flush pipeline can issue multiple concurrent I/Os per page if the device supports it.

---

### Decision 6: Device Trait — Completion-Based for Runtime-Agnostic I/O

**Status:** DECIDED
**Date:** 2026-03-05
**Authority:** Gandalf (architecture), per user directive on no-async-in-core

**Decision:** The `Device` trait uses completion callbacks (`Box<dyn FnOnce(Result<usize, IoError>) + Send>`) rather than returning `Future`s.

**Trait design:**
```rust
pub trait Device: Send + Sync {
    fn read_async(&self, offset: u64, buf: &mut [u8],
                  callback: Box<dyn FnOnce(Result<usize, IoError>) + Send>);
    fn write_async(&self, offset: u64, buf: &[u8],
                   callback: Box<dyn FnOnce(Result<usize, IoError>) + Send>);
    fn read_sync(&self, offset: u64, buf: &mut [u8]) -> Result<usize, IoError>;
    fn write_sync(&self, offset: u64, buf: &[u8]) -> Result<usize, IoError>;
    fn flush(&self);
    fn truncate(&self, new_len: u64) -> Result<(), IoError>;
    fn sector_size(&self) -> usize;
    fn max_concurrent_ios(&self) -> usize;
}
```

**Rationale:**
1. **Runtime agnostic:** Callbacks work with any execution model — bare threads, Tokio, monoio, io_uring direct, IOCP, etc. No `Future` or `async` in the trait.
2. **C++ alignment:** Matches C++ FASTER's `AsyncIOCallback` pattern. Behavioral equivalence is straightforward.
3. **Implementor freedom:** A Tokio-based device calls the callback from a Tokio task. A threaded device calls it from an I/O completion thread. A simulation device calls it synchronously. The core doesn't care.
4. **Sync path included:** `read_sync` and `write_sync` for blocking callers. Implementations can delegate to the async path or use platform-native sync I/O.

**Alternatives rejected:**
- **`async fn` in trait:** Requires `async-trait` or Rust's native async trait support (RPITIT). Couples the trait to a specific async model.
- **`Future`-returning trait:** Similar to async fn but manual. Still requires an executor to poll.
- **Poll-based trait:** Too low-level for most implementors. Callbacks are simpler.

**Trade-offs accepted:**
- `Box<dyn FnOnce(...)>` incurs a heap allocation per I/O operation. Mitigation: for the hot path (in-memory operations), no I/O is issued and no callback is allocated. For the cold path (disk I/O), the allocation is dwarfed by I/O latency. If profiling shows this matters, we can switch to a pre-allocated callback pool.
- Implementors must manage their own completion threading. Mitigation: provide reference implementations for common patterns (threaded, Tokio, io_uring).

---

### Decision 7: C FFI — Opaque Handles, Not Translated Types

**Status:** DECIDED
**Date:** 2026-03-05
**Authority:** Gandalf (architecture), Sam (FFI)

**Decision:** The C FFI exposes opaque pointers (`typedef struct FasterKv FasterKv;`) rather than translating Rust types into C-compatible structs.

**Rationale:**
1. **Encapsulation:** Internal representation can change without breaking the C ABI. The C consumer never sees `RecordInfo`, `Address`, or `HashBucket` — only handles and byte buffers.
2. **Safety:** Opaque handles prevent C code from directly manipulating internal state. All mutations go through the FFI function API, which validates inputs.
3. **Simplicity:** No need to maintain parallel C struct definitions for every Rust type. The FFI surface is small: ~15 functions.
4. **Precedent:** This is the standard pattern for Rust FFI (see: rusqlite, lmdb-rkv, tantivy-ffi).

**Alternatives considered:**
- **Translated types:** Expose C-compatible versions of `RecordInfo`, `Address`, etc. Provides more flexibility for C consumers but creates a maintenance burden and ABI stability risk.
- **Flat C API with value types:** Pass all data as raw byte buffers. Too low-level; loses type safety.

**Trade-offs accepted:**
- C consumers cannot inspect internal state for debugging. Mitigation: provide `faster_debug_info()` function returning a human-readable string.
- Every operation crosses the FFI boundary (function call overhead). Mitigation: batch operations for bulk use cases.

---

### Decision 8: Checkpoint Format — Own Format (Not C++/C# Compatible)

**Status:** DECIDED
**Date:** 2026-03-05
**Authority:** Gandalf (architecture), Gimli (storage), Frodo (cross-impl)

**Decision:** The Rust implementation uses its own checkpoint format. Checkpoints are NOT binary-compatible with C++ or C# FASTER. Semantic equivalence is verified via cross-implementation comparison tests.

**Format:**
- Binary format with a version header (4-byte magic + 4-byte version number)
- Record layout may differ (different alignment rules, different padding)
- Metadata (`info.dat`) uses a defined binary schema with explicit field widths
- Forward-compatible: unknown fields are skipped (length-prefixed sections)

**Rationale:**
1. **Alignment differences:** Rust's alignment rules differ from C++ (`#pragma pack`) and C# (`StructLayout.Pack`). Forcing binary compatibility would require unsafe casting and platform-specific code.
2. **Record format evolution:** Our inline variable-length format may not match C++ or C# exactly. Forcing compatibility constrains future optimization.
3. **Version independence:** Checkpoint format can evolve independently of C++/C# releases.
4. **Migration path:** If cross-format compatibility is needed later, a conversion tool is simpler than constraining the format upfront.

**Alternatives considered:**
- **Binary-compatible with C++:** Would enable zero-downtime migration. But imposes significant constraints on record layout and requires matching C++ struct packing exactly.
- **Binary-compatible with C#:** Same issues plus C#'s managed object log has no Rust equivalent.
- **Portable format (protobuf, flatbuffers):** Overhead for checkpoint metadata is minimal, but data pages are raw memory — no serialization format applies.

**Trade-offs accepted:**
- No zero-downtime migration from C++/C# to Rust. Users must export/import data.
- Cross-implementation comparison tests must compare semantics (key-value pairs), not bytes.

---

### Decision 9: Error Handling — `Result<Status, Error>` Taxonomy

**Status:** DECIDED
**Date:** 2026-03-05
**Authority:** Gandalf (architecture), Aragorn (Rust idioms)

**Decision:** Operations return `Result<Status, Error>` where `Status` is a bitflag-style success type and `Error` is a `thiserror`-derived error enum.

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Status(u16);

impl Status {
    pub const OK: Status = Status(0);
    pub const FOUND: Status = Status(1 << 0);
    pub const NOT_FOUND: Status = Status(1 << 1);
    pub const PENDING: Status = Status(1 << 2);
    pub const IN_PLACE_UPDATED: Status = Status(1 << 3);
    pub const CREATED_RECORD: Status = Status(1 << 4);
    pub const COPY_UPDATED: Status = Status(1 << 5);
    pub const EXPIRED: Status = Status(1 << 6);

    pub fn found(&self) -> bool { self.0 & Self::FOUND.0 != 0 }
    pub fn is_pending(&self) -> bool { self.0 & Self::PENDING.0 != 0 }
    // ... etc
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("store is closed")]
    StoreClosed,
    #[error("session already disposed")]
    SessionDisposed,
    #[error("checkpoint failed: {reason}")]
    CheckpointFailed { reason: String },
    #[error("recovery failed: {reason}")]
    RecoveryFailed { reason: String },
    #[error("hash index full (overflow limit reached)")]
    IndexFull,
    #[error("record too large: {size} bytes (max: {max})")]
    RecordTooLarge { size: usize, max: usize },
}
```

**Rationale:**
1. **Rust idiom:** `Result<T, E>` is the standard error handling pattern. Using it enables `?` operator and standard error propagation.
2. **Status vs. Error:** `Status` represents normal operational outcomes (found, not found, pending). `Error` represents exceptional conditions (I/O failure, invalid state). This separation prevents conflating "key not found" (normal) with "disk failed" (exceptional).
3. **Bitflags on Status:** Allows combinations (e.g., `CREATED_RECORD | NOT_FOUND` = "created because key didn't exist"). Matches C++/C# status semantics.
4. **thiserror:** Generates `Display` and `From` impls automatically. Zero runtime overhead.

**Alternatives considered:**
- **Single enum for everything:** `enum Result { Found(V), NotFound, Pending, Error(E) }`. Loses composability and `?` integration.
- **C-style error codes:** Not idiomatic Rust. Forces manual error checking.

---

### Decision 10: Session Model — Thread-Affine (!Send)

**Status:** DECIDED
**Date:** 2026-03-05
**Authority:** Gandalf (architecture), Faramir (C# insights)

**Decision:** Sessions are marked `!Send` (cannot be moved between threads). This enforces compile-time thread affinity matching FASTER's mono-threaded session contract.

**Implementation:**
```rust
pub struct Session<'a, K, V, F: Functions<K, V>> {
    // ... fields
    _not_send: PhantomData<*const ()>,  // Makes Session !Send + !Sync
}
```

**Rationale:**
1. **Safety:** FASTER sessions are not thread-safe. Concurrent use of a single session is undefined behavior in C++/C#. In Rust, `!Send` makes this a compile-time error.
2. **Performance:** No synchronization overhead within a session. Epoch protection, pending queues, serial numbers — all thread-local with no atomic operations.
3. **C# lesson learned:** Faramir's analysis notes that C# sessions are mono-threaded "but nothing prevents misuse at compile time." Rust's type system eliminates this class of bugs.

**Alternatives considered:**
- **Send + Sync sessions with internal locking:** Enables sharing across threads but defeats the performance purpose. Sessions are designed for thread-local use.
- **Send but not Sync:** Allows moving sessions between threads but not sharing. Could be useful for work-stealing runtimes, but FASTER's epoch registration is thread-specific. Moving sessions requires re-registering, which adds complexity for marginal benefit.

**Trade-offs accepted:**
- Users who want multi-threaded access must create multiple sessions (one per thread). This is the intended usage pattern.
- Async runtimes that move tasks between threads (Tokio's work-stealing) need special handling in the adapter layer (pin session to a specific thread).

---

### Decision 11: Variable-Length Key/Value Strategy

**Status:** DECIDED
**Date:** 2026-03-05
**Authority:** Gandalf (architecture), Frodo (cross-impl analysis)

**Decision:** Support both fixed-size and variable-length keys/values through a unified `Key`/`Value` trait system. Variable-length records use inline storage (size-prefixed, contiguous with record header).

```rust
pub trait Key: Eq + Hash {
    fn serialized_size(&self) -> usize;
    fn serialize_into(&self, buf: &mut [u8]);
    fn deserialize_from(buf: &[u8]) -> &Self;
}

pub trait Value {
    fn serialized_size(&self) -> usize;
    fn serialize_into(&self, buf: &mut [u8]);
    fn deserialize_from(buf: &[u8]) -> &Self;
}

// Blanket implementations for fixed-size types
impl Key for u64 { ... }  // 8 bytes, no length prefix
impl Key for [u8; 16] { ... }  // 16 bytes, no length prefix
impl Key for Vec<u8> { ... }  // 4-byte length prefix + data
impl Value for Vec<u8> { ... }
```

**Rationale:**
1. **Unified approach:** One trait for both fixed and variable. No separate allocator types (unlike C#'s `BlittableAllocator` vs. `VarLenBlittableAllocator`).
2. **Inline storage:** C++ model. Better cache locality than C#'s separate object log.
3. **Zero-copy reads:** `deserialize_from` returns a reference into the record buffer. No allocation for reads.
4. **Extensible:** Users can implement `Key`/`Value` for their own types.

**Trade-offs accepted:**
- In-place update of variable-length values only works if new value is less than or equal to old value size. Otherwise, copy-update (append new record).
- Record size must be known before allocation. This requires computing serialized sizes before calling allocate.

---

### Decision 12: Grow (Resize) Protocol

**Status:** DECIDED
**Date:** 2026-03-05
**Authority:** Gandalf (architecture), Saruman (C++ reference)

**Decision:** Hash index growth doubles the table size, coordinated through the epoch system. Growth is a multi-phase operation similar to checkpointing.

**Protocol:**
1. **Trigger:** User calls `grow_index()` or automatic trigger at load factor threshold (configurable, default: disabled).
2. **Allocate:** New table of 2x size allocated (does not replace old yet).
3. **Split phase:** Epoch-coordinated. Each thread processes its share of old buckets:
   - For each old bucket, rehash entries to determine new bucket (old_index vs. old_index + old_size)
   - CAS entries into new table
   - Old bucket marked as "split complete"
4. **Swap:** Once all buckets split (verified via epoch drain), atomically swap old-to-new table pointer.
5. **Reclaim:** Old table memory freed via epoch drain (deferred until all threads have observed the new table).

**Key constraint:** Hash table only grows, never shrinks. Shrinking would invalidate addresses stored in the hash table entries (addresses encode a bucket index implicitly via the hash function).

**Rationale:**
1. **Non-blocking:** Operations continue during growth. Threads that encounter a "being split" bucket assist or wait based on epoch state.
2. **Epoch coordination:** Same mechanism as checkpoint phase transitions. Reuses existing infrastructure.
3. **No address invalidation:** Growing preserves the property that `hash(key) % old_size` maps to either `hash(key) % new_size` or `hash(key) % new_size + old_size`.

**Trade-offs accepted:**
- 2x memory spike during growth (old + new tables coexist). Mitigation: growth is infrequent; size table appropriately at construction.
- Growth is relatively slow (proportional to table size). Mitigation: operations continue during growth; growth is background work.
- No shrink. Mitigation: if memory pressure is a concern, reconstruct the store with a smaller table.


---

## 14. Risk Register

This register catalogs every significant risk to the Rust FASTER implementation, with likelihood (L), impact (I), and specific mitigations. Risks are organized by category and rated on a 5-point scale (1=Very Low, 2=Low, 3=Medium, 4=High, 5=Very High). Risk score = L x I.

### 14.1 Correctness Risks

| # | Risk | L | I | Score | Mitigation |
|---|------|---|---|-------|------------|
| C1 | **Lock-free algorithm bugs in hash index** — CAS loops have subtle ABA problems, lost updates, or ordering violations that manifest only under specific thread interleavings | 4 | 5 | 20 | Deterministic simulation testing (Éowyn) with adversarial scheduling. Linearizability checking on all concurrent operations. Model-check critical CAS loops. Property-based tests with high iteration counts. |
| C2 | **Epoch-based reclamation: use-after-free** — Thread accesses memory freed by another thread that has already advanced its epoch. Possible if drain list callback fires prematurely or epoch tracking has an off-by-one error | 3 | 5 | 15 | Miri verification of epoch unit tests. Custom property tests that model epoch state per-thread. Simulation testing with epoch boundary stress scenarios. AddressSanitizer on all integration tests. |
| C3 | **Crash recovery data loss or corruption** — Checkpoint metadata inconsistent with log contents after power failure. Partial writes corrupt recovery. | 3 | 5 | 15 | Deterministic crash simulation at every checkpoint phase transition. Checksums on metadata. Write-ahead-of-metadata pattern (data flushed before metadata written). Test with 10,000+ crash points. |
| C4 | **CPR version boundary bugs** — Threads in different versions (v, v+1) interact incorrectly during checkpoint. Records created in wrong version. Dual execution context swap has edge cases. | 3 | 5 | 15 | Direct port of C++/C# CPR state machine with behavioral equivalence testing. Cross-implementation comparison of checkpoint/recovery results. Simulation testing of version boundary transitions. |
| C5 | **Epoch drain list ordering** — Callbacks execute in wrong order or at wrong epoch, causing premature memory reclamation or missed phase transitions | 2 | 5 | 10 | Unit tests for drain list ordering with multiple queued actions at different epochs. Property tests verifying monotonic epoch advancement. |
| C6 | **Hash table grow correctness** — During resize, entries are lost, duplicated, or placed in wrong bucket. Concurrent operations during growth see inconsistent state. | 3 | 4 | 12 | Dedicated grow-under-concurrency test suite. Verify all entries present after grow. Linearizability check during grow operations. |
| C7 | **Pending operation completion ordering** — Callbacks for pending operations fire in wrong order or are dropped, causing silent data loss or stale reads | 2 | 4 | 8 | Integration tests that force all operations to go pending (small in-memory region). Verify callback counts match operation counts. Timeout detection for lost callbacks. |

### 14.2 Performance Risks

| # | Risk | L | I | Score | Mitigation |
|---|------|---|---|-------|------------|
| P1 | **Cache miss amplification** — Rust's ownership model forces extra indirection or copying that C++ avoids with placement new and raw pointers, increasing cache misses on the hot path | 3 | 4 | 12 | Profile early (Phase 3) with `cachegrind`. Use `unsafe` pointer arithmetic for record access where cache performance demands it. Benchmark against C++ continuously. |
| P2 | **CAS contention under high thread counts** — Hot buckets experience excessive CAS retries, reducing throughput superlinearly as thread count increases beyond 16 | 3 | 3 | 9 | Benchmark CAS retry rates explicitly. Exponential backoff with `spin_loop` hints. Consider per-bucket combining if contention exceeds threshold. NUMA-aware bucket placement. |
| P3 | **I/O callback overhead** — `Box<dyn FnOnce>` allocation on every I/O operation adds per-operation overhead that degrades throughput for disk-heavy workloads | 2 | 3 | 6 | Profile I/O path. If significant, switch to pre-allocated callback pool or typed callback slots (no Box). On in-memory hot path, no callbacks allocated. |
| P4 | **Epoch acquire/release overhead** — Per-operation atomic store for epoch tracking adds latency to every operation, even in-memory | 2 | 3 | 6 | Benchmark epoch overhead in micro-benchmarks. Batch epoch protection across multiple operations within a session. Use `Relaxed` ordering where safe. |
| P5 | **Page allocation contention** — Atomic tail-address bump becomes a bottleneck when many threads allocate concurrently | 2 | 3 | 6 | Use `fetch_add` (single atomic) not CAS loop for tail bump. Consider per-thread allocation buffers if contention is measured. |
| P6 | **Rust monomorphization bloat** — Heavy use of generics (K, V, F) causes code bloat from monomorphization, increasing instruction cache misses | 2 | 2 | 4 | Use trait objects for cold paths. Keep generic-parameterized code minimal. Measure binary size and icache miss rates. |

### 14.3 Complexity Risks

| # | Risk | L | I | Score | Mitigation |
|---|------|---|---|-------|------------|
| X1 | **Checkpoint state machine complexity** — Multi-phase state machine with epoch coordination, version management, and I/O tracking is the most complex subsystem. Bugs here are hard to find and reproduce. | 4 | 4 | 16 | Dedicated Phase 5 with 5-6 week budget. Éowyn's simulation testing framework specifically targets checkpoint phases. State machine formally documented with transition table. |
| X2 | **Unsafe code volume** — Hash index, record access, page management, and FFI all require `unsafe`. High `unsafe` surface area increases audit burden and risk of soundness bugs. | 3 | 4 | 12 | Galadriel's security audit (Phase 9). Every `unsafe` block has a SAFETY comment. Miri verification for non-I/O paths. Minimize `unsafe` surface: encapsulate in small, well-tested modules. Target < 5% of total LOC. |
| X3 | **Callback threading model complexity** — Without async/await, multi-step operations (pending read -> I/O -> continue -> user callback) require manual state threading through callbacks. Error-prone and hard to debug. | 3 | 3 | 9 | Define clear state machine for each pending operation type. Exhaustive integration tests for all pending paths. Document callback flow with sequence diagrams. |
| X4 | **Dual execution context (prev/cur) during CPR** — Sessions maintain two execution contexts that swap during checkpoint. Incorrect swap timing or context selection causes operations to use wrong version. | 3 | 4 | 12 | Port directly from C++/C# with behavioral equivalence tests. Simulation testing of context swap timing. Unit tests for every phase transition. |
| X5 | **Feature interaction complexity** — Checkpoint + grow + GC + compaction can all be in progress simultaneously. Interactions between these subsystems create exponential state space. | 3 | 4 | 12 | Phase 9 simulation testing explores concurrent subsystem interactions. Limit concurrent background operations in v1 (e.g., no grow during checkpoint). Document allowed combinations. |

### 14.4 Compatibility Risks

| # | Risk | L | I | Score | Mitigation |
|---|------|---|---|-------|------------|
| K1 | **Cross-platform I/O differences** — O_DIRECT behavior, alignment requirements, and file system semantics differ between Linux, macOS, and Windows. io_uring availability varies by kernel version. | 3 | 3 | 9 | Abstract all platform-specific I/O behind `Device` trait. Test on Linux (primary) and macOS (secondary). Windows: document as best-effort. Use conditional compilation (`#[cfg(target_os)]`) for platform-specific code. |
| K2 | **C FFI portability** — Different C compilers (gcc, clang, MSVC) have different ABI conventions, alignment rules, and calling conventions. | 2 | 3 | 6 | Use `#[repr(C)]` for all FFI-visible structs. Test with gcc and clang on CI. Use `cbindgen` for header generation. Keep FFI surface minimal (opaque handles). |
| K3 | **Async runtime compatibility** — Future Rust async runtimes may have different APIs or conventions that our adapter pattern doesn't accommodate. | 2 | 2 | 4 | Design adapter trait with minimal assumptions. Validate with at least 2 different runtimes (Tokio + one other). Document adapter authoring guide. |
| K4 | **Minimum Supported Rust Version (MSRV)** — Using nightly features or very recent stable features limits adoption. | 2 | 2 | 4 | Target stable Rust (latest - 2 versions). No nightly features in core crate. Document MSRV in Cargo.toml. Test on MSRV in CI. |

### 14.5 Scope Risks

| # | Risk | L | I | Score | Mitigation |
|---|------|---|---|-------|------------|
| S1 | **Feature creep toward Tsavorite/Garnet** — Pressure to include Tsavorite features (revivification, record locking enhancements, new index types) before v1 is stable | 3 | 4 | 12 | User directive: Tsavorite/Garnet is explicitly NOT for v1. Captured in decisions.md. Architecture designed for extensibility (trait-based index, log, device) but features deferred to v2+. Gandalf has veto authority on scope expansion. |
| S2 | **F2 two-tier scope expansion** — F2 (hot/cold stores, ColdIndex) is complex (~5 files in C++) and could consume months if pulled into scope prematurely | 2 | 4 | 8 | Explicitly deferred. Architecture uses `trait IndexProvider` and `trait LogProvider` to allow F2 extension without core changes. Phase 2 (hash index) does not preclude ColdIndex addition later. |
| S3 | **FasterLog scope** — FasterLog is a standalone subsystem (~120KB in C#) that could distract from core KV work | 2 | 3 | 6 | Deferred to Full phase (post-MVP). FasterLog is relatively self-contained and can be implemented independently. |
| S4 | **Remote server / distributed features** — Requests for TCP/gRPC server, pub-sub, or distributed recovery before core is mature | 2 | 4 | 8 | Explicitly deferred to Phase 3+. Documented in roadmap. Not architecturally precluded but not prioritized. |
| S5 | **Timeline pressure** — 6-9 month estimate may face pressure to compress, leading to quality shortcuts | 3 | 4 | 12 | User directive: unlimited budget/time, quality is everything. Document this in all planning artifacts. Gandalf enforces quality gates at each phase boundary. No phase completes without success criteria met. |

### 14.6 Risk Heat Map Summary

**Critical (Score >= 15) — Require active mitigation:**
- C1: Lock-free algorithm bugs (20)
- C2: Epoch use-after-free (15)
- C3: Crash recovery corruption (15)
- C4: CPR version boundary (15)
- X1: Checkpoint state machine (16)

**High (Score 10-14) — Require monitoring:**
- C6: Hash table grow (12)
- P1: Cache miss amplification (12)
- X2: Unsafe code volume (12)
- X4: Dual execution context (12)
- X5: Feature interaction (12)
- S1: Tsavorite scope creep (12)
- S5: Timeline pressure (12)
- C5: Epoch drain ordering (10)

**Medium (Score 6-9) — Addressed by standard practices:**
- P2: CAS contention (9)
- K1: Cross-platform I/O (9)
- X3: Callback threading (9)
- C7: Pending operation ordering (8)
- S2: F2 scope expansion (8)
- S4: Remote/distributed (8)
- P3: I/O callback overhead (6)
- P4: Epoch overhead (6)
- P5: Page allocation contention (6)
- K2: C FFI portability (6)
- S3: FasterLog scope (6)

**Low (Score < 6) — Accept:**
- P6: Monomorphization bloat (4)
- K3: Async runtime compat (4)
- K4: MSRV (4)

---

*End of Part 3. Sections 1-10 are in Parts 1 and 2.*
