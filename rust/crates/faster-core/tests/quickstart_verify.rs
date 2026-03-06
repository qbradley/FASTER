//! Tests that verify the QUICKSTART.md code examples compile and pass.
//!
//! Each test is a direct transcription of its QUICKSTART.md example
//! (adapted from `fn main()` to `#[test]`, using tempdir instead of
//! `/tmp`). If the QUICKSTART changes, these tests must be updated to
//! match — and vice versa.

// The QUICKSTART examples intentionally ignore OperationStatus returns
// for readability. Suppress warnings here since these are verbatim copies.
#![allow(unused_must_use)]

// ═══════════════════════════════════════════════════════════════════
// Example 1: Hello, FASTER
// ═══════════════════════════════════════════════════════════════════

#[test]
fn quickstart_example_1_hello_faster() {
    use faster_core::NullDevice;
    use faster_core::store::{FasterKv, FasterKvConfig, SimpleFunctions};

    // Create a store with default config and a null device (in-memory only).
    let store: FasterKv<SimpleFunctions<u64, u64>> = FasterKv::new(
        FasterKvConfig::default(),
        SimpleFunctions::default(),
        NullDevice::new(),
    );

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
}

// ═══════════════════════════════════════════════════════════════════
// Example 2: Persistent storage
// ═══════════════════════════════════════════════════════════════════

#[test]
fn quickstart_example_2_persistent_storage() {
    use faster_core::SyncFileDevice;
    use faster_core::checkpoint::CheckpointType;
    use faster_core::store::{FasterKv, FasterKvConfig, SimpleFunctions};
    use std::path::Path;

    type Store = FasterKv<SimpleFunctions<u64, u64>>;

    /// Open (or create) a FASTER store backed by disk files in `dir`.
    fn open_store(dir: &Path) -> Store {
        let device = SyncFileDevice::new(
            dir,
            "log.",             // log file prefix ("log.0", "log.1", …)
            512,                // sector size
            1024 * 1024 * 1024, // 1 GiB segment size
            4,                  // background I/O threads
        )
        .expect("failed to open device");

        FasterKv::new(
            FasterKvConfig::default(),
            SimpleFunctions::default(),
            device,
        )
    }

    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path();

    // ── Phase 1: Write some data and take a checkpoint ──────────────
    {
        let store = open_store(dir);
        let mut session = store.new_session();

        for i in 0..1000u64 {
            store.upsert_simple(&mut session, &i, &(i * i));
        }

        store.dispose_session(session);

        // Checkpoint persists the hash index + log metadata to disk.
        store
            .checkpoint(dir, CheckpointType::FoldOver)
            .expect("checkpoint failed");

        // `store` is dropped here — the device is closed cleanly.
    }

    // ── Phase 2: Reopen and recover ─────────────────────────────────
    {
        let mut store = open_store(dir);

        // Recover restores the hash index and loads pages into memory.
        let info = store.recover(dir, None).expect("recovery failed");
        println!("Recovered {} index entries", info.num_index_entries);

        let mut session = store.new_session();

        // Data written in Phase 1 is still here.
        assert_eq!(store.read_simple(&mut session, &42), Some(42 * 42));
        assert_eq!(store.read_simple(&mut session, &999), Some(999 * 999));

        // Can continue writing after recovery.
        store.upsert_simple(&mut session, &2000, &7);
        assert_eq!(store.read_simple(&mut session, &2000), Some(7));

        store.dispose_session(session);
    }
}

// ═══════════════════════════════════════════════════════════════════
// Example 3: High-throughput concurrent upserts
// ═══════════════════════════════════════════════════════════════════

#[test]
fn quickstart_example_3_concurrent_upserts() {
    use faster_core::NullDevice;
    use faster_core::store::{FasterKv, FasterKvConfig, SimpleFunctions};
    use std::sync::Arc;
    use std::thread;

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
