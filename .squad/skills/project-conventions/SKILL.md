---
name: "project-conventions"
description: "Core conventions and patterns for FASTER Rust codebase"
domain: "project-conventions"
confidence: "high"
source: "aragorn-history"
---

## Context

FASTER is a high-performance durable hash map with a hybrid log architecture. These conventions ensure consistency, performance, and idiomatic Rust across the codebase.

## Patterns

### Error Handling

**Two-type error model:**
- `OperationStatus` — Control flow outcomes (Ok, Pending, NotFound, InPlaceUpdated, etc.)
- `FasterError` — True errors (IoError, ChecksumMismatch, CorruptedData, etc.)

Rule: If it's an expected operational outcome, use `OperationStatus`. If it's exceptional (I/O failure, corruption), use `FasterError`.

No `thiserror` dependency — manual `Display`/`Error` impls for ~40 lines.

### Testing

**Test Framework:** `cargo nextest` (NOT `cargo test`)
- **Fast tests (tier-1):** Default, every test <1s in isolation
- **Slow tests (tier-2):** `#[ignore]` attribute, run with `--run-ignored`

**Proptest sizing:**
- Tier-1: `cases: 2` (fast feedback)
- Tier-2: `cases: 64-256` (exhaustive)

**Never use `--all-targets`** with nextest — Criterion bench binaries trigger full benchmark runs.

### Code Style

**Formatter:** `cargo fmt` (stable toolchain)
- Note: `imports_granularity` silently degrades on stable vs nightly

**Linter:** `cargo clippy`
- **Required:** `clippy::undocumented_unsafe_blocks` must pass
- Every `unsafe` block needs a `// SAFETY:` comment explaining invariants

**Module-level safety enforcement:**
- Use `#[deny(unsafe_code)]` on modules that should never need unsafe (e.g., `compaction/`)

### File Structure

```
rust/
├── crates/
│   └── faster-core/
│       ├── src/
│       │   ├── store/         # FasterKv, FasterSession, operations
│       │   ├── hash/          # Hash index
│       │   ├── allocator/     # Page allocator
│       │   ├── epoch/         # Epoch framework
│       │   ├── compaction/    # Compaction logic
│       │   ├── device/        # Storage backends
│       │   └── sync.rs        # Loom shim module
│       ├── benches/           # Criterion benchmarks
│       └── tests/             # Integration tests
├── fuzz/                      # Fuzz targets (standalone workspace)
└── docs/                      # Documentation
```

### Build Commands

**Standard workflow:**
```bash
cargo fmt
cargo clippy
cargo nextest run              # Tier-1 tests
cargo nextest run --run-ignored  # Tier-2 tests
```

**Benchmark workflow:**
```bash
cd rust && cargo bench --bench ycsb -p faster-core -- --nocapture
```

### Sync Primitives (Loom Integration)

**Production code:** ALWAYS use `crate::sync` module, never direct `std::sync`.
```rust
use crate::sync::{Arc, Mutex, AtomicU64, Ordering};
```

**Test code:** Can use `std::sync` directly (inside `#[cfg(test)]`).

### Visibility

- **Public API:** Only user-facing methods (builder, session CRUD, etc.)
- **`pub(crate)`:** Cross-module access within crate (FasterKv fields accessed by session.rs)
- **Private:** Everything else (default)

### Builder Pattern

FasterKv builder requires turbofish syntax:
```rust
let kv = FasterKv::<SimpleFunctions<u64, u64>>::builder()
    .with_capacity(1024)
    .build();
```
**Why:** `FasterKvBuilder` is non-generic; type parameter only appears at `build()`.

## Examples

### Correct Error Handling
```rust
// Expected outcome — use OperationStatus
pub fn read(&self, key: &K) -> OperationStatus {
    if !self.hash_index.contains(key) {
        return OperationStatus::NotFound;  // ✅ Not an error
    }
}

// Exception — use FasterError
pub fn open(path: &Path) -> Result<Self, FasterError> {
    let file = File::open(path).map_err(FasterError::IoError)?;  // ✅ True error
}
```

### Correct Unsafe Documentation
```rust
// SAFETY: We hold epoch protection, which prevents page eviction.
// The address was validated during hash lookup. Record alignment
// is guaranteed by the allocator.
unsafe {
    let record = &*(address.as_ptr());
    record.value()
}
```

### Correct Loom Shim Usage
```rust
// ✅ Production code
use crate::sync::{Arc, AtomicU64, Ordering};

// ❌ Wrong in production code
use std::sync::Arc;  // Bypasses loom
```

## Anti-Patterns

- **Don't use `std::sync` in production code** — Use `crate::sync` for loom compatibility
- **Don't use RecordInfo::is_null() to detect unwritten memory** — Version 0 is a valid record
- **Don't assume LogicalAddress::INVALID == 0** — INVALID is 1; 0 means empty bucket
- **Don't omit `Ordering` on atomic operations** — Always specify explicitly (Acquire, Release, etc.)
- **Don't put slow tests in tier-1** — Tests that fill pages (>1M records) or use disk I/O must be `#[ignore]`
- **Don't use `--all-targets` with nextest** — Triggers Criterion benchmark execution
- **Don't add thiserror for small enums** — Manual impls are ~40 lines and avoid dependency

## Confidence

High

## Learned From

Aragorn's history.md — 7 sessions, 190 lines of accumulated knowledge from production implementation and code reviews.
