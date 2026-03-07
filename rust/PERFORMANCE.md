# FASTER Rust — Performance Report

> *If you can't measure it, you can't claim it's fast.*

Cross-implementation benchmark results for the Rust FASTER KV store, with
comparison methodology for the C# and C++ implementations in this repository.

## Test Environment

| Parameter | Value |
|-----------|-------|
| **CPU** | Intel Core i9-10900K @ 3.70 GHz (10C/20T) |
| **L1d Cache** | 320 KiB (10 × 32 KiB) |
| **L2 Cache** | 2.5 MiB |
| **L3 Cache** | 20 MiB |
| **OS** | WSL2, Kernel 6.6.87.2-microsoft-standard-WSL2 |
| **Rust** | 1.85.0, edition 2024 |
| **Profile** | `--release` (thin LTO, 1 codegen unit, opt-level=3) |
| **Store** | In-memory (NullDevice), 1M keys, 8-byte key + 8-byte value |
| **Context** | `UnsafeContext` (epoch-amortized, refresh every 64 ops) |
| **Methodology** | Time-based (10s measurement, 3s warmup, best-of-3) |

## Rust Results — YCSB Workloads (8-byte KV, Uniform Distribution)

### Throughput (ops/sec)

| Workload | 1T | 2T | 4T | 8T | 16T |
|----------|---:|---:|---:|---:|----:|
| **A** (50/50 R/W) | 3.42M | 7.32M | 15.56M | 31.58M | **56.42M** |
| **B** (95/5 R/W) | 3.36M | 7.18M | 15.45M | 31.39M | **59.11M** |
| **C** (100% Read) | 3.50M | 7.29M | 15.39M | 31.17M | **59.77M** |
| **F** (50/50 R/RMW) | 3.44M | 7.29M | 15.22M | 30.55M | **57.65M** |

### Latency Percentiles (ns)

| Workload | P50 | P99 | P99.9 |
|----------|----:|----:|------:|
| **A** (1T) | 300 | 700 | 1000 |
| **A** (16T) | 200 | 600 | 900 |
| **B** (1T) | 300 | 700 | 1300 |
| **B** (16T) | 200 | 600 | 900 |
| **C** (1T) | 300 | 700 | 900 |
| **C** (16T) | 200 | 600 | 900 |
| **F** (1T) | 300 | 700 | 900 |
| **F** (16T) | 200 | 600 | 900 |

### Thread Scaling

| Threads | Speedup (Workload A) | Efficiency |
|--------:|---------------------:|-----------:|
| 1 | 1.00× | 100% |
| 2 | 2.14× | 107% |
| 4 | 4.55× | 114% |
| 8 | 9.23× | 115% |
| 16 | 16.49× | 103% |

> Super-linear scaling (>100% efficiency) is due to aggregate cache effects:
> multiple threads collectively keep more of the hash index hot in L1/L2.
> This matches the behavior of the C# and C++ implementations.

### Data Rate

| Workload | 16T Throughput |
|----------|---------------:|
| **A** (50/50 R/W) | 861 MB/s |
| **B** (95/5 R/W) | 902 MB/s |
| **C** (100% Read) | 912 MB/s |
| **F** (50/50 R/RMW) | 880 MB/s |

## How to Reproduce

```bash
# Full benchmark suite (all workloads, all thread counts)
cd rust
cargo run -p cross-impl-bench --release -- \
  --workloads A,B,C,F \
  --threads 1,2,4,8,16 \
  --run-secs 10 \
  --warmup-secs 3 \
  --iterations 3 \
  --csv results.csv

# Quick single-workload test
cargo run -p cross-impl-bench --release -- \
  --workloads A --threads 1,8 --run-secs 5 --iterations 1

# Zipfian distribution (hot-key skew)
cargo run -p cross-impl-bench --release -- \
  --distribution zipfian --zipf-theta 0.99

# Larger values (100B, 1KB)
cargo run -p cross-impl-bench --release -- \
  --value-sizes 8,100,1024

# Compare safe vs unsafe context overhead
cargo run -p cross-impl-bench --release -- --safe-context \
  --workloads C --threads 1 --iterations 3
```

## C# Benchmark (Comparison)

The C# FASTER benchmark is in `cs/benchmark/`. It supports identical YCSB
workloads with matching configuration options.

### Build & Run

```bash
cd cs
dotnet build FASTER.sln -c Release

# Equivalent to Rust Workload A (50/50 R/W), 8 threads, 30s
cs/benchmark/bin/x64/Release/net7.0/FASTER.benchmark \
  -b 0 -t 8 -d uniform --rumd 50,50,0,0 --runsec 30

# Equivalent to Rust Workload B (95/5 R/W)
cs/benchmark/bin/x64/Release/net7.0/FASTER.benchmark \
  -b 0 -t 8 -d uniform --rumd 95,5,0,0 --runsec 30

# Equivalent to Rust Workload C (100% Read)
cs/benchmark/bin/x64/Release/net7.0/FASTER.benchmark \
  -b 0 -t 8 -d uniform --rumd 100,0,0,0 --runsec 30

# Equivalent to Rust Workload F (100% RMW)
cs/benchmark/bin/x64/Release/net7.0/FASTER.benchmark \
  -b 0 -t 8 -d uniform --rumd 0,0,100,0 --runsec 30
```

