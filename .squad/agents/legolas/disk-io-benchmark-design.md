# Disk I/O Benchmark Design

**Author:** Legolas (Performance Guru)
**Date:** 2026-03-07
**Requested by:** qbradley

## Goal

Compare FASTER's three I/O device implementations head-to-head on a remote VM
with local NVMe storage, measuring throughput, latency, and I/O bandwidth under
disk-bound workloads.

---

## 1. Benchmark Matrix

### Device × Client Configurations

| # | Device | Client | Notes |
|---|--------|--------|-------|
| 1 | `SyncFileDevice` | sync (`std::thread`) | Baseline — pread/pwrite + thread pool |
| 2 | `SyncFileDevice` | Tokio (`AsyncSession`) | Tokio tasks driving sync device |
| 3 | `UringDevice` | sync (`std::thread`) | io_uring with sync client threads |
| 4 | `UringDevice` | Tokio (`AsyncSession`) | io_uring with Tokio tasks |
| 5 | `TokioFileDevice` | sync (`std::thread`) | spawn_blocking device + sync client |
| 6 | `TokioFileDevice` | Tokio (`AsyncSession`) | Full Tokio stack — expected sweet spot |

**Validity notes:**
- Config 2 (SyncFileDevice + Tokio): valid but suboptimal — Tokio's `!Send`
  async sessions work on LocalSet, device still does blocking pread/pwrite.
- Config 5 (TokioFileDevice + sync): valid — TokioFileDevice requires a Tokio
  runtime, so sync threads must run within a Tokio context.
- Config 3/4 (UringDevice): Linux-only, requires kernel 5.10+.

### Thread/Task Counts
- 1, 4, 8, 16

### Total configurations: 6 devices × 4 thread counts = 24 per workload

---

## 2. Workloads

### W1: Overcommit (Cold Read Storm)
- **Key count:** 10× memory capacity (e.g., 50M keys with 1KB values ≈ 50GB data vs 4GB buffer)
- **Access pattern:** Uniform random reads (100% read)
- **Purpose:** Force nearly all reads to hit disk; measures raw disk read throughput
- **Buffer config:** `buffer_size_pages: 128` (≈4MB in-memory)
- **Expected bottleneck:** Device read latency and bandwidth

### W2: Write-Heavy with Flush Pressure
- **Workload:** 100% upsert with continuous key range (sequential inserts)
- **Key count:** Large enough to trigger continuous page flushing (10M keys)
- **Purpose:** Measures write path — page sealing, flushing, device write bandwidth
- **Buffer config:** `buffer_size_pages: 32` (force rapid flush)
- **Expected bottleneck:** Device write throughput, flush pipeline depth

### W3: Mixed Hot/Cold (Zipfian)
- **Workload:** 50% read / 50% upsert (Workload A pattern)
- **Key count:** 10M keys (significantly larger than buffer)
- **Distribution:** Zipfian (theta=0.99) — hot keys in memory, cold keys on disk
- **Buffer config:** `buffer_size_pages: 256` (moderate memory)
- **Purpose:** Realistic workload — measures how well each device handles
  mixed I/O while maintaining in-memory fast path
- **Expected outcome:** Devices differ most on cold-read tail latency

### W4: Sequential Scan
- **Workload:** Sequential key scan over full key range (read keys 0..N in order)
- **Key count:** 5M keys (larger than buffer)
- **Buffer config:** `buffer_size_pages: 64`
- **Purpose:** Compaction-like sequential access; benefits from read-ahead.
  io_uring should excel here due to kernel-level batching.
- **Expected outcome:** UringDevice wins due to SQE batching; SyncFileDevice
  limited by thread pool depth

---

## 3. Metrics

### Per-run metrics:
| Metric | Source | Unit |
|--------|--------|------|
| Throughput | ops counted / elapsed time | ops/sec |
| Latency P50 | Sampled operation timing | nanoseconds |
| Latency P99 | Sampled operation timing | nanoseconds |
| Latency P99.9 | Sampled operation timing | nanoseconds |
| Pending rate | Pending / total ops | percentage |
| Completion latency | Time from Pending to complete | nanoseconds |
| Disk read bandwidth | (pending reads × page size) / time | MB/s |
| Disk write bandwidth | (flushed pages × page size) / time | MB/s |

### System metrics (captured by runner script):
- `iostat -x 1` during benchmark (disk utilization, await, bandwidth)
- `/proc/vmstat` page cache stats before/after
- CPU utilization via `/proc/stat` sampling

---

## 4. Implementation

### Crate: `rust/crates/samples/disk-io-bench/`

### CLI Interface:
```
disk-io-bench [OPTIONS]

  --device <sync|uring|tokio>     Device type (default: sync)
  --client <sync|tokio>           Client type (default: sync)
  --workload <overcommit|write-heavy|mixed|scan>  (default: mixed)
  --threads <N>                   Thread/task count (default: 4)
  --num-keys <N>                  Number of keys (default: 10000000)
  --value-size <N>                Value size in bytes (default: 1024)
  --buffer-pages <N>              In-memory buffer pages (default: 256)
  --duration <N>                  Measurement duration seconds (default: 30)
  --warmup <N>                    Warmup seconds (default: 10)
  --data-dir <path>               Storage directory (default: /tmp/faster-bench-data)
  --refresh-interval <N>          Epoch refresh interval (default: 64)
  --queue-depth <N>               io_uring queue depth (default: 256)
  --direct-io                     Enable O_DIRECT for uring
  --output <json|csv|table>       Output format (default: table)
  --output-file <path>            Write results to file
  --run-all                       Run full matrix (all combos)
```

