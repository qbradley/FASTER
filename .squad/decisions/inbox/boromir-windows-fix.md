# Decision: Skip directory fsync on Windows

**Author:** Boromir (QA Engineer)
**Date:** 2025-07-18
**Status:** Implemented
**Commit:** bbb2f15a

## Context

Windows CI was failing with 64 test failures, all with the same error:
```
failed to write test checkpoint: IoError(Os { code: 5, kind: PermissionDenied, message: "Access is denied." })
```

## Root Cause

`fsync_dir()` in `checkpoint/metadata_store.rs` calls `fs::File::open()` on a directory. On Windows, this requires `FILE_FLAG_BACKUP_SEMANTICS` which Rust's standard library does not set, causing `PermissionDenied`.

## Decision

Use `#[cfg(unix)]` to limit directory fsync to Unix platforms. On Windows (and other non-Unix), the function is a no-op.

**Rationale:** NTFS journals metadata operations, so the atomic rename in `atomic_write()` provides sufficient durability guarantees. This is the same approach used by RocksDB, SQLite, and other cross-platform database engines.

## Impact

- Fixes all 64 Windows CI test failures
- No change to Unix behavior
- No durability regression on Windows (NTFS journaling covers us)

## Team Note

Any future code that touches filesystem directories (open, sync, delete) must be tested for Windows compatibility. `fs::File::open()` on directories is a Unix-ism that doesn't port cleanly.
