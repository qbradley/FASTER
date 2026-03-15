# torture-stress

Comprehensive correctness torture-test for FASTER KV with concurrent oracle verification —
stresses the engine with diverse thread roles under wave-shaped load to detect corruption.

## What It Demonstrates

Spawns diverse worker threads with different roles (heavy writers, light writers, readers,
mixed workers, RMW hammers, deleters) under time-varying load waves (sine, square, sawtooth, spike).
Every read is verified against a concurrent oracle (`DashMap`) — any mismatch is reported as
a corruption bug. Exit code 0 = no violations; exit code 1 = corruption detected.

Perfect for stress-testing FASTER under adversarial conditions before production deployment.

## Key Concepts

- **Correctness Oracle** — DashMap-backed reference implementation for verification
- **Diverse Workloads** — 6 thread types with different access patterns
- **Wave-Shaped Load** — time-varying concurrency to stress synchronization
- **Variable Value Sizes** — tests memory management across different sizes
- **Deterministic Seeds** — reproduce failures with PRNG seeds

## Usage

```bash
# Run standard torture test (60 seconds with default load)
cargo run -p torture-stress --release

# Aggressive test: 8 heavy writers, 4 RMW hammers, spike wave
cargo run -p torture-stress --release -- \
    --duration 120 \
    --heavy-writers 8 --rmw-hammers 4 \
    --wave sine

# Write-heavy stress (high contention)
cargo run -p torture-stress --release -- \
    --heavy-writers 4 --light-writers 4 \
    --readers 1 --deleters 1 \
    --wave spike

# Large key-space with variable-size values
cargo run -p torture-stress --release -- \
    --key-space 10000000 \
    --min-value-size 32 --max-value-size 8192 \
    --duration 300

# Deterministic reproduction (use same seed)
cargo run -p torture-stress --release -- \
    --seed 0x12345678 --duration 60

# Show all options
cargo run -p torture-stress --release -- --help
```

## CLI Options

### Duration & Scale

| Flag | Default | Purpose |
|------|---------|---------|
| `--duration` | `60` | Test duration in seconds |
| `--key-space` | `1000000` | Total unique keys |
| `--min-value-size` | `64` | Minimum value size (bytes) |
| `--max-value-size` | `4096` | Maximum value size (bytes) |

### Thread Configuration

| Flag | Default | Purpose |
|------|---------|---------|
| `--heavy-writers` | `1` | Threads writing large values (100% upsert) |
| `--light-writers` | `1` | Threads writing small values (100% upsert) |
| `--readers` | `2` | Pure read-only threads |
| `--mixed-workers` | `2` | Mixed read/write/delete threads |
| `--rmw-hammers` | `1` | Threads hammering RMW (Read-Modify-Write) |
| `--deleters` | `1` | Threads deleting random keys |

### Load Profile

| Flag | Default | Purpose |
|------|---------|---------|
| `--wave` | `sine` | Load pattern: `sine`, `square`, `sawtooth`, `spike` |
| `--wave-frequency` | `0.1` | Waves per second |
| `--peak-load` | `1.0` | Peak load multiplier (1.0 = baseline) |

### Reproducibility

| Flag | Default | Purpose |
|------|---------|---------|
| `--seed` | random | PRNG seed for deterministic reproduction |
| `--log-dir` | `./torture-data` | Directory for FASTER log files |

## Thread Roles

### Heavy Writers
- Write large values (`--max-value-size`)
- 100% upsert operations (no deletes/reads)
- High memory pressure

### Light Writers
- Write small values (`--min-value-size`)
- 100% upsert operations
- Lower memory load, high operation frequency

### Readers
- Pure reads only
- Verify every read against oracle
- Detect staleness or corruption

### Mixed Workers
- Read (40%), Write (40%), Delete (20%)
- Variable value sizes
- Typical application pattern

### RMW Hammers
- Intense Read-Modify-Write operations
- Stress atomic update logic
- Concurrent increment/decrement

