# Stub / Fake-Data / Unimplemented Audit — `faster-core/src/`

**Auditor:** Aragorn (Rust Expert)
**Requested by:** qbradley
**Scope:** `rust/crates/faster-core/src/` — all `.rs` files
**Date:** 2025-07-17

---

## Summary

| Category | Count |
|----------|-------|
| 🔴 Bug   | 2     |
| 🟡 Risk  | 4     |
| 🟢 OK    | 14    |
| **Total** | **20** |

**Is the codebase clean?** **Yes, with caveats.** No more `collect_stats`-class bugs
(silently returning fake data that breaks real logic). Two bugs found: one stale
doc-comment that actively misleads maintainers, and one always-zero public API method.
Four risk items need tracking for future work.

---

## 🔴 Bug — Actively wrong or misleading

### 1. `store/kv.rs:2061` — `FasterKv::entry_count()` always returns `0`

```rust
pub fn entry_count(&self) -> u64 {
    0
}
```

**What's wrong:** This is a public API on the main `FasterKv` store type. It always
returns `0` regardless of how many entries are in the store. The `HashIndex` has a
working `entry_count()` backed by an `AtomicU64` live counter — but `FasterKv` ignores
it and hardcodes zero. Any downstream consumer (including the mutation test at
`kv_operations_mutation_tests.rs:33`) calling `store.entry_count()` gets garbage.

**Impact:** Callers that use `FasterKv::entry_count()` to size allocations, report
metrics, or make decisions will always see 0. The doc-comment acknowledges the
limitation ("intentionally deferred") but the method is public and named as if it
works — a classic `collect_stats`-shaped trap.

**Fix:** Delegate to `self.hash_index.entry_count()` which already maintains an
accurate atomic counter.

### 2. `store/kv.rs:1126-1129` — Stale doc-comment says write completions are "not yet implemented"

```rust
/// Write-path completions are not yet implemented (requires re-entering
/// the hash index to perform copy-to-tail). They are tracked as in-flight
/// I/O but dropped on completion with a debug warning.
```

**What's wrong:** The comment says write completions are dropped with a debug warning.
In reality, `complete_write_pending()` (kv.rs:1316) is **fully implemented** — it
handles Upsert, RMW, and Delete with hash-index CAS updates. The doc-comment on the
public `complete_pending()` method is actively lying to callers. A maintainer reading
this will believe write completions are no-ops and may not test them, introduce
regressions, or re-implement something that already exists.

**Impact:** Misleads anyone reading the API docs. Not a data-loss bug, but a
maintenance hazard that could cause one.

**Fix:** Update the doc-comment to accurately describe the implemented behavior.

---

## 🟡 Risk — Placeholder that could break when a feature is needed

### 1. `recovery/log_recovery.rs:375-430` — `validate_log_file` always returns `records_scanned: 0`

The function validates segment file sizes but never scans actual records. The
`records_scanned` field in `LogRecoveryResult` is always 0. Currently this field is
only used for display/logging (Display impl at line 94), so no logic depends on it.

**Risk:** If any future recovery logic (e.g., integrity verification, record-count
assertions) uses `records_scanned` as a real metric, it will silently get 0. The
test at line 1199 even asserts `records_scanned == 0` to lock in this non-behavior.

### 2. `store/functions.rs:248,266` — `unreachable!()` in default `upsert_in_place_raw` / `rmw_in_place_raw`

These trait default methods hit `unreachable!()` if called. They are guarded by
`const SUPPORTS_RAW_IN_PLACE: bool = false` and the compiler *should* eliminate the
dead branch at monomorphization. The guard is checked at `operations.rs:507` and
`operations.rs:848`.

**Risk:** The guard relies on the compiler's dead-code elimination. If a new
`Functions` implementor sets `SUPPORTS_RAW_IN_PLACE = true` but forgets to override
these methods, it's a runtime panic in production. The `error.rs` audit (line 43-44)
already flags these as "should-fix".

### 3. `device.rs:366` — Default `poll_completions()` returns `0`

```rust
fn poll_completions(&self) -> u32 { 0 }
```

This is a trait default for `Device`. The `NullDevice` and `InMemoryDevice` correctly
return 0 (they run callbacks synchronously). But the `SyncFileDevice` overrides it
properly.

