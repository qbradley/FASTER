# uring-stress — io_uring Battle Testing for FASTER

Stress test and performance comparison binary that exercises the
`faster-uring` crate's `UringDevice` under realistic and adversarial
workloads.

## Quick Start

```bash
# See all options
cargo run -p uring-stress -- --help

# Default stress run (4 threads, 10K ops, 4KB values)
cargo run -p uring-stress --release

# Comparison mode: UringDevice vs SyncFileDevice
cargo run -p uring-stress --release -- --compare

# Custom workload
cargo run -p uring-stress --release -- \
    --threads 8 --ops 100000 --value-size 1024 --read-pct 50

# Variable value sizes
cargo run -p uring-stress --release -- --variable-sizes

# Large dataset forcing on-disk reads
cargo run -p uring-stress --release -- \
    --threads 8 --ops 500000 --value-size 4096
```

## What It Tests

### Stress Mode (default)
- Writes `--ops` key-value pairs across `--threads` concurrent threads
- Each thread writes unique keys via the `UringDevice`
- Supports configurable value sizes (`--value-size`) or variable sizes (`--variable-sizes`)
- After writes, reads back every key and verifies correctness
- Reports IOPS, throughput (MB/s), and latency percentiles (p50, p99, p999)
- Exercises the full Device trait: `write_async`, `read_async`, cross-segment I/O

### Comparison Mode (`--compare`)
- Runs identical workloads against both `UringDevice` and `SyncFileDevice`
- Prints a side-by-side table of write IOPS, read IOPS, and throughput
- Shows where io_uring excels (batched I/O, high concurrency) and where
  the synchronous file device is comparable

### Recovery Mode (`--recovery`)
- Writes data, drops the device, reopens, and verifies all data survived

## Expected Output

```
═══ FASTER io_uring Stress Test ═══
Config: threads=4, ops=10000, value_size=4096, read_pct=0

▶ Phase 1: Sequential Writes
  Wrote 10000 keys in 1.23s
  Write IOPS:    8130
  Throughput:    31.8 MB/s
  Latency p50:   0.11ms  p99: 0.54ms  p999: 1.2ms

▶ Phase 2: Verification Reads
  Read 10000 keys in 0.98s
  Read IOPS:     10204
  Throughput:    39.9 MB/s
  All 10000 values verified correct ✅
```

## Requirements

- Linux kernel 5.1+ (io_uring support)
- This binary only compiles on Linux (`target_os = "linux"`)
