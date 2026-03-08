# Fuzz Testing — FASTER Rust

Fuzz testing uses [cargo-fuzz](https://github.com/rust-fuzz/cargo-fuzz) (libFuzzer
backend) to generate random inputs and find panics, bounds-check failures, or
undefined behavior in critical parsing and storage paths.

## Prerequisites

```bash
# Nightly toolchain (required by libFuzzer)
rustup toolchain install nightly

# cargo-fuzz
cargo install cargo-fuzz
```

## Quick Start

```bash
# Run all targets for 60 seconds each (CI mode)
cd rust && ./scripts/fuzz

# Run all targets for 5 minutes each
cd rust && ./scripts/fuzz 300

# Run a single target indefinitely
cd rust/fuzz && cargo fuzz run fuzz_hash
```

## Fuzz Targets

| Target | Attack Surface | What It Tests |
|--------|---------------|---------------|
| `fuzz_record_parsing` | `RecordInfo`, `Key::deserialize`, `Value::deserialize` | Arbitrary byte slices fed into deserialization for u32/u64/i32/i64/Vec\<u8\>/String types. Checks roundtrip invariants. |
| `fuzz_record_layout` | `RecordLayout::compute`, `pad_alignment` | Adversarial key/value size pairs. Validates 8-byte alignment, offset ordering, and arithmetic safety. |
| `fuzz_compaction_record_size` | `record_size_from_bytes` | Synthetic log pages with random bytes. Must return `Ok` or `Err(RecordSizeError)` — never panic. Tests fixed, variable, and mixed type combos at multiple offsets. |
| `fuzz_hash` | `Hashable` implementations, `KeyHash` | All key types (u32/u64/i32/i64/bytes/String) through hash functions. Validates tag range, index bounds, and determinism. |
| `fuzz_store_ops` | `FasterKv` CRUD operations | Random sequences of Read/Upsert/RMW/Delete with arbitrary keys and values. Looks for panics, assertion failures, or status code violations. |

## Reproducing Crashes

When a fuzzer finds a crash, it saves the minimal input to `rust/fuzz/artifacts/<target>/`:

```bash
# Reproduce
cd rust/fuzz
cargo fuzz run fuzz_record_parsing artifacts/fuzz_record_parsing/crash-<hash>

# Minimize the failing input
cargo fuzz tmin fuzz_record_parsing artifacts/fuzz_record_parsing/crash-<hash>
```

## Corpus Management

Corpus files accumulate in `rust/fuzz/corpus/<target>/`. These are NOT checked
into git. To share a corpus across CI runs, archive the corpus directory and
restore it before the fuzz run:

```bash
# Save corpus after a long run
tar czf corpus-$(date +%Y%m%d).tar.gz rust/fuzz/corpus/

# Restore before a CI run
tar xzf corpus-latest.tar.gz
```

## Architecture

The fuzz crate lives at `rust/fuzz/` and is **excluded** from the workspace
(`exclude = ["fuzz"]` in the workspace `Cargo.toml`). It uses its own
`[workspace]` section to be fully independent. This means:

- `./scripts/precheckin` does **not** build or test fuzz targets
- Fuzz targets require the nightly toolchain
- The fuzz crate depends on `faster-core` via path dependency

## Adding New Targets

1. Create `rust/fuzz/fuzz_targets/fuzz_<name>.rs` with the `fuzz_target!` macro
2. Add a `[[bin]]` section in `rust/fuzz/Cargo.toml`
3. Add the target name to `TARGETS` array in `rust/scripts/fuzz`

Template:

```rust
#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Your fuzz logic here — must not panic on valid/invalid input
});
```

For structured input, use the `arbitrary` crate:

```rust
#![no_main]
use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;

#[derive(Arbitrary, Debug)]
struct Input {
    key: u64,
    value: u64,
}

fuzz_target!(|input: Input| {
    // Your fuzz logic here
});
```
