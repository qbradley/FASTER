# Skill: Fuzz Target Authoring for FASTER-Rust

## When to Use
When adding a new fuzz target to `rust/fuzz/` for any module in faster-core.

## Fuzz Crate Structure
- **Location:** `rust/fuzz/` (standalone workspace, NOT a workspace member)
- **Cargo.toml:** Each target is a `[[bin]]` entry with `doc = false`
- **Targets directory:** `rust/fuzz/fuzz_targets/`
- **Build command:** `cd rust/fuzz && cargo build` (stable, compilation check)
- **Run command:** `cd rust/fuzz && cargo +nightly fuzz run <target> -- -max_total_time=N`

## Target Template

```rust
#![no_main]

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;

// Import the module under test
use faster_core::some_module::TargetType;

#[derive(Arbitrary, Debug)]
struct FuzzInput {
    // Structured inputs for the fuzzer
    data: Vec<u8>,
}

fuzz_target!(|input: FuzzInput| {
    // Guard against OOM / excessive iteration
    if input.data.len() > SOME_LIMIT {
        return;
    }

    // Exercise the target — must NEVER panic on any input
    let _ = TargetType::parse(&input.data);
});
```

## Patterns

### In-Memory Targets (preferred)
For functions that take `&[u8]` or structured types directly:
```rust
fuzz_target!(|data: &[u8]| { ... });
// or
fuzz_target!(|input: ArbitraryStruct| { ... });
```

### File-Based Targets
For code that reads from `Path` (like checkpoint/recovery):
```rust
let dir = tempfile::tempdir().expect("tempdir");
let path = dir.path().join("test.file");
std::fs::File::create(&path).unwrap().write_all(&input.data).unwrap();
// Then call the file-reading API
let _ = SomeReader::open(&path);
```
Requires `tempfile = "3"` in fuzz Cargo.toml.

### CRC Roundtrip Verification
```rust
let crc = crc32fast::hash(&data);
let trailer = PageTrailer::new(data.len() as u32, crc);
let bytes = trailer.to_bytes();
let recovered = PageTrailer::from_bytes(bytes);
assert_eq!(recovered.crc32, crc);
```

### JSON Deserialization Targets
For serde types, convert fuzz bytes to UTF-8 string first:
```rust
if let Ok(json_str) = std::str::from_utf8(&input.json_bytes) {
    let _ = serde_json::from_str::<TargetType>(json_str);
}
```

## Checklist for New Targets
1. Add `[[bin]]` entry in `rust/fuzz/Cargo.toml`
2. Create `rust/fuzz/fuzz_targets/fuzz_<name>.rs`
3. Guard against OOM with input size limits
4. Use `let _ =` for results — never unwrap in fuzz targets
5. Verify: `cd rust/fuzz && cargo build`
6. Add corpus seeds to `rust/fuzz/corpus/<target>/` (optional but helpful)

## Current Targets (8 total)
| Target | Surface | Type |
|--------|---------|------|
| fuzz_record_parsing | Record deserialization | In-memory |
| fuzz_record_layout | Layout computation | In-memory |
| fuzz_compaction_record_size | Record sizing | In-memory |
| fuzz_hash | Hash function | In-memory |
| fuzz_store_ops | Store CRUD operations | In-memory |
| fuzz_page_trailer | CRC trailer parsing | In-memory |
| fuzz_checkpoint_recovery | Index file + JSON metadata | File-based |
| fuzz_log_recovery | Log recovery pipeline | File-based |
