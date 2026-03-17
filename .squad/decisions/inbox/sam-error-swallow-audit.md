# Error-Swallow & Lost Return Value Audit

**Scope:** `rust/crates/faster-core/src/` — all `.rs` files  
**Auditor:** Sam (Systems & Storage Expert)  
**Date:** 2025-07-15  
**Requested by:** qbradley

---

## Summary

| Category | Count |
|----------|-------|
| 🔴 Bug — must fix | 6 |
| 🟡 Risk — needs comment or review | 16 |
| 🟢 OK — correct as-is | ~80+ (incl. test code) |

**Is the codebase clean? NO.**

Six production-path issues were found where errors are silently swallowed or
only reported in debug builds. The most critical are in the I/O completion,
checkpoint, and shutdown paths — exactly the places where silent failures
cause data loss.

---

## 🔴 Bugs — Must Fix

### BUG-1: Flush error swallowed on Drop (data loss on unclean shutdown)

**File:** `store/kv.rs:276`  
```rust
let _ = self.flusher.flush_page_sync(page, page_table, ...);
```

**Context:** `impl Drop for FasterKv` — the ordered shutdown path. If
`flush_page_sync` returns `Err(FlushError)`, sealed pages are silently
lost. The store drops normally, giving the caller no indication that data
was not persisted.

**Fix:** At minimum, `eprintln!` the error. Ideally, set a "dirty shutdown"
flag or log to a well-known location so recovery can detect incomplete
flushes.

---

### BUG-2: Flush error swallowed during checkpoint (corrupt checkpoint)

**File:** `store/kv.rs:759`  
```rust
let _ = self.flusher.flush_page_sync(page, page_table, ...);
```

**Context:** `checkpoint()` method. All in-memory pages must be flushed to
the device before the checkpoint metadata is written. If any page flush
fails, the checkpoint proceeds with stale data on disk. Recovery from this
checkpoint would silently lose writes.

**Fix:** Propagate `Err` — the `checkpoint()` method already returns
`Result<CheckpointToken, CheckpointError>`.

---

### BUG-3: I/O error silently becomes "flushed 0" (silent flush failure)

**File:** `store/kv.rs:1517-1521`  
```rust
.flush_sealed_pages(&self.allocator, self.device.as_ref())
.unwrap_or(crate::hybrid_log::FlushBatchResult {
    flushed: 0,
    queue_full: false,
});
```

**Context:** `pub fn flush(&self) -> u32`. If `flush_sealed_pages` returns
an `Err(FlushError::IoError(_))`, it is converted to "0 pages flushed, no
backpressure" — indistinguishable from "nothing to flush". Callers have
no way to detect the I/O failure.

**Fix:** Change `flush()` to return `Result<u32, FlushError>` or at
minimum log the error and return 0 with a flag.

---

### BUG-4: I/O completion error only logged in debug builds

**File:** `store/kv.rs:1265-1271`  
```rust
if cio.status != IoStatus::Success {
    #[cfg(debug_assertions)]
    eprintln!("process_completed_io: I/O completed with non-success status: {:?}", cio.status);
    return None;
}
```

**Context:** `process_completed_io` — handles I/O completions for async
reads. In release builds, a failed I/O returns `None` with zero logging.
The caller treats `None` as "pending operation not yet complete" or
"no result available", silently swallowing disk read errors.

**Fix:** Log in all builds (at `warn!` level minimum). Consider returning
an explicit error variant instead of `None`.

---

### BUG-5: Missing I/O status defaults to Success (masks errors)

**File:** `store/pending_io.rs:382`  
```rust
.unwrap_or(IoStatus::Success)
```

**Context:** `try_complete()` for pending I/O operations. If the I/O status
mutex was never populated (e.g., callback never fired, or was lost), the
operation reports success. This masks a class of bugs where the completion
callback fails to deliver status.

**Fix:** Default to `IoStatus::Error(-1)` or a dedicated
`IoStatus::Unknown` variant to make missing completions visible.

---

### BUG-6: CAS failure only caught by debug_assert (corrupt state in release)

**File:** `state/system_state.rs:187`  
```rust
debug_assert!(result.is_ok(), "intermediate → next CAS must succeed");
```