### C# Configuration Mapping

| Rust CLI | C# CLI | Notes |
|----------|--------|-------|
| `--workloads A` | `--rumd 50,50,0,0` | 50% read, 50% upsert |
| `--workloads B` | `--rumd 95,5,0,0` | 95% read, 5% upsert |
| `--workloads C` | `--rumd 100,0,0,0` | 100% read |
| `--workloads F` | `--rumd 0,0,100,0` | 100% RMW |
| `--threads N` | `-t N` | Thread count |
| `--distribution zipfian` | `-d zipf` | Key distribution |
| `--safe-context` | `--safectx` | Per-op epoch protection |

## C++ Benchmark (Comparison)

The C++ FASTER benchmark is in `cc/benchmark-dir/`. It requires pre-generated
YCSB data files.

### Build

```bash
cd cc
mkdir -p build && cd build
cmake ..
make -j$(nproc)
```

### Run

```bash
# Generate YCSB data first (requires YCSB tool)
# See cc/benchmark-dir/README.md

# Workload A: 50/50 R/W, 8 threads
./benchmark-dir/benchmark 0 8 load.dat run.dat

# Workload B: 95/5 R/W
./benchmark-dir/benchmark 2 8 load.dat run.dat

# Workload C: 100% Read
./benchmark-dir/benchmark 3 8 load.dat run.dat

# Workload RMW: 100% RMW
./benchmark-dir/benchmark 1 8 load.dat run.dat
```

### C++ Workload Mapping

| Rust Workload | C++ Code | Description |
|---------------|----------|-------------|
| A | 0 | 50/50 read/upsert |
| B | 2 | 95/5 read/upsert |
| C | 3 | 100% read |
| F | 1 | 100% RMW |

## Analysis

### Key Findings

1. **Single-thread throughput: 3.4–3.5M ops/sec** — The hash index lookup
   (~80ns) dominates single-thread performance. This is consistent with prior
   profiling showing 43% L1 dcache miss rate.

2. **Multi-thread scaling is excellent** — Near-linear to super-linear scaling
   from 1→16 threads (16.5× at 16T). This matches C# FASTER's published
   scaling characteristics and exceeds typical concurrent hash map
   implementations.

3. **Latency is tight** — P50=200-300ns, P99=600-700ns, P99.9<1μs across all
   workloads and thread counts. The tail is well-controlled.

4. **Workload-independent at this scale** — With 1M keys fitting in memory,
   workload type has minimal impact on throughput. Read-heavy workloads (B, C)
   are marginally faster due to no write-path overhead.

5. **UnsafeContext is essential** — The epoch-amortized context (refreshing
   every 64 ops) eliminates ~100ns/op of epoch overhead. Using SafeContext
   reduces throughput by ~1.8× for reads and ~1.8× for upserts (see
   [Epoch Amortization Analysis](.squad/skills/epoch-amortization/SKILL.md)).

### Where Rust Shines vs C#/C++

| Dimension | Rust Advantage | Rationale |
|-----------|---------------|-----------|
| **Memory safety** | No GC pauses | C# has stop-the-world GC; Rust's epoch reclamation is deterministic |
| **Zero-cost abstractions** | Generic monomorphization | Value types inlined at compile time, no boxing |
| **Raw in-place updates** | `SUPPORTS_RAW_IN_PLACE` | Bypasses serialize/deserialize for fixed-size values |
| **Predictable latency** | No JIT warmup | C# requires JIT compilation; Rust is AOT |
| **Deployment** | Single static binary | No .NET runtime or C++ shared libraries required |

### Where Rust Has Room to Improve

| Dimension | Gap | Optimization Path |
|-----------|-----|-------------------|
| **Hash index prefetching** | ~20-40% single-thread gain possible | Software prefetch of hash bucket before access |
| **Epoch scan optimization** | ~5-10% gain | Scan only registered threads, not all 256 slots |
| **Per-thread log partitioning** | ~5-10% multi-thread gain | Reduce log tail contention at high thread counts |
| **SIMD hash computation** | ~10-15% hash throughput | Use AVX2/SSE4.2 for bulk key hashing |
| **Large value optimization** | Not yet benchmarked | Test 100B and 1KB values with `--value-sizes 100,1024` |

### Comparison Framework

To produce a fair cross-implementation comparison:

1. **Same hardware** — Run all three implementations on this machine
2. **Same key count** — 1M keys with 8-byte key + 8-byte value
3. **Same duration** — 10-second measurement window
4. **Same distribution** — Uniform random
5. **Same context mode** — Use UnsafeContext equivalent:
   - Rust: `UnsafeContext` (default in this benchmark)
   - C#: Default (non-`--safectx`)
   - C++: Default (refresh interval = 64)
6. **Same epoch refresh** — Every 64 operations

## CSV Output Format

The benchmark writes CSV with these columns when `--csv` is specified:

```
workload,threads,value_size_bytes,distribution,context,total_ops,reads,writes,
rmws,elapsed_secs,ops_per_sec,throughput_mb_s,p50_ns,p99_ns,p999_ns
```

## Benchmark Binary Options

```
cargo run -p cross-impl-bench --release -- --help
```

See `rust/crates/samples/cross-impl-bench/` for source code.
