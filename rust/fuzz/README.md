# FASTER Rust — Fuzz Testing

Fuzz testing for the FASTER Rust implementation using
[cargo-fuzz](https://github.com/rust-fuzz/cargo-fuzz) (libFuzzer backend).

## Prerequisites

Fuzz testing **requires the Rust nightly toolchain** because libFuzzer
instrumentation is only available on nightly.

```bash
# Install nightly (if not already present)
rustup toolchain install nightly

# Install cargo-fuzz
cargo install cargo-fuzz
```

## Fuzz Targets

| Target | What It Covers |
|--------|---------------|
| `fuzz_record_parsing` | `RecordInfo::from_raw`, `Key`/`Value` deserialization for fixed and variable-length types, `serialized_size_from_bytes`, `eq_from_bytes`. Exercises all record parsing paths with arbitrary byte slices. |
| `fuzz_record_layout` | `RecordLayout::compute` and `pad_alignment` arithmetic with adversarial key/value sizes. Verifies 8-byte alignment invariants and overflow safety. |
| `fuzz_compaction_record_size` | `record_size_from_bytes` for multiple type combinations (`u64/u64`, `Vec<u8>/Vec<u8>`, `u32/Vec<u8>`). Simulates compaction scanner walking corrupted pages. |
| `fuzz_hash` | All `Hashable` trait implementations (`u64`, `u32`, `i64`, `i32`, `[u8]`, `Vec<u8>`, `String`). Verifies `KeyHash` tag/index invariants and hash determinism. |
| `fuzz_store_ops` | End-to-end `FasterKv` sequences of Read/Upsert/RMW/Delete with random keys on an in-memory store. Catches panics, deadlocks, and assertion failures in the core engine. |

## Quick Start

### Smoke Test (10 seconds per target — for PR validation)

```bash
cd rust
./scripts/fuzz-ci --smoke
```

### CI Mode (60 seconds per target — default)

```bash
cd rust
./scripts/fuzz-ci
```

### Extended Fuzzing (custom duration)

```bash
cd rust
FUZZ_DURATION=300 ./scripts/fuzz-ci          # 5 minutes per target
FUZZ_DURATION=3600 ./scripts/fuzz-ci         # 1 hour per target
```

### Run a Single Target

```bash
cd rust
./scripts/fuzz-ci --target record_parsing             # CI mode, one target
./scripts/fuzz-ci --target fuzz_hash --smoke           # Smoke test, one target
```

### Run Interactively (indefinite, with live output)

```bash
cd rust/fuzz
cargo fuzz run fuzz_record_parsing                     # Runs until Ctrl-C
cargo fuzz run fuzz_store_ops -- -max_len=8192         # Custom max input size
```

## Investigating Crashes

When a fuzz target finds a crash, the artifact is saved to
`rust/fuzz/artifacts/<target>/`. To reproduce:

```bash
cd rust/fuzz

# Reproduce the crash
cargo fuzz run fuzz_record_parsing artifacts/fuzz_record_parsing/crash-<hash>

# Minimize the crash input to its smallest reproduction
cargo fuzz tmin fuzz_record_parsing artifacts/fuzz_record_parsing/crash-<hash>
```

### Debugging with LLDB/GDB

```bash
cd rust/fuzz

# Build with debug symbols (default for fuzz builds)
cargo fuzz build

# Run under a debugger
cargo fuzz run fuzz_record_parsing \
    artifacts/fuzz_record_parsing/crash-<hash> \
    -- -runs=1 2>&1 | head -1
# Copy the binary path from the output, then:
# lldb <binary> -- artifacts/fuzz_record_parsing/crash-<hash>
```

## Corpus Management

Seed inputs live in `rust/fuzz/corpus/<target>/`. The fuzzer loads these
at startup and discovers new interesting inputs during each run.

### Adding New Seed Inputs

1. Create a file with representative bytes in the appropriate corpus
   directory:
   ```bash
   # Example: add a seed for record_parsing
   echo -ne '\x00\x00\x00\x00\x00\x00\x00\x00' > rust/fuzz/corpus/fuzz_record_parsing/my_seed
   ```
2. Commit the seed file. Good seeds are:
   - Minimal inputs that exercise specific code paths
   - Previously-found crash inputs (after minimization)
   - Inputs from real-world data formats
3. File names don't matter — the fuzzer reads all files in the directory.

### Minimizing the Corpus

Over time, corpus directories grow as the fuzzer discovers new inputs.
Minimize to remove redundant files while preserving coverage:

```bash
cd rust
./scripts/fuzz-minimize                    # Minimize all targets
./scripts/fuzz-minimize --target hash      # Minimize one target
```

After minimization, commit the reduced corpus.

## Adding New Fuzz Targets

1. **Create the target** at `rust/fuzz/fuzz_targets/fuzz_<name>.rs`:
   ```rust
   #![no_main]
   use libfuzzer_sys::fuzz_target;

   fuzz_target!(|data: &[u8]| {
       // Exercise the code under test with `data`
   });
   ```
   For structured input, use the `arbitrary` crate:
   ```rust
   use arbitrary::Arbitrary;

   #[derive(Arbitrary, Debug)]
   struct MyInput { /* fields */ }

   fuzz_target!(|input: MyInput| { /* ... */ });
   ```

2. **Register the binary** in `rust/fuzz/Cargo.toml`:
   ```toml
   [[bin]]
   name = "fuzz_<name>"
   path = "fuzz_targets/fuzz_<name>.rs"
   doc = false
   ```

3. **Add seed corpus** at `rust/fuzz/corpus/fuzz_<name>/` with at least
   one representative input.

4. **Update the target list** in both:
   - `rust/scripts/fuzz-ci` — add to `ALL_TARGETS` array
   - `rust/scripts/fuzz-minimize` — add to `ALL_TARGETS` array

5. **Verify** with a quick smoke test:
   ```bash
   cd rust && ./scripts/fuzz-ci --target <name> --smoke
   ```

## CI Integration

The `fuzz-ci` script is designed for CI pipelines:

- **Smoke mode** (`--smoke`): 10s per target, suitable for PR checks.
- **CI mode** (default): 60s per target, suitable for nightly runs.
- **Exit code**: 0 on success, non-zero if any target crashes.
- **Artifacts**: Crash files saved to `rust/fuzz/artifacts/<target>/`
  for post-mortem investigation.
- **Environment overrides**: `FUZZ_DURATION`, `FUZZ_MAX_LEN`, `FUZZ_JOBS`.

### Pipeline Example

```yaml
# GitHub Actions / Azure Pipelines snippet
- script: |
    cd rust
    ./scripts/fuzz-ci --smoke
  displayName: 'Fuzz smoke test'
  condition: eq(variables['Build.Reason'], 'PullRequest')

- script: |
    cd rust
    FUZZ_DURATION=300 ./scripts/fuzz-ci
  displayName: 'Fuzz CI (nightly)'
  condition: eq(variables['Build.Reason'], 'Schedule')
```

## Architecture Notes

- The fuzz crate (`rust/fuzz/`) is an **independent workspace** — it has
  its own `[workspace]` declaration in `Cargo.toml` to avoid interfering
  with the parent Rust workspace. This prevents race conditions during
  concurrent builds.
- All targets depend on `faster-core` via a path dependency.
- The `fuzz_store_ops` target uses `InMemoryDevice` to avoid disk I/O
  and limits operations to 2048 per run to prevent timeouts.
- String deserialization is only tested with valid UTF-8 (panics on
  invalid UTF-8 are by design, not bugs).
