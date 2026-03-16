# Skill: Runtime-Agnostic Async Design

## When to Use

When designing async layers for FASTER that work with any async runtime (Tokio, async-std, smol, or future runtimes). Use this pattern to:
- Avoid hard-coding runtime dependencies in library code
- Support feature flags for optional runtime integration
- Build async interfaces that live in `std`, not runtime-specific crates

## Pattern

### Architecture Layers

```
┌─────────────────────────────────────────┐
│  faster-tokio (feature: tokio)         │  ← Runtime-specific convenience
│  - TokioFileDevice                      │
│  - AsyncFasterKv (background tasks)     │
└─────────────────────────────────────────┘
                    ↓ uses
┌─────────────────────────────────────────┐
│  faster-core/async_bridge.rs            │  ← Runtime-agnostic
│  - MaybePending<T>                       │
│  - PendingFuture<T>                      │
│  - Only std::future + std::task          │
└─────────────────────────────────────────┘
                    ↓ bridges
┌─────────────────────────────────────────┐
│  faster-core (pure sync)                 │  ← No async
│  - FasterKv, FasterSession               │
│  - CompletionCallback model              │
│  - std::thread + channels                │
└─────────────────────────────────────────┘
```

### Design Principles

1. **Core is sync + threaded:**
   - ❌ No `async fn` in faster-core
   - ❌ No async runtime dependencies (tokio, async-std, futures)
   - ✅ `std::thread` for background I/O
   - ✅ `std::sync::mpsc` for channels

2. **Bridge is runtime-agnostic:**
   - ✅ Only `std::future::Future` trait
   - ✅ Only `std::task::{Context, Poll, Waker}`
   - ❌ No `tokio::sync`, `async_std::sync`, `futures::channel`
   - ✅ Single crate for the bridge (`faster-core/async_bridge.rs` or separate `faster-async`)

3. **Runtime-specific crates are thin:**
   - `faster-tokio`: `TokioFileDevice`, `AsyncFasterKv` with `tokio::spawn` background tasks
   - `faster-async-std`: `AsyncStdFileDevice`, similar pattern
   - Each depends on `faster-core` and its respective runtime

### Feature Flag Pattern

```toml
# faster-core/Cargo.toml
[features]
default = []
async = []  # Enables async_bridge module

# faster-tokio/Cargo.toml
[dependencies]
faster-core = { path = "../faster-core", features = ["async"] }
tokio = { version = "1", features = ["rt", "fs", "sync"] }

[features]
default = ["tokio/rt-multi-thread"]
```

### Public API Pattern

```rust
// In faster-core (runtime-agnostic)
#[cfg(feature = "async")]
pub mod async_bridge {
    pub enum MaybePending<T> {
        Ready(T),
        Pending(PendingFuture<T>),
    }
    
    pub struct PendingFuture<T> { /* std-only types */ }
    
    impl<T> IntoFuture for MaybePending<T> {
        type Output = T;
        type IntoFuture = MaybePendingFuture<T>;
    }
}

// In faster-tokio (Tokio-specific)
pub struct AsyncFasterKv<K, V> {
    inner: Arc<FasterKv<K, V>>,
    maintenance_handle: JoinHandle<()>,  // OK here
}

impl<K, V> AsyncFasterKv<K, V> {
    pub async fn upsert(&self, key: &K, value: &V) -> Result<Status> {
        let inner = Arc::clone(&self.inner);
        tokio::task::spawn_blocking(move || {
            let mut session = inner.session();
            inner.upsert(&mut session, key, value)
        }).await?
    }
}
```

### Testing Strategy

1. **Core bridge tests:** No `#[tokio::test]` for the bridge itself
   ```rust
   #[test]
   fn ready_path_no_allocation() { /* ... */ }
   ```

2. **Runtime integration tests:** Separate test files with runtime features
   ```rust
   // tests/tokio_integration.rs
   #[tokio::test]
   async fn concurrent_futures_from_threads() { /* ... */ }
   ```

3. **Multi-runtime CI:** Test matrix for Tokio, async-std, smol

### Checklist

- [ ] Core library has zero async runtime dependencies
- [ ] Bridge types use ONLY `std::future` and `std::task`
- [ ] Runtime-specific types live in separate crates
- [ ] Feature flags enable optional integration, not core functionality
- [ ] Public API does NOT leak `tokio::*` or `async_std::*` types (use generics/traits)
- [ ] Documentation shows examples for multiple runtimes

### Anti-Patterns

❌ **Leaking runtime types in public API:**
```rust
pub fn new(executor: tokio::runtime::Handle) -> Self { /* ... */ }
```

❌ **Hard dependency on runtime:**
```toml
[dependencies]
tokio = "1"  # ❌ Should be optional
```

❌ **Runtime-specific code in core:**
```rust
// In faster-core
use tokio::sync::Mutex;  // ❌ Wrong crate
```

## Confidence: high

## Learned From

- **faster-tokio bridge.rs (2026-03-06):** Zero external deps beyond `std` for the bridge. `tokio` is dev-dependency only for tests.
- **Architecture review (2026-03-05):** Gandalf's Decision #3 - "no async in core, custom epoch."
- **Integration tests (2026-03-10):** Demonstrated that runtime-agnostic bridge compiles and tests without Tokio features.
