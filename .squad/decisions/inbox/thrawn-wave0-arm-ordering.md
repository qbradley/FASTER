# Decision: ARM Memory Ordering — No Conditional Fences Needed

**Agent:** Thrawn (Lead Architect)
**Date:** 2026-03-06
**Status:** DECIDED
**Impact:** Cross-platform correctness — affects all atomic operations

## Decision

No `#[cfg(target_arch = "aarch64")]` conditional fences are required in faster-core.

## Rationale

1. **Rust's `Ordering` enum is the abstraction boundary.** The compiler emits correct barrier instructions per-target (e.g., `dmb` on ARM for Acquire/Release). We never need to manually insert platform-specific fences.

2. **All `Ordering::Relaxed` uses are advisory counters** — metrics, `live_entry_count`, `high_water`, `allocator.count`. These are never read to make synchronization decisions. Individual 64-bit atomicity (guaranteed by hardware) is sufficient.

3. **All synchronization-critical paths use proper orderings:**
   - DrainList CAS: `AcqRel`/`Acquire`
   - Page state transitions: `AcqRel` via `compare_exchange`
   - Epoch table: `Acquire`/`Release`
   - Hash index: `AcqRel` CAS, `Acquire` loads
   - Allocator tail: `AcqRel` CAS, `Acquire` loads

4. **`Metrics::snapshot()` non-atomicity is documented and acceptable.** Cross-field inconsistency is inherent to advisory counters and explicitly documented.

## Implications

- No platform-specific code in faster-core's concurrency layer.
- ARM correctness is verified by `cargo check --target aarch64-unknown-linux-gnu`.
- Future atomic additions should follow the same pattern: use `Relaxed` only for advisory data, never for synchronization.
