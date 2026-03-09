//! Async event generation via `spawn_blocking`.
//!
//! # The Async/Sync Bridge Pattern
//!
//! FASTER's `FasterSession` is `!Send` — it cannot be held across `.await`
//! points or sent between Tokio tasks. This is by design: sessions use
//! thread-local epoch protection that would break if moved between OS threads.
//!
//! The solution is `tokio::task::spawn_blocking`:
//! ```text
//! ┌─────────────────────┐
//! │  Tokio async runtime │  ← orchestrates tasks, handles I/O, signals
//! │                     │
//! │  spawn_blocking(|| { │  ← moves closure to Tokio's blocking pool
//! │    let session = ... │  ← session lives entirely on one OS thread
//! │    loop { rmw(...) } │  ← FASTER ops are synchronous, CPU-bound
//! │  })                  │
//! └─────────────────────┘
//! ```
//!
//! # KEY DIFFERENCE from the Thread Version
//!
//! In the thread-based `event-counter`, workers are `std::thread::spawn`
//! with an `Arc<AtomicBool>` for shutdown signaling. Here, workers are
//! `tokio::task::spawn_blocking` — they run on Tokio's blocking thread
//! pool instead of manually managed threads. The practical difference is
//! small (both get their own OS thread), but the Tokio version integrates
//! with Tokio's lifecycle management, graceful shutdown, and resource
//! accounting.
//!
//! # When to Use spawn_blocking vs tokio::spawn
//!
//! | Pattern | Use when... |
//! |---------|------------|
//! | `tokio::spawn` | The task is truly async (network I/O, timers, channels) |
//! | `spawn_blocking` | The task does CPU-bound or blocking work |
//!
//! FASTER operations are CPU-bound (hash lookups, in-memory record access)
//! with occasional blocking I/O (disk reads for cold records). Both of these
//! would block Tokio's async worker threads if run directly. `spawn_blocking`
//! moves the work to a dedicated thread pool, keeping the async runtime
//! responsive for control-plane tasks (stats, shutdown, checkpoints).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use faster_core::status::OperationStatus;
use faster_core::store::FasterKv;
use rand::Rng;

use crate::functions::{CampaignFunctions, CampaignStats, EventInput, EventType};

// ── Worker Statistics ───────────────────────────────────────────────────

/// Per-worker event counters for verification.
///
/// Each worker tracks exactly how many events of each type it generated
/// for each campaign. After the run, these are summed across workers and
/// compared against the FASTER store to verify correctness.
///
/// # Why Per-Worker Instead of Global?
///
/// Using per-worker vectors avoids atomic contention during the hot loop.
/// We sum them after all workers complete, which is much cheaper than
/// millions of atomic increments.
pub struct WorkerStats {
    /// clicks[campaign_id] = total clicks this worker generated for that campaign
    pub clicks: Vec<u64>,
    /// impressions[campaign_id] = total impressions this worker generated
    pub impressions: Vec<u64>,
    /// spend[campaign_id] = total spend (cents) this worker generated
    pub spend: Vec<u64>,
}

impl WorkerStats {
    pub fn new(num_campaigns: usize) -> Self {
        Self {
            clicks: vec![0; num_campaigns],
            impressions: vec![0; num_campaigns],
            spend: vec![0; num_campaigns],
        }
    }
}

// ── Shared Counters ─────────────────────────────────────────────────────

/// Atomically-shared operation counters for live progress reporting.
///
/// These use `Relaxed` ordering because they're only used for approximate
/// progress display — not for correctness. The exact counts come from
/// `WorkerStats` above.
pub struct LiveCounters {
    pub total_ops: AtomicU64,
    pub rmw_ops: AtomicU64,
    pub read_ops: AtomicU64,
    #[allow(dead_code)]
    pub delete_ops: AtomicU64,
}

impl LiveCounters {
    pub fn new() -> Self {
        Self {
            total_ops: AtomicU64::new(0),
            rmw_ops: AtomicU64::new(0),
            read_ops: AtomicU64::new(0),
            delete_ops: AtomicU64::new(0),
        }
    }
}

// ── Worker Function ─────────────────────────────────────────────────────

