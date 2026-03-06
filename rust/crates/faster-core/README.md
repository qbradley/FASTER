# faster-core

Core key-value engine for [Microsoft FASTER](https://github.com/microsoft/FASTER) — a high-performance, concurrent, latch-free hash-table-based store designed for larger-than-memory workloads. **Zero async runtime dependencies.**

## Features

- **CRUD operations** — `read`, `upsert`, `rmw` (read-modify-write), `delete` with pluggable semantics via the `Functions` trait
- **Checkpoint / Recovery** — fold-over and snapshot checkpoints persist both the hash index and hybrid log to durable storage
- **Hash table grow** — online resize doubles bucket count while concurrent readers/writers continue operating
- **Pending I/O** — transparent async disk reads when records are not in memory; callers drain completions via the session
- **Epoch protection** — safe memory reclamation without locks; threads coordinate through a shared epoch table
- **Builder pattern** — `FasterKvBuilder` provides fluent, validated store construction

## Quick Start

```rust
use faster_core::{FasterKv, SimpleFunctions, NullDevice};
use faster_core::status::OperationStatus;

// Create a store (in-memory only via NullDevice)
let store: FasterKv<SimpleFunctions<u64, u64>> = FasterKv::builder()
    .hash_index_size_log2(16)   // 64K buckets
    .buffer_size_pages(16)
    .build(SimpleFunctions::default(), NullDevice::new())
    .expect("valid config");

// Open a thread-local session
let mut session = store.new_session();

// Upsert a key-value pair
store.upsert(&mut session, &1u64, &42u64, ());

// Read it back
let mut output: Option<u64> = None;
let status = store.read(&mut session, &1u64, &0u64, &mut output, ());
assert_eq!(status, OperationStatus::Ok);
assert_eq!(output, Some(42));

// Clean up
store.dispose_session(session);
```

## Building

```bash
# From the workspace root (rust/)
cargo check -p faster-core
cargo test -p faster-core
cargo doc -p faster-core --no-deps --open
```

## Running Examples

```bash
cargo run -p faster-core --example basic_usage
```

## Architecture

| Component | Description |
|-----------|-------------|
| `FasterKv<F>` | Main store — owns hash index, hybrid log, epoch table, device |
| `FasterSession<F>` | Thread-affine (`!Send`) handle for CRUD operations |
| `Functions` trait | User-defined callbacks for read/upsert/RMW/delete semantics |
| `HashIndex` | Latch-free concurrent hash table with overflow chains |
| `HybridLogAllocator` | Circular page buffer spanning memory and disk |
| `EpochTable` | Epoch-based safe memory reclamation (no locks) |
| `Device` trait | Pluggable I/O backend (`NullDevice`, `InMemoryDevice`, `SyncFileDevice`) |

## License

MIT — see [LICENSE](../../LICENSE) in the repository root.