### Architecture:
1. **Phase 1 — Population:** Insert all keys sequentially with enough buffer
   to avoid flushing delays. Flush and wait for all I/O.
2. **Phase 2 — Warmup:** Run workload for `--warmup` seconds, discarding metrics.
3. **Phase 3 — Measurement:** Run workload for `--duration` seconds, collecting
   latency samples and counting ops. Track pending vs completed inline.
4. **Phase 4 — Report:** Compute percentiles, format output.

### Pending I/O handling:
- Sync client: After each op that returns `Pending`, call
  `try_complete_pending(false)` in a tight loop.
- Tokio client: After batch of ops, call `session.complete_pending().await`.
- Track pending rate as a key metric (pending_count / total_ops).

### Output format (JSON):
```json
{
  "config": {
    "device": "uring",
    "client": "tokio",
    "workload": "mixed",
    "threads": 8,
    "num_keys": 10000000,
    "value_size": 1024,
    "buffer_pages": 256
  },
  "results": {
    "ops_per_sec": 1234567,
    "throughput_mb_s": 1205.8,
    "p50_ns": 850,
    "p99_ns": 45000,
    "p999_ns": 120000,
    "pending_rate": 0.15,
    "avg_pending_latency_ns": 35000,
    "disk_read_mb_s": 980.5,
    "disk_write_mb_s": 0.0,
    "total_ops": 37034010,
    "total_reads": 18517005,
    "total_writes": 18517005,
    "elapsed_secs": 30.0
  }
}
```

---

## 5. Runner Script

`rust/scripts/disk-io-bench-matrix.sh`

Runs the full benchmark matrix:
- All 6 device×client combinations (skipping uring on non-Linux)
- All 4 workloads
- Thread counts: 1, 4, 8, 16
- Collects system metrics alongside
- Outputs a combined CSV and summary table

---

## 6. VM Requirements

### Hardware
| Resource | Minimum | Recommended |
|----------|---------|-------------|
| CPU | 8 vCPU | 16 vCPU (Standard_F16s_v2) |
| RAM | 16 GB | 32 GB |
| Disk | NVMe local SSD | Azure temp disk or premium SSD v2 |
| Disk IOPS | 50K+ | 100K+ |
| Disk bandwidth | 500 MB/s+ | 1 GB/s+ |

### Current bench VM
- **IP:** 20.59.58.117
- **SKU:** Standard_F16s_v2 (16 vCPU, 32GB RAM)
- **Temp disk:** ~128GB NVMe local SSD at `/mnt`

### OS/Kernel Requirements
- **OS:** Ubuntu 22.04+ or Azure Linux 3
- **Kernel:** 5.10+ (io_uring support); 5.15+ preferred for IORING_SETUP_COOP_TASKRUN
- **liburing:** Not required (we use raw syscalls via our Ring wrapper)

### Setup Commands
```bash
# Verify kernel supports io_uring
uname -r  # should be >= 5.10

# Install Rust toolchain
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source ~/.cargo/env
rustup default stable

# Clone and build
git clone https://github.com/microsoft/FASTER.git
cd FASTER/rust
cargo build --release -p disk-io-bench

# Prepare NVMe temp disk
sudo mkdir -p /mnt/faster-bench
sudo chown $(whoami) /mnt/faster-bench

# Tune for benchmarking
echo 1 | sudo tee /proc/sys/vm/drop_caches  # clear page cache before runs
echo mq-deadline | sudo tee /sys/block/nvme0n1/queue/scheduler  # optimal for NVMe

# Run full matrix
./scripts/disk-io-bench-matrix.sh --data-dir /mnt/faster-bench --output results.csv
```

### Monitoring During Benchmark
```bash
# In a separate terminal:
iostat -xz 1                    # Disk I/O stats
vmstat 1                        # Memory/CPU overview
perf stat -a -- sleep 30        # CPU counters during a run
```

---

## 7. Expected Outcomes

### Hypothesis by workload:

| Workload | Expected Winner | Reasoning |
|----------|----------------|-----------|
| W1 (Overcommit) | UringDevice + Tokio | io_uring batches concurrent reads; kernel-level async |
| W2 (Write-heavy) | UringDevice + sync | Write batching via SQE coalescing |
| W3 (Mixed) | UringDevice + Tokio | Best of both worlds for mixed I/O |
| W4 (Sequential) | UringDevice | Read-ahead and batching benefit sequential patterns |
| All (1T baseline) | Close between all | Single-thread doesn't saturate any device |

### What to look for:
1. **UringDevice advantage magnitude** — Is io_uring 1.2× or 3× better?
2. **TokioFileDevice vs SyncFileDevice** — Does spawn_blocking add measurable overhead?
3. **Sync vs Tokio client** — Does async session overhead matter under disk I/O?
4. **Pending rate correlation** — Does higher pending rate correlate with higher throughput?
5. **Tail latency** — Which device has the best P99.9 under load?
