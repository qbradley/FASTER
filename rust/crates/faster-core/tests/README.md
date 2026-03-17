# faster-core Test Suite

## Test Tiers

| Tier | Filter | Run time | When to run |
|------|--------|----------|-------------|
| **Tier-1** | `cargo nextest run` | <1s each | Every commit |
| **Tier-2** | `cargo nextest run --run-ignored` | 1-30s each | Pre-merge, CI |

## ThreadSanitizer (TSan) Tests

**File:** `tsan_compaction.rs`

TSan tests detect data races in the compaction scanner–evictor interaction.
They are `#[ignore]` and require nightly Rust with the TSan sanitizer.

### Quick start

```bash
cd rust
./scripts/run-tsan.sh
```

### Manual invocation

```bash
cd rust
RUSTFLAGS="-Z sanitizer=thread" \
TSAN_OPTIONS="suppressions=crates/faster-core/tests/tsan_suppressions.txt" \
  cargo +nightly test -p faster-core --test tsan_compaction -- --ignored --nocapture
```

### Prerequisites

```bash
rustup toolchain install nightly
```

TSan is included with nightly on `x86_64-unknown-linux-gnu`. Other targets
may need additional components.

### Interpreting results

- **`WARNING: ThreadSanitizer: data race`** → Real race detected. The report
  shows two conflicting accesses with stack traces. Look for page/frame
  pointers in the scanner and evictor stacks.
- **Clean pass** → No races detected in this run. The race may still exist
  but require different timing. Increase `DURATION_SECS` in the test or run
  multiple times.
- **Suppressed races** → Controlled by `tsan_suppressions.txt`. Currently
  suppresses benign races in the epoch system and RecordInfo atomic flags.

### Test scenarios

| Test | What it exercises |
|------|-------------------|
| `tsan_compaction_scanner_vs_eviction` | Primary race: scanner reads pages while evictor frees them |
| `tsan_concurrent_compaction_and_writers` | Production-like: 4 writers + compaction + maintenance |
| `tsan_begin_address_advance_during_scan` | Lossy mode: begin_address moves past scanner's read position |

### TSan + Rust notes

- TSan requires nightly (`cargo +nightly`)
- TSan requires `RUSTFLAGS="-Z sanitizer=thread"`
- TSan adds 5-15× overhead — tests are capped at ~5s wall time each
- `RUST_TEST_THREADS=1` recommended (TSan serializes tests internally)
- False positives are managed via `tsan_suppressions.txt`

## Other Specialized Test Files

| File | Purpose |
|------|---------|
| `miri_tests.rs` | Single-threaded UB detection via Miri |
| `loom_tests.rs` | Model-checked concurrency via Loom |
| `deadlock_tests.rs` | Multi-writer deadlock regression |
| `compaction_integration.rs` | Compaction orchestration correctness |
| `property_tests.rs` | Proptest-based property checks |
