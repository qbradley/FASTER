# Decision: TypedIoContext Double-Free Guard (P2-A)

**Author:** Sam (Systems & Storage Expert)
**Date:** 2026-07-25
**Status:** Implemented
**Commit:** `a92e44ff`
**Branch:** `rust`

## Problem

`TypedIoContext::from_raw` reconstructs a `Box<T>` from a raw pointer passed through the I/O callback FFI boundary. The debug-only generation counter detected double-consumption in debug builds, but **release builds had zero protection** — a device bug invoking a callback twice would call `Box::from_raw` on freed memory, causing silent heap corruption.

Identified as **P2-A** in Aragorn's unsafe code survey (`aragorn-unsafe-survey.md`).

## Solution

Replaced the debug-only `ContextEnvelope<T>` with an always-present `IoContextEnvelope<T>`:

```rust
struct IoContextEnvelope<T> {
    sentinel: AtomicU64,          // ALIVE → CONSUMED on consumption
    #[cfg(debug_assertions)]
    generation: u64,              // extra diagnostics in debug
    data: ManuallyDrop<T>,        // prevents double-drop on envelope teardown
}
```

Both `from_raw()` and `reclaim()` atomically swap the sentinel before touching the Box:
- Old value == `ALIVE` → proceed (first and only consumption)
- Old value != `ALIVE` → **panic** before `Box::from_raw` can corrupt the heap

## What This Catches

| Scenario | Detection |
|----------|-----------|
| Concurrent double-callback (two threads) | Atomic swap serialises — only one observes ALIVE |
| Sequential double-free (device bug) | Second call sees CONSUMED sentinel |
| `reclaim()` after callback already fired | Sentinel already swapped to CONSUMED |

## What This Cannot Catch

If the allocator has already reused the freed memory before the second call arrives, reading the sentinel is technically UB. This is inherent to any post-free detection scheme — but the guard catches the common case (memory not yet reused) and is strictly better than the previous zero-check release code.

## Overhead

- **Memory:** +8 bytes per I/O context (the `AtomicU64` sentinel)
- **CPU:** One `AtomicU64::swap` (~1ns) per I/O completion; one extra `Box::new` per `from_raw` to re-box extracted data
- **Context:** I/O completions involve disk latency (µs–ms); overhead is unmeasurable

## Alternatives Considered

- **Option A: `Option<NonNull<T>>` on the struct** — Doesn't work because `from_raw` is a static method that receives a raw `*mut u8` from the FFI boundary. There's no struct instance to set to `None`.
- **Option B: Global pointer set tracking live contexts** — Correct but adds synchronization overhead and memory pressure on every I/O operation. Overkill for a defensive guard.
- **Option C: Don't free envelope on first consumption (leak-based detection)** — Would enable perfect detection but leaks memory on every I/O operation. Unacceptable for production.

## Files Changed

- `rust/crates/faster-core/src/device.rs` — +185 / -52 lines

## Tests Added

| Test | What it verifies |
|------|-----------------|
| `typed_io_context_from_raw_round_trip` | Normal path: new → as_raw → from_raw works |
| `typed_io_context_reclaim_round_trip` | Normal path: new → reclaim works |
| `typed_io_context_double_from_raw_panics` | Consumed sentinel triggers panic in from_raw |
| `typed_io_context_reclaim_after_from_raw_panics` | Consumed sentinel triggers panic in reclaim |
| `typed_io_context_from_raw_with_drop_type` | Heap-allocated inner types (Vec) drop correctly |

## Test Results

- **1960 tests pass** (faster-core + faster-tokio)
- Clippy clean
- Pre-existing `faster-dst` compile error (unrelated `flush_page` Arc signature change) not caused by this work
