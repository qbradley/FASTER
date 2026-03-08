# Decision: Sealed Bit in RecordInfo (P-03)

**Author:** Sam (Systems Programming Expert)
**Date:** 2026-03-07
**Status:** Implemented
**Branch:** `sam/sealed-bit-p03`
**Commit:** `f2d516c1`

## Context

P-03 requires a "sealed" bit in RecordInfo that provides write-protection during RCU transitions. This is a prerequisite for P-04 (revivification), which needs to prevent in-place mutations to records being copied.

## Decision

Narrowed the checkpoint version field from 13 to 12 bits (max 4095) and allocated bit 60 as the sealed flag. All flag bits are now contiguous at bits 60-63.

### Bit Layout Change
- **Before:** `[63:Final][62:Tombstone][61:Invalid][60..48:Version(13)][47..0:PreviousAddr(48)]`
- **After:** `[63:Final][62:Tombstone][61:Invalid][60:Sealed][59..48:Version(12)][47..0:PreviousAddr(48)]`

### Semantic Rules
- In-place Upsert and RMW must check `is_sealed()` and fall back to copy-to-tail when set
- Delete (tombstone placement) is permitted on sealed records — it's metadata-only
- Records are never created sealed; sealing happens via atomic operations

### Memory Ordering
- `seal()`: Release — publishes all prior value writes
- `is_sealed()`: Acquire — observes writes made before sealing
- `try_seal()`: AcqRel/Acquire CAS — standard optimistic pattern

## Consequences

- Maximum checkpoint version reduced from 8191 to 4095 (sufficient for epoch tracking)
- All write paths audited and sealed checks added where needed
- P-04 can now use the sealed bit to protect records during revivification
