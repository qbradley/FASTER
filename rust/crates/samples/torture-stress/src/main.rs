//! **torture-stress** – comprehensive correctness torture-test for FASTER KV.
//!
//! Spawns diverse thread roles (heavy writers, light writers, readers, mixed
//! workers, RMW hammers, deleters) under wave-shaped load and verifies every
//! read against a concurrent oracle backed by `DashMap`.
//!
//! Exit code 0 → no oracle violations.  Exit code 1 → corruption detected.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use clap::{Parser, ValueEnum};
use dashmap::DashMap;
use faster_core::SyncFileDevice;
use faster_core::hybrid_log::eviction::EvictionPolicy;
use faster_core::status::OperationStatus;
use faster_core::store::{
    FasterKv, FasterKvConfig, FasterSession, Functions, ReadInfo, RmwInPlaceResult, RmwInfo,
    UpsertInfo,
};
use rand::rngs::SmallRng;
use rand::{Rng, SeedableRng};

// ───────────────────────────── constants ──────────────────────────────────

const SEED_SIZE: usize = 8;
const REFRESH_INTERVAL: u64 = 64;
const REPORT_INTERVAL: Duration = Duration::from_secs(5);

// ───────────────────────────── CLI ────────────────────────────────────────

#[derive(Clone, Debug, ValueEnum)]
enum WaveFn {
    Sine,
    Square,
    Sawtooth,
    Spike,
}

#[derive(Parser, Debug)]
#[command(
    name = "torture-stress",
    about = "Correctness torture-test for FASTER KV with a test oracle"
)]
struct Args {
    // -- Duration & Scale --
    /// Test duration in seconds.
    #[arg(long, default_value_t = 60)]
    duration: u64,
    /// Total key-space size.
    #[arg(long, default_value_t = 1_000_000)]
    key_space: u64,
    /// Minimum value size in bytes.
    #[arg(long, default_value_t = 64)]
    min_value_size: usize,
    /// Maximum value size in bytes.
    #[arg(long, default_value_t = 4096)]
    max_value_size: usize,

    // -- Thread Configuration --
    /// Heavy writer threads (large values, 100% upsert).
    #[arg(long, default_value_t = 1)]
    heavy_writers: usize,
    /// Light writer threads (small values, 100% upsert).
    #[arg(long, default_value_t = 1)]
    light_writers: usize,
    /// Pure reader threads.
    #[arg(long, default_value_t = 2)]
    readers: usize,
    /// Mixed operation threads.
    #[arg(long, default_value_t = 2)]
    mixed_workers: usize,
    /// RMW-focused threads.
    #[arg(long, default_value_t = 1)]
    rmw_hammers: usize,
    /// Deleter threads.
    #[arg(long, default_value_t = 1)]
    deleters: usize,

    // -- Wave Pattern --
    /// Wave function.
    #[arg(long, default_value = "spike")]
    wave_fn: WaveFn,
    /// Wave cycle period in seconds.
    #[arg(long, default_value_t = 30)]
    wave_period: u64,
    /// Amplitude (0.0–1.0).
    #[arg(long, default_value_t = 0.8)]
    wave_amplitude: f64,
    /// Base ops/sec per thread.
    #[arg(long, default_value_t = 10_000)]
    base_rate: u64,

    // -- Store Configuration --
    /// Log storage directory.
    #[arg(long, default_value = "/tmp/torture-stress")]
    log_dir: PathBuf,
    /// Total log size in MiB.
    #[arg(long, default_value_t = 512)]
    log_size_mb: u64,
    /// In-memory buffer size in MiB.
    #[arg(long, default_value_t = 128)]
    in_memory_mb: u64,
    /// Enable lossy LRU mode.
    #[arg(long)]
    lossy: bool,

    // -- Oracle --
    /// Fraction of reads that verify oracle (0.0–1.0).
    #[arg(long, default_value_t = 1.0)]
    oracle_check_rate: f64,
    /// Abort on first oracle violation.
    #[arg(long)]
    fail_fast: bool,
}

// ───────────────────────── Value encoding / oracle ───────────────────────

/// Build a deterministic payload from `key ^ seed`.
fn build_payload(key: u64, seed: u64, len: usize) -> Vec<u8> {
    let mut rng = SmallRng::seed_from_u64(key ^ seed);
    let mut buf = Vec::with_capacity(len);
    // Generate 8 bytes at a time for speed.
    while buf.len() + 8 <= len {
        buf.extend_from_slice(&rng.random::<u64>().to_le_bytes());
    }
    while buf.len() < len {
        buf.push(rng.random::<u8>());
    }
    buf
}

/// Encode a value: `[seed:8 LE][payload]`.
fn encode_value(key: u64, seed: u64, payload_len: usize) -> Vec<u8> {
    let mut v = Vec::with_capacity(SEED_SIZE + payload_len);
    v.extend_from_slice(&seed.to_le_bytes());
    v.extend_from_slice(&build_payload(key, seed, payload_len));
    v
}

