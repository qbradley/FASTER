# tokio-kv-server

An async TCP key-value server built on **FASTER** + **Tokio**, demonstrating
how to integrate FASTER's high-performance concurrent hash map with Tokio's
async runtime.

## What This Demonstrates

| Concept | Implementation |
|---------|---------------|
| **AsyncFasterKv** lifecycle | Store creation with background maintenance, graceful shutdown |
| **!Send session handling** | `spawn_blocking` bridge for thread-affine FASTER sessions |
| **Multi-client TCP server** | `tokio::spawn` per connection with shared `Arc<FasterKv>` |
| **Graceful shutdown** | `tokio::signal::ctrl_c()` + `broadcast` channel coordination |
| **Text protocol** | Line-based command parsing, suitable for `nc`/`telnet` |

## Architecture

```
                         ┌─────────────────────┐
                         │   AsyncFasterKv      │
                         │   (background maint) │
                         └──────────┬──────────┘
                                    │ store_arc()
              ┌─────────────────────┼─────────────────────┐
              │                     │                     │
     ┌────────▼────────┐  ┌────────▼────────┐  ┌────────▼────────┐
     │  Connection #1   │  │  Connection #2   │  │  Connection #N   │
     │  (tokio::spawn)  │  │  (tokio::spawn)  │  │  (tokio::spawn)  │
     └────────┬────────┘  └────────┬────────┘  └────────┬────────┘
              │                     │                     │
        spawn_blocking        spawn_blocking        spawn_blocking
              │                     │                     │
     ┌────────▼────────┐  ┌────────▼────────┐  ┌────────▼────────┐
     │  FasterSession   │  │  FasterSession   │  │  FasterSession   │
     │  (!Send, local)  │  │  (!Send, local)  │  │  (!Send, local)  │
     └─────────────────┘  └─────────────────┘  └─────────────────┘
```

Each command is dispatched to `spawn_blocking` where a short-lived
`FasterSession` is created, the operation is performed, and the session
is dropped. This respects FASTER's thread-affine session model while
keeping the connection handler fully async.

## Running

```bash
# Start the server (default port 8888, 4 worker threads)
cargo run -p tokio-kv-server

# Custom configuration
cargo run -p tokio-kv-server -- --port 9999 --threads 8 --log-size 22

# Show all options
cargo run -p tokio-kv-server -- --help
```

## Protocol

Connect with `nc`, `telnet`, or any TCP client:

```bash
nc localhost 8888
```

### Commands

| Command | Description | Response |
|---------|-------------|----------|
| `SET <key> <value>` | Store a u64 key-value pair | `+OK` |
| `GET <key>` | Retrieve value by key | `:<value>` or `$-1` (not found) |
| `DEL <key>` | Delete a key | `+OK` or `$-1` (not found) |
| `BENCH <n>` | Run n random write+read ops | `+BENCH: ...` throughput report |
| `STATS` | Show store configuration | `+STATS: ...` |
| `HELP` | Show available commands | Multi-line help text |
| `QUIT` | Close connection | `+BYE` |

### Example Session

```
$ nc localhost 8888
+FASTER KV Server ready (127.0.0.1:56789)
SET 1 42
+OK
GET 1
:42
SET 2 100
+OK
GET 999
$-1
BENCH 10000
+BENCH: 20000 ops (10000 writes + 10000 reads, 5012 hits) in 8.234ms (2429012 ops/sec)
STATS
+STATS: hash_index=2^20 (1048576 buckets), buffer=16 pages, mutable_fraction=90%, connections=1
DEL 1
+OK
GET 1
$-1
QUIT
+BYE
```

## Testing

```bash
# Unit tests (command parsing, CRUD logic)
cargo test -p tokio-kv-server --lib

# Integration tests (full TCP round-trip)
cargo test -p tokio-kv-server --test integration

# All tests
cargo test -p tokio-kv-server
```

## Design Notes

### Why `spawn_blocking`?

FASTER sessions are `!Send` — they use thread-local epoch protection for
lock-free concurrent access. Tokio's `spawn` requires `Send` futures, so
we bridge the gap with `spawn_blocking`, which runs closures on a
dedicated thread pool.

For a production server, you'd want to keep a long-lived session per
worker thread (using channels or a session pool) rather than creating one
per command. This sample prioritizes clarity over micro-optimization.

### Why u64 keys and values?

FASTER's `SimpleFunctions<K, V>` requires `V: Copy`, which rules out
`String` and `Vec<u8>`. For variable-length values, implement the
[`Functions`](faster_core::store::Functions) trait directly. This sample
uses `u64` to keep the focus on the Tokio integration patterns.
