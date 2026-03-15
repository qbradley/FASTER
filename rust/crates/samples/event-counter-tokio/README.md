# event-counter-tokio

Async/Tokio real-time event aggregation (ad click counter) using FASTER — demonstrates
the async/sync bridge pattern for CPU-bound FASTER operations with Tokio's runtime.

## What It Demonstrates

This sample shows how to integrate FASTER with Tokio for realistic real-time workloads.
Multiple async worker tasks generate simulated ad click events and aggregate them using
FASTER's RMW (Read-Modify-Write) operation. Worker threads run on Tokio's blocking pool
(because `FasterSession` is `!Send`), while coordination tasks (stats, shutdown, checkpoints)
run as native async Tokio tasks.

This is the **async companion** to the sync event-counter sample — use both to compare
threading models.

## Key Concepts

- **Async/Sync Bridge** — `spawn_blocking` for CPU-bound FASTER sessions, native async for control plane
- **TokioFileDevice** — disk I/O dispatched through Tokio instead of hand-rolled threads
- **AsyncFasterKv** — background maintenance task for periodic compaction and eviction
- **RMW Operations** — atomic read-modify-write for concurrent aggregation
- **Checkpoint & Recovery** — multi-phase workflow: setup → ingest → verify → checkpoint → recover

## Usage

```bash
# Run with default settings (16 workers, 60 seconds)
cargo run -p event-counter-tokio --release

# Customize worker count and duration
cargo run -p event-counter-tokio --release -- --workers 8 --duration 30

# High-throughput test: 32 workers, variable value sizes
cargo run -p event-counter-tokio --release -- \
    --workers 32 --variable-sizes

# Show all options
cargo run -p event-counter-tokio --release -- --help
```

## CLI Options

| Flag | Default | Purpose |
|------|---------|---------|
| `--workers` | `16` | Number of async worker tasks |
| `--duration` | `60` | Runtime in seconds |
| `--log-size` | `20` | Log capacity (2^N pages, ~32 MiB each) |
| `--checkpoint-interval` | `10` | Checkpoint every N seconds |
| `--stats-interval` | `1` | Print stats every N seconds |
| `--variable-sizes` | (flag) | Use random value sizes (64–256 bytes) |

## Workflow

**Phase 1: Setup** — Create store with TokioFileDevice and background maintenance task

**Phase 2: Ingest** — 16 worker tasks generate events via `spawn_blocking` + RMW

**Phase 3: Verify** — Read all campaigns, check counts match expected aggregates

**Phase 4: Checkpoint** — Persist state to disk via `spawn_blocking` wrapper

**Phase 5: Recover** — Simulate crash, reopen store, verify recovery correctness

## Expected Output

```
═══ FASTER Async Event Counter ═══

▶ Phase 1: Setup
  Store created with TokioFileDevice
  Background maintenance started

▶ Phase 2: Ingest (60 seconds)
  Workers: 16 threads
  Throughput: 456,123 events/sec

▶ Phase 3: Verify
  Campaigns verified: 1,000
  Total events: 27,367,380
  Oracle check: ✅ PASS

▶ Phase 4: Checkpoint
  Checkpoint completed in 1.23s

▶ Phase 5: Recovery
  Recovered 1,000 campaigns
  Recovered 27,367,380 events
  Recovery check: ✅ PASS

═══ Summary: ALL PHASES PASSED ═══
```

## When to Use Async

The Tokio version is beneficial when:

1. **Disk-intensive workloads** — TokioFileDevice pools threads efficiently
2. **Control-plane tasks** — Stats, timers, signals are natural async operations
3. **Application integration** — Your main application is already async (web server, etc.)

For CPU-bound FASTER operations, `spawn_blocking` performance equals raw threads.

## Platform Support

- Runs on all platforms (Linux, macOS, Windows)
- Tokio runtime required
- Works with any storage device backend