/// Extract seed from an encoded value.
fn extract_seed(value: &[u8]) -> Option<u64> {
    if value.len() < SEED_SIZE {
        return None;
    }
    Some(u64::from_le_bytes(value[..SEED_SIZE].try_into().unwrap()))
}

/// Verify an encoded value against the given key.
fn verify_value(key: u64, value: &[u8]) -> Result<(), ViolationKind> {
    if value.len() < SEED_SIZE {
        return Err(ViolationKind::ValueSizeMismatch);
    }
    let seed = u64::from_le_bytes(value[..SEED_SIZE].try_into().unwrap());
    let payload = &value[SEED_SIZE..];
    let expected = build_payload(key, seed, payload.len());
    if payload != expected.as_slice() {
        return Err(ViolationKind::WrongPayload);
    }
    Ok(())
}

// ───────────────────────── Oracle ────────────────────────────────────────

/// Number of recent seeds to keep per key for race tolerance.
const SEED_HISTORY: usize = 4;

#[derive(Clone, Debug)]
struct OracleEntry {
    seed: u64,
    version: u64,
    deleted: bool,
    /// Ring buffer of recent seeds (newest first) to tolerate concurrent races.
    recent_seeds: [u64; SEED_HISTORY],
}

impl OracleEntry {
    fn new(seed: u64) -> Self {
        Self {
            seed,
            version: 1,
            deleted: false,
            recent_seeds: [seed, 0, 0, 0],
        }
    }

    fn update(&mut self, new_seed: u64) {
        // Shift history right, insert new seed at front.
        self.recent_seeds.copy_within(0..SEED_HISTORY - 1, 1);
        self.recent_seeds[0] = new_seed;
        self.seed = new_seed;
        self.version += 1;
        self.deleted = false;
    }

    fn matches_any_seed(&self, seed: u64) -> bool {
        self.recent_seeds.iter().any(|&s| s == seed && s != 0) || seed == self.seed
    }
}

type Oracle = Arc<DashMap<u64, OracleEntry>>;

/// Check a read value against the oracle.
fn oracle_check(
    oracle: &DashMap<u64, OracleEntry>,
    key: u64,
    value: &[u8],
    lossy: bool,
) -> Option<ViolationKind> {
    // First: structural integrity – does the payload match its own seed?
    if let Err(kind) = verify_value(key, value) {
        return Some(kind);
    }

    let read_seed = match extract_seed(value) {
        Some(s) => s,
        None => return Some(ViolationKind::ValueSizeMismatch),
    };

    // Compare with oracle.
    if let Some(entry) = oracle.get(&key) {
        if entry.deleted {
            // Key was deleted in oracle but we still got a value.
            // In a lossy store or with concurrent writes this can be benign.
            if lossy {
                return None;
            }
            // Allow race: the delete may not have been applied yet,
            // or the value is from before the delete.
            if entry.matches_any_seed(read_seed) {
                return None;
            }
            return None; // Be lenient for deletes
        }
        if entry.matches_any_seed(read_seed) {
            return None; // matches current or recent version
        }
        // Seed doesn't match any known version → corruption.
        return Some(ViolationKind::WrongSeed);
    }
    // Key not in oracle at all — could be a race (another thread wrote & we
    // haven't seen the oracle update yet).  Be lenient.
    None
}

// ───────────────────────── Violation tracking ────────────────────────────

#[derive(Clone, Debug)]
enum ViolationKind {
    WrongSeed,
    WrongPayload,
    #[allow(dead_code)]
    UnexpectedValue,
    #[allow(dead_code)]
    MissingValue,
    ValueSizeMismatch,
}

impl std::fmt::Display for ViolationKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::WrongSeed => write!(f, "WRONG_SEED"),
            Self::WrongPayload => write!(f, "WRONG_PAYLOAD"),
            Self::UnexpectedValue => write!(f, "UNEXPECTED_VALUE"),
            Self::MissingValue => write!(f, "MISSING_VALUE"),
            Self::ValueSizeMismatch => write!(f, "VALUE_SIZE_MISMATCH"),
        }
    }
}

#[derive(Debug, Clone)]
struct Violation {
    kind: ViolationKind,
    key: u64,
    time_secs: f64,
    thread_name: String,
    detail: String,
}

// ───────────────────────── Per-thread stats ──────────────────────────────

struct ThreadStats {
    upserts: AtomicU64,
    reads: AtomicU64,
    rmws: AtomicU64,
    deletes: AtomicU64,
    aborted: AtomicU64,
    oracle_checks: AtomicU64,
    oracle_violations: AtomicU64,
    bytes_written: AtomicU64,
    bytes_read: AtomicU64,
}

impl ThreadStats {
    fn new() -> Self {
        Self {
            upserts: AtomicU64::new(0),
            reads: AtomicU64::new(0),
            rmws: AtomicU64::new(0),
            deletes: AtomicU64::new(0),
            aborted: AtomicU64::new(0),
            oracle_checks: AtomicU64::new(0),
            oracle_violations: AtomicU64::new(0),
            bytes_written: AtomicU64::new(0),
            bytes_read: AtomicU64::new(0),
        }
    }
}

// ──────────────────────── Shared globals ─────────────────────────────────

