# Skill: Test Device Authoring for FASTER-Rust

## When to Use
When writing tests for I/O error handling, flow control (QueueFull), or timing-dependent behavior in faster-core or faster-device.

## Device Testing Architecture

FASTER uses test doubles that implement the `Device` trait to inject faults and control timing without real I/O.

### Device Error Injection Layers

**Two distinct error injection points:**

1. **Submission-level errors** (pre-callback)
   - Returned from `write_async()` / `read_async()` before any callback fires
   - Example: `IoRequestResult::QueueFull` — device queue is full, retry later
   - Use case: Test retry logic, backpressure handling, flow control

2. **Callback-level errors** (post-submission)
   - Returned via the callback's `IoStatus::Error` after `write_async()` returns `Submitted` or `CompletedSync`
   - Use case: Test I/O failure handling, corruption detection, recovery paths

### Existing Test Devices

**In `faster_core::test_utils::devices`:**

| Device | Error Point | Behavior |
|--------|-------------|----------|
| `InMemoryDevice` | None | Synchronous, returns `CompletedSync` |
| `SlowDevice` | None | Async with delay, returns `Submitted` |
| `FaultInjectingDevice` | Callback | Injects `IoStatus::Error` via callback |
| `QueueFullDevice` | Submission | Returns `QueueFull` after N writes |
| `QueueFullThenSucceedDevice` | Submission | QueueFull for M writes, then succeeds |

**Key Pattern: CompletedSync vs Submitted:**
- `InMemoryDevice` returns `CompletedSync` and fires callback immediately
- `SlowDevice` wraps `InMemoryDevice` but returns `Submitted`, fires callback on background thread after delay
- Real devices (`SyncFileDevice`) return `Submitted` and complete asynchronously
- Tests must handle both paths to match production behavior

## Pattern 1: QueueFull Test Device

For testing retry logic when device queue is full:

```rust
use faster_core::test_utils::devices::QueueFullDevice;

let device = QueueFullDevice::queue_full_after(5); // First 5 succeed, rest fail
// OR
let device = QueueFullDevice::toggleable(); // Use device.set_queue_full(true/false)
```

**Implementation details:**
- Wraps `InMemoryDevice`
- Uses `AtomicUsize` counter for writes (fetch_add pattern)
- Returns `IoRequestResult::QueueFull` after threshold
- Does NOT fire the callback on QueueFull (submission failed)

**Atomic Counter Pattern:**
```rust
let count = self.write_count.fetch_add(1, Ordering::SeqCst);
if count >= self.queue_full_after {
    return IoRequestResult::QueueFull;
}
```
⚠️ `fetch_add(1)` returns the **pre-increment** value, so compare `count >= n`, not loading the atomic after.

## Pattern 2: Fault Injection Device

For testing callback-level errors:

```rust
use faster_core::test_utils::devices::FaultInjectingDevice;

let device = FaultInjectingDevice::new(base_device, |_key, _offset| {
    if should_fail {
        IoStatus::Error
    } else {
        IoStatus::Success
    }
});
```

**Behavior:**
- Returns `CompletedSync` (submission succeeds)
- Callback receives `IoStatus::Error` instead of `Success`
- Exercises error handling *after* operation submitted

## Pattern 3: Slow Device (Timing Tests)

For testing async completion paths, avoiding race conditions in tests:

```rust
use faster_core::test_utils::devices::SlowDevice;

let device = SlowDevice::with_delay(
    InMemoryDevice::new(),
    Duration::from_millis(50)
);
```

**Why needed:**
- `InMemoryDevice` returns `CompletedSync`, skipping async paths
- Real devices (`SyncFileDevice`) return `Submitted`, fire callbacks later
- `SlowDevice` matches real device behavior: `Submitted` + async callback
- Prevents test flakiness from timing assumptions

## Pattern 4: Stateful Toggle Device

For tests needing dynamic fault injection:

```rust
let device = QueueFullDevice::toggleable();

// Phase 1: Let writes succeed
device.set_queue_full(false);
store.upsert(&key1, &value1);

// Phase 2: Trigger QueueFull
device.set_queue_full(true);
let result = store.upsert(&key2, &value2);
assert!(result.is_aborted() || matches retry logic);

// Phase 3: Recovery
device.set_queue_full(false);
store.upsert(&key3, &value3);
```

## Checklist for New Test Devices

1. Decide error injection point: submission or callback?
2. Wrap `InMemoryDevice` for in-memory testing
3. Use `AtomicUsize` / `AtomicBool` for thread-safe state
4. For async behavior, return `Submitted` and spawn callback on thread
5. Document CompletedSync vs Submitted behavior
6. Add to `faster_core::test_utils::devices` module
7. Write example test demonstrating the device's purpose

## Testing with Device Doubles

**OperationOutcome API:**
```rust
let outcome = store.upsert(&key, &value);
assert!(outcome.is_success());      // Not is_ok() — this is not Result
assert!(outcome.is_aborted());      // Operation failed
assert_eq!(outcome.status(), OperationStatus::Success);
```

⚠️ `OperationOutcome<C>` is NOT `Result`. Use `.is_success()`, `.is_aborted()`, `.status()`.

## Confidence
**High** — Battle-tested in deadlock fix (7 passing tests), mutation campaign (17 gaps killed).

## Learned From
- Deadlock test harness (2026-03-11, sam/deadlock-fix branch)
- QueueFull vs callback error distinction learned from flush_page retry logic
- Atomic counter off-by-one caught during device implementation
