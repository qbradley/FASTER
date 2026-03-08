# Decision: Revivification via CAS-Unseal on Sealed Records

**Agent:** Sam (Systems Programming Expert)
**Date:** 2026-03-08
**Status:** DECIDED — Implemented
**Impact:** Upsert/RMW performance on sealed records in the mutable region

## Decision

Sealed records in the mutable region can be **revivified** (updated in-place) by atomically clearing the sealed bit via CAS, rather than always falling back to copy-to-tail (RCU).

## Rationale

1. **Performance**: Copy-to-tail allocates new log space, writes a full record copy, and CAS-updates the hash entry. Revivification avoids all of this — just a CAS + value overwrite.
2. **Log growth**: Revivification reduces write amplification by not appending redundant copies.
3. **Thread safety**: Only one thread can win the CAS race; losers fall back to the safe copy-to-tail path. No correctness compromise.
4. **Backward compatibility**: The `Revivified` status is a new variant — existing code matching on `is_success()` or `is_modified()` automatically includes it.

## Memory Ordering

- `try_revivify`: AcqRel on success (publishes unseal), Acquire on failure (observes winner)
- Matches the existing `try_seal` pattern — seal/unseal are symmetric CAS operations

## Conditions for Revivification

All must hold:
1. Record is in the **mutable** region (above safe_read_only_address)
2. Record is **sealed** (bit 60 set)
3. CAS to **unseal** succeeds (no concurrent modification)
4. Value **fits** in the existing allocation (no growth)

## What This Means For Others

- **Boromir/Faramir**: `OperationStatus::Revivified` is now a possible return from `upsert()` and `rmw()`. Treat it as a successful in-place update.
- **Legolas**: Revivification should improve hot-path latency for workloads that seal-then-update records. Benchmark opportunity.
- **Galadriel**: No new unsafe code paths — `try_revivify` uses the existing `compare_exchange` wrapper.
- **Éowyn**: New test surface for concurrent seal/unseal races.

## Branch

`sam/revivification` — commit `2bb358a5`
