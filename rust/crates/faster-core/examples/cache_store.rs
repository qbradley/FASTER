//! Cache-store sample — demonstrates FASTER as a disk-backed cache/KV store.
//!
//! This is the Rust equivalent of the C# `cs/samples/CacheStore` sample.
//! It shows how to:
//!   1. Create a [`SyncFileDevice`]-backed store with 1M key-value pairs
//!   2. Populate the store and report throughput
//!   3. Run a random-read workload that correctly handles [`OperationStatus::Pending`]
//!      (on-disk reads) by calling [`complete_pending`] periodically
//!   4. Optionally run an interactive read mode for per-key latency exploration
//!
//! # Running
//!
//! ```sh
//! # In-memory reads (data stays in buffer pool — fast, zero pending)
//! cargo run -p faster-core --example cache_store
//!
//! # Force data to disk first (triggers on-disk reads — many pending)
//! cargo run -p faster-core --example cache_store -- --evict
//!
//! # Interactive mode — type key IDs and see per-read latency
//! cargo run -p faster-core --example cache_store -- --interactive
//!
//! # Combine: evict + interactive
//! cargo run -p faster-core --example cache_store -- --evict --interactive
//! ```

use std::io::{self, BufRead, Write};
use std::time::Instant;

use faster_core::status::OperationStatus;
use faster_core::{FasterKv, SimpleFunctions, SyncFileDevice};

/// Type alias for our store — u64 keys and u64 values.
type Store = FasterKv<SimpleFunctions<u64, u64>>;

const NUM_KEYS: u64 = 1_000_000;
const PROGRESS_MASK: u64 = (1 << 19) - 1; // report every 524,288 keys
const PENDING_DRAIN_INTERVAL: u64 = 100;

// ─── Simple xorshift64 PRNG (no external dependency) ─────────────────────

struct Xorshift64(u64);