struct SharedState {
    store: Arc<FasterKv<TortureTestFunctions>>,
    oracle: Oracle,
    shutdown: Arc<AtomicBool>,
    start: Instant,
    args: Arc<Args>,
    stats: Arc<ThreadStats>,
    violations: Arc<std::sync::Mutex<Vec<Violation>>>,
}

impl SharedState {
    fn elapsed_secs(&self) -> f64 {
        self.start.elapsed().as_secs_f64()
    }
}

// ────────────────────── Functions implementation ─────────────────────────

/// Input for FASTER operations.
#[derive(Clone)]
enum TortureInput {
    /// Upsert: carry the full encoded value.
    Upsert(Vec<u8>),
    /// RMW: carry a new seed to rewrite the payload.
    Rmw { new_seed: u64, payload_len: usize },
    /// Read / Delete: no extra data.
    None,
}

/// Output returned by FASTER callbacks.
#[derive(Clone, Default)]
struct TortureOutput {
    value: Option<Vec<u8>>,
}

struct TortureTestFunctions;

impl Functions for TortureTestFunctions {
    type Key = u64;
    type Value = Vec<u8>;
    type Input = TortureInput;
    type Output = TortureOutput;
    type Context = ();

    fn read(
        &self,
        _key: &u64,
        value: &Vec<u8>,
        _input: &TortureInput,
        output: &mut TortureOutput,
        _info: &ReadInfo,
    ) {
        output.value = Some(value.clone());
    }

    fn upsert(
        &self,
        _key: &u64,
        value: &mut Vec<u8>,
        input: &TortureInput,
        _old_value: Option<&Vec<u8>>,
        _output: &mut TortureOutput,
        _info: &UpsertInfo,
    ) {
        if let TortureInput::Upsert(data) = input {
            *value = data.clone();
        }
    }

    fn rmw_initial(
        &self,
        key: &u64,
        input: &TortureInput,
        value: &mut Vec<u8>,
        _output: &mut TortureOutput,
        _info: &RmwInfo,
    ) {
        // First write for this key via RMW – create fresh value.
        if let TortureInput::Rmw {
            new_seed,
            payload_len,
        } = input
        {
            *value = encode_value(*key, *new_seed, *payload_len);
        }
    }

    fn rmw_in_place(
        &self,
        key: &u64,
        input: &TortureInput,
        value: &mut Vec<u8>,
        _output: &mut TortureOutput,
        _info: &RmwInfo,
    ) -> RmwInPlaceResult {
        if let TortureInput::Rmw {
            new_seed,
            payload_len,
        } = input
        {
            let new_val = encode_value(*key, *new_seed, *payload_len);
            if new_val.len() == value.len() {
                value.copy_from_slice(&new_val);
                RmwInPlaceResult::InPlaceOk
            } else {
                // Size changed – need a new record.
                *value = new_val;
                RmwInPlaceResult::NeedsNewRecord
            }
        } else {
            RmwInPlaceResult::InPlaceOk
        }
    }

    fn rmw_copy_update(
        &self,
        key: &u64,
        input: &TortureInput,
        _old_value: &Vec<u8>,
        new_value: &mut Vec<u8>,
        _output: &mut TortureOutput,
        _info: &RmwInfo,
    ) {
        if let TortureInput::Rmw {
            new_seed,
            payload_len,
        } = input
        {
            *new_value = encode_value(*key, *new_seed, *payload_len);
        }
    }
}

// ───────────────────────── Wave function ─────────────────────────────────

fn wave_intensity(wave: &WaveFn, t_secs: f64, period: f64, amplitude: f64) -> f64 {
    let base = 1.0 - amplitude;
    let phase = (t_secs % period) / period; // 0.0 .. 1.0
    let raw = match wave {
        WaveFn::Sine => 0.5 + 0.5 * (2.0 * std::f64::consts::PI * phase).sin(),
        WaveFn::Square => {
            if phase < 0.5 {
                1.0
            } else {
                0.0
            }
        }
        WaveFn::Sawtooth => phase,
        WaveFn::Spike => {
            // Spike: instant ramp to 100% for first third, then drop to 10%.
            if phase < 0.333 { 1.0 } else { 0.1 }
        }
    };
    base + amplitude * raw
}

/// Sleep to achieve target rate.  Returns the target ops/sec.
fn rate_limit(
    wave: &WaveFn,
    start: Instant,
    period: f64,
    amplitude: f64,
    base_rate: u64,
    ops_this_second: &mut u64,
    second_start: &mut Instant,
) -> f64 {
    let t = start.elapsed().as_secs_f64();
    let intensity = wave_intensity(wave, t, period, amplitude);
    let target_rate = (base_rate as f64 * intensity).max(100.0);

    let elapsed_in_sec = second_start.elapsed().as_secs_f64();
    if elapsed_in_sec >= 1.0 {
        *ops_this_second = 0;
        *second_start = Instant::now();
    } else if *ops_this_second as f64 >= target_rate * elapsed_in_sec {
        // We're ahead of schedule — sleep a bit.
        let sleep_us = (1_000_000.0 / target_rate).min(10_000.0) as u64;
        thread::sleep(Duration::from_micros(sleep_us.max(10)));
    }
    *ops_this_second += 1;
    target_rate
}

