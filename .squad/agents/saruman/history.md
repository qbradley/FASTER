# Project Context

- **Owner:** qbradley
- **Project:** Rust implementation of Microsoft FASTER — a high-performance durable hash map
- **Stack:** Rust (primary), C++ (reference), C# (reference), C FFI
- **Goal:** Production-grade, no async runtime required, seamless Tokio integration, idiomatic Rust API + C FFI interface. Quality bar: mission-critical cloud services at planetary scale.
- **Created:** 2026-03-05

## Core Context

- **Role:** C++ analysis expert + integration test builder. Analyzed the full C++ FASTER codebase (~21K LOC) producing `.squad/agents/saruman/cpp-analysis.md` (45KB). Your async/await recommendation was overridden by user directive — async ergonomics delivered via adapter layer (Elrond's bridge).
- **C++ FASTER key findings:** 5 subsystems (hash index, hybrid log, epochs, state machines, device layer). Lock-free CAS hash table. 32MB pages. Callback chains for async I/O. CPR checkpoint/recovery. Template metaprogramming → Rust traits. Thread-local epoch IDs (96-thread limit). Critical files: `address.h`, `record.h`, `faster.h` (2500+ lines), `persistent_memory_malloc.h`, `light_epoch.h`, `state_transitions.h`.
- **Architecture (Gandalf 2026-03-05):** 12 binding decisions — no async in core, custom epoch, inline VL records, lock-free hash, 32MB pages, completion-based Device, opaque FFI handles, own checkpoint format, Result<Status,Error>, thread-affine sessions (!Send), Key/Value traits, epoch-grow protocol. Full spec in `.squad/agents/gandalf/rust-faster-architecture.md`.
- **C++ wrapper (2026-03-07):** `faster_cpp.h` (725-line header-only C++17 wrapper). RAII FasterKv+Session, Serializer<T> trait. cbindgen renders `Option<fn>` as opaque struct — wrapper redeclares FFI surface directly. Checkpoint+recovery must use same directory. 23/23 checks pass.
- **C++ integration test suite (2026-03-07):** 5 suites, 76 tests, 163 assertions. FFI gaps: Snapshot not wired, ContinueSession stub, no raw_handle(), no compaction/scan/stats FFI.

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

---

## 2026-03-09: I/O Error Injection Integration Tests

**What:** Created `rust/crates/faster-core/tests/io_error_injection.rs` — 29 integration tests (1,200+ LOC) that deliberately trigger I/O errors and verify the store handles them correctly.

**Branch:** `saruman/io-error-injection`

**Test Harness:**
- `FaultInjectingDevice` — a `Device` wrapper around `InMemoryDevice` with configurable fault injection. Supports write failures (ENOSPC), read failures (EIO), torn writes, various error codes, and runtime fault toggling via shared `AtomicBool`.
- Self-contained in the test file to avoid circular dependency on `faster-dst`.

**Test Categories (29 tests):**
1. **In-memory isolation (1):** Device errors don't affect mutable-region operations.
2. **Write failures (3):** `write_sync` error propagation, data preservation after N writes, fault log tracking.
3. **Read failures (2):** `read_sync` error propagation, failure after N successful reads.
4. **Async callback errors (2):** `read_async` and `write_async` deliver `IoStatus::Error` through callbacks.
5. **Error code variants (2):** 6 error codes propagate correctly; `IoStatus` variant discrimination.
6. **Torn writes (3):** Partial data written at configurable fractions, minimum 1-byte write, fault logging.
7. **Runtime toggling (2):** Enable/disable faults at runtime for both read and write paths.
8. **Store resilience (2):** Store continues operating after flush failures; recovers after transient errors.
9. **Concurrent sessions (1):** 4 threads, one triggers device errors during flush, all verify their data.
10. **Checkpoint/recovery (2):** Checkpoint handles write failures gracefully (no panic); recovery handles read failures (no panic).
11. **Counter accuracy (1):** Read/write counters track sync and async operations.
12. **Maintenance under faults (2):** Maintenance with read errors; interleaved success/failure flush cycles.
13. **RMW with faults (1):** Read-modify-write on in-memory records unaffected by device errors.
14. **Stress (2):** 1K ops with toggling faults; 10K ops with periodic fault windows.
15. **Device compliance (2):** Passthrough behavior when faults disabled; truncate delegation.

**Key Design Decisions:**
1. **No `faster-dst` dependency** — Built a self-contained `FaultInjectingDevice` inside the test file. `faster-dst` depends on `faster-core`, so adding it as a dev-dependency would create a circular dependency.
2. **Deterministic faults** — Threshold-based (`fail_after_n`) and flag-based (`AtomicBool`) injection, not probabilistic. Zero false positives.
3. **C++ behavioral contracts** — Tests mirror C++ error handling: `IoStatus::Error(code)` through callbacks, `io::Error` from sync paths, in-memory operations isolated from device errors.

**Runtime:** All 29 tests complete in < 0.5 seconds. Zero flakiness.

**Precheckin:** 2033/2033 workspace tests pass (29 new + 2004 existing).



---

## 2026-03-09T1817Z: Wave 2 Completion — Team Context Update

**Wave 2 summary (all 5 agents merged to `squad`, 2,067 tests pass in 58s):**

- **Saruman (you):** 29 I/O error injection tests with `FaultInjectingDevice` in `rust/crates/faster-core/tests/io_error_injection.rs` (1,307 LOC). Commits `eed208ac`, `b4239705`. Note: Éowyn's stray commit `f5f9ed7a` was dropped by coordinator before merge.
- **Elrond:** 29 tokio integration tests in `rust/crates/faster-tokio/tests/integration.rs`.
- **Legolas:** Performance regression detection scripts (`bench-compare.sh`, `bench-baseline.sh`) + `benchmarking.md`.
- **Éowyn:** DST expanded 5→108 scenario templates (14 categories, 324 test cases in ~30s).
- **Boromir:** 20 recovery edge case tests in `rust/crates/faster-core/tests/recovery_edge_cases.rs` (1,064 LOC). Filed `SyncFileDevice` `"log."` prefix coupling decision.

**Key decisions added to `decisions.md`:**
- SyncFileDevice prefix must be `"log."` for recovery compatibility
- DST parameterized expansion architecture (108 templates)
- hash_layout_bench OA prototype removal
- Performance regression detection infrastructure
