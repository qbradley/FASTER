# FASTER for Rust — Quick Start

Get up and running with FASTER in under five minutes.

## Add the dependency

```toml
[dependencies]
faster-core = { path = "crates/faster-core" }
```

## 1. Hello, FASTER

Create an in-memory key-value store, insert a value, and read it back:

```rust
use faster_core::store::{FasterKv, FasterKvConfig, SimpleFunctions};
use faster_core::NullDevice;

fn main() {
    // Create a store with default config and a null device (in-memory only).
    let store: FasterKv<SimpleFunctions<u64, u64>> =
        FasterKv::new(FasterKvConfig::default(), SimpleFunctions::default(), NullDevice::new());

    // All operations go through a session — a lightweight, thread-local handle.
    let mut session = store.new_session();

    // Upsert a few keys.
    store.upsert_simple(&mut session, &1, &100);
    store.upsert_simple(&mut session, &2, &200);
    store.upsert_simple(&mut session, &3, &300);

    // Read them back.
    assert_eq!(store.read_simple(&mut session, &1), Some(100));
    assert_eq!(store.read_simple(&mut session, &2), Some(200));
    assert_eq!(store.read_simple(&mut session, &3), Some(300));

    // Delete a key.
    store.delete_simple(&mut session, &2);
    assert_eq!(store.read_simple(&mut session, &2), None);

    // Always dispose the session when you're done.
    store.dispose_session(session);

    println!("Hello, FASTER!");
}
```

That's it — a fully functional hash map backed by FASTER's hybrid log.

## 2. Persistent storage

The real power of FASTER is that your data survives process restarts.
Swap `NullDevice` for `SyncFileDevice` and your log is written to disk:

```rust
use faster_core::store::{FasterKv, FasterKvConfig, SimpleFunctions};
use faster_core::SyncFileDevice;
use std::path::Path;

type Store = FasterKv<SimpleFunctions<u64, u64>>;

/// Open (or create) a FASTER store at the given directory.
fn open_store(dir: &Path) -> Store {
    let device = SyncFileDevice::new(
        dir,
        "hlog",                  // log file prefix ("hlog.0", "hlog.1", …)
        512,                     // sector size
        1024 * 1024 * 1024,     // 1 GiB segment size
        4,                       // background I/O threads
    )
    .expect("failed to open device");

    FasterKv::new(FasterKvConfig::default(), SimpleFunctions::default(), device)
}

fn main() {
    let dir = Path::new("/tmp/faster-quickstart");

    // ── Phase 1: Write some data ────────────────────────────────────
    {
        let store = open_store(dir);
        let mut session = store.new_session();

        for i in 0..1000u64 {
            store.upsert_simple(&mut session, &i, &(i * i));
        }

        // Flush in-memory pages to disk.
        store.maintenance();

        store.dispose_session(session);
        // `store` is dropped here — the device is closed cleanly.
    }

    // ── Phase 2: Reopen and read it back ────────────────────────────
    {
        let store = open_store(dir);
        let mut session = store.new_session();

        // Data written in Phase 1 is still here.
        assert_eq!(store.read_simple(&mut session, &42), Some(42 * 42));
        assert_eq!(store.read_simple(&mut session, &999), Some(999 * 999));

        store.dispose_session(session);
    }

    println!("Data survived a restart!");

    // Clean up the demo directory.
    std::fs::remove_dir_all(dir).ok();
}
```

## 3. High-throughput concurrent upserts

FASTER is designed for multi-threaded workloads. Each thread gets its own
session — no locking required on the hot path:

```rust
use faster_core::store::{FasterKv, FasterKvConfig, SimpleFunctions};
use faster_core::NullDevice;
use std::sync::Arc;
use std::thread;

fn main() {
    let store = Arc::new(FasterKv::new(
        FasterKvConfig::default(),
        SimpleFunctions::<u64, u64>::default(),
        NullDevice::new(),
    ));

    let num_threads = 8;
    let ops_per_thread = 1_000_000;

    let start = std::time::Instant::now();

    let handles: Vec<_> = (0..num_threads)
        .map(|t| {
            let store = Arc::clone(&store);
            thread::spawn(move || {
                let mut session = store.new_session();
                let base = t * ops_per_thread;
                for i in 0..ops_per_thread as u64 {
                    store.upsert_simple(&mut session, &(base as u64 + i), &i);
                }
                store.dispose_session(session);
            })
        })
        .collect();

    for h in handles {
        h.join().unwrap();
    }

    let elapsed = start.elapsed();
    let total_ops = num_threads * ops_per_thread;
    println!(
        "{total_ops} upserts in {:.2}s — {:.2}M ops/sec",
        elapsed.as_secs_f64(),
        total_ops as f64 / elapsed.as_secs_f64() / 1_000_000.0
    );
}
```

## Key concepts

| Concept | Description |
|---------|-------------|
| **Store** | `FasterKv<F>` — the main handle. `Send + Sync`, safe to share via `Arc`. |
| **Session** | `FasterSession<F>` — a thread-local handle for CRUD ops. `!Send` by design. |
| **Functions** | The `Functions` trait defines how reads, upserts, and RMWs behave. `SimpleFunctions` covers the common case. |
| **Device** | Where the log lives: `NullDevice` (discard), `InMemoryDevice` (RAM-only), or `SyncFileDevice` (disk). |
| **Hybrid Log** | FASTER's core data structure: recent records live in memory, older records spill to the device. Operations on spilled records return `Pending` and complete asynchronously. |
| **Maintenance** | Call `store.maintenance()` periodically to flush pages and reclaim memory. |

## Next steps

- **Read-Modify-Write (RMW):** Use `CounterFunctions` for atomic counters, or implement the `Functions` trait for custom merge logic.
- **Builder API:** `FasterKv::builder()` provides a fluent interface with validation for production configs.
- **Checkpoints:** Take consistent snapshots with `CheckpointOrchestrator` for crash recovery.
- **`session_scope`:** Auto-managed sessions that clean up on drop — great for simple use cases:
  ```rust
  store.session_scope(|session, store| {
      store.upsert_simple(session, &1u64, &42u64);
  });
  ```
