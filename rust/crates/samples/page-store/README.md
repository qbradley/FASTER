# page-store

Durable page store using FASTER with delete-based retention — demonstrates
durable storage semantics with TTL-style cleanup as an alternative to lossy caching.

## What It Demonstrates

Similar to `page-cache`, but with **durability guarantees**. Writer threads actively
delete the oldest pages when the live key count approaches `--max-live-pages`, preventing
unbounded growth while maintaining ACID semantics. Perfect for logging systems, time-series
databases, and audit trails where data must persist until explicitly aged out.

Supports linear (sequential) and Zipfian (skewed) key distributions, with configurable
retention policies via delete operations.

## Key Concepts

- **Durable Retention** — data persists until explicitly deleted (not lost on wrap-around)
- **Bounded Storage** — writer threads delete old pages to maintain `--max-live-pages` limit
- **Distribution Modes** — linear (sequential writes) vs. Zipfian (skewed access)
- **Optional Readers** — concurrent reader threads can verify stored values
- **Atomic Deletes** — remove entries via FASTER's delete operation

## Usage

```bash
# Run with defaults (60 seconds, 100K max live pages)
cargo run -p page-store --release

# Large retention window with multiple readers
cargo run -p page-store --release -- \
    --duration 300 --max-live-pages 1000000 --readers 4

# Write-only store (no readers), small retention window
cargo run -p page-store --release -- \
    --max-live-pages 10000 --readers 0

# Durable store with Zipfian distribution (realistic pattern)
cargo run -p page-store --release -- \
    --distribution zipf --zipf-exponent 1.2 \
    --key-space 1000000 --max-live-pages 500000

# Tuned for persistence: large log, small memory window
cargo run -p page-store --release -- \
    --log-size-mb 8192 --in-memory-mb 128 \
    --storage-dir ./durable-log

# Show all options
cargo run -p page-store --release -- --help
```

## CLI Options

| Flag | Default | Purpose |
|------|---------|---------|
| `--duration` | `60` | Run for N seconds |
| `--storage-dir` | `./page-store-data` | Directory for FASTER log files |
| `--log-size-mb` | `1024` | Total log size in MB |
| `--in-memory-mb` | `256` | In-memory portion in MB |
| `--page-size` | `4096` | Value size per page in bytes |
| `--distribution` | `linear` | `linear` or `zipf` (skewed) |
| `--zipf-exponent` | `1.0` | Zipfian θ parameter |
| `--key-space` | `10000000` | Total unique keys |
| `--max-live-pages` | `100000` | Target max live key count |
| `--readers` | `0` | Number of reader threads (0 = write-only) |
| `--writer-threads` | `4` | Number of writer threads |
| `--stats-interval` | `1000` | Print stats every N milliseconds |

## Example Output

```
═══ FASTER Durable Page Store ═══
Config: max_live=100K, log=1024 MB, readers=2

▶ Phase 1: Warmup
  Wrote 100,000 pages in 1.89s (52.9 MB/s)

▶ Phase 2: Steady State (60 seconds)
  Writers:        4 threads (sequential keys)
  Readers:        2 threads (verification)
  Write rate:     1,234,567 pages/sec
  Read rate:      456,789 pages/sec
  Live keys:      100,000 (at target)
  Deletions/sec:  18,500

▶ Final Stats
  Total written:  74,074,020 pages
  Total deleted:  73,974,020 pages
  Live pages:     100,000 (durable)
  Reader verifications: ✅ 100% match
```

## Retention Policy

The store actively maintains `--max-live-pages` by deleting the oldest keys:

1. Writer threads insert pages with sequential or Zipfian keys
2. When live key count exceeds threshold, writer threads delete the **oldest keys**
3. Deleted keys free space for new inserts
4. Storage never grows beyond `--log-size-mb`

Example with `--max-live-pages 100000`:
- Once the store reaches 100,000 keys, each new write triggers a delete
- The deleted key is always the oldest (lowest key ID in linear mode)
- Storage remains stable at ~100,000 active entries

## Comparison: Page Cache vs. Page Store

| Aspect | page-cache | page-store |
|--------|-----------|-----------|
| **Semantics** | Lossy cache | Durable store |
| **Eviction** | Automatic (log wrapping) | Explicit (delete operations) |
| **TTL** | Implicit (working set size) | Explicit (manual cleanup) |
| **Guarantees** | Best-effort | ACID durability |
| **Use case** | CDN cache, buffer pools | Time-series DB, audit log |

## Reader Verification

If `--readers > 0`, reader threads periodically:
1. Select a random live key from the store
2. Read its value
3. Verify it matches the expected value
4. Report any mismatches (corruption detection)

This is useful for integrity testing under concurrent writes and deletes.

## Notes

- Compile with `--release` for realistic throughput
- Data persists on disk (directory specified by `--storage-dir`)
- Use `--readers 0` for write-only workloads (skip verification overhead)
- Delete operations are transactional but may be batched for efficiency