### Deleters
- Delete random keys
- Exercise free list and recovery
- Concurrent with all other operations

## Wave Functions

**Sine Wave** — smooth oscillation, realistic load patterns
```
Load
  │     ╱╲    ╱╲
  │    ╱  ╲  ╱  ╲
  └───────────────
    (frequency = 0.1 Hz → 10 second period)
```

**Square Wave** — sudden load changes, tests synchronization boundaries
```
Load
  │ ╱──╲  ╱──╲
  │╱    ╲╱    ╲
  └─────────────
    (frequency = 0.1 Hz → 5 seconds high, 5 seconds low)
```

**Sawtooth** — ramp up then drop, realistic for batch workloads
```
Load
  │╱   ╱   ╱
  │  ╱   ╱
  └ ╱   ╱
    (frequency = 0.1 Hz → 10 second ramp + drop)
```

**Spike** — sudden spikes, tests bursty concurrency
```
Load
  │ │ │ │ │
  │ │ │ │ │
  └─┴─┴─┴─┴─
    (frequency = 0.1 Hz → spikes every 10 seconds)
```

## Oracle Verification

Every read operation is checked:
```
1. Read key K from FASTER store
2. Read key K from DashMap oracle
3. If values differ → CORRUPTION DETECTED
   - Log key, expected value, actual value
   - Exit with code 1
4. If values match → ✅ continue
```

Mismatches indicate:
- Data corruption (memory corruption, synchronization bugs)
- Stale reads (concurrency bugs in epoch management)
- Lost updates (missing writes, CAS failures)

## Example Output

```
═══ FASTER Torture Stress Test ═══
Config: duration=60s, key_space=1M, wave=sine

▶ Thread Configuration
  Heavy writers:   1  (4096 byte values)
  Light writers:   1  (64 byte values)
  Readers:         2  (verification)
  Mixed workers:   2  (40% read, 40% write, 20% delete)
  RMW hammers:     1  (atomic increments)
  Deleters:        1  (random delete)
  Total threads:   8

▶ Load Waves
  Pattern: sine (0.1 Hz frequency)
  Peak: 1.0× baseline load

▶ Statistics (every 5 seconds)
  t=5s   : ops=547,234  writes=218,947  reads=328,287  deletes=0       oracle_pass=100%
  t=10s  : ops=623,445  writes=249,778  reads=373,667  deletes=0       oracle_pass=100%
  t=15s  : ops=512,334  writes=204,933  reads=307,401  deletes=0       oracle_pass=100%
  ...

▶ Final Summary (60 seconds)
  Total operations:    37,234,567
  - Writes:          14,893,827
  - Reads:           18,567,123
  - Deletes:          3,773,617
  
  Oracle verification: ✅ PASS (100% match rate)
  No corruption detected.
  
  Exit code: 0 (success)
```

## Corruption Detection Example

```
▶ Statistics (every 5 seconds)
  t=48s  : ops=18,234,567  writes=7,293,827  reads=9,867,123  ...  oracle_pass=100%
  t=53s  : ops=21,234,567  writes=8,493,827  reads=11,267,123 ...  oracle_pass=100%

▶ ORACLE VIOLATION at t=54.3s
  Key: 573488
  Expected value: [u64: 12345]
  Actual value:   [u64: 12344]
  
  Thread: Reader-1
  Timestamp: 54.321s
  
  ✗ CORRUPTION DETECTED
  
  Exit code: 1 (failure)
```

## Reproducibility & Debugging

To reproduce a failure:

```bash
# Save the seed from the failing run
# Re-run with the same seed
cargo run -p torture-stress --release -- \
    --seed 0x9a8b7c6d \
    --duration 60
```

Same seed = same PRNG sequence = identical operations = deterministic failure.

## Notes

- Compile with `--release` for realistic concurrency
- Exit code 0 = success; exit code 1 = corruption found
- Use `--wave spike` for maximum stress during releases
- High memory workloads may trigger GC; use `--max-value-size 1024` for better throughput
- Perfect as a pre-release validation tool
