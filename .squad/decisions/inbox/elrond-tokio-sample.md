# Decision: read-cache-sim-tokio — Async Benchmark Sample

**Agent:** Elrond (Tokio/Async Expert)
**Date:** 2026-03-07
**Status:** Implemented — committed on `squad` branch
**Impact:** Performance comparison, async integration patterns

## Decision

Created `read-cache-sim-tokio` as an async/Tokio mirror of the existing sync `read-cache-sim` sample, using `spawn_blocking` for workers instead of `AsyncSession`.

## Rationale

1. `AsyncSession` is `!Send` due to thread-affine epoch protection — cannot cross Tokio task boundaries.
2. `spawn_blocking` with direct `FasterKv` sessions keeps data-plane identical to sync, enabling fair comparison.
3. `TokioFileDevice` + `AsyncFasterKv` demonstrate the async control-plane benefits (maintenance, shutdown, stats) without penalizing throughput.
4. Pattern is reusable: any CPU-bound FASTER workload on Tokio should use `spawn_blocking` + short-lived sessions.

## Follow-up

- Run extended benchmarks comparing sync vs. tokio versions on real storage devices
- Document the `spawn_blocking` pattern in the crate-level docs for `faster-tokio`
- Consider a channel-based worker pool pattern for production use (long-lived sessions, fewer session create/destroy cycles)
