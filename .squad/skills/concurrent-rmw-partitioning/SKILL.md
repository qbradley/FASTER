# Skill: Partitioned Key Space for Concurrent RMW

## When to Use

When building multi-worker applications with FASTER that perform Read-Modify-Write (RMW) operations. Use this pattern to:
- Avoid lost updates from concurrent RMW on the same keys
- Distribute work across multiple async tasks or threads
- Ensure correct aggregation/counting with `rmw()` operations

## Pattern

### The Problem

FASTER's `rmw()` is lock-free but NOT atomic across concurrent sessions:

```rust
// ❌ WRONG: Lost updates with concurrent RMW
// Thread 1: rmw(key=100, +1) → reads 0, writes 1
// Thread 2: rmw(key=100, +1) → reads 0, writes 1 (RACE)
// Expected: 2, Actual: 1
```

Both workers read the same initial value (0) and write their increment (1), losing one update.

### The Solution: Partition Keys by Worker

Assign each worker a disjoint subset of the key space:

```rust
const NUM_WORKERS: usize = 4;
const KEYS_PER_WORKER: usize = 1000;

fn worker_key_range(worker_id: usize) -> Range<u64> {
    let start = (worker_id * KEYS_PER_WORKER) as u64;
    let end = start + KEYS_PER_WORKER as u64;
    start..end
}

// Worker 0: keys [0, 1000)
// Worker 1: keys [1000, 2000)
// Worker 2: keys [2000, 3000)
// Worker 3: keys [3000, 4000)
```

Each worker only RMWs keys in its partition → no races.

### Implementation Pattern (Tokio)

```rust
use tokio::task::JoinSet;
use faster_core::FasterKv;
use std::sync::Arc;

pub async fn concurrent_rmw_aggregation(
    store: Arc<FasterKv<u64, u64>>,
    num_workers: usize,
    keys_per_worker: usize,
    operations_per_worker: usize,
) -> Result<()> {
    let mut tasks = JoinSet::new();
    
    for worker_id in 0..num_workers {
        let store = Arc::clone(&store);
        
        tasks.spawn(async move {
            let key_start = (worker_id * keys_per_worker) as u64;
            let key_end = key_start + keys_per_worker as u64;
            
            tokio::task::spawn_blocking(move || {
                let mut session = store.session();
                let mut rng = rand::thread_rng();
                
                for _ in 0..operations_per_worker {
                    // Pick a key in THIS worker's partition
                    let key = rng.gen_range(key_start..key_end);
                    
                    // Safe to RMW - no other worker touches this key
                    store.rmw(&mut session, &key, &1)?;
                    
                    if i % 256 == 0 {
                        store.refresh(&mut session);
                    }
                }
                
                Ok::<_, Error>(())
            }).await?
        });
    }
    
    // Wait for all workers
    while let Some(result) = tasks.join_next().await {
        result??;
    }
    
    Ok(())
}
```

### Verification

After all workers complete, verify totals:

```rust
pub fn verify_aggregation(
    store: &FasterKv<u64, u64>,
    num_workers: usize,
    keys_per_worker: usize,
    expected_ops_per_key: u64,
) -> Result<()> {
    let mut session = store.session();
    
    for worker_id in 0..num_workers {
        let key_start = (worker_id * keys_per_worker) as u64;
        let key_end = key_start + keys_per_worker as u64;
        
        for key in key_start..key_end {
            let status = store.read(&mut session, &key)?;
            if let Status::Found(value) = status {
                assert_eq!(value, expected_ops_per_key,
                    "Key {} mismatch: expected {}, got {}",
                    key, expected_ops_per_key, value);
            }
        }
    }
    
    Ok(())
}
```

### Alternative: Hash-Based Partitioning

For non-sequential key spaces (e.g., strings, UUIDs):

```rust
fn worker_for_key<K: Hash>(key: &K, num_workers: usize) -> usize {
    let mut hasher = DefaultHasher::new();
    key.hash(&mut hasher);
    (hasher.finish() % num_workers as u64) as usize
}

// Each worker only processes keys that hash to its ID
if worker_for_key(&key, num_workers) == worker_id {
    store.rmw(&mut session, &key, &delta)?;
}
```

### When NOT to Partition

✅ **No partitioning needed when:**
- Operations are read-only (`read()`, `scan()`)
- Operations are full overwrites (`upsert()`, `delete()`)
- Using a single writer thread (no concurrency)

❌ **Partitioning required when:**
- Multiple workers call `rmw()` on overlapping key sets
- Aggregating/counting events across threads/tasks
- Building histograms or statistics

### Checklist

- [ ] Key space divided into non-overlapping partitions
- [ ] Each worker assigned a specific partition (range or hash-based)
- [ ] Workers never RMW keys outside their partition
- [ ] Verification test checks all partitions independently
- [ ] Consider using `upsert()` if full overwrite is acceptable (no partitioning needed)

## Confidence: high

## Learned From

- **event-counter-tokio sample (2026-03-07):** Learned the hard way that concurrent RMW without partitioning loses updates. Original implementation had workers randomly selecting from full key space → totals were incorrect. Fixed by partitioning campaigns per worker → verification passed.
- **Integration tests (2026-03-10):** Concurrent multi-session test uses 8 workers with partitioned key ranges (200 keys each) to verify cross-task visibility without RMW conflicts.
