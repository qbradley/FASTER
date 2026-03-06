//! Tests that verify the QUICKSTART.md code examples compile and pass.
//!
//! Each test is a direct transcription of its QUICKSTART.md example
//! (adapted from `fn main()` to `#[test]`, using tempdir instead of
//! `/tmp`). If the QUICKSTART changes, these tests must be updated to
//! match — and vice versa.

// ═══════════════════════════════════════════════════════════════════
// Example 1: Hello, FASTER
// ═══════════════════════════════════════════════════════════════════

#[test]
#[allow(unused_must_use)]
fn quickstart_example_1_hello_faster() {
    use faster_core::NullDevice;
    use faster_core::store::{FasterKv, FasterKvConfig, SimpleFunctions};

    let store: FasterKv<SimpleFunctions<u64, u64>> = FasterKv::new(
        FasterKvConfig::default(),
        SimpleFunctions::default(),
        NullDevice::new(),
    );

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
}

// ═══════════════════════════════════════════════════════════════════
// Example 2: Persistent storage
// ═══════════════════════════════════════════════════════════════════

#[test]
fn quickstart_example_2_persistent_storage() {
    use faster_core::SyncFileDevice;
    use faster_core::checkpoint::CheckpointType;
    use faster_core::status::OperationStatus;
    use faster_core::store::{FasterKv, FasterKvConfig, SimpleFunctions};
    use std::path::Path;

    type Store = FasterKv<SimpleFunctions<u64, u64>>;

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

    // ── Phase 1: Write data and take a checkpoint ───────────────
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
        store
            .checkpoint(dir, CheckpointType::FoldOver)
            .expect("checkpoint failed");
    }

    // ── Phase 2: Reopen and recover ─────────────────────────────────
    {
        let mut store = open_store(dir);

        // Recover restores the hash index and loads pages into memory.
        let info = store.recover(dir, None).expect("recovery failed");
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
}

// ═══════════════════════════════════════════════════════════════════
// Example 3: High-throughput concurrent upserts
// ═══════════════════════════════════════════════════════════════════

#[test]
#[allow(unused_must_use)]
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
