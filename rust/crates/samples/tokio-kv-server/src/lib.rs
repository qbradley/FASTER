//! Async key-value server library powered by FASTER + Tokio.
//!
//! This module provides the server logic for a TCP-accessible key-value store
//! that uses [`AsyncFasterKv`] from `faster-tokio` for background maintenance
//! and the core [`FasterKv`] API for operations. The protocol is a simple
//! text-based format suitable for `nc`/`telnet` interaction.
//!
//! # Architecture
//!
//! ```text
//! ┌──────────────┐     tokio::spawn      ┌──────────────────┐
//! │  TcpListener  │ ──────────────────▶  │  Connection Task  │
//! │  (accept loop)│                      │  (async read/write)│
//! └──────────────┘                      └────────┬─────────┘
//!                                                │
//!                                       spawn_blocking
//!                                                │
//!                                       ┌────────▼─────────┐
//!                                       │  FASTER Session   │
//!                                       │  (thread-local,   │
//!                                       │   !Send)          │
//!                                       └──────────────────┘
//! ```
//!
//! Each command from a client is executed via [`tokio::task::spawn_blocking`]
//! because FASTER sessions are `!Send` (thread-affine). A fresh session is
//! created per command batch, which is fine for interactive use. For
//! high-throughput scenarios, a dedicated worker thread per connection with
//! a long-lived session would be preferable.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use faster_core::status::OperationStatus;
use faster_core::store::{FasterKv, SimpleFunctions};
use rand::Rng;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::broadcast;

/// The Functions type for our key-value store: u64 keys, u64 values.
pub type KvFunctions = SimpleFunctions<u64, u64>;

/// Shared server state passed to each connection handler.
#[derive(Clone)]
pub struct ServerState {
    /// The underlying FASTER store (thread-safe, shared).
    pub store: Arc<FasterKv<KvFunctions>>,
    /// Active connection count.
    pub connections: Arc<AtomicU64>,
}

// ── Protocol ────────────────────────────────────────────────────────────

/// A parsed client command.
#[derive(Debug, PartialEq)]
pub enum Command {
    /// `SET <key> <value>` — store a key-value pair.
    Set(u64, u64),
    /// `GET <key>` — retrieve a value by key.
    Get(u64),
    /// `DEL <key>` — delete a key.
    Del(u64),
    /// `BENCH <n>` — run n write+read operations, report throughput.
    Bench(u64),
    /// `STATS` — show store configuration and connection info.
    Stats,
    /// `QUIT` — close this connection.
    Quit,
    /// `HELP` — show available commands.
    Help,
    /// Unrecognized command.
    Unknown(String),
    /// Empty line (no-op).
    Empty,
}

/// Parse a single line of input into a [`Command`].
pub fn parse_command(line: &str) -> Command {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Command::Empty;
    }

    let mut parts = trimmed.splitn(3, char::is_whitespace);
    let verb = parts.next().unwrap_or("");

    match verb.to_ascii_uppercase().as_str() {
        "SET" => {
            let key_str = parts.next().unwrap_or("");
            let val_str = parts.next().unwrap_or("").trim();
            match (key_str.parse::<u64>(), val_str.parse::<u64>()) {
                (Ok(k), Ok(v)) => Command::Set(k, v),
                _ => Command::Unknown("SET requires two integers: SET <key> <value>".into()),
            }
        }
        "GET" => match parts.next().unwrap_or("").trim().parse::<u64>() {
            Ok(k) => Command::Get(k),
            Err(_) => Command::Unknown("GET requires an integer key: GET <key>".into()),
        },
        "DEL" => match parts.next().unwrap_or("").trim().parse::<u64>() {
            Ok(k) => Command::Del(k),
            Err(_) => Command::Unknown("DEL requires an integer key: DEL <key>".into()),
        },
        "BENCH" => match parts.next().unwrap_or("").trim().parse::<u64>() {
            Ok(n) if n > 0 => Command::Bench(n),
            _ => Command::Unknown("BENCH requires a positive integer: BENCH <n>".into()),
        },
        "STATS" => Command::Stats,
        "QUIT" | "EXIT" => Command::Quit,
        "HELP" | "?" => Command::Help,
        _ => Command::Unknown(format!("unknown command: {verb}")),
    }
}

// ── Connection Handler ──────────────────────────────────────────────────

