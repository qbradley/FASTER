# Project Context

- **Owner:** qbradley
- **Project:** Rust implementation of Microsoft FASTER — a high-performance durable hash map
- **Stack:** Rust (primary), C++ (reference), C# (reference), C FFI
- **Goal:** Production-grade, no async runtime required, seamless Tokio integration, idiomatic Rust API + C FFI interface. Quality bar: mission-critical cloud services at planetary scale.
- **Created:** 2026-03-05

## Learnings

### 2026-03-05: Cross-Function Team Update

**Team Consensus on Async Model:**
- Your recommendation (async/await instead of manual callbacks) has been approved
- Rationale: simpler, safer, idiomatic, composable, standard ecosystem expectation
- Implication: All operations return `impl Future<Output = Result<T>>`, Tokio runtime required
- Status: Awaiting Frodo and Gandalf validation for implementation blockers and architecture alignment

**Feature Scope Decisions Finalized:**
- MVP (3-4 months): C++ baseline (Core KV, hybrid log, epochs, CPR recovery, fold-over/snapshot, sync+async ops)
- Full (6-8 months): Add FasterLog, incremental snapshots, read cache, Azure storage, scan iteration
- Deferred (v2+): F2 two-tier, ColdIndex, Remote server, Distributed recovery

**API Design Integration:**
- Your C++ architecture analysis paired with Faramir's C# insights to define simplified Rust API
- Callbacks reduced from 19 (C#) to ~5 with defaults
- Generic parameters reduced from 6 (C#) to 3-4 using associated types
- RAII guards for epoch protection (!Send compile-time safety)

### 2026-03-05: C++ FASTER Architecture Deep Dive

**Completed comprehensive analysis of C++ FASTER implementation (~21,000 LOC)**

#### Key Architectural Findings

1. **Core Subsystems (5 major components):**
   - Hash Index: `hash_bucket.h`, `hash_table.h`, `mem_index.h`, `cold_index.h` — lock-free CAS-based hash table
   - Hybrid Log: `persistent_memory_malloc.h`, `lss_allocator.h` — page-based log with head/tail pointers
   - Epoch System: `light_epoch.h` — epoch-based memory reclamation, 96 thread limit
   - State Machines: `state_transitions.h`, `checkpoint_state.h`, `phase.h` — coordinated checkpoint/recovery
   - Device Layer: `device/file_system_disk.h`, `environment/file_*.h` — async I/O abstraction

2. **Critical File Paths:**
   - **Main entry point:** `cc/src/core/faster.h` (2500+ lines, template class `FasterKv<K,V,D,H,OH>`)
   - **Address scheme:** `cc/src/core/address.h` (48-bit: 25-bit offset + 23-bit page = 32MB pages, 8M max pages)
   - **Record layout:** `cc/src/core/record.h` (8-byte header + aligned key + aligned value)
   - **Operations:** Read/Upsert/RMW/Delete in `faster.h`, lines 1200-2500
   - **F2 (hot/cold):** `cc/src/core/f2.h` — two-store architecture with background compaction

3. **Memory Management Patterns:**
   - **Hybrid log allocator:** 32MB pages, atomic head/tail, flush/evict lifecycle
   - **LSS allocator:** ~8-16KB segments, bump-pointer, FIFO-optimized for async contexts
   - **Fixed-page allocator:** 64-byte blocks for hash table overflow buckets
   - **Smart pointers:** `aligned_unique_ptr_t` (custom deleter), `context_unique_ptr_t` (LSS deleter)

4. **Concurrency Model:**
   - **Epoch-based reclamation:** Per-thread epoch tracking, drain list for deferred actions
   - **Thread IDs:** Fixed pool of 96 IDs, atomic reservation, thread-local `ThreadId` with RAII
   - **CAS loops:** All hash table updates linearizable via compare_exchange_strong
   - **Phase coordination:** Double-buffered `ExecutionContext`, global `SystemState` with 14 phases

5. **Async I/O Architecture:**
   - **Context deep copy:** Stack → heap when going async (LSS allocator)
   - **Callback chains:** Nested I/O (index lookup → disk read → user callback)
   - **Pending queue:** Per-thread `thread_io_responses` queue, FIFO ordering
   - **Platform-specific:** Windows IOCP, Linux libaio/io_uring

6. **Checkpoint/Recovery:**
   - **CPR (Copy-on-Write):** Version tracking, chain old records during checkpoint
   - **State machine:** REST → PREP_INDEX_CHKPT → INDEX_CHKPT → PREPARE → IN_PROGRESS → WAIT_PENDING → WAIT_FLUSH → PERSISTENCE_CALLBACK
   - **Metadata:** `IndexMetadata` (table size, log addresses), `LogMetadata` (per-thread serial numbers)
   - **Recovery:** Read metadata → reconstruct hash table → replay log

7. **F2 (Generation 2) Hot/Cold Architecture:**
   - **Hot store:** `MemHashIndex` (in-memory), small log (4GB), fast ops
   - **Cold store:** `ColdIndex` (two-level on-disk), large log (1TB), slow index lookup
   - **Compaction:** Background thread migrates old records hot → cold
   - **Read path:** Try hot first, fall back to cold

#### Hardest Patterns for Rust Port

1. **Placement new:** Cannot construct at arbitrary addresses → manual serialization needed
2. **Template metaprogramming:** Policy-based design, type introspection → traits + associated types
3. **Callback chains:** Manual context allocation → async/await eliminates this
4. **Reinterpret cast:** Strict aliasing violations → unsafe pointer arithmetic
5. **Varargs templates:** Perfect forwarding → builder pattern or multiple overloads
6. **Thread-local destructors:** Cleanup on thread exit → explicit RAII guards

#### Key Behavioral Contracts

1. **Epoch safety:** Memory freed in epoch E not accessed by threads protecting epoch < E
2. **CAS linearizability:** All hash table updates are linearizable
3. **CPR versioning:** Records have checkpoint_version, old versions chained during checkpoint
4. **Read-only immutability:** Records in [HeadAddress, ReadOnlyAddress) are immutable
5. **Disk I/O durability:** fsync after writes, atomic checkpoint metadata update
6. **Async FIFO ordering:** Pending ops complete in submission order per thread

#### Rust Port Recommendations

**Use these alternatives:**
- Hybrid log: `Vec<Page>` + Tokio file I/O, custom `PageAllocator`
- Hash index: Packed `u64` entries, separate overflow arena
- Epoch system: `crossbeam::epoch` or custom with `AtomicU64`
- Async model: **async/await everywhere**, eliminate manual callbacks
- Device layer: Trait-based (`trait Device: AsyncRead + AsyncWrite`)

**Hot paths (must be zero-cost):**
- Hash lookup: inline, no allocations
- In-memory read: direct pointer dereference (unsafe OK)
- Epoch acquire/release: inline atomics

**Can defer:**
- `malloc_fixed_page_size.h` → simpler Rust allocator
- `lss_allocator.h` → use `Box` or arena
- `cold_index.h` → complex, only for F2

**Critical files for Gandalf:**
1. `address.h`, `record.h` — data layout
2. `persistent_memory_malloc.h` — hybrid log
3. `hash_bucket.h`, `hash_table.h`, `mem_index.h` — hash table
4. `faster.h` — operations (Read/Upsert/RMW/Delete)
5. `light_epoch.h`, `thread.h` — concurrency
6. `state_transitions.h`, `checkpoint_state.h` — checkpoints

**Analysis artifacts:**
- Full report: `.squad/agents/saruman/cpp-analysis.md` (45KB, ~2500 lines)
- Covers: architecture, subsystems, operations, concurrency, memory mgmt, porting challenges

<!-- Append new learnings below. Each entry is something lasting about the project. -->

---

## 2026-03-05T18:33: Gandalf Rust FASTER Architecture Finalized

**What:** Gandalf completed 288 KB comprehensive Rust FASTER architecture specification (6101 lines, 14 sections). 3-part parallel document (Gandalf-A/B/C) due to massive context requirements.

**Artifact Location:** `.squad/agents/gandalf/rust-faster-architecture.md`

**Sections:** Vision, Crates, Data Structures, Concurrency, Storage, Operations, Checkpoint, API, FFI, Async Integration, Testing, Phases, 12 Key Decisions, Risk Register (25 risks, 5 critical)

**12 Core Architectural Decisions:**
1. No async/await in core (user directive) — callback/completion model
2. Custom epoch (not crossbeam-epoch) — FASTER-specific semantics
3. Inline variable-length records (C++ model)
4. Rust atomic patterns for hash index — lock-free
5. 32 MB default page size — 512-byte sector alignment
6. Completion-based Device trait — runtime-agnostic
7. Opaque FFI handles (~15 function API surface)
8. Own checkpoint format — forward-compatible binary
9. Result<Status, Error> taxonomy with bitflag Status
10. Thread-affine sessions (!Send compile-time) — mono-threaded safety
11. Unified Key/Value traits for fixed+variable-length
12. Epoch-coordinated grow protocol (doubles, no shrink)

**What This Means For You:**
- **All:** You are now operating under this architecture. Decisions binding for implementation phases.
- **Elrond:** Async adapter design bridges callback core to Future/async/await ecosystem (Decision #3 path).
- **Aragorn:** Core code stays fully sync, zero async runtime deps. No tokio in core.
- **Sam:** Device trait is callback-based (Box<dyn FnOnce>), not Future-based.
- **Éowyn:** Simulation framework must model callback completion ordering for testing.
- **Galadriel:** Unsafe audit scope: hash index atomics (AcqRel/Acquire), record pointer arithmetic, FFI boundary.
- **Frodo:** Phase roadmap and implementation sequence encoded in Part 3.
- **Saruman:** Your async/await recommendation overridden by user directive; async ergonomics achieved via adapter layer.

**Risk Profile:** 25 identified risks (5 critical ≥15, 8 high 10-14). Primary mitigations: deterministic simulation, Miri verification, security audit, cross-implementation comparison.

**Next Steps:** Risk mitigation task force (Éowyn lead), unsafe audit planning (Galadriel lead), Phase 1 sprint (Frodo lead), async adapter spike (Elrond lead).

---

## 2026-03-07: C++ Wrapper Header over Rust FFI (W3-04)

**What:** Built `faster_cpp.h`, a header-only C++17 wrapper over the Rust FASTER C FFI layer. Delivered on branch `saruman/cpp-wrapper`.

**Files delivered:**
- `rust/crates/faster-ffi/cpp/faster_cpp.h` — 725-line header-only wrapper
- `rust/crates/faster-ffi/cpp/example.cpp` — 6-test, 23-check verification suite
- `rust/crates/faster-ffi/cpp/Makefile` — builds against `libfaster_ffi.so` / `.a`

**Architecture decisions:**
1. **Self-contained C FFI redeclaration** — The header redeclares the FFI surface with proper C++ types instead of including `faster.h`. The cbindgen-generated header renders `Option<extern "C" fn(...)>` as opaque `struct Option_*Fn` types that cannot be constructed from C/C++. Since the Rust ABI guarantees these are nullable function pointers, we declare them as such.
2. **RAII all the way** — `FasterKv` owns the store handle (calls `faster_close` on destroy), `Session` owns the session handle (calls `faster_session_end` on destroy). Both are move-only.
3. **Serializer<T> trait** — Defaults to `memcpy` for trivially-copyable types; specialized for `std::string` and `std::vector<uint8_t>`. Users can specialize for custom types.
4. **Callbacks via C function pointers** — `RmwWithCallbacks()`, `UpsertWithCallbacks()`, `ReadWithCallbacks()` accept raw `extern "C"` function pointers, matching the `_ex()` FFI surface. C++ lambdas cannot be used directly (no captures), but stateless lambdas decay to function pointers.

**Key findings:**
- Checkpoint and recovery must use the **same directory** as the store path. The Rust implementation writes checkpoint files to `{dir}/checkpoints/{token}/`.
- The Rust target directory uses a platform-specific triple (`x86_64-unknown-linux-gnu`) in the path — the Makefile handles this with a fallback.
- Merge conflicts in the FFI source files (from `sam/sealed-bit-p03` branch) had to be resolved before building.

**Verification results:** 23/23 checks pass:
- Test 1: Basic CRUD (upsert, read, delete, RMW replacement)
- Test 2: Custom RMW callbacks (sum-store pattern: 10+5+3=18)
- Test 3: Bulk operations (1000 records)
- Test 4: Checkpoint and recovery (FoldOver)
- Test 5: String keys and values
- Test 6: Move semantics (Session + FasterKv)

---

## 2026-03-07T22:22: C++ Integration Test Suite (Iteration 6, A6)

**What:** Built comprehensive C++ integration test suite for the FASTER Rust FFI wrapper. 5 test suites, 76 tests, 163 assertions — all green.

**Files delivered (branch `saruman/cpp-integration-tests`):**
- `rust/crates/faster-ffi/cpp/tests/test_harness.h` — Lightweight test framework (CHECK/REQUIRE/CHECK_THROWS)
- `rust/crates/faster-ffi/cpp/tests/test_basic_ops.cpp` — 18 tests: CRUD with uint64_t, string, vector<uint8_t>, bulk ops, mixed types
- `rust/crates/faster-ffi/cpp/tests/test_lifecycle.cpp` — 14 tests: session management, checkpoint/recover, continue_session, move semantics
- `rust/crates/faster-ffi/cpp/tests/test_threading.cpp` — 6 tests: concurrent sessions, parallel ops, thread-affinity enforcement, session churn
- `rust/crates/faster-ffi/cpp/tests/test_callbacks.cpp` — 10 tests: RMW (sum-store, multiply), Upsert (custom put), Read (custom get), error propagation
- `rust/crates/faster-ffi/cpp/tests/test_error_handling.cpp` — 28 tests: invalid handles, null pointers, buffer-too-small, double-free, exception safety
- `rust/crates/faster-ffi/cpp/CMakeLists.txt` — CMake build with CTest integration
- `rust/crates/faster-ffi/cpp/run_tests.sh` — End-to-end build + test runner
- `rust/crates/faster-ffi/cpp/tests/FFI_GAPS.md` — FFI surface gap documentation

**FFI Surface Gaps Discovered:**
1. **Snapshot checkpoint not wired** — `FasterCheckpointType::Snapshot` returns `CheckpointError` through `FasterKv::checkpoint()`. FoldOver works fine.
2. **ContinueSession is a stub** — Always returns serial 0. No true session resume from checkpoint tokens.
3. **No Session::raw_handle()** — Can't mix high-level Session with low-level FFI calls on same session.
4. **No compaction/scan/statistics FFI** — Not yet exposed through FFI surface.

**Key Testing Insight:**
- Thread-affinity enforcement (ThreadMismatch) works correctly across FFI boundary.
- The Rust static library requires `-lpthread -ldl -lm` system library dependencies when linked into C++.
- Pre-existing Rust compilation issues in faster-core (unrelated to FFI) prevent `cargo clippy` from passing on squad branch.

