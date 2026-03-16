# Skill: Async/Sync Bridge Pattern (Callback→Future)

## When to Use

When bridging FASTER's completion-callback model (where callbacks fire from OS threads) to Rust's async/await ecosystem. Use this when:
- Exposing FASTER operations as Futures for Tokio/async-std users
- Converting `CompletionCallback` into awaitable operations
- Building async wrappers around core sync APIs

## Pattern

### Core Types

```rust
// Zero-alloc path for synchronous completions
pub enum MaybePending<T> {
    Ready(T),
    Pending(PendingFuture<T>),
}

// Linked pair for async completions
pub struct PendingFuture<T> { shared: Arc<Mutex<SharedState<T>>> }
pub struct CompletionSender<T> { shared: Arc<Mutex<SharedState<T>>> }

struct SharedState<T> {
    result: Option<T>,
    waker: Option<Waker>,
}
```

### Implementation Checklist

1. **Runtime-agnostic:** Use ONLY `std::future::Future` and `std::task::Waker`
   - ❌ No Tokio types (no `tokio::sync::oneshot`, no `JoinHandle`)
   - ✅ Single allocation: `Arc<Mutex<SharedState>>`

2. **IntoFuture impl:** `MaybePending<T>` should impl `IntoFuture` for `.await` ergonomics
   ```rust
   impl<T> IntoFuture for MaybePending<T> {
       type Output = T;
       type IntoFuture = MaybePendingFuture<T>;
   }
   ```

3. **Unpin safety:** Mark `MaybePendingFuture<T>: Unpin` if T is never structurally pinned
   ```rust
   impl<T> Unpin for MaybePendingFuture<T> {}
   ```

4. **Waker replacement:** Latest waker wins (Future contract requirement)
   ```rust
   fn poll(&mut self, cx: &mut Context<'_>) -> Poll<T> {
       let mut state = self.shared.lock().unwrap();
       if let Some(result) = state.result.take() {
           Poll::Ready(result)
       } else {
           state.waker = Some(cx.waker().clone()); // Replace old waker
           Poll::Pending
       }
   }
   ```

5. **Completion from any thread:**
   ```rust
   impl<T> CompletionSender<T> {
       pub fn complete(self, result: T) {
           let mut state = self.shared.lock().unwrap();
           state.result = Some(result);
           if let Some(waker) = state.waker.take() {
               waker.wake();
           }
       }
   }
   ```

6. **Drop safety:**
   - Dropping future before completion: No-op for sender (orphaned result)
   - Dropping sender without completing: Future permanently pending (expected)

### Testing Requirements

- ✅ Multi-threaded completion (100+ concurrent futures from OS threads)
- ✅ Waker replacement scenario
- ✅ Both drop-safety cases
- ✅ Synchronous Ready path (no allocation)
- ✅ Async Pending path (single Arc allocation)

### Example Usage

```rust
// In core library (sync)
pub fn read_async<K>(&mut self, key: &K) -> MaybePending<ReadResult> {
    if self.can_complete_sync(key) {
        MaybePending::Ready(self.read_sync(key))
    } else {
        let (future, sender) = PendingFuture::new();
        self.dispatch_io(key, move |result| sender.complete(result));
        MaybePending::Pending(future)
    }
}

// In async wrapper (Tokio/async-std)
pub async fn read<K>(&self, key: &K) -> ReadResult {
    self.inner.read_async(key).await
}
```

## Confidence: high

## Learned From

- **Wave 4 T1 (2026-03-06):** Initial `bridge.rs` implementation with 9 unit tests + 4 doc-tests
- **Integration tests (2026-03-10):** 29 tests exercising the bridge under concurrent load, timeouts, shutdown scenarios
- **tokio-kv-server sample (2026-03-06):** Real-world usage with TCP server + spawn_blocking