**Context:** `AtomicSystemState::try_advance_with_hooks`. After winning the
first CAS (expected → intermediate), the second CAS (intermediate → next)
"must" succeed because we hold exclusive ownership via the intermediate
state. If it ever fails in release (e.g., memory corruption, ABA), the
system state is stuck in the intermediate state permanently — all future
state transitions deadlock.

**Fix:** Replace `debug_assert!` with `assert!` or
`result.expect("intermediate → next CAS must succeed")`. The performance
cost is negligible (one extra branch in a rare path).

---

## 🟡 Risks — Should Add Justification Comment

### RISK-1: Page state transition discarded after flush_page_sync success

**File:** `hybrid_log/flush.rs:363`  
```rust
let _ = frame.state().try_transition(PageState::Flushing, PageState::Flushed);
```

Another thread may have already transitioned the page. Safe because
Flushing→Flushed is idempotent, but should have a comment explaining why.

---

### RISK-2–3: Best-effort page seal after allocation (2 sites)

**Files:**  
- `hybrid_log/log_allocator.rs:169`  
- `hybrid_log/log_allocator.rs:462`  
```rust
let _ = frame.state().try_transition(PageState::Open, PageState::Sealed);
```

Another thread may seal the page first. Race-tolerant by design.  
**Recommend:** Add `// Best-effort: another thread may have already sealed this page.`

---

### RISK-4: CAS cleanup of tentative hash entry (duplicate detected)

**File:** `hash/table.rs:412`  
```rust
let _ = slot.compare_exchange(tentative_entry, HashBucketEntry::EMPTY, ...);
```

Cleans up a tentative entry when a committed duplicate is found. If CAS
fails, another thread already cleaned it up. Race-tolerant.  
**Recommend:** Has good surrounding comments already — add inline note on the `let _`.

---

### RISK-5–7: CAS cleanup of tentative hash entries on abort (3 sites)

**Files:**  
- `store/operations.rs:408`  
- `store/operations.rs:723`  
- `store/operations.rs:743`  
```rust
let _ = ctx.hash_index.update(result.slot, result.entry, HashBucketEntry::EMPTY);
```

Abort paths that CAS a tentative entry back to EMPTY. If CAS fails,
another concurrent operation already claimed the slot. Race-tolerant.  
**Recommend:** Add `// Best-effort cleanup: concurrent operation may have already claimed this slot.`

---

### RISK-8: Thread join result discarded on shutdown

**File:** `sync_file_device.rs:656`  
```rust
let _ = h.join();
```

If a worker thread panicked, the panic payload is silently lost. During
normal shutdown this is acceptable, but during debugging the lost panic
info is frustrating.  
**Recommend:** Log the panic payload: `if let Err(e) = h.join() { eprintln!(...); }`

---

### RISK-9–10: Checkpoint force_abort discards state machine errors (2 sites)

**Files:**  
- `checkpoint/orchestrator.rs:256`  
- `checkpoint/orchestrator.rs:259`  
```rust
let _ = self.state_machine.try_advance(window[0], window[1]);
let _ = self.state_machine.reset();
```

This is `force_abort()` — a best-effort cleanup path with an explicit
comment: "Errors are silently ignored — this is a cleanup path."  
**Recommend:** Acceptable as-is given the comment. Consider debug-logging
on failure for diagnostics.

---

### RISK-11–12: Best-effort page seals in checkpoint and flush_all (2 sites)

**Files:**  
- `store/kv.rs:753` (checkpoint)  
- `store/kv.rs:1569` (flush_all_pages)  
```rust
let _ = frame.state().try_transition(PageState::Open, PageState::Sealed);
```

Another thread may have sealed the page already. Race-tolerant.  
**Recommend:** Add `// Best-effort seal: may already be sealed by concurrent operation.`

---

### RISK-13: begin_grow error discarded in check_grow

**File:** `store/kv.rs:1976`  
```rust
let _ = self.grow_manager.begin_grow(&self.hash_index);
```

`check_grow` is a "try to grow if needed" wrapper. If begin_grow fails
(e.g., already in progress), that's expected.  
**Recommend:** Add `// Intentionally discarded: grow may already be in progress or disabled.`

