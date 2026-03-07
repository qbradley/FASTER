# Decision: io_uring Async Read Beyond EOF Semantics

**Author:** Gimli (Database/Storage Expert)
**Date:** 2026-03-07
**Status:** Observation (informational)

## Context

During battle testing of UringDevice, discovered that io_uring's `read` opcode returns a short read (0 bytes) when reading beyond the end of a file, rather than filling the buffer with zeros.

The sync path (`read_sync` → `read_complete_at`) explicitly handles this by zero-filling the remainder of the buffer on short reads/EOF.

The async path does not — the io_uring CQE result indicates the number of bytes actually read, and the callback reports success with partial bytes.

## Implication

Any caller using `read_async` at offsets beyond what was written may receive partial or zero-length reads. The hybrid log allocator and recovery code must account for this if they rely on zero-initialized unwritten regions.

## Recommendation

If FASTER's higher layers depend on unwritten regions returning zeros, the UringDevice should add a zero-fill step in the completion handler for short reads (matching the sync path behavior). This is a future enhancement — the current behavior is correct per the Device trait contract which makes no promise about unwritten regions.
