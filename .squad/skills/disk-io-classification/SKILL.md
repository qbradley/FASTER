# Disk I/O Classification Skill — FASTER

## When to Use
Determining whether a FASTER benchmark is truly disk-bound or running from page cache. Critical for validating hybrid log benchmarks.

## Pattern

### I/O Pending Rate Classification
```
<0.1% pending ops    = IN-MEMORY   (page cache serving all reads)
0.1% - 5% pending    = MIXED       (partial disk I/O)
≥5% pending ops      = DISK-BOUND  (true disk stress)
```

### Measuring Pending Rate
All FASTER benchmarks with `--enable-logging` report:
```
Total ops: 10,000,000
Pending ops: 520,000
Pending rate: 5.2%    ← THIS IS THE KEY METRIC
```

### Forcing Disk-Bound Behavior
The `--force-disk` flag auto-tunes `buffer_pages` to ensure ≥5% pending rate:
```bash
# page-cache sample with forced disk I/O
cargo run --release --bin page-cache -- \
    --log-size-gb 4 \
    --memory-size-mb 512 \
    --buffer-pages 16 \      # Small in-memory window
    --force-disk             # Auto-validates pending rate
```

### Memory Budget Formula
```
In-memory window = buffer_pages × 32 KiB
```

For true disk-bound:
- Dataset size >> RAM (or use O_DIRECT)
- `buffer_pages` small enough that log wraps frequently
- OS page cache flushed between runs (or use `--direct-io` if available)

### Default Buffer Page Sizes
- **Old defaults (pre-2026-03-07)**: 128-256 buffer pages → almost always IN-MEMORY
- **Current defaults**: 16-32 buffer pages → realistic MIXED/DISK-BOUND scenarios

### Validation Workflow
1. Run benchmark with default settings
2. Check pending rate in output
3. If <0.1%, either:
   - Accept it's an in-memory benchmark (valid for cache performance testing)
   - Add `--force-disk` to reduce buffer pages and retry
4. Report classification in results: "IN-MEMORY at 0.03% pending" or "DISK-BOUND at 7.2% pending"

## Anti-Patterns
- **Claiming "disk benchmark" without checking pending rate**: Page cache is VERY effective. Always measure.
- **Using default buffer_pages for disk stress**: Old defaults (128-256) make disk I/O nearly impossible unless dataset is >16GB.
- **Ignoring wrap-around effects**: First-pass through log is always faster than steady-state (after wrapping). Run ≥5 minutes for DISK-BOUND.

## Confidence
**High** — Methodology established in Disk I/O Benchmark Fix (A2+A11, 2026-03-07).

## Learned From
- Disk I/O Benchmark Methodology Fix (2026-03-07): Discovered >99% of "disk benchmarks" were actually IN-MEMORY
- page-cache Disk Throughput Stress Test (2026-03-10): Validated classification across sync vs uring, single vs multi-writer
