---
name: "sample-readme-template"
description: "Proven template for writing clear, user-focused sample/demo crate documentation"
domain: "documentation"
confidence: "high"
source: "2026-03-14 sample READMEs"
---

## When to Use

When writing a README for a sample, tutorial, benchmark, or demo crate. Ensures consistency, clarity, and helps new users quickly understand what the code demonstrates and how to run it.

## Pattern

### Structure (in order)

```markdown
# [Crate Name]

[One-line description of what it does]

## What It Demonstrates

2–3 sentences explaining the purpose. Which FASTER features? What workload pattern?

Example:
"A lossy working-set cache workload with configurable eviction policy. Demonstrates 
the hybrid log's page cache behavior under memory pressure, comparing FASTER's 
read-heavy workload handling against fixed-capacity cache designs."

## Key Concepts

Bullet list of FASTER features shown:
- Hash index lookups
- Hybrid log appends
- Page-based I/O
- Epoch coordination
- [etc.]

## Usage

### Basic Run

```bash
cargo run --release -- [typical args]
```

Example output if applicable.

### Common Scenarios

```bash
# Scenario 1: what this flag does
cargo run --release -- --threads 8 --dataset 100_000_000

# Scenario 2: different behavior
cargo run --release -- --eviction lru --budget 256MB
```

## CLI Options

Reference table of all command-line flags:

| Flag | Default | Description |
|------|---------|-------------|
| `--threads` | num_cpus | Worker threads |
| `--dataset` | 1M | Total records to generate |
| `--eviction` | lru | Policy: lru, lfu, fifo, etc. |
| `--budget` | 512MB | Cache memory budget |

## Example Output

Show realistic sample output from a typical run (actual run output, not mock):

```
$ cargo run --release -- --threads 8
...
[INFO] Loaded 1,000,000 records into cache
[INFO] Starting workload: 10M operations @ 8 threads
...
Throughput: 50M ops/sec
Cache hit rate: 87.3%
```

## Notes

Platform/runtime/memory requirements, caveats, when to use this vs. other samples.

Example:
"Requires 8+ GB free memory. Linux io_uring optional but recommended for --device=uring. 
Suitable for understanding FASTER's memory management and page eviction under load. 
See `disk-io-bench` for comparing device implementations."

## Architecture Diagram (if helpful)

Simple ASCII or brief prose explaining data flow. Only if code isn't self-obvious.

Example for async samples:
```
User Input
    ↓
Tokio Runtime ← (async bridge)
    ↓
FASTER KV (sync)
    ↓
Device (SyncFileDevice, io_uring, etc.)
```

## See Also

Links to related samples, docs, or concepts.
```

### Writing Tone

- **Direct:** Assume the reader is someone who wants to decide in 10 minutes whether this code is worth studying
- **Action-oriented:** "Run this. Here's what you'll see. Here's how to change it."
- **No marketing:** Describe what it actually does, not why it's amazing
- **Example-heavy:** Every feature explained should have a CLI example showing it in use

### Common Pitfalls to Avoid

- **Do not:** Explain FASTER internals in the README — link to docs instead. The README is "what does this demo show" not "how does FASTER work."
- **Do not:** Bury usage under prose. Use code blocks and tables; they scan faster.
- **Do not:** Assume the reader has run other samples. Each README stands alone.
- **Do not:** Show pseudocode or simplified examples. Show the real CLI flags and actual output.

## Confidence: high

Template applied to 8 sample crate READMEs in 2026-03-14. All samples now have consistent, user-friendly documentation. Pattern proven to reduce user confusion about which sample to study first.

## Learned From

2026-03-14: Wrote READMEs for cross-impl-bench, disk-io-bench, event-counter-tokio, page-cache, page-store, read-cache-sim, read-cache-sim-tokio, torture-stress. Template derived from successful tokio-kv-server and uring-stress READMEs. Result: ~1052 lines, consistent voice, direct action-oriented approach.