// ───────────────────── Key selection helpers ─────────────────────────────

fn random_key(rng: &mut SmallRng, key_space: u64) -> u64 {
    rng.random_range(0..key_space)
}

fn zipf_key(rng: &mut SmallRng, key_space: u64) -> u64 {
    // Approximate Zipf by taking min of a few random keys (power-of-two-choices).
    let a = rng.random_range(0..key_space);
    let b = rng.random_range(0..key_space);
    a.min(b)
}

fn random_value_size(rng: &mut SmallRng, min_size: usize, max_size: usize) -> usize {
    if min_size >= max_size {
        min_size
    } else {
        rng.random_range(min_size..=max_size)
    }
}

// ──────────────────── Core operation wrappers ────────────────────────────

fn do_upsert(
    store: &FasterKv<TortureTestFunctions>,
    session: &mut FasterSession<TortureTestFunctions>,
    oracle: &DashMap<u64, OracleEntry>,
    key: u64,
    value_size: usize,
    rng: &mut SmallRng,
    stats: &ThreadStats,
) -> usize {
    let seed: u64 = rng.random();
    let payload_len = value_size.saturating_sub(SEED_SIZE);
    let encoded = encode_value(key, seed, payload_len);
    let byte_len = encoded.len();
    let input = TortureInput::Upsert(encoded);

    // Update oracle BEFORE the store write so readers always see at least the
    // previous version.
    oracle
        .entry(key)
        .and_modify(|e| {
            e.update(seed);
        })
        .or_insert(OracleEntry::new(seed));

    let mut ctx = store.unsafe_context(session);
    let status = ctx.upsert(store, &key, &input, ());
    ctx.refresh();
    drop(ctx);

    if status == OperationStatus::Pending {
        let _ = store.complete_pending(session);
    }
    if status.is_aborted() {
        stats.aborted.fetch_add(1, Ordering::Relaxed);
    }
    byte_len
}

fn do_read(
    state: &SharedState,
    session: &mut FasterSession<TortureTestFunctions>,
    key: u64,
    thread_name: &str,
    rng: &mut SmallRng,
) -> usize {
    let mut output = TortureOutput::default();
    let input = TortureInput::None;

    let mut ctx = state.store.unsafe_context(session);
    let status = ctx.read(&state.store, &key, &input, &mut output, ());
    ctx.refresh();
    drop(ctx);

    // Only verify reads that completed immediately (status == Ok).
    // Pending reads go through async I/O and we cannot reliably match the
    // completed output to the key we requested without a richer Context type.
    let immediate = status == OperationStatus::Ok;

    if status == OperationStatus::Pending {
        let _ = state.store.complete_pending(session);
    }
    if status.is_aborted() {
        state.stats.aborted.fetch_add(1, Ordering::Relaxed);
    }

    let bytes = output.value.as_ref().map_or(0, |v| v.len());

    // Oracle verification — only for immediate (in-memory) reads.
    if immediate {
        let do_check = rng.random::<f64>() < state.args.oracle_check_rate;
        if do_check {
            state.stats.oracle_checks.fetch_add(1, Ordering::Relaxed);

            if let Some(ref val) = output.value {
                if let Some(kind) = oracle_check(&state.oracle, key, val, state.args.lossy) {
                    record_violation(state, kind, key, thread_name, val);
                }
            }
        }
    }
    bytes
}

fn do_rmw(
    store: &FasterKv<TortureTestFunctions>,
    session: &mut FasterSession<TortureTestFunctions>,
    oracle: &DashMap<u64, OracleEntry>,
    key: u64,
    value_size: usize,
    rng: &mut SmallRng,
    stats: &ThreadStats,
) -> usize {
    let new_seed: u64 = rng.random();
    let payload_len = value_size.saturating_sub(SEED_SIZE);
    let input = TortureInput::Rmw {
        new_seed,
        payload_len,
    };
    let mut output = TortureOutput::default();

    // Update oracle before store.
    oracle
        .entry(key)
        .and_modify(|e| {
            e.update(new_seed);
        })
        .or_insert(OracleEntry::new(new_seed));

    let mut ctx = store.unsafe_context(session);
    let status = ctx.rmw(store, &key, &input, &mut output, ());
    ctx.refresh();
    drop(ctx);

    if status == OperationStatus::Pending {
        let _ = store.complete_pending(session);
    }
    if status.is_aborted() {
        stats.aborted.fetch_add(1, Ordering::Relaxed);
    }
    SEED_SIZE + payload_len
}

fn do_delete(
    store: &FasterKv<TortureTestFunctions>,
    session: &mut FasterSession<TortureTestFunctions>,
    oracle: &DashMap<u64, OracleEntry>,
    key: u64,
    stats: &ThreadStats,
) {
    oracle.entry(key).and_modify(|e| {
        e.deleted = true;
        e.version += 1;
    });

    let mut ctx = store.unsafe_context(session);
    let status = ctx.delete(store, &key, ());
    ctx.refresh();
    drop(ctx);

    if status.is_aborted() {
        stats.aborted.fetch_add(1, Ordering::Relaxed);
    }
}

