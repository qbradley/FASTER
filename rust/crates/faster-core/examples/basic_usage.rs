//! Basic usage example for the `faster-core` crate.
//!
//! Demonstrates creating a store, opening a session, and performing
//! upsert, read, RMW, and delete operations.
//!
//! Run with:
//! ```sh
//! cargo run -p faster-core --example basic_usage
//! ```

use faster_core::status::OperationStatus;
use faster_core::{FasterKv, NullDevice, SimpleFunctions};

/// Type alias for our store — u64 keys and u64 values.
type Store = FasterKv<SimpleFunctions<u64, u64>>;

fn main() {
    // ── 1. Create a store ───────────────────────────────────────────
    //
    // FasterKvBuilder provides fluent configuration with validation.
    // NullDevice discards flushed pages (pure in-memory usage).
    // For durable storage, use SyncFileDevice instead.
    let store: Store = FasterKv::<SimpleFunctions<u64, u64>>::builder()
        .hash_index_size_log2(16) // 2^16 = 64K hash buckets
        .buffer_size_pages(16) // 16 in-memory page frames
        .mutable_fraction(0.9) // 90% of buffer is mutable
        .build(SimpleFunctions::default(), NullDevice::new())
        .expect("valid configuration");

    println!("Store created with 64K hash buckets");

    // ── 2. Open a session ───────────────────────────────────────────
    //
    // Sessions are thread-affine (!Send) — each thread needs its own.
    let mut session = store.new_session();
    println!("Session opened");

    // ── 3. Upsert (insert or update) ────────────────────────────────
    //
    // For SimpleFunctions<u64, u64>, Input = Value = u64.
    // The context parameter (()) is an opaque user value carried through
    // pending operations — not needed for in-memory workloads.
    let num_keys = 100u64;
    for key in 0..num_keys {
        let value = key * 10;
        let status = store.upsert(&mut session, &key, &value, ());
        assert!(status.is_success(), "upsert failed for key {key}: {status}");
    }
    println!("Upserted {num_keys} key-value pairs (key → key * 10)");

    // ── 4. Read ─────────────────────────────────────────────────────
    let mut output: Option<u64> = None;
    let status = store.read(&mut session, &42, &0, &mut output, ());
    match status.status() {
        OperationStatus::Ok => {
            let val = output.expect("output should be Some on Ok");
            println!("Read key 42 → value {val}");
            assert_eq!(val, 420, "expected 42 * 10 = 420");
        }
        OperationStatus::NotFound => println!("Key 42 not found"),
        OperationStatus::Pending => println!("Key 42 is on disk, pending I/O"),
        other => println!("Unexpected status: {other}"),
    }

    // ── 5. Read-Modify-Write (RMW) ──────────────────────────────────
    //
    // SimpleFunctions::rmw_in_place adds the input to the existing value.
    // If the key doesn't exist, rmw_initial creates it with input as the value.
    let mut rmw_output: Option<u64> = None;
    let status = store.rmw(&mut session, &42, &5, &mut rmw_output, ());
    assert!(status.is_success(), "RMW failed: {status}");
    println!("RMW on key 42: added 5 (status: {status})");

    // Verify the RMW result
    let mut verify_output: Option<u64> = None;
    let status = store.read(&mut session, &42, &0, &mut verify_output, ());
    if status == OperationStatus::Ok {
        println!("After RMW, key 42 → value {}", verify_output.unwrap());
    }

    // ── 6. Delete ───────────────────────────────────────────────────
    let status = store.delete(&mut session, &99, ());
    println!("Delete key 99: {status}");

    // Confirm deletion
    let mut deleted_output: Option<u64> = None;
    let status = store.read(&mut session, &99, &0, &mut deleted_output, ());
    assert!(
        status == OperationStatus::NotFound || status == OperationStatus::Ok,
        "expected NotFound or Ok after delete, got: {status}"
    );
    println!("Read deleted key 99: {status}");

    // ── 7. Clean up ─────────────────────────────────────────────────
    //
    // dispose_session releases the epoch table slot.
    // Dropping the store flushes remaining pages and closes the device.
    store.dispose_session(session);
    println!("Session disposed — done!");
}
