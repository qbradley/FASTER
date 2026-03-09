//! Async/Tokio event counter (ad click aggregator) using FASTER.
//!
//! # Overview
//!
//! This sample demonstrates how to use FASTER with Tokio for a realistic
//! real-time event aggregation workload. Multiple async worker tasks
//! (running on Tokio's blocking pool) generate simulated ad events and
//! aggregate them into per-campaign statistics using FASTER's RMW
//! (Read-Modify-Write) operation.
//!
//! # Architecture: The Async/Sync Bridge
//!
//! ```text
//!                    Tokio Async Runtime
//!     ┌──────────────────────────────────────────────┐
//!     │                                              │
//!     │   ┌─────────┐  ┌──────────┐  ┌───────────┐  │
//!     │   │ Stats   │  │ Shutdown │  │ Checkpoint│  │
//!     │   │ Reporter│  │ Handler  │  │ Task      │  │  ← async tasks
//!     │   │ (spawn) │  │ (spawn)  │  │ (spawn)   │  │    (timers, signals,
//!     │   └─────────┘  └──────────┘  └───────────┘  │     coordination)
//!     │                                              │
//!     │   ┌──────────────────────────────────────┐   │
//!     │   │         Tokio Blocking Pool           │   │
//!     │   │  ┌────────┐ ┌────────┐ ┌────────┐    │   │
//!     │   │  │Worker 0│ │Worker 1│ │Worker N│    │   │  ← spawn_blocking
//!     │   │  │Session │ │Session │ │Session │    │   │    (one OS thread
//!     │   │  │  !Send │ │  !Send │ │  !Send │    │   │     per worker)
//!     │   │  └───┬────┘ └───┬────┘ └───┬────┘    │   │
//!     │   └──────┼──────────┼──────────┼─────────┘   │
//!     └──────────┼──────────┼──────────┼─────────────┘
//!                │          │          │
//!         ┌──────▼──────────▼──────────▼──────┐
//!         │          FasterKv (Arc)            │
//!         │   ┌──────────┐  ┌──────────────┐  │
//!         │   │Hash Index│  │ Hybrid Log   │  │
//!         │   └──────────┘  │ (memory+disk)│  │
//!         │                 └──────────────┘  │
//!         │   Device: TokioFileDevice         │
//!         └───────────────────────────────────┘
//! ```
//!
//! # Five-Phase Workflow
//!
//! 1. **SETUP:** Create store with `TokioFileDevice` + `AsyncFasterKv`
//! 2. **INGEST:** Workers generate events via `spawn_blocking` + RMW
//! 3. **QUERY + VERIFY:** Read all campaigns, check counts match
//! 4. **CHECKPOINT + RECOVER:** Persist state, simulate crash, recover
//! 5. **REPORT:** Final summary with pass/fail verdict
//!
//! # KEY DIFFERENCES from Thread Version
//!
//! | Aspect | Thread Version | Tokio Version |
//! |--------|---------------|---------------|
//! | Workers | `std::thread::spawn` | `tokio::task::spawn_blocking` |
//! | Device | `SyncFileDevice` | `TokioFileDevice` |
//! | Maintenance | Workers call `maintenance()` | `AsyncFasterKv` background task |
//! | Shutdown | `Arc<AtomicBool>` | `Arc<AtomicBool>` (same — simple works) |
//! | Stats | Periodic thread with `thread::sleep` | Async task with `tokio::time::interval` |
//! | Checkpoint | Inline call | `spawn_blocking` wrapper |
//!
//! # When Does Tokio Actually Help?
//!
//! For CPU-bound FASTER operations, `spawn_blocking` gives you the same
//! performance as raw threads. The Tokio advantage comes from:
//!
//! 1. **TokioFileDevice:** Disk I/O goes through Tokio's thread pool
//!    instead of dedicated threads, sharing resources with the rest of
//!    your async application.
//! 2. **Control plane:** Stats, timers, shutdown, and periodic tasks
//!    are natural async tasks — no manual thread management.
//! 3. **Integration:** If your application is already async (web server,
//!    gRPC service, etc.), FASTER fits in without a separate thread pool.

mod checkpoint;
mod functions;
mod verify;
mod workload;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use clap::Parser;
use faster_core::checkpoint::CheckpointType;
use faster_core::grow::GrowConfig;
use faster_core::hybrid_log::eviction::EvictionPolicy;
use faster_core::store::FasterKvConfig;
use faster_tokio::TokioFileDevice;

