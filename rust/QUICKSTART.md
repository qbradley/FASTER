# FASTER for Rust — Quick Start

Get up and running with FASTER in under five minutes.

## Add the dependency

```toml
[dependencies]
faster-core = { git = "https://github.com/qbradley/FASTER", branch = "squad" }
```

## Understanding FASTER's I/O model

FASTER is a hybrid in-memory/disk storage engine. Recent records live in
memory; older records are evicted to disk. This means **any operation can
return `Pending`** when it touches an on-disk record:

```text
┌──────────────────────────────────────────┐
│  In-memory (mutable)   → Ok immediately  │
│  In-memory (read-only) → Ok immediately  │
│  On-disk (evicted)     → Pending         │
└──────────────────────────────────────────┘
```

When you get `Pending`, the I/O has been dispatched to the device in the
background. Call **`complete_pending`** to collect results. Always drain
pending operations before disposing a session — `dispose_session` drops
any outstanding pending work.

## 1. Hello, FASTER

Create an in-memory key-value store, insert a value, and read it back.
With `NullDevice` and fresh keys, all records stay in memory — no
`Pending` results to worry about:

```rust
use faster_core::store::{FasterKv, FasterKvConfig, SimpleFunctions};
use faster_core::NullDevice;

fn main() {
    let store: FasterKv<SimpleFunctions<u64, u64>> =
        FasterKv::new(FasterKvConfig::default(), SimpleFunctions::default(), NullDevice::new());

    let mut session = store.new_session();

    // Upsert a few keys. Fresh inserts always succeed in-memory.
    store.upsert_simple(&mut session, &1, &100);
    store.upsert_simple(&mut session, &2, &200);
    store.upsert_simple(&mut session, &3, &300);

    // Read them back. In-memory reads return Ok immediately.
    assert_eq!(store.read_simple(&mut session, &1), Some(100));
    assert_eq!(store.read_simple(&mut session, &2), Some(200));
    assert_eq!(store.read_simple(&mut session, &3), Some(300));

    // Delete a key.
    store.delete_simple(&mut session, &2);
    assert_eq!(store.read_simple(&mut session, &2), None);

    store.dispose_session(session);

    println!("Hello, FASTER!");
}
```

> **Note:** `read_simple` returns `None` for **both** `NotFound` and
> `Pending`. For production code, use `store.read()` and check the
> `OperationStatus` to distinguish "key doesn't exist" from "key is on
> disk, I/O in progress."

## 2. Persistent storage

FASTER uses an **explicit checkpoint model** — data is only durable after
you take a checkpoint. Swap `NullDevice` for `SyncFileDevice`, checkpoint
before closing, and recover after reopening:

```rust
use faster_core::checkpoint::CheckpointType;
use faster_core::status::OperationStatus;
use faster_core::store::{FasterKv, FasterKvConfig, SimpleFunctions};
use faster_core::SyncFileDevice;
use std::path::Path;

type Store = FasterKv<SimpleFunctions<u64, u64>>;

fn open_store(dir: &Path) -> Store {
    let device = SyncFileDevice::new(
        dir,
        "log.",                  // log file prefix ("log.0", "log.1", …)
        512,                     // sector size
        1024 * 1024 * 1024,     // 1 GiB segment size
        4,                       // background I/O threads
    )
    .expect("failed to open device");

    FasterKv::new(FasterKvConfig::default(), SimpleFunctions::default(), device)
}

fn main() {
    let dir = Path::new("/tmp/faster-quickstart");

    // ── Phase 1: Write data and take a checkpoint ───────────────────
    {
        let store = open_store(dir);
        let mut session = store.new_session();

        for i in 0..1000u64 {
            let status = store.upsert(&mut session, &i, &(i * i), ());
            // Fresh inserts into a new store are always in-memory.
            assert_ne!(status, OperationStatus::Pending);
        }

        // Drain any pending ops before disposing. In this case there are
        // none, but this is the correct pattern.
        store.complete_pending(&mut session);
        store.dispose_session(session);

        // Checkpoint persists the hash index + log metadata to disk.
        // Without this, reopening the store gives an empty hash index.
        store.checkpoint(dir, CheckpointType::FoldOver)
            .expect("checkpoint failed");
    }

    // ── Phase 2: Reopen and recover ─────────────────────────────────
    {
        let mut store = open_store(dir);

        // Recover restores the hash index and loads pages into memory.
        let info = store.recover(dir, None)
            .expect("recovery failed");
        println!("Recovered {} index entries", info.num_index_entries);

        let mut session = store.new_session();

        // After recovery with all pages loaded, reads are in-memory.
        let mut out: Option<u64> = None;
        let status = store.read(&mut session, &42, &0u64, &mut out, ());
        assert_eq!(status, OperationStatus::Ok);
        assert_eq!(out, Some(42 * 42));

        // Reads for on-disk records would return Pending — handle both:
        let mut out2: Option<u64> = None;
        let status = store.read(&mut session, &999, &0u64, &mut out2, ());
        match status {
            OperationStatus::Ok => {
                assert_eq!(out2, Some(999 * 999));
            }
            OperationStatus::Pending => {
                // I/O dispatched — wait for it, then retry.
                store.complete_pending_sync(&mut session);
                let mut retry: Option<u64> = None;
                let s = store.read(&mut session, &999, &0u64, &mut retry, ());
                assert_eq!(s, OperationStatus::Ok);
                assert_eq!(retry, Some(999 * 999));
            }
            other => panic!("unexpected: {other:?}"),
        }

        // Can continue writing after recovery.
        let _ = store.upsert(&mut session, &2000, &7, ());

        // Always drain pending ops before disposing the session.
        store.complete_pending(&mut session);
        store.dispose_session(session);
    }

    println!("Data survived a restart!");
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
                // Drain pending before dispose — correct even when there
                // are none (NullDevice never evicts, so no Pending here).
                store.complete_pending(&mut session);
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
| **Hybrid Log** | FASTER's core data structure: recent records live in memory, older records spill to the device. |
| **Pending** | Operations on evicted (on-disk) records return `Pending`. The I/O is dispatched automatically — call `complete_pending` to collect results. |
| **complete_pending** | Drains finished I/O and returns results. `complete_pending_sync` blocks until ALL in-flight I/O finishes. Always call before `dispose_session`. |
| **Checkpoint** | `store.checkpoint(dir, CheckpointType::FoldOver)` persists the hash index and log state to disk. |
| **Recovery** | `store.recover(dir, None)` restores from the latest checkpoint after reopening. |
| **Maintenance** | Call `store.maintenance()` periodically to flush pages and reclaim memory. |

## Next steps

- **Read-Modify-Write (RMW):** Use `CounterFunctions` for atomic counters, or implement the `Functions` trait for custom merge logic.
- **Builder API:** `FasterKv::builder()` provides a fluent interface with validation for production configs.
- **`session_scope`:** Auto-managed sessions that clean up on drop — great for simple use cases:
  ```rust
  store.session_scope(|session, store| {
      store.upsert_simple(session, &1u64, &42u64);
  });
  ```