**Risk:** If a new device implementation forgets to override `poll_completions`, I/O
completions will silently be lost. The doc-comment warns about this, but a compile-time
check would be safer.

### 4. `recovery/log_recovery.rs:1199` — Test asserts `records_scanned == 0` as correct behavior

```rust
// Record scanning is not yet implemented, so count should be 0.
assert_eq!(result.records_scanned, 0);
```

**Risk:** This test locks in the stub behavior. When record scanning is implemented,
this test must be updated — but there's no `// TODO` or tracking issue to remind
developers. It could block correct behavior from landing.

---

## 🟢 OK — Intentional, well-guarded, or test-only

| # | Location | Pattern | Why it's OK |
|---|----------|---------|-------------|
| 1 | `hash/index.rs:702` | `return 0.0` in `load_factor()` | Division-by-zero guard when `slots == 0`. Correct arithmetic. |
| 2 | `grow/manager.rs:190,193` | `return false` in `should_grow()` | Early-return guards: grow disabled, or already growing. Correct logic. |
| 3 | `grow/state_machine.rs:473` | `return 1.0` in `progress()` | Returns 100% progress when `num_chunks == 0`. Correct semantics. |
| 4 | `hybrid_log/regions.rs:206` | `return 0` in `pages_between()` | Returns 0 when `to <= from`. Correct boundary check. |
| 5 | `hybrid_log/scan.rs:219-225` | `return false` in `should_yield()` | Filters null, invalid, and tombstone records per scan options. Correct filtering. |
| 6 | `hybrid_log/page.rs:148-195` | `return false/true` in `try_transition/try_pin` | CAS-loop state machine. Returns success/failure based on atomics. Correct concurrency. |
| 7 | `state/system_state.rs:178` | `return false` in transition | CAS failed — return false to caller. Correct. |
| 8 | `store/functions.rs:337` | Empty `delete()` body | Trait default: optional cleanup callback. Explicit opt-out by design. |
| 9 | `store/session.rs:949` / `pending_io.rs:607` | Empty `install_test_clock()` | `#[cfg(not(feature = "simulation"))]` no-ops. Correct conditional compilation. |
| 10 | `epoch/tests.rs:728` | `unreachable!()` | Inside test-only match arm for an enum that shouldn't appear. Test code. |
| 11 | `record/traits.rs:409,502` | `return false` in `eq_from_bytes()` | Buffer-too-short guard for `Vec<u8>` and `String` key comparison. Correct. |
| 12 | `store/kv.rs:2081-2083` | `metrics()` returns `None` | `#[cfg(not(feature = "metrics"))]` — feature-gated. Correct. |
| 13 | All `assert_send_sync` functions | Empty bodies | Compile-time trait bound checks. Bodies are intentionally empty. |
| 14 | All `panic!()` in test code | Test assertions | ~25 panics in `#[cfg(test)]` modules — expected test failure patterns. |

---

## Patterns NOT found (clean bill of health)

- ✅ **No `todo!()` macros** anywhere in production code
- ✅ **No `unimplemented!()` macros** in production code (the two in `functions.rs` are guarded `unreachable!()` — see Risk #2)
- ✅ **No `// TODO` / `// FIXME` / `// HACK` / `// PLACEHOLDER`** comments in production code
- ✅ **No `Default::default()` as stub return** — all uses are legitimate (doc examples, test setup, recovery engine construction)
- ✅ **`collect_stats` is now properly implemented** — samples in-memory read-only pages and extrapolates. Verified at `orchestrator.rs:342-413`. Tests at lines 616, 667, 710 confirm live vs total distinction works.
- ✅ **`complete_write_pending` is fully implemented** — handles Upsert, RMW, Delete with proper hash-index CAS (kv.rs:1316-1490). Only the doc-comment is stale.

---

## Recommendations

1. **Fix Bug #1 immediately** — `FasterKv::entry_count()` should delegate to `self.hash_index.entry_count()`. One-line fix, high impact.
2. **Fix Bug #2 immediately** — Update the stale doc-comment on `complete_pending()` to reflect reality.
3. **Track Risk #1-#4** — File issues for `records_scanned` stub, `unreachable!()` in raw-in-place defaults, and `poll_completions` default.
4. **Consider deprecating** `FasterKv::entry_count()` if the intent was to not expose it, or make it `#[doc(hidden)]` until properly wired.