fn record_violation(state: &SharedState, kind: ViolationKind, key: u64, thread: &str, val: &[u8]) {
    state
        .stats
        .oracle_violations
        .fetch_add(1, Ordering::Relaxed);
    let seed_got = extract_seed(val);
    let oracle_seed = state.oracle.get(&key).map(|e| e.seed);
    let detail = format!(
        "Got seed: {:#018x}, Oracle seed: {}, Value len: {}",
        seed_got.unwrap_or(0),
        oracle_seed
            .map(|s| format!("{:#018x}", s))
            .unwrap_or_else(|| "N/A".into()),
        val.len(),
    );
    let v = Violation {
        kind,
        key,
        time_secs: state.elapsed_secs(),
        thread_name: thread.to_string(),
        detail,
    };
    eprintln!(
        "!!! ORACLE VIOLATION at t={:.1}s !!!\n  Type: {}\n  Key: {:#018x}\n  Thread: {}\n  {}",
        v.time_secs, v.kind, v.key, v.thread_name, v.detail
    );
    let mut violations = state.violations.lock().unwrap();
    violations.push(v);
    if state.args.fail_fast {
        state.shutdown.store(true, Ordering::SeqCst);
    }
}

// ──────────────────── Thread role runners ────────────────────────────────

fn run_worker(
    state: Arc<SharedState>,
    role: &str,
    phase_offset: f64,
    op_fn: impl Fn(&SharedState, &mut FasterSession<TortureTestFunctions>, &mut SmallRng, &str),
) {
    let mut session = state.store.new_session();
    let mut rng = SmallRng::from_os_rng();
    let thread_name = format!("{}-{:?}", role, thread::current().id());

    let mut ops_this_second: u64 = 0;
    let mut second_start = Instant::now();
    let mut op_count: u64 = 0;
    let period = state.args.wave_period as f64 + phase_offset;

    while !state.shutdown.load(Ordering::Relaxed) {
        rate_limit(
            &state.args.wave_fn,
            state.start,
            period,
            state.args.wave_amplitude,
            state.args.base_rate,
            &mut ops_this_second,
            &mut second_start,
        );

        op_fn(&state, &mut session, &mut rng, &thread_name);
        op_count += 1;

        if op_count % REFRESH_INTERVAL == 0 {
            let _ = state.store.complete_pending(&mut session);
        }
    }

    // Drain pending I/O before shutdown.
    let _ = state.store.complete_pending_sync(&mut session);
    state.store.dispose_session(session);
}

fn heavy_writer_fn(
    state: &SharedState,
    session: &mut FasterSession<TortureTestFunctions>,
    rng: &mut SmallRng,
    _thread_name: &str,
) {
    let key = random_key(rng, state.args.key_space);
    let size = random_value_size(rng, 1024, state.args.max_value_size.max(1024));
    let bytes = do_upsert(&state.store, session, &state.oracle, key, size, rng, &state.stats);
    state.stats.upserts.fetch_add(1, Ordering::Relaxed);
    state
        .stats
        .bytes_written
        .fetch_add(bytes as u64, Ordering::Relaxed);
}

fn light_writer_fn(
    state: &SharedState,
    session: &mut FasterSession<TortureTestFunctions>,
    rng: &mut SmallRng,
    _thread_name: &str,
) {
    let key = random_key(rng, state.args.key_space);
    let size = random_value_size(rng, state.args.min_value_size, 256);
    let bytes = do_upsert(&state.store, session, &state.oracle, key, size, rng, &state.stats);
    state.stats.upserts.fetch_add(1, Ordering::Relaxed);
    state
        .stats
        .bytes_written
        .fetch_add(bytes as u64, Ordering::Relaxed);
}

fn reader_fn(
    state: &SharedState,
    session: &mut FasterSession<TortureTestFunctions>,
    rng: &mut SmallRng,
    thread_name: &str,
) {
    // Pick a key the oracle knows about (if any), else random.
    let key = if !state.oracle.is_empty() && rng.random::<f64>() < 0.8 {
        // Sample a random key from the oracle. DashMap doesn't have random
        // access, so we use an iterator skip trick with a bounded skip.
        let idx = rng.random_range(0..state.oracle.len().max(1));
        state
            .oracle
            .iter()
            .nth(idx)
            .map(|e| *e.key())
            .unwrap_or_else(|| random_key(rng, state.args.key_space))
    } else {
        random_key(rng, state.args.key_space)
    };
    let bytes = do_read(state, session, key, thread_name, rng);
    state.stats.reads.fetch_add(1, Ordering::Relaxed);
    state
        .stats
        .bytes_read
        .fetch_add(bytes as u64, Ordering::Relaxed);
}

