# FFI Surface Gaps — C++ Integration Test Findings

Discovered during Iteration 6 (C++ wrapper integration test suite).

## Known Gaps

### 1. Snapshot Checkpoint Not Wired Through `FasterKv::checkpoint()`

**Severity:** Medium  
**Status:** Known limitation  
**Test:** `test_lifecycle::checkpoint_snapshot_and_recover` (gracefully skipped)

The `FasterCheckpointType::Snapshot` enum variant exists in the FFI surface,
but calling `faster_checkpoint()` with it returns `CheckpointError`. The
checkpoint metadata infrastructure supports snapshot type, but the
`FasterKv::checkpoint()` method in faster-core may not implement the
snapshot write path yet. FoldOver checkpoints work correctly.

**Workaround:** Use `FasterCheckpointType::FoldOver` for now.

### 2. Session Continuation Is a Stub

**Severity:** Low  
**Status:** Documented in FFI  

`faster_continue_session()` creates a fresh session and always returns
serial number 0. True session continuation (resuming from a checkpoint
token with a specific serial number) is not yet implemented.

**Impact:** Applications cannot resume exactly where they left off after
recovery — they must re-issue operations from the last known checkpoint.

### 3. No `Session::raw_handle()` Accessor

**Severity:** Low  
**Status:** Design choice  

The C++ `Session<K,V>` class does not expose its raw FFI handle. This
makes it impossible to mix high-level Session operations with low-level
`faster_*()` FFI calls on the same session. Tests that need raw FFI
access must create a separate session via `faster_session_start()`.

**Recommendation:** Consider adding `FasterHandle raw_handle() const`
to the Session class for advanced use cases.

### 4. No Compaction / Log Truncation FFI Surface

**Severity:** Medium  
**Status:** Not yet exposed  

The C++ wrapper has no API for triggering compaction, log truncation,
or querying the hybrid log address boundaries (head/safe-read-only/tail).
These operations exist in the C++ reference FASTER but are not yet
exposed through the Rust FFI.

### 5. No Scan / Iteration API

**Severity:** Medium  
**Status:** Not in MVP scope  

There is no FFI surface for scanning or iterating over key-value pairs.
This is a planned post-MVP feature per the architecture spec.

### 6. No Store Size / Statistics API

**Severity:** Low  
**Status:** Not yet exposed  

No FFI functions for querying store size, record count, or memory usage
statistics. Useful for monitoring and capacity planning.
