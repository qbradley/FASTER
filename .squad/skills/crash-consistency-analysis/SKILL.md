# Skill: Crash Consistency Analysis for Durable Storage

## When to Use

When designing or reviewing any code path that involves persistent state: writes to mmap'd files, checkpoint operations, log flushing, metadata updates. Every durability claim must survive the "power failure at any instruction" test.

## Pattern

### The Three Questions

For every write path, answer these systematically:

1. **What if power fails right here?**
   - Walk through the code line-by-line
   - At each write operation, consider: what state is on disk vs in memory?
   - What invariants are temporarily broken?

2. **What does recovery see?**
   - Partial writes (torn pages)?
   - Missing data that memory held?
   - Orphaned resources?
   - Inconsistent metadata vs data?

3. **Can we detect and repair?**
   - Checksums for corruption detection
   - Version numbers for staleness detection
   - Idempotent operations for retry safety
   - Append-only structures for monotonic progress

### Checkpoint Protocol Pattern

```
Phase 1: Prepare
  - Mark what will be checkpointed (logical snapshot)
  - No disk writes yet
  - Crash: nothing on disk, safe to restart

Phase 2: Persist Data
  - Flush log pages
  - Flush index structures
  - Each has checksums/validation
  - Crash: partial checkpoint on disk, metadata not yet committed

Phase 3: Commit Metadata
  - Atomic write of checkpoint token/manifest
  - Contains version, extents, checksums
  - Crash after: recovery sees complete checkpoint
  - Crash before: recovery ignores partial checkpoint
```

### Validation Checklist

- [ ] All durable data has checksums (page-level minimum)
- [ ] Metadata updated atomically (single write or WAL)
- [ ] Version numbers prevent stale data from being trusted
- [ ] Recovery validates checksums before using data
- [ ] Recovery handles missing files gracefully
- [ ] Torn writes are detectable (page checksums, magic numbers)

## Confidence: High

## Learned From

- **Charter principle**: "Every write path gets a crash analysis: what happens if power fails at each step?"
- **EPVS Implementation**: Two-phase protocol with version bumps preventing ABA
- **Checkpoint work**: Metadata widening (u32→u64) for version tracking
- **Recovery prefix fix**: Hardcoded assumptions break when device config changes

## Key Considerations

1. **Atomicity boundaries**: Where is the commit point? What's recoverable before vs after?
2. **Ordering dependencies**: Must index flush wait for data flush? Use barriers/sync points.
3. **Idempotence**: Can recovery apply the same checkpoint twice safely?
4. **Backward compatibility**: Can old code read new formats? Version fields critical.

## Example: RecordInfo Sealing

✅ **Correct**: Sealing uses atomic bit flip (Release ordering). Crash at any point:
- Before: record unsealed, writes can continue
- After: record sealed, recovery sees sealed flag and won't write

❌ **Wrong**: If sealing required 2+ operations without atomicity:
- Crash between ops → inconsistent state
- Recovery can't tell if partially sealed

## Key Files

- `rust/crates/faster-core/src/recovery/log_recovery.rs` (validation, checksums)
- `rust/crates/faster-core/src/checkpoint/` (checkpoint state machine)
- `rust/crates/faster-core/src/state/` (EPVS versioning protocol)
