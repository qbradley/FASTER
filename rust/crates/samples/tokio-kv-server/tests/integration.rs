//! Integration tests for the tokio-kv-server.
//!
//! Starts an in-process server on a random port, connects as a TCP client,
//! and verifies CRUD operations through the text protocol.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use faster_core::NullDevice;
use faster_core::store::{FasterKvConfig, SimpleFunctions};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast;
use tokio_kv_server::{ServerState, handle_connection};

/// Spin up a test server and return (address, shutdown sender).
async fn start_test_server() -> (std::net::SocketAddr, broadcast::Sender<()>) {
    let store = Arc::new(faster_core::store::FasterKv::new(
        FasterKvConfig::default(),
        SimpleFunctions::<u64, u64>::default(),
        NullDevice::new(),
    ));

    let state = ServerState {
        store,
        connections: Arc::new(AtomicU64::new(0)),
    };

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (shutdown_tx, _) = broadcast::channel::<()>(1);
    let tx = shutdown_tx.clone();

    tokio::spawn(async move {
        let mut shutdown_rx = shutdown_tx.subscribe();
        loop {
            tokio::select! {
                result = listener.accept() => {
                    match result {
                        Ok((socket, peer)) => {
                            state.connections.fetch_add(1, Ordering::Relaxed);
                            let conn_state = state.clone();
                            let mut rx = shutdown_tx.subscribe();
                            tokio::spawn(async move {
                                handle_connection(socket, peer, conn_state.clone(), &mut rx).await;
                                conn_state.connections.fetch_sub(1, Ordering::Relaxed);
                            });
                        }
                        Err(_) => break,
                    }
                }
                _ = shutdown_rx.recv() => break,
            }
        }
    });

    (addr, tx)
}

/// Helper: connect to the server and consume the welcome banner.
async fn connect(
    addr: std::net::SocketAddr,
) -> (
    BufReader<tokio::net::tcp::OwnedReadHalf>,
    tokio::net::tcp::OwnedWriteHalf,
) {
    let stream = TcpStream::connect(addr).await.unwrap();
    let (reader, writer) = stream.into_split();
    let mut reader = BufReader::new(reader);

    // Consume welcome line
    let mut welcome = String::new();
    reader.read_line(&mut welcome).await.unwrap();
    assert!(welcome.starts_with("+FASTER KV Server ready"));

    (reader, writer)
}

/// Send a command and read the response line.
async fn roundtrip(
    reader: &mut BufReader<tokio::net::tcp::OwnedReadHalf>,
    writer: &mut tokio::net::tcp::OwnedWriteHalf,
    cmd: &str,
) -> String {
    writer
        .write_all(format!("{cmd}\r\n").as_bytes())
        .await
        .unwrap();
    let mut response = String::new();
    reader.read_line(&mut response).await.unwrap();
    response
}

// ── Tests ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn crud_operations() {
    let (addr, shutdown) = start_test_server().await;
    let (mut reader, mut writer) = connect(addr).await;

    // SET
    let resp = roundtrip(&mut reader, &mut writer, "SET 1 42").await;
    assert_eq!(resp.trim(), "+OK");

    // GET existing
    let resp = roundtrip(&mut reader, &mut writer, "GET 1").await;
    assert_eq!(resp.trim(), ":42");

    // GET missing
    let resp = roundtrip(&mut reader, &mut writer, "GET 999").await;
    assert_eq!(resp.trim(), "$-1");

    // Overwrite
    let resp = roundtrip(&mut reader, &mut writer, "SET 1 100").await;
    assert_eq!(resp.trim(), "+OK");
    let resp = roundtrip(&mut reader, &mut writer, "GET 1").await;
    assert_eq!(resp.trim(), ":100");

    // DEL existing
    let resp = roundtrip(&mut reader, &mut writer, "DEL 1").await;
    assert_eq!(resp.trim(), "+OK");

    // GET after DEL
    let resp = roundtrip(&mut reader, &mut writer, "GET 1").await;
    assert_eq!(resp.trim(), "$-1");

    // DEL missing
    let resp = roundtrip(&mut reader, &mut writer, "DEL 999").await;
    assert_eq!(resp.trim(), "$-1");

    // QUIT
    writer.write_all(b"QUIT\r\n").await.unwrap();
    let mut bye = String::new();
    reader.read_line(&mut bye).await.unwrap();
    assert_eq!(bye.trim(), "+BYE");

    let _ = shutdown.send(());
}