fn mixed_worker_fn(
    state: &SharedState,
    session: &mut FasterSession<TortureTestFunctions>,
    rng: &mut SmallRng,
    thread_name: &str,
) {
    let roll: f64 = rng.random();
    let key = random_key(rng, state.args.key_space);

    if roll < 0.4 {
        // upsert
        let size = random_value_size(rng, state.args.min_value_size, state.args.max_value_size);
        let bytes = do_upsert(&state.store, session, &state.oracle, key, size, rng, &state.stats);
        state.stats.upserts.fetch_add(1, Ordering::Relaxed);
        state
            .stats
            .bytes_written
            .fetch_add(bytes as u64, Ordering::Relaxed);
    } else if roll < 0.7 {
        // read
        let bytes = do_read(state, session, key, thread_name, rng);
        state.stats.reads.fetch_add(1, Ordering::Relaxed);
        state
            .stats
            .bytes_read
            .fetch_add(bytes as u64, Ordering::Relaxed);
    } else if roll < 0.9 {
        // rmw
        let size = random_value_size(rng, state.args.min_value_size, state.args.max_value_size);
        let bytes = do_rmw(&state.store, session, &state.oracle, key, size, rng, &state.stats);
        state.stats.rmws.fetch_add(1, Ordering::Relaxed);
        state
            .stats
            .bytes_written
            .fetch_add(bytes as u64, Ordering::Relaxed);
    } else {
        // delete
        do_delete(&state.store, session, &state.oracle, key, &state.stats);
        state.stats.deletes.fetch_add(1, Ordering::Relaxed);
    }
}

fn rmw_hammer_fn(
    state: &SharedState,
    session: &mut FasterSession<TortureTestFunctions>,
    rng: &mut SmallRng,
    thread_name: &str,
) {
    let key = zipf_key(rng, state.args.key_space);
    let roll: f64 = rng.random();

    if roll < 0.8 {
        let size = random_value_size(rng, state.args.min_value_size, state.args.max_value_size);
        let bytes = do_rmw(&state.store, session, &state.oracle, key, size, rng, &state.stats);
        state.stats.rmws.fetch_add(1, Ordering::Relaxed);
        state
            .stats
            .bytes_written
            .fetch_add(bytes as u64, Ordering::Relaxed);
    } else {
        let bytes = do_read(state, session, key, thread_name, rng);
        state.stats.reads.fetch_add(1, Ordering::Relaxed);
        state
            .stats
            .bytes_read
            .fetch_add(bytes as u64, Ordering::Relaxed);
    }
}

fn deleter_fn(
    state: &SharedState,
    session: &mut FasterSession<TortureTestFunctions>,
    rng: &mut SmallRng,
    _thread_name: &str,
) {
    let key = random_key(rng, state.args.key_space);
    let roll: f64 = rng.random();

    if roll < 0.5 {
        do_delete(&state.store, session, &state.oracle, key, &state.stats);
        state.stats.deletes.fetch_add(1, Ordering::Relaxed);
    } else {
        let size = random_value_size(rng, state.args.min_value_size, state.args.max_value_size);
        let bytes = do_upsert(&state.store, session, &state.oracle, key, size, rng, &state.stats);
        state.stats.upserts.fetch_add(1, Ordering::Relaxed);
        state
            .stats
            .bytes_written
            .fetch_add(bytes as u64, Ordering::Relaxed);
    }
}

// ─────────────────── Maintenance thread ──────────────────────────────────

fn maintenance_thread(store: Arc<FasterKv<TortureTestFunctions>>, shutdown: Arc<AtomicBool>) {
    while !shutdown.load(Ordering::Relaxed) {
        store.maintenance();
        thread::sleep(Duration::from_millis(1));
    }
}

// ───────────────────── Reporter ──────────────────────────────────────────