use crate::functions::CampaignFunctions;
use crate::workload::LiveCounters;

// ── CLI ─────────────────────────────────────────────────────────────────

/// Async/Tokio event counter (ad click aggregator) using FASTER.
///
/// Simulates an advertising platform where multiple workers generate
/// click/impression/spend events. Demonstrates RMW, checkpoint/recovery,
/// delete, and correctness verification — all orchestrated by Tokio.
#[derive(Parser, Debug)]
#[command(name = "event-counter-tokio")]
struct Args {
    /// Number of ad campaigns (key space size).
    #[arg(long, default_value_t = 1000)]
    campaigns: usize,

    /// Events per worker thread (0 = run until duration/Ctrl+C).
    #[arg(long, default_value_t = 100_000)]
    events_per_thread: u64,

    /// Number of worker threads.
    #[arg(long, default_value_t = default_threads())]
    threads: usize,

    /// Storage directory for FASTER data and checkpoints.
    #[arg(long, default_value = "./event-counter-tokio-data")]
    data_dir: PathBuf,

    /// Enable post-run correctness verification.
    #[arg(long)]
    verify: bool,

    /// Run duration limit in seconds (0 = unlimited).
    #[arg(long, default_value_t = 0)]
    duration: u64,

    /// Percentage of operations that are reads (0-100).
    #[arg(long, default_value_t = 10)]
    read_pct: u8,

    /// Take a checkpoint after ingestion and verify recovery.
    #[arg(long)]
    checkpoint: bool,

    /// Delete a fraction of campaigns after ingestion (0-100).
    /// Used to exercise the delete + read path.
    #[arg(long, default_value_t = 10)]
    delete_pct: u8,
}

fn default_threads() -> usize {
    thread::available_parallelism()
        .map(|n| n.get().min(4))
        .unwrap_or(2)
}

// ── FASTER Page Size ────────────────────────────────────────────────────

/// FASTER's internal page size: 32 MiB.
///
/// This must match the page size used by `faster_core`. All memory
/// accounting is in units of these pages.
const FASTER_PAGE_SIZE: usize = 1 << 25;

// ── Async Stats Reporter ────────────────────────────────────────────────

/// Periodic progress reporter running as a Tokio async task.
///
/// # Why Async?
///
/// The stats reporter does no FASTER operations — it just reads atomic
/// counters and prints them. This is a perfect fit for a lightweight
/// async task: it sleeps between reports using `tokio::time::interval`
/// (which is zero-cost when idle) and never blocks.
///
/// In the thread version, this would be a dedicated OS thread sleeping
/// in a loop — wasteful for such a simple job.
async fn stats_reporter(counters: Arc<LiveCounters>, shutdown: Arc<AtomicBool>, start: Instant) {
    let mut ticker = tokio::time::interval(Duration::from_secs(1));
    ticker.tick().await; // skip first immediate tick

    let mut prev_ops: u64 = 0;
    let mut prev_time = start;

    while !shutdown.load(Ordering::Relaxed) {
        ticker.tick().await;
        if shutdown.load(Ordering::Relaxed) {
            break;
        }

        let now = Instant::now();
        let total = counters.total_ops.load(Ordering::Relaxed);
        let rmw = counters.rmw_ops.load(Ordering::Relaxed);
        let reads = counters.read_ops.load(Ordering::Relaxed);
        let dt = now.duration_since(prev_time).as_secs_f64();
        let elapsed = now.duration_since(start).as_secs_f64();

        if dt < 0.001 {
            prev_ops = total;
            prev_time = now;
            continue;
        }

        let inst_ops = (total - prev_ops) as f64 / dt;
        let ops_label = if inst_ops >= 1_000_000.0 {
            format!("{:.2}M ops/s", inst_ops / 1_000_000.0)
        } else if inst_ops >= 1_000.0 {
            format!("{:.1}K ops/s", inst_ops / 1_000.0)
        } else {
            format!("{:.0} ops/s", inst_ops)
        };

        eprintln!(
            "  [{elapsed:>6.1}s] total={total:<12} rmw={rmw:<12} reads={reads:<10} {ops_label}"
        );

        prev_ops = total;
        prev_time = now;
    }
}

// ── Main ────────────────────────────────────────────────────────────────

