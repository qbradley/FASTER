# Decision: Batch Operation API Design (A12)

**Author:** Aragorn (Rust Expert)  
**Date:** 2025-07-25  
**Status:** Implemented  

## Context

FASTER's session API performs epoch enter/exit on every individual operation.
For bulk workloads, this per-operation overhead dominates. We needed a batch
API that amortizes epoch protection and leverages hash bucket prefetching.

## Decision

**Slice-based API** with `impl UnsafeContext` batch methods in a dedicated
`store::batch` module.

### API Surface

| Method | Signature |
|--------|-----------|
| `batch_read` | `(&self, session, keys, outputs) -> BatchResult` |
| `batch_upsert` | `(&self, session, keys, inputs) -> BatchResult` |
| `batch_rmw` | `(&self, session, keys, inputs, outputs) -> BatchResult` |
| `batch_delete` | `(&self, session, keys) -> BatchResult` |
| `batch_execute` | `(&self, session, ops, outputs) -> BatchResult` |

All methods also available directly on `UnsafeContext` for manual epoch control.

### Key Design Choices

1. **Slice-based over builder pattern**: More idiomatic Rust, zero allocation
   overhead, natural borrow semantics for parallel key/value slices.

2. **UnsafeContext impl in batch.rs**: Keeps batch logic self-contained.
   Required making `UnsafeContext.session` field `pub(super)`.

3. **No atomicity**: Each operation independently succeeds/fails. BatchResult
   tracks per-operation OperationStatus with aggregate helpers.

4. **BATCH_REFRESH_INTERVAL = 256**: Epoch refresh every 256 ops. ~50ns per
   refresh, amortized to < 0.2ns per op.

5. **Hash-prefetch-all then execute-all**: Simple two-phase approach. All hash
   buckets prefetched before any operations execute.

## Alternatives Considered

- **Builder pattern**: More flexible but heavier API, unnecessary allocation.
- **Batch methods directly in session.rs**: Would bloat an already large file.
- **Async/stream API**: Premature; batch API covers the primary use case.

## Consequences

- New public types: `BatchOp`, `BatchResult` (re-exported at crate root)
- `UnsafeContext.session` field changed from private to `pub(super)`
- All batch methods require `F::Context: Default`
