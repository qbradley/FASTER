# Decision: Adaptive Submission Batching (U4)

**Agent:** Gimli (Database/Storage Expert)  
**Date:** 2026-03-08  
**Status:** Implemented (Wave 5, U4)  
**Impact:** I/O thread performance — reduces syscall overhead under load

## Decision

Added configurable submission coalescing to the `UringDevice` I/O thread. Under high load, SQEs are accumulated before calling `io_uring_submit()` (up to 32 entries or 1µs window). Under low load, the batch window expires quickly to keep latency low. An adaptive tuner auto-adjusts the window based on observed throughput.

## API Surface

- `BatchPolicy` — configures max_batch (default 32), batch_window_us (default 1µs), adaptive flag
- `BatchPolicy::default()` — adaptive batching with 32/1µs
- `BatchPolicy::immediate()` — no coalescing (max_batch=1, window=0)
- `UringDeviceConfig::batch_policy` — new required field

## Ring Changes

- `Ring::unsubmitted()` — getter for SQEs pushed but not yet submitted to kernel
- `Ring::try_reap_completions()` — non-blocking CQ drain for use during batch accumulation
- `Ring::submit()` now resets the unsubmitted counter

## Design Rationale

1. **Spin-wait over `recv_timeout`**: The 1µs default window is shorter than OS scheduler quantum, so `recv_timeout` would overshoot badly. A `try_recv` + `spin_loop` approach gives microsecond-precision batch windows.
2. **Adaptive tuning via EMA**: Exponential moving average of batch utilisation (ratio of actual batch size to max_batch) is tracked with α=0.1. Every 64 submits, the window doubles if utilisation > 75% or halves if < 25%.
3. **Sync ops unaffected**: `read_sync`/`write_sync` bypass the ring entirely (pread/pwrite on caller thread), so batching has no effect on the recovery/metadata path.

## Implications

- **Legolas (Perf):** Benchmark test `bench_batched_vs_immediate` compares both modes. Under high concurrent load, batching reduces syscalls ~32x.
- **All agents:** `UringDeviceConfig` now requires `batch_policy` field. Use `BatchPolicy::default()` for standard behavior.