/// Run one worker's event generation loop.
///
/// This function runs synchronously inside `spawn_blocking`. It:
/// 1. Creates a FASTER session (thread-local, `!Send`)
/// 2. Generates random events for its assigned campaign range via RMW
/// 3. Periodically refreshes the session (advances the epoch)
/// 4. Returns per-worker stats for post-run verification
///
/// # Key Space Partitioning
///
/// Each worker is assigned a contiguous range of campaign IDs:
/// `[start_campaign, start_campaign + campaign_count)`. This avoids
/// concurrent RMW on the same key from different workers, which is the
/// recommended pattern for FASTER when exact correctness verification
/// is required. (Concurrent RMW on overlapping keys works correctly for
/// throughput but can lose individual updates due to in-place update races.)
///
/// # Arguments
///
/// * `store` — Shared FASTER store (Arc, thread-safe)
/// * `shutdown` — Cooperative shutdown flag
/// * `counters` — Live progress counters for the stats reporter
/// * `worker_id` — For logging
/// * `start_campaign` — First campaign ID owned by this worker
/// * `campaign_count` — Number of campaigns this worker manages
/// * `events_per_thread` — How many events to generate (0 = unlimited)
/// * `read_pct` — Percentage of operations that are reads (0-100)
///
/// # KEY DESIGN NOTE: Session Lifetime
///
/// The session is created at the start and disposed at the end of this
/// function. It never crosses an `await` boundary because there are no
/// `await` points — this is a pure sync function. This is the correct
/// pattern for FASTER + Tokio.
pub fn run_worker(
    store: Arc<FasterKv<CampaignFunctions>>,
    shutdown: Arc<AtomicBool>,
    counters: Arc<LiveCounters>,
    worker_id: usize,
    start_campaign: usize,
    campaign_count: usize,
    events_per_thread: u64,
    read_pct: u8,
) -> WorkerStats {
    // ── Create a thread-local session ───────────────────────────────
    //
    // Each spawn_blocking closure gets its own OS thread from Tokio's
    // blocking pool. The session binds to this thread via epoch
    // protection. NEVER try to move this session to another thread.
    let mut session = store.new_session();
    let mut rng = rand::rng();
    let mut stats = WorkerStats::new(start_campaign + campaign_count);
    let mut ops: u64 = 0;

    // Dummy input for reads — the EventInput is unused by read() but
    // the FASTER API requires it. Using Click/0 as a no-op sentinel.
    let read_input = EventInput {
        event_type: EventType::Click,
        amount: 0,
    };

    // ── Main Event Loop ─────────────────────────────────────────────
    //
    // Each iteration: pick a random campaign, pick a random event type,
    // apply it via RMW. If read_pct > 0, some iterations do reads instead.
    //
    // The loop terminates when either:
    // (a) we've generated `events_per_thread` events, or
    // (b) the shutdown flag is set (Ctrl+C or duration limit)
    while !shutdown.load(Ordering::Relaxed) {
        if events_per_thread > 0 && ops >= events_per_thread {
            break;
        }

        let campaign_id = start_campaign as u64 + rng.random_range(0..campaign_count as u64);

        // Decide: read or write?
        let is_read = read_pct > 0 && rng.random_range(0u8..100) < read_pct;

        if is_read {
            // ── Read Path ───────────────────────────────────────────
            //
            // Reads verify that the store returns consistent data.
            // We don't track read results for verification (the
            // authoritative check happens in the verify phase), but
            // we exercise the read path to stress pending I/O handling
            // when records are on disk.
            let mut output: Option<CampaignStats> = None;
            let status = store.read(&mut session, &campaign_id, &read_input, &mut output, ());

            // If the read returned Pending, the record is on disk.
            // complete_pending() will issue the I/O and block until
            // it completes. In a real async app, you'd use the async
            // session's .await-able API instead — but inside
            // spawn_blocking, blocking is fine.
            if status == OperationStatus::Pending {
                store.complete_pending(&mut session);
            }

            counters.read_ops.fetch_add(1, Ordering::Relaxed);
        } else {
            // ── RMW Path (the hot path) ─────────────────────────────
            //
            // Pick a random event type and apply it. The RMW operation
            // will call one of our three CampaignFunctions callbacks
            // depending on where the record currently lives:
            //   - rmw_initial: campaign doesn't exist yet → create it
            //   - rmw_in_place: campaign is in mutable region → fast update
            //   - rmw_copy_update: campaign is in read-only/disk → copy + update
            let event_type = match rng.random_range(0u8..3) {
                0 => EventType::Click,
                1 => EventType::Impression,
                _ => EventType::Spend,
            };

            // Amount: 1 for clicks/impressions, 1-100 cents for spend
            let amount = match event_type {
                EventType::Spend => rng.random_range(1..=100),
                _ => 1,
            };

            let input = EventInput { event_type, amount };

            // Apply the RMW — this is the core FASTER operation
            let mut output: Option<CampaignStats> = None;
            let status = store.rmw(&mut session, &campaign_id, &input, &mut output, ());

            if status == OperationStatus::Pending {
                store.complete_pending(&mut session);
            }

            // Only count the event in expected stats if the operation
            // actually succeeded. Aborted or failed operations should
            // not be counted to avoid verification mismatches.
            if status.is_success() || status == OperationStatus::Pending {
                match event_type {
                    EventType::Click => stats.clicks[campaign_id as usize] += amount,
                    EventType::Impression => stats.impressions[campaign_id as usize] += amount,
                    EventType::Spend => stats.spend[campaign_id as usize] += amount,
                }
            }

            counters.rmw_ops.fetch_add(1, Ordering::Relaxed);
        }

        ops += 1;
        counters.total_ops.fetch_add(1, Ordering::Relaxed);

        // ── Periodic Session Refresh ────────────────────────────────
        //
        // Every 256 operations, refresh the session. This does two things:
        // 1. Advances the epoch — allows FASTER to reclaim memory
        // 2. Completes any pending I/O callbacks
        //
        // Without refresh, the epoch can't advance and FASTER can't
        // evict pages from memory. This is the same pattern used in
        // the sync version — the only difference is that AsyncFasterKv
        // handles *global* maintenance (compaction, flushing) in the
        // background, while we handle per-session refresh here.
        //
        // KEY DIFFERENCE from thread version: In the thread version,
        // workers call store.maintenance() periodically. With
        // AsyncFasterKv, maintenance runs as an async background task,
        // so workers only need refresh().
        if ops % 256 == 0 {
            store.refresh(&mut session);
        }
    }

    // ── Clean up ────────────────────────────────────────────────────
    //
    // Flush any remaining pending operations before disposing. Without
    // this, the last few RMW ops (between the final refresh and exit)
    // may still be pending, causing count mismatches in verification.
    store.refresh(&mut session);
    store.complete_pending(&mut session);
    store.dispose_session(session);
    eprintln!("  worker {worker_id}: {ops} ops completed");

    stats
}
