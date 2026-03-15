# cross-impl-bench

Cross-implementation YCSB benchmark suite for FASTER — runs standardized workloads
comparable to the C# and C++ reference implementations.

## What It Demonstrates

This sample runs the Yahoo Cloud Serving Benchmark (YCSB) workload suite against
the Rust FASTER implementation. It's designed for comparison with official YCSB
results from the C# and C++ implementations in the same repository.

Supports workloads A (50/50 read/write), B (95% read), C (100% read), and F (read-modify-write),
with customizable thread counts, key distributions (uniform/Zipfian), and value sizes.

## Key Concepts

- **YCSB Standardization** — use official benchmark workloads for cross-platform comparison
- **Throughput and Latency** — reports ops/sec with p50, p99, p999 latency percentiles
- **Zipfian Distribution** — models realistic skewed access patterns (configurable θ)
- **RMW (Read-Modify-Write)** — workload F demonstrates FASTER's atomic updates

## Usage

```bash
# Run all workloads with default settings
cargo run -p cross-impl-bench --release

# Specify workloads and thread counts
cargo run -p cross-impl-bench --release -- --workloads A,B,C,F --threads 1,4,8,16

# Custom settings: Zipfian distribution, 1M keys, 100-byte values
cargo run -p cross-impl-bench --release -- \
    --distribution zipfian --zipf-theta 0.99 \
    --num-keys 1000000 --value-sizes 100

# Full configuration options
cargo run -p cross-impl-bench --release -- --help
```

## CLI Options

| Flag | Default | Purpose |
|------|---------|---------|
| `--workloads` | `all` | Comma-separated: A, B, C, F or "all" |
| `--threads` | `1,4,8,16` | Comma-separated thread counts to test |
| `--distribution` | `uniform` | `uniform` or `zipfian` |
| `--zipf-theta` | `0.99` | Skew factor for Zipfian (0 = uniform, 1 = extreme) |
| `--value-sizes` | `8` | Comma-separated sizes in bytes |
| `--num-keys` | `1000000` | Keys to pre-load before measurement |
| `--measurement-duration` | `10` | Seconds per workload run |

## Example Output

```
Workload A (50% read, 50% write) @ 1 thread
  Throughput: 45,230 ops/sec
  Latency:    p50=22.1µs  p99=145.2µs  p999=412.8µs

Workload C (100% read) @ 4 threads
  Throughput: 187,540 ops/sec
  Latency:    p50=7.2µs  p99=38.1µs  p999=124.5µs
```

## Notes

- Compile with `--release` for realistic benchmark results
- Pre-loads all keys to memory before measurement begins
- Compares results against official YCSB benchmarks from the same codebase
- Variable-length values require custom `Functions` trait implementation
