//! Async key-value server powered by FASTER + Tokio.
//!
//! A sample application demonstrating how to integrate FASTER's
//! high-performance concurrent hash map with Tokio for an async
//! TCP server. Run with `--help` for options.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use clap::Parser;
use faster_core::NullDevice;
use faster_core::store::{FasterKvConfig, SimpleFunctions};
use faster_tokio::AsyncFasterKv;
use tokio::net::TcpListener;
use tokio::sync::broadcast;
use tokio_kv_server::{KvFunctions, ServerState, handle_connection};

// ── CLI ─────────────────────────────────────────────────────────────────

/// Async key-value server powered by FASTER + Tokio.
///
/// Starts a TCP server that accepts text-based commands for storing,
/// retrieving, and deleting u64 key-value pairs. Uses FASTER's
/// high-performance concurrent hash map as the backing store.
#[derive(Parser, Debug)]
#[command(name = "tokio-kv-server", version, about)]
struct Cli {
    /// TCP port to listen on.
    #[arg(long, default_value_t = 8888)]
    port: u16,

    /// Data directory (reserved for future file-backed device support).
    #[arg(long)]
    data_dir: Option<String>,

    /// FASTER hash index size as log₂ of bucket count.
    /// Default 20 = 1M buckets; increase for larger datasets.
    #[arg(long, default_value_t = 20)]
    log_size: usize,

    /// Number of Tokio worker threads.
    #[arg(long, default_value_t = 4)]
    threads: usize,
}

// ── Main ────────────────────────────────────────────────────────────────

fn main() {
    let cli = Cli::parse();

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(cli.threads)
        .enable_all()
        .build()
        .expect("failed to build Tokio runtime");

    rt.block_on(run(cli));
}

async fn run(cli: Cli) {
    // ── FASTER store with async background maintenance ──────────
    let config = FasterKvConfig {
        hash_index_size_log2: cli.log_size,
        ..FasterKvConfig::default()
    };

    let mut kv: AsyncFasterKv<KvFunctions> =
        AsyncFasterKv::with_defaults(config, SimpleFunctions::default(), NullDevice::new());

    let state = ServerState {
        store: kv.store_arc(),
        connections: Arc::new(AtomicU64::new(0)),
    };

    // ── TCP listener ────────────────────────────────────────────
    let addr = format!("0.0.0.0:{}", cli.port);
    let listener = TcpListener::bind(&addr)
        .await
        .unwrap_or_else(|e| panic!("failed to bind {addr}: {e}"));

    eprintln!("╔══════════════════════════════════════════════╗");
    eprintln!("║   FASTER KV Server (Tokio)                  ║");
    eprintln!("╠══════════════════════════════════════════════╣");
    eprintln!("║  Listening: {:<33}║", addr);
    eprintln!("║  Workers:   {:<33}║", cli.threads);
    eprintln!(
        "║  Index:     2^{} ({} buckets) {:<15}║",
        cli.log_size,
        1u64 << cli.log_size,
        ""
    );
    eprintln!("╠══════════════════════════════════════════════╣");
    eprintln!("║  Connect:   nc localhost {:<20}║", cli.port);
    eprintln!("║  Shutdown:  Ctrl+C                          ║");
    eprintln!("╚══════════════════════════════════════════════╝");

    // ── Shutdown coordination ───────────────────────────────────
    let (shutdown_tx, _) = broadcast::channel::<()>(1);

    // ── Accept loop ─────────────────────────────────────────────
    loop {
        tokio::select! {
            result = listener.accept() => {
                match result {
                    Ok((socket, addr)) => {
                        let n = state.connections.fetch_add(1, Ordering::Relaxed) + 1;
                        eprintln!("[{addr}] connected (#{n} active)");

                        let conn_state = state.clone();
                        let mut shutdown_rx = shutdown_tx.subscribe();

                        tokio::spawn(async move {
                            handle_connection(
                                socket, addr, conn_state.clone(), &mut shutdown_rx,
                            ).await;
                            let remaining = conn_state.connections.fetch_sub(1, Ordering::Relaxed) - 1;
                            eprintln!("[{addr}] disconnected ({remaining} active)");
                        });
                    }
                    Err(e) => eprintln!("accept error: {e}"),
                }
            }
            _ = tokio::signal::ctrl_c() => {
                eprintln!("\nShutting down gracefully...");
                let _ = shutdown_tx.send(());
                break;
            }
        }
    }

    // ── Graceful shutdown ───────────────────────────────────────
    kv.shutdown().await;
    eprintln!("Server stopped.");
}