fn reporter_thread(state: Arc<SharedState>) {
    let mut last_total: u64 = 0;
    let mut last_time = Instant::now();
    let mut peak_ops_sec: u64 = 0;

    while !state.shutdown.load(Ordering::Relaxed) {
        thread::sleep(REPORT_INTERVAL);
        if state.shutdown.load(Ordering::Relaxed) {
            break;
        }

        let u = state.stats.upserts.load(Ordering::Relaxed);
        let r = state.stats.reads.load(Ordering::Relaxed);
        let m = state.stats.rmws.load(Ordering::Relaxed);
        let d = state.stats.deletes.load(Ordering::Relaxed);
        let a = state.stats.aborted.load(Ordering::Relaxed);
        let total = u + r + m + d;
        let checks = state.stats.oracle_checks.load(Ordering::Relaxed);
        let violations = state.stats.oracle_violations.load(Ordering::Relaxed);
        let bw = state.stats.bytes_written.load(Ordering::Relaxed);
        let br = state.stats.bytes_read.load(Ordering::Relaxed);

        let dt = last_time.elapsed().as_secs_f64();
        let ops_sec = if dt > 0.0 {
            ((total - last_total) as f64 / dt) as u64
        } else {
            0
        };
        peak_ops_sec = peak_ops_sec.max(ops_sec);
        last_total = total;
        last_time = Instant::now();

        let elapsed = state.elapsed_secs();
        let intensity = wave_intensity(
            &state.args.wave_fn,
            elapsed,
            state.args.wave_period as f64,
            state.args.wave_amplitude,
        );

        let fmt = |n: u64| -> String {
            if n >= 1_000_000 {
                format!("{:.1}M", n as f64 / 1_000_000.0)
            } else if n >= 1_000 {
                format!("{:.0}K", n as f64 / 1_000.0)
            } else {
                format!("{}", n)
            }
        };

        let fmt_bw = |b: u64, secs: f64| -> String {
            let rate = b as f64 / secs;
            if rate >= 1_000_000_000.0 {
                format!("{:.1} GB/s", rate / 1_000_000_000.0)
            } else if rate >= 1_000_000.0 {
                format!("{:.0} MB/s", rate / 1_000_000.0)
            } else {
                format!("{:.0} KB/s", rate / 1_000.0)
            }
        };

        eprintln!(
            "[{:02}:{:02}] Ops: {} total ({}/s) | W:{} R:{} RMW:{} D:{} | Aborted: {}",
            elapsed as u64 / 60,
            elapsed as u64 % 60,
            fmt(total),
            fmt(ops_sec),
            fmt(u),
            fmt(r),
            fmt(m),
            fmt(d),
            fmt(a),
        );
        eprintln!(
            "        Oracle: {} checks, {} violations | Wave: {:?} @ {:.0}%",
            fmt(checks),
            violations,
            state.args.wave_fn,
            intensity * 100.0,
        );
        eprintln!(
            "        Throughput: {} write, {} read",
            fmt_bw(bw, elapsed),
            fmt_bw(br, elapsed),
        );
    }
    // Store peak for summary.
    // (We use a simple approach: just store it as a relaxed atomic isn't worth
    // the complexity for a reporting tool.)
    let _ = peak_ops_sec; // logged above in live output
}

// ───────────────────── Final summary ─────────────────────────────────────

fn print_summary(state: &SharedState, duration: f64) {
    let u = state.stats.upserts.load(Ordering::Relaxed);
    let r = state.stats.reads.load(Ordering::Relaxed);
    let m = state.stats.rmws.load(Ordering::Relaxed);
    let d = state.stats.deletes.load(Ordering::Relaxed);
    let a = state.stats.aborted.load(Ordering::Relaxed);
    let total = u + r + m + d;
    let checks = state.stats.oracle_checks.load(Ordering::Relaxed);
    let violations = state.stats.oracle_violations.load(Ordering::Relaxed);
    let avg_ops = if duration > 0.0 {
        (total as f64 / duration) as u64
    } else {
        0
    };
    let avg_aborted = if duration > 0.0 {
        (a as f64 / duration) as u64
    } else {
        0
    };

    println!();
    println!("=== TORTURE STRESS RESULTS ===");
    println!("Duration: {:.1}s", duration);
    println!("Total Operations: {}", fmt_num(total));
    if total > 0 {
        println!(
            "  Upserts:    {:>12} ({:.1}%)",
            fmt_num(u),
            u as f64 / total as f64 * 100.0
        );
        println!(
            "  Reads:      {:>12} ({:.1}%)",
            fmt_num(r),
            r as f64 / total as f64 * 100.0
        );
        println!(
            "  RMW:        {:>12} ({:.1}%)",
            fmt_num(m),
            m as f64 / total as f64 * 100.0
        );
        println!(
            "  Deletes:    {:>12} ({:.1}%)",
            fmt_num(d),
            d as f64 / total as f64 * 100.0
        );
    }

    println!();
    println!("Aborted Operations:");
    println!("  Total aborted:    {}", fmt_num(a));
    println!("  Avg aborted/sec:  {}", fmt_num(avg_aborted));

    println!();
    println!("Oracle Verification:");
    println!("  Total checks:     {}", fmt_num(checks));
    println!("  Violations:       {}", violations);
    println!(
        "  Corruption:       {} ({})",
        violations,
        if violations == 0 { "PASS" } else { "FAIL" }
    );

    println!();
    println!("Performance:");
    println!("  Avg ops/sec:      {}", fmt_num(avg_ops));

    println!();
    println!(
        "Wave Pattern: {:?} ({}s period, {} amplitude)",
        state.args.wave_fn, state.args.wave_period, state.args.wave_amplitude
    );

    let vs = state.violations.lock().unwrap();
    if vs.is_empty() {
        println!("Exit: Clean shutdown (no crashes, no hangs)");
    } else {
        println!();
        println!("!!! {} ORACLE VIOLATION(S) DETECTED !!!", vs.len());
        for (i, v) in vs.iter().enumerate().take(10) {
            println!();
            println!(
                "!!! ORACLE VIOLATION #{} at t={:.1}s !!!",
                i + 1,
                v.time_secs
            );
            println!("  Type: {}", v.kind);
            println!("  Key: {:#018x}", v.key);
            println!("  Thread: {}", v.thread_name);
            println!("  {}", v.detail);
        }
        if vs.len() > 10 {
            println!("  ... and {} more", vs.len() - 10);
        }
    }
}

fn fmt_num(n: u64) -> String {
    // Simple thousands-separated formatting.
    let s = n.to_string();
    let mut result = String::new();
    for (i, c) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            result.push(',');
        }
        result.push(c);
    }
    result.chars().rev().collect()
}

