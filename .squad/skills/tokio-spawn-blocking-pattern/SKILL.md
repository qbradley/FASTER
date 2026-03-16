# Skill: Tokio spawn_blocking for !Send Sessions

## When to Use

When integrating FASTER's `!Send` sessions (thread-affine epoch protection) with Tokio's async runtime. Use this pattern when:
- Building async servers/services with Tokio that need FASTER operations
- Each request handler runs as an async task (`tokio::spawn`)
- FASTER sessions cannot cross `.await` boundaries (not Send)

## Pattern

### The Problem

`FasterSession` is `!Send` because:
- Thread-affine epoch protection (pinned to thread-local slot)
- Cannot be sent across threads or held across `.await` points
- This conflicts with Tokio's work-stealing async executor

### The Solution: spawn_blocking with Short-Lived Sessions

```rust
// ❌ WRONG: Session cannot cross .await
pub async fn handle_request(store: Arc<FasterKv>, key: u64) -> Result<u64> {
    let mut session = store.session();  // Pinned to current OS thread
    let result = store.read(&mut session, &key).await?; // ❌ .await moves session
    Ok(result)
}

// ✅ CORRECT: Session lives entirely within spawn_blocking
pub async fn handle_request(store: Arc<FasterKv>, key: u64) -> Result<u64> {
    tokio::task::spawn_blocking(move || {
        let mut session = store.session(); // Created on blocking pool
        store.read(&mut session, &key)     // No .await - runs to completion
    }).await?
}
```

### Implementation Patterns

#### Pattern 1: Session-per-Command (Simple, Correct)

Best for:
- Interactive servers (TCP/HTTP)
- Low request rate
- Simplicity over maximum throughput

```rust
pub struct AsyncKvServer {
    store: Arc<FasterKv<u64, u64>>,
}

impl AsyncKvServer {
    pub async fn handle_connection(&self, socket: TcpStream) {
        let (reader, writer) = socket.split();
        let mut lines = BufReader::new(reader).lines();
        
        while let Some(line) = lines.next_line().await.unwrap() {
            let cmd = parse_command(&line);
            let store = Arc::clone(&self.store);
            
            let result = tokio::task::spawn_blocking(move || {
                let mut session = store.session();
                match cmd {
                    Command::Get(key) => store.read(&mut session, &key),
                    Command::Set(key, val) => store.upsert(&mut session, &key, &val),
                    Command::Delete(key) => store.delete(&mut session, &key),
                }
            }).await.unwrap();
            
            // Send response via writer
        }
    }
}
```

#### Pattern 2: Dedicated Worker Threads (High Throughput)

Best for:
- Production services
- High request rate
- Want to amortize session creation cost

```rust
pub struct KvWorkerPool {
    channels: Vec<mpsc::Sender<WorkItem>>,
}

impl KvWorkerPool {
    pub fn new(store: Arc<FasterKv>, num_workers: usize) -> Self {
        let mut channels = Vec::new();
        
        for _ in 0..num_workers {
            let (tx, mut rx) = mpsc::channel(1024);
            let store = Arc::clone(&store);
            
            std::thread::spawn(move || {
                let mut session = store.session(); // Long-lived
                while let Some(item) = rx.blocking_recv() {
                    let result = match item.cmd {
                        Command::Get(key) => store.read(&mut session, &key),
                        // ...
                    };
                    store.refresh(&mut session); // Periodic epoch refresh
                    item.reply.send(result).ok();
                }
            });
            
            channels.push(tx);
        }
        
        Self { channels }
    }
    
    pub async fn execute(&self, cmd: Command) -> Result<Value> {
        let worker = self.select_worker(&cmd);
        let (tx, rx) = oneshot::channel();
        self.channels[worker].send(WorkItem { cmd, reply: tx }).await?;
        rx.await?
    }
}
```

### Checklist

1. **Session lifetime:** Entirely within `spawn_blocking` closure (no .await inside)
2. **Store wrapping:** Wrap `FasterKv` in `Arc` for clone-per-request
3. **Shutdown:** Use `tokio::sync::broadcast` to signal workers to exit
4. **Maintenance:** Call `store.refresh(&mut session)` every N ops (256 is typical)
5. **Error handling:** `spawn_blocking` returns `JoinHandle<Result<T>>` - unwrap twice

### Common Mistakes

❌ **Holding session across .await:**
```rust
let mut session = store.session();
tokio::time::sleep(Duration::from_secs(1)).await; // ❌ Compile error
```

❌ **Forgetting to refresh long-lived sessions:**
```rust
// Worker never calls refresh() → epoch slots exhausted
```

❌ **Not using Arc for store:**
```rust
tokio::spawn_blocking(|| store.upsert(...)); // ❌ Can't move store
```

## Confidence: high

## Learned From

- **tokio-kv-server sample (2026-03-06):** Session-per-command pattern with spawn_blocking. 13 unit tests + 7 integration tests.
- **read-cache-sim-tokio (2026-03-07):** Worker threads calling `refresh()` every 256 ops.
- **event-counter-tokio (2026-03-07):** Partitioned key space per worker to avoid concurrent RMW races.
- **Integration tests (2026-03-10):** 8 concurrent spawn_blocking tasks, 1600 keys, cross-task visibility verified.