#[tokio::test]
async fn bench_command() {
    let (addr, shutdown) = start_test_server().await;
    let (mut reader, mut writer) = connect(addr).await;

    let resp = roundtrip(&mut reader, &mut writer, "BENCH 100").await;
    assert!(resp.starts_with("+BENCH:"), "unexpected: {resp}");
    assert!(resp.contains("ops/sec"), "missing throughput: {resp}");

    let _ = shutdown.send(());
}

#[tokio::test]
async fn stats_command() {
    let (addr, shutdown) = start_test_server().await;
    let (mut reader, mut writer) = connect(addr).await;

    let resp = roundtrip(&mut reader, &mut writer, "STATS").await;
    assert!(resp.starts_with("+STATS:"), "unexpected: {resp}");
    assert!(resp.contains("buckets"), "missing bucket info: {resp}");

    let _ = shutdown.send(());
}

#[tokio::test]
async fn help_command() {
    let (addr, shutdown) = start_test_server().await;
    let (mut reader, mut writer) = connect(addr).await;

    writer.write_all(b"HELP\r\n").await.unwrap();

    // HELP returns multiple lines; read until we see the blank terminator.
    let mut lines = Vec::new();
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        if line.trim() == "+" {
            break;
        }
        lines.push(line);
    }
    assert!(!lines.is_empty(), "HELP should return text");
    assert!(lines.iter().any(|l| l.contains("SET")));
    assert!(lines.iter().any(|l| l.contains("GET")));

    let _ = shutdown.send(());
}

#[tokio::test]
async fn unknown_command() {
    let (addr, shutdown) = start_test_server().await;
    let (mut reader, mut writer) = connect(addr).await;

    let resp = roundtrip(&mut reader, &mut writer, "FOOBAR").await;
    assert!(resp.starts_with("-ERR"), "unexpected: {resp}");

    let _ = shutdown.send(());
}

#[tokio::test]
async fn multiple_clients() {
    let (addr, shutdown) = start_test_server().await;

    // Client A writes, Client B reads
    let (mut ra, mut wa) = connect(addr).await;
    let (mut rb, mut wb) = connect(addr).await;

    // A sets key 10
    let resp = roundtrip(&mut ra, &mut wa, "SET 10 555").await;
    assert_eq!(resp.trim(), "+OK");

    // B reads key 10
    let resp = roundtrip(&mut rb, &mut wb, "GET 10").await;
    assert_eq!(resp.trim(), ":555");

    // B sets key 20
    let resp = roundtrip(&mut rb, &mut wb, "SET 20 777").await;
    assert_eq!(resp.trim(), "+OK");

    // A reads key 20
    let resp = roundtrip(&mut ra, &mut wa, "GET 20").await;
    assert_eq!(resp.trim(), ":777");

    let _ = shutdown.send(());
}

#[tokio::test]
async fn case_insensitive_commands() {
    let (addr, shutdown) = start_test_server().await;
    let (mut reader, mut writer) = connect(addr).await;

    let resp = roundtrip(&mut reader, &mut writer, "set 5 50").await;
    assert_eq!(resp.trim(), "+OK");

    let resp = roundtrip(&mut reader, &mut writer, "get 5").await;
    assert_eq!(resp.trim(), ":50");

    let resp = roundtrip(&mut reader, &mut writer, "del 5").await;
    assert_eq!(resp.trim(), "+OK");

    let _ = shutdown.send(());
}
