# Orchestration Log: Aragorn (Wave 2 — Task 1g)

**Spawn Time:** 2026-03-05T20:02:00Z  
**Agent:** Aragorn (Claude Opus 4.6)  
**Mode:** Background  
**Scope:** Rust expert, epoch-based reclamation & concurrent memory safety  

## Task Executed

### Task 1g: Epoch-Based Reclamation System
- Designed custom `EpochTable` (~400 lines) instead of wrapping crossbeam-epoch
- Implemented `EpochThread` per-thread epoch tracking with local phase markers
- Designed `EpochGuard` (thread-affine via `PhantomData<*const ()>`) for lexical protection windows
- Designed `DrainList` for epoch-keyed drain callbacks with mutex-based queue (push/drain are cold paths)
- Memory ordering strategy:
  - Per-thread updates (Relaxed): single-writer, no synchronization needed
  - Inter-thread coordination (SeqCst on bump): total ordering for epoch consistency
  - Guard protection (Release/Acquire pair): ensures visibility of protected-region side effects
- Implemented `try_drain` for safe callback execution (collected outside lock to prevent deadlocks)
- EpochGuard is !Send (!Sync for safety) — must be dropped on same thread that created it
- **Result:** 340 tests passing

## Decisions Written to Inbox

1. **aragorn-epoch-system.md** — Custom system not crossbeam-epoch, mutex-based drain list, memory ordering strategy, thread-affine guards

## Code Output

- `rust/faster-core/src/epoch.rs` — EpochTable, EpochThread, EpochGuard, DrainList
- Concurrent stress tests (multi-threaded epoch progression, concurrent guard creation/drop)
- Memory ordering tests validating Release/Acquire semantics
- Thread-affinity tests confirming !Send constraint on guards

## Test Summary

- Epoch module: 340 tests passing (cumulative workspace: 1,070 tests)
- Stress tests validate concurrent epoch progression under load
- Guard lifecycle tests confirm proper protect/unprotect ordering
- Drain callback tests verify epoch-safe execution (no use-after-free)

## Integration Points

- **All modules:** EpochGuard used in read-side critical sections for memory safety
- **Overflow allocator:** free_at_epoch(addr, epoch) defers reclamation until epoch is safe
- **Hash table:** Epoch coordination enables wait-free reads with deferred updates
- **Session model:** EpochThread lifecycle tied to FASTER session (1 per thread)

## Architectural Notes

- EpochGuard thread-affinity matches FASTER session model (Decision #10: thread-affine sessions)
- Guard must be dropped on same thread to unprotect (borrow checker enforces lifetime invariant)
- Memory ordering conservative (SeqCst on bump) — could be relaxed to AcqRel with careful analysis, but bump path not hot enough to warrant risk
- Custom system simpler than crossbeam adapter: FASTER needs drain callbacks + phase markers, not just per-object deferred deallocation

## Next Steps

- Task 2b (Overflow Allocator) integrates free_at_epoch() for safe bucket reclamation
- Aragorn (2d: Hash Table Core) uses guards for lock-free lookup/insert correctness
- Future hybrid log will use epoch system for page-level reclamation

## Status

✅ **COMPLETE** — All 1g objectives met. Concurrent stress validated. Ready for merge.
