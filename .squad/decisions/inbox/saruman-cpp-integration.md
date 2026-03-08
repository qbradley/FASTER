# Decision: Snapshot Checkpoint Not Functional via FFI

**Agent:** Saruman (C++ Expert)
**Date:** 2026-03-07
**Status:** Observation (not a decision — needs team awareness)
**Impact:** Checkpoint/recovery FFI surface

## Finding

During C++ integration testing (A6), `faster_checkpoint()` with
`FasterCheckpointType_Snapshot` returns `FasterStatus_CheckpointError`.
FoldOver checkpoints work correctly.

The `CheckpointType::Snapshot` enum exists in the metadata layer, but
the write path through `FasterKv::checkpoint()` does not support it yet.

## Recommendation

1. Document Snapshot as unsupported in the FFI header and C++ wrapper
2. Add a Rust-side integration test for Snapshot to track when it becomes functional
3. C++ tests gracefully skip this path with a `[SKIP]` annotation

## Who Needs to Know

- **Gandalf/Aragorn:** Core team should confirm if Snapshot is planned for current phase
- **Sam:** Device layer may need snapshot-specific write path
- **Frodo:** Roadmap impact — Snapshot checkpoint is listed in MVP scope

---

# Decision: C++ Test Infrastructure Conventions

**Agent:** Saruman (C++ Expert)
**Date:** 2026-03-07
**Status:** Proposed convention
**Impact:** C++ test organization

## Convention

- C++ integration tests live at `rust/crates/faster-ffi/cpp/tests/`
- Each test suite is a standalone executable with its own `main()`
- Test harness is `test_harness.h` (minimal, no external deps)
- `run_tests.sh` is the canonical way to build and run all C++ tests
- CMakeLists.txt integrates with CTest for IDE and CI support
- Build artifacts go in `cpp/build/` (gitignored)

## Rationale

Self-contained test executables are simpler than a monolithic test binary
and allow parallel execution. The lightweight harness avoids pulling in
Google Test or Catch2 for what is fundamentally a cross-language integration
test suite (not a unit test framework).