impl Xorshift64 {
    fn new(seed: u64) -> Self {
        Self(if seed == 0 { 0xDEAD_BEEF_CAFE } else { seed })
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let do_evict = args.iter().any(|a| a == "--evict");
    let interactive = args.iter().any(|a| a == "--interactive");

    // ── 1. Create a temp directory and SyncFileDevice-backed store ────────

    let tmp_dir = std::env::temp_dir().join("faster_cache_store_sample");
    if tmp_dir.exists() {
        let _ = std::fs::remove_dir_all(&tmp_dir);
    }
    std::fs::create_dir_all(&tmp_dir).expect("create temp dir");
    println!("Store path: {}", tmp_dir.display());

    let device = SyncFileDevice::new(
        &tmp_dir,
        "hlog.", // segment file prefix
        512,     // sector size
        1 << 30, // 1 GiB segment size
        4,       // I/O threads
    )
    .expect("create SyncFileDevice");

    let store: Store = FasterKv::<SimpleFunctions<u64, u64>>::builder()
        .hash_index_size_log2(20) // 2^20 ≈ 1M hash buckets
        .buffer_size_pages(16)
        .mutable_fraction(0.9)
        .build(SimpleFunctions::default(), device)
        .expect("valid configuration");

    // ── 2. Populate store ────────────────────────────────────────────────

    println!("\n--- Populating store with {NUM_KEYS} keys ---");
    let mut session = store.new_session();
    let pop_start = Instant::now();

    for i in 0..NUM_KEYS {
        let status = store.upsert(&mut session, &i, &i, ());
        assert!(status.is_success(), "upsert failed for key {i}: {status}");

        if i & PROGRESS_MASK == 0 && i > 0 {
            let elapsed = pop_start.elapsed().as_secs_f64();
            println!("  inserted {i} keys ({elapsed:.2}s)");
        }
    }

    let pop_elapsed = pop_start.elapsed().as_secs_f64();
    let pop_rate = NUM_KEYS as f64 / pop_elapsed;
    println!(
        "Total time to upsert {NUM_KEYS} elements: {pop_elapsed:.2}s ({pop_rate:.0} inserts/sec)"
    );

    // ── 3. Optionally flush + evict to force data to disk ────────────────

    if do_evict {
        println!("\n--- Flushing and evicting (--evict) ---");
        let evict_start = Instant::now();
        let (flushed, evicted) = store.flush_and_evict();
        println!(
            "Flush + evict completed in {:.2}s — {flushed} pages flushed, {evicted} evicted — reads will hit disk",
            evict_start.elapsed().as_secs_f64()
        );
    } else {
        println!("\nData stays in buffer pool (pass --evict to force on-disk reads)");
    }

    // ── 4. Read workload ─────────────────────────────────────────────────

    if interactive {
        interactive_read_workload(&store, &mut session);
    } else {
        random_read_workload(&store, &mut session);
    }

    // ── 5. Clean up ──────────────────────────────────────────────────────

    store.complete_pending(&mut session);
    store.dispose_session(session);
    drop(store);

    let _ = std::fs::remove_dir_all(&tmp_dir);
    println!("\nCleaned up temp directory. Done!");
}

// ─── Random-read workload ────────────────────────────────────────────────

fn random_read_workload(
    store: &Store,
    session: &mut faster_core::store::FasterSession<SimpleFunctions<u64, u64>>,
) {
    println!("\n--- Random read workload ({NUM_KEYS} reads) ---");
    let mut rng = Xorshift64::new(42);
    let mut output: Option<u64>;
    let mut pending_count: u64 = 0;
    let mut found_count: u64 = 0;
    let mut not_found_count: u64 = 0;
    let mut completed_from_pending: u64 = 0;

    let read_start = Instant::now();

    for i in 0..NUM_KEYS {
        let key = rng.next_u64() % NUM_KEYS;
        output = None;

        let status = store.read(session, &key, &0u64, &mut output, ()).status();
        match status {
            OperationStatus::Ok => {
                let val = output.expect("output should be Some on Ok");
                assert_eq!(val, key, "value mismatch for key {key}");
                found_count += 1;
            }
            OperationStatus::Pending => {
                pending_count += 1;
                // Drain completed async I/Os periodically
                if pending_count % PENDING_DRAIN_INTERVAL == 0 {
                    let results = store.complete_pending(session);
                    completed_from_pending += results.len() as u64;
                }
            }
            OperationStatus::NotFound => {
                not_found_count += 1;
            }
            other => {
                eprintln!("Unexpected status on read {i}, key {key}: {other}");
            }
        }
    }

    // Drain all remaining pending operations
    let final_results = store.complete_pending_sync(session);
    completed_from_pending += final_results.len() as u64;

    let read_elapsed = read_start.elapsed().as_secs_f64();
    let read_rate = NUM_KEYS as f64 / read_elapsed;

    println!("Total time to read {NUM_KEYS} keys: {read_elapsed:.2}s ({read_rate:.0} reads/sec)");
    println!("  Completed synchronously (in-memory): {found_count}");
    println!("  Completed with PENDING (on-disk I/O): {pending_count}");
    println!("  Async completions drained: {completed_from_pending}");
    println!("  Not found: {not_found_count}");
}

// ─── Interactive-read workload ───────────────────────────────────────────

fn interactive_read_workload(
    store: &Store,
    session: &mut faster_core::store::FasterSession<SimpleFunctions<u64, u64>>,
) {
    println!("\n--- Interactive read mode ---");
    println!("Enter a key ID (0–{}), or 'q' to quit:", NUM_KEYS - 1);

    let stdin = io::stdin();
    let mut stdout = io::stdout();

    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l.trim().to_string(),
            Err(_) => break,
        };
        if line.eq_ignore_ascii_case("q") || line == "-1" {
            break;
        }
        let key: u64 = match line.parse() {
            Ok(k) => k,
            Err(_) => {
                println!("  Invalid input. Enter a number or 'q' to quit.");
                continue;
            }
        };

        let mut output: Option<u64> = None;
        let op_start = Instant::now();
        let status = store.read(session, &key, &0u64, &mut output, ()).status();

        match status {
            OperationStatus::Ok => {
                let latency = op_start.elapsed();
                let val = output.unwrap();
                println!(
                    "  Sync: key {key} → value {val}, latency = {:.3}ms",
                    latency.as_secs_f64() * 1000.0
                );
            }
            OperationStatus::Pending => {
                // Wait for the async I/O to complete
                let results = store.complete_pending_sync(session);
                let latency = op_start.elapsed();
                if let Some((Some(val), _)) = results.first() {
                    println!(
                        "  Async: key {key} → value {val}, latency = {:.3}ms",
                        latency.as_secs_f64() * 1000.0
                    );
                } else {
                    println!(
                        "  Async: key {key} → completed (no output), latency = {:.3}ms",
                        latency.as_secs_f64() * 1000.0
                    );
                }
            }
            OperationStatus::NotFound => {
                let latency = op_start.elapsed();
                println!(
                    "  NotFound: key {key}, latency = {:.3}ms",
                    latency.as_secs_f64() * 1000.0
                );
            }
            other => {
                println!("  Unexpected status for key {key}: {other}");
            }
        }

        print!("> ");
        let _ = stdout.flush();
    }
}
