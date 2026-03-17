# Decision: P2-B — Replace Raw PageTable Pointer in Flush Callbacks with Arc

**Author:** Aragorn (Rust Expert)
**Date:** 2026-07-25
**Status:** Implemented
**Commit:** `908af7bf`
**Branch:** `rust`

## Problem

`FlushCallbackContext` stored a `*const PageTable` raw pointer (flush.rs:137). The pointer's validity depended entirely on `FasterKv::Drop` draining all in-flight I/O before the allocator (and its owned `PageTable`) was dropped. This was correct in practice but:

1. **No compile-time guarantee** — a future refactor changing `FasterKv` field order or adding an early-return in `Drop` would silently create a use-after-free.
2. **Required `unsafe impl Send`** — the raw pointer forced a manual Send impl with a prose-only safety argument.
3. **Hard to diagnose** — a dangling pointer dereference in an I/O completion callback would SIGSEGV on a worker thread with no obvious stack trace back to the root cause.

## Solution

Wrap `PageTable` in `Arc<PageTable>` inside `HybridLogAllocator`. Flush callbacks clone the Arc into their context, holding a strong reference that guarantees the `PageTable` is alive when the callback fires — regardless of drop order.

### Changes (4 files, +47/-33 lines)

| File | Change |
|------|--------|
| `log_allocator.rs` | `page_table: PageTable` → `page_table: Arc<PageTable>`. Added `page_table_arc()` accessor. |
| `flush.rs` | `FlushCallbackContext.page_table`: `*const PageTable` → `Arc<PageTable>`. Removed `unsafe impl Send`. Removed unsafe deref in callback. `flush_page` takes `&Arc<PageTable>`. |
| `kv.rs` | Updated `Drop` comment to reflect Arc-based design. |
| `hybrid_log_mutation_tests.rs` | Wrapped test `PageTable` in `Arc` for `flush_page` calls. |

### Alternatives Considered

- **`Weak<PageTable>`**: Unnecessary — callbacks should always succeed if there's in-flight I/O. A failed `upgrade()` would indicate a logic bug, not a valid state.
- **Lifetime-bound closure**: Impossible — callbacks are `'static` (sent across threads to I/O workers).
- **Keep raw pointer + documentation**: Status quo. Rejected because a 1-line Arc clone per flush is negligible overhead vs. eliminating a class of memory safety bugs.

### Performance Impact

Zero measurable impact. The Arc clone (atomic increment) happens once per page flush — an operation that also involves a device `write_async` call with microsecond-to-millisecond latency. The refcount cost is noise.

### Unsafe Reduction

- Removed 1 `unsafe impl Send`
- Removed 1 `unsafe { &*ctx.page_table }` deref in callback
- Net: −2 unsafe sites

### Verification

- All 1745 tests pass
- Clippy clean
- `flush_page_sync` path (no callbacks) unchanged — no Arc overhead on sync flush