---

### RISK-14: Unused variable suppression masks debug_assert dependency

**File:** `grow/splitter.rs:251`  
```rust
let _ = new_size_bits; // Used only for debug assertions below.
```

Has an explanatory comment. Correct pattern but worth noting: the
`debug_assert_eq!` below is the only production validation of this
invariant. If violated, release builds silently do the wrong thing.

---

### RISK-15: Unknown checkpoint phase defaults to Rest

**File:** `checkpoint/state_machine.rs:176`  
```rust
CheckpointPhase::from_phase(sys.phase()).unwrap_or(CheckpointPhase::Rest)
```

If the system phase doesn't map to a known checkpoint phase, it defaults
to `Rest`. This could mask an invalid state.  
**Recommend:** Add `// Phases outside the checkpoint set are treated as Rest (no checkpoint active).`

---

### RISK-16: Unknown page state bits default to Free

**File:** `hybrid_log/page.rs:231`  
```rust
PageState::from_u8((raw & Self::STATE_MASK) as u8).unwrap_or(PageState::Free)
```

In a `Debug` impl only — used for display. Acceptable for formatting but
could mask corruption if used as page state in logic.  
**Recommend:** Confirm this is only used in `Debug` formatting (it is — line 228).

---

## 🟢 OK — Verified Correct (selected highlights)

| Pattern | Count | Reasoning |
|---------|-------|-----------|
| `let _ =` in test code | ~55 | Tests: return values irrelevant to test assertions |
| `let _ = io_ctx.reclaim()` in error paths | 4 | I/O never submitted; reclaim() returns owned data for cleanup |
| `let _ = Box::from_raw(...)` in Drop/race | 3 | Deallocating owned memory; return value is the Box itself |
| `let _ = (); // no-op` | 2 | Explicit no-ops |
| `let _ = (key, value_ptr, ...)` in unreachable defaults | 2 | Suppress unused before `unreachable!()` |
| `let _ = $label` in sim_hooks | 1 | No-op macro when simulation feature disabled |
| `let _ = dir` on non-unix cfg | 1 | Platform-conditional parameter suppression |
| `let _ = fs::remove_file(...)` | 1 | Best-effort removal with explanatory comment |
| `let _ = Ordering::Relaxed` | 1 | Suppress unused import |
| `let _ = store.compact()` in doc comments | 2 | Doc example code, not production |
| `#[allow(unused_must_use)]` | 0 | None found — good! |
| CAS results properly checked via match/is_err | ~25 | All non-test CAS operations properly handle results |
| `debug_assert` for invariant checking | ~40 | Most are genuine invariant checks (alignment, bounds, null) |
| `#[cfg(debug_assertions)]` for generation counters | ~14 | Extra diagnostics in debug; sentinel assertion runs in all builds |

---

## Methodology

1. `grep -rn 'let _ =' rust/crates/faster-core/src/ --include='*.rs'` — 130+ hits
2. `grep -rn 'unused_must_use\|unused_result'` — 0 hits
3. `grep -rn '\.ok()\|\.unwrap_or('` — 7 hits
4. `grep -rn 'debug_assert'` — 60+ hits
5. `grep -rn 'debug_assertions'` — 18 hits
6. `grep -rn 'compare_exchange\|compare_and_swap'` — 70+ hits

Each hit was manually classified by examining surrounding context, function
boundaries, and whether the code is inside `#[cfg(test)]` modules.

---

## Recommendations

1. **Fix all 🔴 Bugs before next release.** BUG-1, BUG-2, BUG-3, and BUG-4
   are in hot paths for data persistence — they can cause silent data loss.
2. **Add inline comments** for all 🟡 Risk sites explaining why discarding
   is intentional.
3. **Consider a `#[must_use]` audit** — verify that `flush_page_sync`,
   `flush_sealed_pages`, and `try_advance` are marked `#[must_use]` so
   the compiler catches future regressions.
4. **Promote BUG-6 (`debug_assert` → `assert`)** — the cost of one branch
   in a rare state-machine transition is negligible; the cost of a stuck
   intermediate state is catastrophic.