/// Entry point: the `#[tokio::main]` macro sets up a multi-threaded Tokio
/// runtime before calling this function.
///
/// # Why `#[tokio::main]` Instead of Manual Runtime?
///
/// For a sample, `#[tokio::main]` is simpler and idiomatic. In production,
/// you might build the runtime manually to control thread counts and names.
/// The choice doesn't affect FASTER integration at all.
#[tokio::main]
async fn main() {
    let args = Args::parse();

    // ── Validate ────────────────────────────────────────────────────
    assert!(args.campaigns > 0, "campaigns must be > 0");
    assert!(args.threads > 0, "threads must be > 0");
    assert!(args.read_pct <= 100, "read-pct must be 0..=100");
    assert!(args.delete_pct <= 100, "delete-pct must be 0..=100");

    // ── Print Configuration ─────────────────────────────────────────
    eprintln!("╔══════════════════════════════════════════════════╗");
    eprintln!("║  Event Counter (Tokio) — Ad Click Aggregator    ║");
    eprintln!("╚══════════════════════════════════════════════════╝");
    eprintln!();
    eprintln!("  runtime:          Tokio (async)");
    eprintln!("  campaigns:        {}", args.campaigns);
    eprintln!(
        "  events/thread:    {}",
        if args.events_per_thread > 0 {
            format!("{}", args.events_per_thread)
        } else {
            "unlimited".to_string()
        }
    );
    eprintln!("  threads:          {}", args.threads);
    eprintln!("  read %:           {}%", args.read_pct);
    eprintln!("  delete %:         {}%", args.delete_pct);
    eprintln!("  verify:           {}", args.verify);
    eprintln!("  checkpoint:       {}", args.checkpoint);
    eprintln!("  data dir:         {}", args.data_dir.display());
    if args.duration > 0 {
        eprintln!("  duration:         {}s", args.duration);
    }
    eprintln!();

    // ════════════════════════════════════════════════════════════════
    // PHASE 1: SETUP
    // ════════════════════════════════════════════════════════════════
    //
    // Create the FASTER store with TokioFileDevice. This is the first
    // key difference from the thread version: we use TokioFileDevice
    // instead of SyncFileDevice. Under the hood, TokioFileDevice
    // dispatches disk reads/writes through Tokio's blocking pool
    // instead of maintaining its own I/O thread pool.
    //
    // AsyncFasterKv wraps FasterKv with a background maintenance task
    // that periodically calls maintenance() on a spawn_blocking thread.
    // This frees individual workers from having to call maintenance().

    eprintln!("═══ Phase 1: SETUP ═══");

    // Clean up any previous run data
    if args.data_dir.exists() {
        std::fs::remove_dir_all(&args.data_dir).ok();
    }
    std::fs::create_dir_all(&args.data_dir).expect("failed to create data directory");

    // Use the data directory as the checkpoint base. Recovery expects
    // log segment files (log.0, log.1, ...) in the same directory as
    // the checkpoint metadata. Using a single directory for both keeps
    // the layout simple and recovery straightforward.
    let checkpoint_dir = args.data_dir.clone();

    // Size the hash index to ~2x the key space for low collision rate.
    let hash_index_log2 = ((args.campaigns as f64 * 2.0).log2().ceil() as usize).max(10);
    // Use enough pages to hold the working set in memory.
    let buffer_size_pages = 16usize.next_power_of_two();

    let config = FasterKvConfig {
        hash_index_size_log2: hash_index_log2,
        buffer_size_pages,
        mutable_fraction: 0.9,
        sector_size: 512,
        eviction_policy: EvictionPolicy::default(),
        grow_config: GrowConfig::default(),
        auto_compact: false,
    };

    // ── TokioFileDevice ─────────────────────────────────────────────
    //
    // KEY DIFFERENCE: TokioFileDevice vs SyncFileDevice
    //
    // SyncFileDevice (thread version): Spawns a fixed-size thread pool
    // for disk I/O. Each thread blocks on file read/write calls.
    //
    // TokioFileDevice (this version): Uses Tokio's spawn_blocking for
    // disk I/O. No separate thread pool — I/O shares Tokio's blocking
    // pool with your application's other blocking work.
    //
    // When to prefer TokioFileDevice:
    // - Your app already runs on Tokio (web server, gRPC, etc.)
    // - You want unified resource management (one thread pool, not two)
    // - You're running in a container with limited CPU cores
    //
    // When to prefer SyncFileDevice:
    // - Maximum I/O throughput with dedicated threads
    // - No async runtime in your application
    // - You want FASTER to own its I/O threads completely

    let device = TokioFileDevice::new(&args.data_dir, "log.", 512, 1u64 << 30)
        .expect("failed to create TokioFileDevice");

    // AsyncFasterKv wraps FasterKv and runs background maintenance
    // (epoch draining, page flushing, compaction) as an async task.
    let async_kv = faster_tokio::AsyncFasterKv::new(
        config.clone(),
        CampaignFunctions,
        device,
        Duration::from_millis(50),
    );
    let store = async_kv.store_arc();

    eprintln!(
        "  ✓ FASTER store created (hash_index=2^{hash_index_log2}, pages={buffer_size_pages})"
    );
    eprintln!();

    // ════════════════════════════════════════════════════════════════
    // PHASE 2: INGEST — Generate Events
    // ════════════════════════════════════════════════════════════════
    //
    // Spawn workers on Tokio's blocking pool. Each worker:
    // 1. Creates its own FasterSession (!Send, thread-local)
    // 2. Generates random events for random campaigns
    // 3. Applies them via FASTER's RMW operation
    // 4. Returns per-worker stats for verification
    //
    // KEY PATTERN: spawn_blocking returns a JoinHandle<T> that is
    // a Future — we can .await it from the async world to get the
    // worker's result. This is the bridge between sync FASTER ops
    // and async orchestration.

    eprintln!("═══ Phase 2: INGEST ═══");

    let shutdown = Arc::new(AtomicBool::new(false));
    let counters = Arc::new(LiveCounters::new());
    let start = Instant::now();

    // Start the async stats reporter
    let stats_handle = {
        let counters = Arc::clone(&counters);
        let shutdown = Arc::clone(&shutdown);
        tokio::spawn(stats_reporter(counters, shutdown, start))
    };

    // Duration timer (if configured)
    if args.duration > 0 {
        let shutdown_timer = Arc::clone(&shutdown);
        let duration_secs = args.duration;
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(duration_secs)).await;
            eprintln!("\n  Duration limit reached ({duration_secs}s). Shutting down...");
            shutdown_timer.store(true, Ordering::SeqCst);
        });
    }

    // Ctrl+C handler
    let shutdown_ctrlc = Arc::clone(&shutdown);
    tokio::spawn(async move {
        tokio::signal::ctrl_c().await.ok();
        eprintln!("\n  Ctrl+C received. Shutting down...");
        shutdown_ctrlc.store(true, Ordering::SeqCst);
    });

    // ── Spawn Workers ───────────────────────────────────────────────
    //
    // Each worker runs in spawn_blocking — this gives it a dedicated
    // OS thread from Tokio's blocking pool. The worker function is
    // purely synchronous; it creates a FasterSession that's bound to
    // that thread for its entire lifetime.
    //
    // The JoinHandle<WorkerStats> lets us await the result asynchronously.
    // When the worker finishes, we get its per-campaign event counts
    // back for verification.

    let mut worker_handles = Vec::with_capacity(args.threads);
    for wid in 0..args.threads {
        let store = Arc::clone(&store);
        let shutdown = Arc::clone(&shutdown);
        let counters = Arc::clone(&counters);
        let events_per_thread = args.events_per_thread;
        let read_pct = args.read_pct;

        // Partition the campaign key space evenly among workers.
        // Each worker gets a disjoint range of campaign IDs to ensure
        // no two workers do concurrent RMW on the same key, which
        // avoids known in-place update races in the current core.
        let campaigns_per_worker = args.campaigns / args.threads;
        let start_campaign = wid * campaigns_per_worker;
        let campaign_count = if wid == args.threads - 1 {
            args.campaigns - start_campaign // last worker takes remainder
        } else {
            campaigns_per_worker
        };

        worker_handles.push(tokio::task::spawn_blocking(move || {
            workload::run_worker(
                store,
                shutdown,
                counters,
                wid,
                start_campaign,
                campaign_count,
                events_per_thread,
                read_pct,
            )
        }));
    }

    // ── Await All Workers ───────────────────────────────────────────
    //
    // This is idiomatic Tokio: we await each JoinHandle, which resolves
    // when the spawn_blocking task completes. The main async function
    // is free to do other work while waiting (though in this case we
    // just collect results).

    let mut all_worker_stats = Vec::with_capacity(args.threads);
    for handle in worker_handles {
        match handle.await {
            Ok(stats) => all_worker_stats.push(stats),
            Err(e) => eprintln!("  worker panicked: {e}"),
        }
    }

    // Stop the stats reporter
    shutdown.store(true, Ordering::SeqCst);
    let _ = stats_handle.await;

    let ingest_elapsed = start.elapsed();
    let total_ops = counters.total_ops.load(Ordering::Relaxed);
    let total_rmw = counters.rmw_ops.load(Ordering::Relaxed);
    let total_reads = counters.read_ops.load(Ordering::Relaxed);

    eprintln!();
    eprintln!(
        "  ✓ Ingestion complete: {} ops in {:.2}s ({:.0} ops/s)",
        total_ops,
        ingest_elapsed.as_secs_f64(),
        total_ops as f64 / ingest_elapsed.as_secs_f64().max(0.001)
    );
    eprintln!("    RMW: {total_rmw}  Reads: {total_reads}");
    eprintln!();

    // ════════════════════════════════════════════════════════════════
    // PHASE 3: QUERY + DELETE + VERIFY
    // ════════════════════════════════════════════════════════════════
    //
    // First verify the ingested data (if --verify), then delete some
    // campaigns to exercise the delete path.

    if args.verify {
        eprintln!("═══ Phase 3: VERIFY (pre-checkpoint) ═══");

        // Run a maintenance cycle and allow epoch to fully drain before
        // verification. This ensures all pending operations from workers
        // have been flushed and are visible to the verifier.
        {
            let store_m = Arc::clone(&store);
            tokio::task::spawn_blocking(move || {
                store_m.maintenance();
                // A brief pause allows the epoch system to fully drain
                // any in-flight operations from the recently-disposed
                // worker sessions.
                std::thread::sleep(Duration::from_millis(100));
                store_m.maintenance();
            })
            .await
            .ok();
        }

        let (exp_clicks, exp_impressions, exp_spend) =
            verify::aggregate_expected(&all_worker_stats, args.campaigns);

        // Run verification in spawn_blocking (needs a FasterSession)
        let store_v = Arc::clone(&store);
        let num_campaigns = args.campaigns;
        let pre_ok = tokio::task::spawn_blocking(move || {
            verify::verify_store(
                &store_v,
                &exp_clicks,
                &exp_impressions,
                &exp_spend,
                num_campaigns,
                "pre-checkpoint",
            )
        })
        .await
        .unwrap_or(false);

        if !pre_ok {
            eprintln!("  ✗ Pre-checkpoint verification FAILED");
            std::process::exit(1);
        }
        eprintln!();
    }

    // ── Delete Phase ────────────────────────────────────────────────
    //
    // Delete a fraction of campaigns to exercise the delete path.
    // Deleted campaigns should return NotFound on subsequent reads.
    //
    // We track which campaigns were deleted so the post-recovery
    // verification can account for them (deleted campaigns won't be
    // in the recovered store if the checkpoint happens after deletion).

    let num_to_delete = (args.campaigns as u64 * args.delete_pct as u64) / 100;
    let mut deleted_campaigns = Vec::new();

    if num_to_delete > 0 {
        eprintln!("═══ Phase 3b: DELETE ({num_to_delete} campaigns) ═══");

        let store_d = Arc::clone(&store);
        let delete_count = num_to_delete;

        deleted_campaigns = tokio::task::spawn_blocking(move || {
            let mut session = store_d.new_session();
            let mut deleted = Vec::new();

            for campaign_id in 0..delete_count {
                let status = store_d.delete(&mut session, &campaign_id, ());
                if status.is_success() || status == faster_core::status::OperationStatus::Pending {
                    deleted.push(campaign_id);
                }
                if campaign_id % 256 == 0 {
                    store_d.refresh(&mut session);
                }
            }

            store_d.dispose_session(session);
            deleted
        })
        .await
        .unwrap_or_default();

        eprintln!("  ✓ Deleted {} campaigns", deleted_campaigns.len());
        eprintln!();
    }

    // ════════════════════════════════════════════════════════════════
    // PHASE 4: CHECKPOINT + CRASH + RECOVER
    // ════════════════════════════════════════════════════════════════
    //
    // This phase simulates a crash-recovery cycle:
    // 1. Take a checkpoint (persist current state to disk)
    // 2. Drop the store (simulates a crash)
    // 3. Create a fresh store and recover from the checkpoint
    // 4. Verify the recovered data matches expectations
    //
    // # KEY ASYNC CONSIDERATION:
    //
    // Recovery requires &mut FasterKv, which is incompatible with Arc.
    // We must drop the AsyncFasterKv (which drops the Arc), create a
    // fresh FasterKv, recover into it, and re-wrap in AsyncFasterKv.
    //
    // This exactly mirrors what happens after a real crash: your process
    // restarts, constructs a new store, and recovers.

    if args.checkpoint {
        eprintln!("═══ Phase 4: CHECKPOINT + RECOVER ═══");

        // Step 1: Take checkpoint
        let token = checkpoint::take_checkpoint(
            Arc::clone(&store),
            &checkpoint_dir,
            CheckpointType::FoldOver,
        )
        .await;

        let token = match token {
            Some(t) => t,
            None => {
                eprintln!("  ✗ Checkpoint failed, skipping recovery test");
                eprintln!();
                // Skip to report
                print_final_report(total_ops, total_rmw, total_reads, ingest_elapsed, false);
                return;
            }
        };

        // Step 2: Drop the store (simulates crash)
        //
        // We must drop AsyncFasterKv first (it holds an Arc to the store
        // and a background maintenance task). Then the Arc<FasterKv> in
        // `store` is the last reference, and dropping it releases all
        // in-memory state.
        //
        // IMPORTANT: Drop order matters! AsyncFasterKv's Drop impl
        // signals the maintenance task to stop. If we dropped the store
        // Arc first, the maintenance task would panic accessing freed data.
        eprintln!("  Simulating crash (dropping store)...");
        drop(async_kv);
        drop(store);
        eprintln!("  ✓ Store dropped");

        // Step 3: Create fresh store and recover
        //
        // The device must point to the same directory — that's where
        // the checkpoint and log segment files live.
        eprintln!("  Recovering from checkpoint {token}...");

        let device = TokioFileDevice::new(&args.data_dir, "log.", 512, 1u64 << 30)
            .expect("failed to create TokioFileDevice for recovery");

        let (recovered_store, recovery_info) =
            checkpoint::recover_store(&checkpoint_dir, Some(token), config, device)
                .expect("recovery failed");

        eprintln!(
            "  ✓ Recovered: {} index entries, {} pages loaded",
            recovery_info.num_index_entries, recovery_info.pages_loaded
        );

        // Wrap recovered store in AsyncFasterKv for consistency
        // (though we only need it for verification reads)
        let recovered_arc = Arc::new(recovered_store);

        // Step 4: Verify recovered data
        if args.verify {
            eprintln!();
            eprintln!("═══ Phase 4b: VERIFY (post-recovery) ═══");

            let (mut exp_clicks, mut exp_impressions, mut exp_spend) =
                verify::aggregate_expected(&all_worker_stats, args.campaigns);

            // Zero out expectations for deleted campaigns (they were
            // deleted before the checkpoint, so they shouldn't exist
            // in the recovered store).
            for &cid in &deleted_campaigns {
                let idx = cid as usize;
                if idx < args.campaigns {
                    exp_clicks[idx] = 0;
                    exp_impressions[idx] = 0;
                    exp_spend[idx] = 0;
                }
            }

            let store_v = Arc::clone(&recovered_arc);
            let num_campaigns = args.campaigns;
            let post_ok = tokio::task::spawn_blocking(move || {
                verify::verify_store(
                    &store_v,
                    &exp_clicks,
                    &exp_impressions,
                    &exp_spend,
                    num_campaigns,
                    "post-recovery",
                )
            })
            .await
            .unwrap_or(false);

            if !post_ok {
                eprintln!("  ✗ Post-recovery verification FAILED");
                std::process::exit(1);
            }
        }

        eprintln!();
    }

    // ════════════════════════════════════════════════════════════════
    // PHASE 5: REPORT
    // ════════════════════════════════════════════════════════════════

    print_final_report(total_ops, total_rmw, total_reads, ingest_elapsed, true);
}

/// Print the final summary report.
fn print_final_report(
    total_ops: u64,
    total_rmw: u64,
    total_reads: u64,
    elapsed: Duration,
    success: bool,
) {
    let secs = elapsed.as_secs_f64().max(0.001);

    eprintln!("╔══════════════════════════════════════════════════╗");
    eprintln!("║  Final Report                                   ║");
    eprintln!("╚══════════════════════════════════════════════════╝");
    eprintln!();
    eprintln!("  Total ops:        {total_ops}");
    eprintln!("  RMW ops:          {total_rmw}");
    eprintln!("  Read ops:         {total_reads}");
    eprintln!("  Elapsed:          {:.2}s", secs);
    eprintln!("  Throughput:       {:.0} ops/s", total_ops as f64 / secs);
    eprintln!();

    if success {
        eprintln!("  ✓ All phases completed successfully");
    } else {
        eprintln!("  ✗ Some phases failed — see above for details");
    }
}
