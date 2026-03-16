# Skill: TokioFileDevice Checkpoint Compatibility

## When to Use

When creating `TokioFileDevice` instances in async/Tokio code that need to be checkpoint/recovery compatible. Critical for:
- Sample applications demonstrating checkpoint/recovery
- Async benchmarks that checkpoint progress
- Production services with persistence requirements

## Pattern

### The Problem

FASTER's recovery system expects:
1. **Segment naming:** Log files must be named `log.{n}` (e.g., `log.0`, `log.1`, ...)
2. **Directory coupling:** Checkpoint metadata and log segments must be in the same directory
3. **Recovery device:** Must use the same prefix pattern to find segments

If any of these is wrong, `recover()` fails with "No segments found" or metadata mismatch.

### The Solution: Matching Prefixes

```rust
// ✅ CORRECT: Checkpoint-compatible setup
let device = TokioFileDevice::new(
    "./data",         // base_path - where segments are written
    SegmentSize::new(32 * 1024 * 1024).unwrap(),
    8,                // max_concurrent_io
    "log."            // ❗ CRITICAL: Must be "log." for recovery
)?;

let store = FasterKv::builder()
    .with_disk(device.clone())
    .build()?;

// Checkpoint writes metadata to checkpoint_dir, expects segments in same dir
store.checkpoint("./data/checkpoints/chk1")?;

// Recovery expects log.{n} files in parent of checkpoint metadata
let recovered = FasterKv::builder()
    .with_disk(device)
    .build()?;
recovered.recover("./data/checkpoints/chk1")?;
```

### Common Mistakes

❌ **Wrong prefix:**
```rust
let device = TokioFileDevice::new("./data", size, 8, "segment.")?;
// Writes: segment.0, segment.1, ...
// Recovery looks for: log.0, log.1, ... → NOT FOUND
```

❌ **Separate directories:**
```rust
let device = TokioFileDevice::new("./data/log", size, 8, "log.")?;
store.checkpoint("./data/checkpoints/chk1")?;
// Checkpoint metadata in ./data/checkpoints/chk1/
// Log segments in ./data/log/
// Recovery looks in ./data/checkpoints/chk1/ for log.{n} → NOT FOUND
```

❌ **Missing dot in prefix:**
```rust
let device = TokioFileDevice::new("./data", size, 8, "log")?;
// Writes: log0, log1, log2 (no dot separator)
// Recovery regex expects: log.{n} → NOT FOUND
```

### Directory Structure

```
./data/
├── log.0              ← Segment 0
├── log.1              ← Segment 1
├── log.2              ← Segment 2
└── checkpoints/
    └── chk1/
        ├── checkpoint.meta   ← Points to ../log.{n}
        └── index.dat
```

### Full Pattern

```rust
use faster_tokio::{TokioFileDevice, AsyncFasterKv};
use faster_core::{FasterKv, SegmentSize};

pub async fn checkpoint_recover_workflow() -> Result<()> {
    let data_dir = "./data";
    let checkpoint_dir = format!("{}/checkpoints/chk1", data_dir);
    
    // 1. Create device with "log." prefix
    let device = TokioFileDevice::new(
        data_dir,
        SegmentSize::new(32 * 1024 * 1024).unwrap(),
        8,
        "log."  // ← Critical
    )?;
    
    // 2. Build store and populate
    let store = FasterKv::builder()
        .with_disk(device.clone())
        .build()?;
    
    // Wrap in AsyncFasterKv for async operations
    let async_store = AsyncFasterKv::from(store);
    
    // Insert data...
    
    // 3. Checkpoint to subdirectory
    async_store.checkpoint(&checkpoint_dir).await?;
    async_store.shutdown().await?;
    
    // 4. Recovery - SAME device setup
    let recovery_device = TokioFileDevice::new(
        data_dir,          // Same base path
        SegmentSize::new(32 * 1024 * 1024).unwrap(),
        8,
        "log."             // Same prefix
    )?;
    
    let recovered = FasterKv::builder()
        .with_disk(recovery_device)
        .build()?;
    
    // 5. Recover from checkpoint
    recovered.recover(&checkpoint_dir)?;
    
    // Verify data...
    
    Ok(())
}
```

### SyncFileDevice Equivalent

The pattern is identical for `SyncFileDevice`:

```rust
let device = SyncFileDevice::new(
    data_dir,
    SegmentSize::new(32 * 1024 * 1024).unwrap(),
    4,          // num_threads for sync device
    "log."      // Same requirement
)?;
```

### Checklist

- [ ] Device prefix is exactly `"log."` (with trailing dot)
- [ ] Base path is parent of checkpoint subdirectory
- [ ] Same prefix used for both initial device and recovery device
- [ ] Checkpoint directory is a subdirectory of base path (or same directory)
- [ ] Test recovery before deploying to production

## Confidence: high

## Learned From

- **event-counter-tokio sample (2026-03-07):** Learned the hard way that `TokioFileDevice::new` prefix must be `"log."` for recovery to find segments. Data/checkpoint must be in compatible directories.
- **Wave 2 team context (2026-03-09):** Boromir filed the decision that `SyncFileDevice` prefix is coupled to recovery expectations.
- **Integration tests (2026-03-10):** Checkpoint + recovery test with TokioFileDevice validates 200-key write → checkpoint → reopen → recover → verify.