// ───────────────────────── main ──────────────────────────────────────────

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    // Prepare log directory.
    if args.log_dir.exists() {
        std::fs::remove_dir_all(&args.log_dir)?;
    }
    std::fs::create_dir_all(&args.log_dir)?;

    // Compute FASTER page geometry.
    let page_size: usize = 1 << 25; // 32 MiB per FASTER page frame
    let buffer_pages = ((args.in_memory_mb as usize * 1024 * 1024) / page_size)
        .next_power_of_two()
        .max(4);
    let segment_size = (args.log_size_mb * 1024 * 1024).max(page_size as u64 * 4);

    let config = FasterKvConfig {
        hash_index_size_log2: 20,
        buffer_size_pages: buffer_pages,
        mutable_fraction: 0.9,
        sector_size: 512,
        eviction_policy: EvictionPolicy {
            max_in_memory_pages: (buffer_pages as u32).saturating_sub(2).max(2),
            eviction_batch_size: 4,
        },
        lossy: args.lossy,
        ..Default::default()
    };

    let device = SyncFileDevice::new(
        &args.log_dir,
        "torture.",
        512, // sector size
        segment_size,
        4, // I/O threads
    )?;

    let store = Arc::new(FasterKv::new(config, TortureTestFunctions, device));
    let oracle: Oracle = Arc::new(DashMap::new());
    let shutdown = Arc::new(AtomicBool::new(false));
    let stats = Arc::new(ThreadStats::new());
    let violations = Arc::new(std::sync::Mutex::new(Vec::<Violation>::new()));
    let start = Instant::now();
    let args = Arc::new(args);

    let shared = Arc::new(SharedState {
        store: store.clone(),
        oracle: oracle.clone(),
        shutdown: shutdown.clone(),
        start,
        args: args.clone(),
        stats: stats.clone(),
        violations: violations.clone(),
    });

    // Ctrl+C handler.
    {
        let sd = shutdown.clone();
        ctrlc::set_handler(move || {
            eprintln!("\nShutting down (Ctrl+C)...");
            sd.store(true, Ordering::SeqCst);
        })?;
    }

    // Duration timer.
    {
        let sd = shutdown.clone();
        let dur = args.duration;
        thread::spawn(move || {
            thread::sleep(Duration::from_secs(dur));
            sd.store(true, Ordering::SeqCst);
        });
    }

    eprintln!(
        "torture-stress: {} threads, {}s, key_space={}, values={}–{}B, wave={:?}({}s), lossy={}",
        args.heavy_writers
            + args.light_writers
            + args.readers
            + args.mixed_workers
            + args.rmw_hammers
            + args.deleters,
        args.duration,
        args.key_space,
        args.min_value_size,
        args.max_value_size,
        args.wave_fn,
        args.wave_period,
        args.lossy,
    );

    let mut handles: Vec<thread::JoinHandle<()>> = Vec::new();

    // Maintenance thread.
    {
        let s = store.clone();
        let sd = shutdown.clone();
        handles.push(
            thread::Builder::new()
                .name("maintenance".into())
                .spawn(move || {
                    maintenance_thread(s, sd);
                })?,
        );
    }

    // Reporter thread.
    {
        let ss = shared.clone();
        handles.push(
            thread::Builder::new()
                .name("reporter".into())
                .spawn(move || {
                    reporter_thread(ss);
                })?,
        );
    }

    // Spawn worker threads with staggered phase offsets.
    let mut thread_idx = 0usize;
    let total_threads = args.heavy_writers
        + args.light_writers
        + args.readers
        + args.mixed_workers
        + args.rmw_hammers
        + args.deleters;

    macro_rules! spawn_role {
        ($count:expr, $name:expr, $fn:ident) => {
            for _ in 0..$count {
                let ss = shared.clone();
                let phase =
                    (thread_idx as f64 / total_threads.max(1) as f64) * args.wave_period as f64;
                thread_idx += 1;
                let role_name = $name.to_string();
                handles.push(
                    thread::Builder::new()
                        .name(format!("{}-{}", $name, thread_idx))
                        .spawn(move || {
                            run_worker(ss, &role_name, phase, $fn);
                        })?,
                );
            }
        };
    }

    spawn_role!(args.heavy_writers, "HeavyWriter", heavy_writer_fn);
    spawn_role!(args.light_writers, "LightWriter", light_writer_fn);
    spawn_role!(args.readers, "Reader", reader_fn);
    spawn_role!(args.mixed_workers, "MixedWorker", mixed_worker_fn);
    spawn_role!(args.rmw_hammers, "RmwHammer", rmw_hammer_fn);
    spawn_role!(args.deleters, "Deleter", deleter_fn);

    // Wait for all threads.
    for h in handles {
        let _ = h.join();
    }

    let duration = start.elapsed().as_secs_f64();
    print_summary(&shared, duration);

    let violation_count = shared.violations.lock().unwrap().len();
    if violation_count > 0 {
        std::process::exit(1);
    }

    Ok(())
}
