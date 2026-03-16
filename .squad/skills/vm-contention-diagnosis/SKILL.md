# VM CPU Contention Diagnosis Skill

## When to Use
Benchmark results show unexpected regressions or high variance. Need to determine if it's real performance change or environmental noise.

## Pattern

### Pre-Benchmark Health Check
```bash
# 1. Check CPU load
uptime
# Look for: load avg should be <80% of CPU count
# Example: 20 CPUs → load avg should be <16

# 2. Check for CPU-intensive processes
ps aux --sort=-%cpu | head -20
# Look for: cargo, rustc, cargo-mutants, node, other compilation

# 3. Check I/O wait
iostat -x 5 2
# Look for: %iowait column — should be <10% for CPU benchmarks
```

### Load Average Interpretation
```
load average: 16.42, 12.18, 8.35
              ^^^^^  (1 min)  ← Most important for short benchmarks

On 20-CPU VM:
- <10  = CLEAN (0-50% utilization)
- 10-16 = ACCEPTABLE (50-80%)
- >16  = CONTENTION (>80%, expect noise)
```

### Contention Impact Patterns
| Benchmark Type | Load Avg >80% Impact | Notes |
|---------------|---------------------|-------|
| Micro (ns-scale) | +50% to +100% variance | Extremely sensitive to cache pollution |
| YCSB (ms-scale) | +5% to +15% variance | Moderate impact |
| Disk I/O (s-scale) | +5% to +10% variance | Less sensitive, I/O-bound |

### Diagnosis Workflow
1. **Unexpected regression found** (e.g., -10% throughput)
2. **Check load average** at benchmark time:
   - `uptime` during run
   - Or check historical: `sar -q` if sysstat installed
3. **If load >80%**: Mark result as "⚠️ contention" and re-run in isolation
4. **If load <50%**: Investigate as real regression

### Isolating Benchmarks
```bash
# 1. Stop competing processes
pkill -f cargo-mutants  # or other long-running builds

# 2. Wait for load to settle
watch -n 1 uptime
# Wait until load avg <50% of CPU count

# 3. Run benchmark
cargo bench --bench ycsb -- --bench

# 4. Record environment in results
echo "Benchmark env: $(uptime), $(uname -r), $(lscpu | grep 'Model name')" >> results.txt
```

### Variance Analysis
If running benchmarks under contention is unavoidable, use multiple runs:
```bash
for i in {1..5}; do
  cargo bench --bench ycsb -- --bench --save-baseline run$i
done
# Then manually compare: look for consistent direction vs random scatter
```

Consistent 5-run trend = real change
Random scatter = noise

### Known Contention Sources on FASTER VMs
- **cargo-mutants**: Sustained 100% CPU on multiple cores (~6-8 hours for full run)
- **Concurrent benchmarks**: Multiple agents running `cargo bench` simultaneously
- **Background compilation**: `cargo build` or IDE language servers
- **OS updates**: Automatic security patches (check `systemctl list-units --type=service`)

## Anti-Patterns
- **Ignoring load average**: "Benchmark says -10%, must be regression" → Wrong if load was >80%.
- **Running benchmarks during CI**: CI runners often share CPUs. Use dedicated benchmark VMs.
- **Single-run conclusions**: Always run 2-3 times if results seem anomalous.

## Confidence
**High** — Pattern discovered and validated during Post-Backlog-Sprint Benchmark (2026-03-11).

## Learned From
- Post-Backlog-Sprint Benchmark (2026-03-11): cargo-mutants running concurrently caused +102% false regression in MallocFixedPageSize micro-benchmark. Diagnosed via load avg 16/20 during benchmark time.