/// Handle a single TCP client connection.
///
/// Reads lines from the socket, parses commands, executes them against the
/// FASTER store, and writes responses back. Terminates on `QUIT`, EOF, I/O
/// error, or server shutdown signal.
pub async fn handle_connection(
    socket: TcpStream,
    addr: SocketAddr,
    state: ServerState,
    shutdown_rx: &mut broadcast::Receiver<()>,
) {
    let (reader, mut writer) = socket.into_split();
    let mut reader = BufReader::new(reader);
    let mut line = String::new();

    let welcome = format!("+FASTER KV Server ready ({})\r\n", addr);
    let _ = writer.write_all(welcome.as_bytes()).await;

    loop {
        line.clear();

        tokio::select! {
            result = reader.read_line(&mut line) => {
                match result {
                    Ok(0) => break,
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
            _ = shutdown_rx.recv() => {
                let _ = writer.write_all(b"-Server shutting down\r\n").await;
                break;
            }
        }

        let cmd = parse_command(&line);

        match cmd {
            Command::Quit => {
                let _ = writer.write_all(b"+BYE\r\n").await;
                break;
            }
            Command::Empty => continue,
            _ => {
                let response = execute_command(&state, cmd).await;
                if writer.write_all(response.as_bytes()).await.is_err() {
                    break;
                }
            }
        }
    }
}

// ── Command Execution ───────────────────────────────────────────────────

const HELP_TEXT: &str = "\
+Commands:\r\n\
+  SET <key> <value>  — store a u64 key-value pair\r\n\
+  GET <key>          — retrieve value by key\r\n\
+  DEL <key>          — delete a key\r\n\
+  BENCH <n>          — run n random write+read ops, report throughput\r\n\
+  STATS              — show store info\r\n\
+  HELP               — this message\r\n\
+  QUIT               — close connection\r\n\
+\r\n";

/// Execute a single command against the FASTER store.
///
/// Operations are dispatched to [`tokio::task::spawn_blocking`] because
/// FASTER sessions are `!Send`. Each invocation creates a short-lived
/// session for the operation.
pub async fn execute_command(state: &ServerState, cmd: Command) -> String {
    match cmd {
        Command::Help => HELP_TEXT.to_string(),
        Command::Stats => format_stats(state),
        Command::Empty | Command::Quit => String::new(),
        Command::Unknown(msg) => format!("-ERR {msg}\r\n"),
        // All FASTER operations go through spawn_blocking
        cmd => {
            let store = state.store.clone();
            tokio::task::spawn_blocking(move || execute_on_store(&store, cmd))
                .await
                .unwrap_or_else(|e| format!("-ERR internal: {e}\r\n"))
        }
    }
}

/// Execute a FASTER operation on the blocking thread pool.
///
/// Creates a thread-local session, performs the operation, and returns
/// the response string. The session is dropped (epoch slot freed) when
/// this function returns.
fn execute_on_store(store: &FasterKv<KvFunctions>, cmd: Command) -> String {
    let mut session = store.new_session();

    let result = match cmd {
        Command::Set(key, value) => {
            let status = store.upsert(&mut session, &key, &value, ());
            if status.is_success() {
                "+OK\r\n".to_string()
            } else {
                format!("-ERR upsert failed: {status}\r\n")
            }
        }
        Command::Get(key) => {
            let value: Option<u64> = store.read_simple(&mut session, &key);
            match value {
                Some(v) => format!(":{v}\r\n"),
                None => "$-1\r\n".to_string(),
            }
        }
        Command::Del(key) => {
            let status = store.delete(&mut session, &key, ());
            match status {
                OperationStatus::Deleted => "+OK\r\n".to_string(),
                OperationStatus::NotFound => "$-1\r\n".to_string(),
                other => format!("-ERR delete: {other}\r\n"),
            }
        }
        Command::Bench(n) => run_benchmark(store, &mut session, n),
        _ => String::new(),
    };

    // Session dropped here — epoch table slot freed.
    drop(session);
    result
}

/// Run a write+read benchmark and format the results.
fn run_benchmark(
    store: &FasterKv<KvFunctions>,
    session: &mut faster_core::store::FasterSession<KvFunctions>,
    n: u64,
) -> String {
    let mut rng = rand::rng();

    let start = Instant::now();

    // Phase 1: Random writes
    for _ in 0..n {
        let key: u64 = rng.random_range(0..n.saturating_mul(10).max(1000));
        let value: u64 = rng.random_range(0..u64::MAX);
        let _ = store.upsert(session, &key, &value, ());
    }

    // Phase 2: Random reads
    let mut hits = 0u64;
    for _ in 0..n {
        let key: u64 = rng.random_range(0..n.saturating_mul(10).max(1000));
        let result: Option<u64> = store.read_simple(session, &key);
        if result.is_some() {
            hits += 1;
        }
    }

    let elapsed = start.elapsed();
    let total_ops = n * 2;
    let ops_per_sec = if elapsed.as_secs_f64() > 0.0 {
        total_ops as f64 / elapsed.as_secs_f64()
    } else {
        f64::INFINITY
    };

    format!(
        "+BENCH: {total_ops} ops ({n} writes + {n} reads, {hits} hits) \
         in {elapsed:.3?} ({ops_per_sec:.0} ops/sec)\r\n"
    )
}

/// Format store statistics as a response string.
fn format_stats(state: &ServerState) -> String {
    let config = state.store.config();
    let conns = state.connections.load(Ordering::Relaxed);

    format!(
        "+STATS: hash_index=2^{} ({} buckets), buffer={} pages, \
         mutable_fraction={:.0}%, connections={conns}\r\n",
        config.hash_index_size_log2,
        1u64 << config.hash_index_size_log2,
        config.buffer_size_pages,
        config.mutable_fraction * 100.0,
    )
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use faster_core::store::FasterKvConfig;

    #[test]
    fn parse_set() {
        assert_eq!(parse_command("SET 1 42"), Command::Set(1, 42));
        assert_eq!(parse_command("set 100 200"), Command::Set(100, 200));
    }

    #[test]
    fn parse_get() {
        assert_eq!(parse_command("GET 1"), Command::Get(1));
        assert_eq!(parse_command("get 42"), Command::Get(42));
    }

    #[test]
    fn parse_del() {
        assert_eq!(parse_command("DEL 1"), Command::Del(1));
    }

    #[test]
    fn parse_bench() {
        assert_eq!(parse_command("BENCH 1000"), Command::Bench(1000));
    }

    #[test]
    fn parse_stats() {
        assert_eq!(parse_command("STATS"), Command::Stats);
    }

    #[test]
    fn parse_quit() {
        assert_eq!(parse_command("QUIT"), Command::Quit);
        assert_eq!(parse_command("EXIT"), Command::Quit);
    }

    #[test]
    fn parse_help() {
        assert_eq!(parse_command("HELP"), Command::Help);
        assert_eq!(parse_command("?"), Command::Help);
    }

    #[test]
    fn parse_empty() {
        assert_eq!(parse_command(""), Command::Empty);
        assert_eq!(parse_command("   "), Command::Empty);
    }

    #[test]
    fn parse_unknown() {
        matches!(parse_command("FOOBAR"), Command::Unknown(_));
    }

    #[test]
    fn parse_bad_set() {
        matches!(parse_command("SET abc def"), Command::Unknown(_));
        matches!(parse_command("SET 1"), Command::Unknown(_));
    }

    #[tokio::test]
    async fn execute_crud_roundtrip() {
        let store = Arc::new(FasterKv::new(
            FasterKvConfig::default(),
            SimpleFunctions::<u64, u64>::default(),
            faster_core::NullDevice::new(),
        ));
        let state = ServerState {
            store,
            connections: Arc::new(AtomicU64::new(0)),
        };

        // SET
        let resp = execute_command(&state, Command::Set(1, 42)).await;
        assert_eq!(resp, "+OK\r\n");

        // GET existing
        let resp = execute_command(&state, Command::Get(1)).await;
        assert_eq!(resp, ":42\r\n");

        // GET missing
        let resp = execute_command(&state, Command::Get(999)).await;
        assert_eq!(resp, "$-1\r\n");

        // DEL existing
        let resp = execute_command(&state, Command::Del(1)).await;
        assert_eq!(resp, "+OK\r\n");

        // GET after DEL
        let resp = execute_command(&state, Command::Get(1)).await;
        assert_eq!(resp, "$-1\r\n");

        // DEL missing
        let resp = execute_command(&state, Command::Del(999)).await;
        assert_eq!(resp, "$-1\r\n");
    }

    #[tokio::test]
    async fn execute_bench_completes() {
        let store = Arc::new(FasterKv::new(
            FasterKvConfig::default(),
            SimpleFunctions::<u64, u64>::default(),
            faster_core::NullDevice::new(),
        ));
        let state = ServerState {
            store,
            connections: Arc::new(AtomicU64::new(0)),
        };

        let resp = execute_command(&state, Command::Bench(100)).await;
        assert!(resp.starts_with("+BENCH:"));
        assert!(resp.contains("ops/sec"));
    }

    #[tokio::test]
    async fn execute_stats() {
        let store = Arc::new(FasterKv::new(
            FasterKvConfig::default(),
            SimpleFunctions::<u64, u64>::default(),
            faster_core::NullDevice::new(),
        ));
        let state = ServerState {
            store,
            connections: Arc::new(AtomicU64::new(2)),
        };

        let resp = execute_command(&state, Command::Stats).await;
        assert!(resp.starts_with("+STATS:"));
        assert!(resp.contains("connections=2"));
    }
}
