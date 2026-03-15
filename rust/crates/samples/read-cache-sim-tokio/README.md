# read-cache-sim-tokio

Async/Tokio read-cache simulation using FASTER — async companion to the sync
`read-cache-sim` for side-by-side threading model comparison.

## What It Demonstrates

Identical workload to `read-cache-sim`, but built on **Tokio async runtime**.
Workers run as `spawn_blocking` tasks (because `FasterSession` is `!Send`),
while stats reporting, shutdown signalling, and maintenance run as native
async Tokio tasks.

Use this to benchmark Tokio integration patterns and compare async/sync performance
on the same workload.

## Key Concepts

- **Async/Sync Bridge** — `spawn_blocking` for CPU-bound FASTER sessions, native async for coordination
- **TokioFileDevice** — disk I/O dispatched through Tokio's thread pool
- **AsyncFasterKv** — background maintenance as an async task
- **Mixed Read/Write** — configurable ratio of reads vs. writes
- **Performance Comparison** — benchmark against sync version for your use case

## Usage

```bash
# Run with defaults (4 workers, 4 KB blocks, 100K unique keys)
cargo run -p read-cache-sim-tokio --release

# High concurrency read-heavy workload
cargo run -p read-cache-sim-tokio --release -- \
    --workers 16 --read-pct 95 --duration 60

# Variable block sizes for mixed workload
cargo run -p read-cache-sim-tokio --release -- \
    --block-size 65536 --variable-blocks

# Large working set with compaction
cargo run -p read-cache-sim-tokio --release -- \
    --key-space 10000000 --compact --compact-threshold 2.5

# Customize memory and disk
cargo run -p read-cache-sim-tokio --release -- \
    --memory-size $((512 * 1024 * 1024)) \
    --segment-size $((2 * 1024 * 1024 * 1024)) \
    --key-space 1000000

# Show all options
cargo run -p read-cache-sim-tokio --release -- --help
```

## CLI Options

| Flag | Default | Purpose |
|------|---------|---------|
| `--workers` | CPU count | Number of async worker tasks |
| `--block-size` | `4096` | Fixed block size in bytes |
| `--variable-blocks` | (flag) | Use random sizes (512 bytes–block-size) |
| `--key-space` | `100000` | Number of unique cache keys |
| `--duration` | (none) | Run for N seconds (default: forever) |
| `--memory-size` | `128 MiB` | In-memory buffer size |
| `--segment-size` | `1 GiB` | On-disk segment size |
| `--compact` | (flag) | Enable compaction |
| `--stats-interval` | `1000` | Print stats every N milliseconds |
| `--read-pct` | `50` | Percentage reads vs. writes (0–100) |

## Architecture

```
                    Tokio Async Runtime
         ┌──────────────────────────────────────┐
         │                                      │
         │ ┌────────────┐  ┌──────────────┐    │
         │ │Stats Task  │  │Shutdown Task │    │ ← native async tasks
         │ │(interval)  │  │(ctrl-c)      │    │   (no spawn_blocking)
         │ └────────────┘  └──────────────┘    │
         │                                      │
         │  ┌──────────────────────────────┐   │
         │  │  Tokio Blocking Pool         │   │
         │  │  ┌────────┐  ┌────────┐      │   │
         │  │  │Worker 0│  │Worker N│      │   │ ← spawn_blocking
         │  │  │Session │  │Session │      │   │   (one OS thread per)
         │  │  └───┬────┘  └───┬────┘      │   │
         │  └──────┼───────────┼───────────┘   │
         └─────────┼───────────┼───────────────┘
                   │           │
            ┌──────▼───────────▼──────┐
            │   FasterKv (Arc)         │
            │   - Hash Index           │
            │   - Hybrid Log           │
            │   - TokioFileDevice      │
            └──────────────────────────┘
```

## Example Output

```
═══ FASTER Async Read Cache (Tokio) ═══
Config: workers=4, block_size=4096, key_space=100000, device=tokio

▶ Phase 1: Warmup
  Filled cache with 100,000 keys in 0.38s

▶ Phase 2: Steady State (60 seconds)
  Throughput:     1,198,456 ops/sec (50% read, 50% write)
  Read hits:      49.8%
  Read misses:    50.2%
  Latency p50:    3.4µs  p99: 22.1µs  p999: 156.2µs

▶ Statistics
  ops/sec: 1.20M  |  reads: 598K  |  writes: 600K  |  hit%: 49.8%

═══ Summary: Complete in 60.23s ═══
```

## Sync vs. Async Comparison

Run both `read-cache-sim` and `read-cache-sim-tokio` with identical parameters:

```bash
# Terminal 1: Sync version
time cargo run -p read-cache-sim --release -- \
    --threads 4 --duration 60

# Terminal 2: Async version
time cargo run -p read-cache-sim-tokio --release -- \
    --workers 4 --duration 60
```

**Expected Results:**
- Throughput should be **nearly identical** for CPU-bound FASTER operations
- Latency distributions may vary slightly due to Tokio task scheduling
- Async version shows advantage if your application also runs async I/O (shared pools)

## When Tokio Helps

For **FASTER alone**, `spawn_blocking` gives parity with raw threads. Tokio's benefits:

1. **Shared Resource Pools** — if your app is already async (web server, gRPC), FASTER fits in
2. **Simpler Control Plane** — stats tasks, timers, shutdown are natural async code
3. **TokioFileDevice** — disk I/O shares Tokio's thread pool, not a separate thread pool

For throughput-only benchmarks, use `read-cache-sim` (sync). For integration testing,
use both and verify performance parity.

## Notes

- Compile with `--release` for realistic performance
- Run without `--duration` to stress-test indefinitely (Ctrl+C to stop)
- Same command-line semantics as `read-cache-sim` for easy comparison
- Perfect for evaluating Tokio integration in your application
