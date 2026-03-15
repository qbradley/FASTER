# read-cache-sim

Read-cache simulation using FASTER with fixed working set — demonstrates cache behavior
under mixed read/write workloads with optional compaction.

## What It Demonstrates

Continuously writes blocks to a fixed set of keys, simulating a read cache with a bounded
working set. Supports both fixed and variable block sizes, and optionally runs compaction
when space amplification exceeds a threshold. Perfect for modeling L1/L2 caches, read-ahead
buffers, or CDN staging caches.

The sync version — compare with `read-cache-sim-tokio` for async patterns.

## Key Concepts

- **Fixed Working Set** — writes recycle a limited number of keys
- **Mixed Read/Write** — configurable ratio of reads vs. writes
- **Variable Block Sizes** — optionally randomize sizes (512 bytes to `--block-size`)
- **Compaction** — optional space reclamation when amplification exceeds threshold
- **Performance Metrics** — throughput, latency percentiles, hit/miss rates

## Usage

```bash
# Run with defaults (4 threads, 4 KB blocks, 100K unique keys)
cargo run -p read-cache-sim --release

# High concurrency read-heavy workload
cargo run -p read-cache-sim --release -- \
    --threads 16 --read-pct 95 --duration 60

# Variable block sizes (1 KB min, 64 KB max)
cargo run -p read-cache-sim --release -- \
    --block-size 65536 --variable-blocks

# Large working set with compaction
cargo run -p read-cache-sim --release -- \
    --key-space 10000000 --compact --compact-threshold 2.5

# Customize memory and disk budgets
cargo run -p read-cache-sim --release -- \
    --memory-size $((512 * 1024 * 1024)) \
    --segment-size $((2 * 1024 * 1024 * 1024)) \
    --key-space 1000000

# Show all options
cargo run -p read-cache-sim --release -- --help
```

## CLI Options

| Flag | Default | Purpose |
|------|---------|---------|
| `--threads` | CPU count | Number of worker threads |
| `--block-size` | `4096` | Fixed block size in bytes |
| `--variable-blocks` | (flag) | Use random sizes (512 bytes–block-size) |
| `--key-space` | `100000` | Number of unique cache keys |
| `--duration` | (none) | Run for N seconds (default: forever) |
| `--memory-size` | `128 MiB` | In-memory buffer size |
| `--segment-size` | `1 GiB` | On-disk segment size |
| `--compact` | (flag) | Enable compaction |
| `--compact-threshold` | `2.0` | Trigger compaction at N× space amplification |
| `--stats-interval` | `1000` | Print stats every N milliseconds |
| `--read-pct` | `50` | Percentage of operations that are reads (0–100) |

## Example Output

```
═══ FASTER Read Cache Simulator ═══
Config: threads=4, block_size=4096, key_space=100000

▶ Phase 1: Warmup
  Filled cache with 100,000 keys in 0.34s

▶ Phase 2: Steady State
  Throughput:     1,234,567 ops/sec (50% read, 50% write)
  Read hits:      50.2%
  Read misses:    49.8%
  Latency p50:    3.2µs  p99: 18.4µs  p999: 142.8µs

▶ Statistics (every 1s)
  ops/sec: 1.23M  |  reads: 613K  |  writes: 617K  |  hit%: 50.1%

▶ Compaction
  Space amplification: 2.1× (triggered compaction)
  Compaction completed in 0.45s
  Post-compaction size: 450 MB
```

## Workload Characteristics

**Read-Heavy (--read-pct 95):**
- Most operations are cache hits
- Few writes touch the working set
- Tests cache coherency under read load

**Write-Heavy (--read-pct 5):**
- Cache constantly evicts old blocks
- Replacement rate high (turnover fast)
- Tests write throughput and space reclamation

**Mixed (--read-pct 50):**
- Balanced read/write stress
- Models realistic cache usage patterns
- Both hit rate and write throughput matter

## Compaction

When enabled with `--compact`:

1. Track **space amplification** (on-disk size / live data size)
2. Trigger compaction when amplification exceeds `--compact-threshold`
3. Rewrite live data, skip dead entries
4. Reduce fragmentation and reclaim space

Typical thresholds:
- `1.0` — never compact (100% utilization, unrealistic)
- `2.0` — compact when 50% space is wasted (common)
- `3.0` — allow more fragmentation, compact less often (high-throughput mode)

## Fixed vs. Variable Block Sizes

| Mode | Block Size | Use Case |
|------|-----------|----------|
| Fixed | `--block-size 4096` | Uniform cache blocks (buffer pools) |
| Variable | `--variable-blocks` | Mixed-size blobs (thumbnail cache) |

## Sync vs. Async

This is the **sync** version with standard OS threads. Compare with:
- `read-cache-sim-tokio` — same workload, Tokio async runtime + TokioFileDevice

For CPU-bound FASTER operations, both achieve similar throughput. Tokio's advantage
comes from shared resource pooling if your application is already async.

## Notes

- Compile with `--release` for realistic performance
- Run without `--duration` to stress-test indefinitely (Ctrl+C to stop)
- Compaction improves space efficiency but adds CPU overhead
- Perfect for capacity planning and performance profiling
