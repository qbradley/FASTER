# page-cache

Disk-I/O-heavy lossy page cache using FASTER — demonstrates working-set-based
caching where old entries naturally evict as storage wraps around.

## What It Demonstrates

A cache workload where pages (identified by key = page number, value = byte buffer)
are written continuously. As the storage log fills, oldest entries fall out of the
working set naturally — this is cache behavior, not durable storage. Perfect for
modeling CDN edge caches, database buffer pools, or image thumbnail caches.

Supports both linear (sequential) and Zipfian (skewed) key distributions, with
optional io_uring device for high-concurrency I/O.

## Key Concepts

- **Lossy Eviction** — data doesn't persist after leaving the hybrid log; designed for cache semantics
- **Tunable Memory/Disk Ratio** — control working set size via `--in-memory-mb` and `--log-size-mb`
- **Configurable Page Size** — model different cache block sizes
- **Distribution Modes** — linear writes (each write is new page) vs. Zipf (some pages updated more)

## Usage

```bash
# Run with defaults (60 second cache stress test, 1 GB log)
cargo run -p page-cache --release

# Long-running cache simulation with statistics every 5 seconds
cargo run -p page-cache --release -- \
    --duration 300 --stats-interval 5

# Large dataset with small memory window to force eviction
cargo run -p page-cache --release -- \
    --log-size-mb 8192 --in-memory-mb 128 \
    --page-size 4096 --threads 8

# Zipfian distribution (realistic skewed access)
cargo run -p page-cache --release -- \
    --distribution zipf --zipf-exponent 1.2 --key-space 1000000

# io_uring device (Linux kernel 5.1+, requires --features io_uring)
cargo run -p page-cache --release --features io_uring -- --device uring

# Show all options
cargo run -p page-cache --release -- --help
```

## CLI Options

| Flag | Default | Purpose |
|------|---------|---------|
| `--duration` | `60` | Run for N seconds |
| `--storage-dir` | `./page-cache-data` | Directory for FASTER log files |
| `--log-size-mb` | `1024` | Total log size in MB |
| `--in-memory-mb` | `256` | In-memory portion (pages) in MB |
| `--page-size` | `4096` | Value size per page in bytes |
| `--distribution` | `linear` | `linear` (sequential) or `zipf` (skewed) |
| `--zipf-exponent` | `1.0` | Zipfian θ parameter (1.0–2.0 realistic) |
| `--key-space` | `10000000` | Total unique keys for Zipf |
| `--threads` | `4` | Worker threads |
| `--device` | `sync` | `sync` or `uring` (with feature flag) |
| `--stats-interval` | `1000` | Print stats every N milliseconds |

## Example Output

```
═══ FASTER Page Cache ═══
Config: log=1024 MB, memory=256 MB, page_size=4096

▶ Phase 1: Warmup
  Wrote 131,072 pages in 2.34s (56 MB/s)

▶ Phase 2: Steady State (60 seconds)
  Throughput:     1,234,567 pages/sec
  Memory usage:   256 MB (in-memory window)
  Eviction rate:  ~18,500 pages/sec (natural aging)

▶ Final Stats
  Total writes:   74,074,020 pages
  Total evicted:  73,942,948 pages (99.8%)
  Working set:    131,072 pages (256 MB)
  Cache hit rate: N/A (write-only workload)
```

## Workload Design

**Linear Distribution:**
- Each write uses an incrementing key
- Every write is a **new** page (no overwrites)
- Storage fills completely and wraps around
- Tests maximum write throughput

**Zipfian Distribution:**
- Skewed access: some keys hot, others cold
- Popular pages get overwritten frequently
- Cold pages naturally age out
- Realistic for production cache patterns

## Device Options

- **`--device sync`** — Blocking thread-pool I/O (all platforms)
- **`--device uring`** — Linux io_uring (kernel 5.1+, compile with `--features io_uring`)

## Notes

- Log wrapping is **not** safe for durability — data is lost when old entries are reclaimed
- Page cache is ideal for caching workloads; use page-store for durable storage with TTL
- Compile with `--release` for peak throughput
- io_uring requires Linux kernel 5.1 or later and appropriate kernel configuration
