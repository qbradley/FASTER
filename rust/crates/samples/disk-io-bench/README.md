# disk-io-bench

Disk I/O benchmark comparing FASTER device implementations — SyncFileDevice,
UringDevice, and TokioFileDevice under disk-bound workloads.

## What It Demonstrates

Measures I/O throughput and latency for different device backends (sync, io_uring,
Tokio-async) with both synchronous and asynchronous client modes. Designed to isolate
disk I/O performance on systems with high-speed NVMe storage.

For realistic results, the dataset size must exceed the in-memory buffer to force
reads through the device layer. On a 32 GB system with 10M keys and 1 KiB values,
use `--buffer-pages 16` (512 KiB window) to ensure the vast majority of operations
hit disk.

## Key Concepts

- **Device Comparison** — benchmarks sync I/O, async-aware device, and io_uring implementations
- **Memory Budgeting** — forces disk I/O by constraining the in-memory working set
- **Workload Variety** — overcommit (high concurrency), write-heavy, mixed, scan patterns
- **Dual Client Modes** — tests device performance under sync threads vs. async tasks

## Usage

```bash
# Stress test with default settings (sync device, sync clients)
cargo run -p disk-io-bench --release

# io_uring device (requires Linux + kernel 5.1+)
cargo run -p disk-io-bench --release --features io_uring -- --device uring

# Async client mode (uses Tokio's thread pool for all I/O)
cargo run -p disk-io-bench --release -- --client tokio

# Disk-focused benchmark: 10M keys with small buffer (forces disk I/O)
cargo run -p disk-io-bench --release -- \
    --device sync --workload mixed --num-keys 10000000 \
    --buffer-pages 16 --value-size 1024

# Compare all devices on the same workload
cargo run -p disk-io-bench --release -- --run-all

# JSON output for analysis
cargo run -p disk-io-bench --release -- \
    --output json --output-file results.json --iterations 3
```

## CLI Options

| Flag | Default | Purpose |
|------|---------|---------|
| `--device` | `sync` | `sync`, `uring`, or `tokio` |
| `--client` | `sync` | `sync` (threads) or `tokio` (async) |
| `--workload` | `mixed` | `overcommit`, `write-heavy`, `mixed`, `scan` |
| `--threads` | `4` | Number of worker threads/tasks |
| `--num-keys` | `10000000` | Dataset size (total records to load) |
| `--value-size` | `1024` | Value size in bytes |
| `--buffer-pages` | `256` | In-memory buffer (pages × 32 KiB) |
| `--output` | `human` | `human` or `json` |
| `--iterations` | `1` | Repeat benchmark N times |
| `--force-disk` | (flag) | Skip FASTER warm-up to force cold disk reads |

## Memory Budget Calculation

For a benchmark with **true disk I/O**:
```
dataset_size = num_keys * (8 byte key + value_size)
buffer_size = buffer_pages * 32 KiB

Requirement: dataset_size >> buffer_size
```

Example with default 10M keys and 1 KiB values:
- Dataset: ~10 GB
- Default buffer: 256 pages = 8 MB
- Result: ✅ Vast majority of reads go through device layer

## Notes

- Compile with `--release` for accurate performance results
- io_uring feature requires Linux kernel 5.1+ (kernel-async I/O)
- TokioFileDevice dispatches I/O through Tokio's internal blocking pool
- Useful for capacity planning and device selection on your hardware
