//! Opaque handle table for the C FFI layer.
//!
//! Maps opaque `u64` handles to typed Rust objects. Handles are monotonically
//! increasing and never reused, preventing ABA-style bugs from C callers that
//! cache stale handle values.
//!
//! # Thread Safety
//!
//! All operations are thread-safe. The table uses a [`RwLock<HashMap>`] internally —
//! FFI calls are not on the hot path, so simplicity is preferred over lock-free
//! complexity.

use std::any::Any;
use std::collections::HashMap;
use std::sync::RwLock;
use std::sync::atomic::{AtomicU64, Ordering};

/// Opaque handle returned to C callers. Never zero (0 = invalid).
pub type FasterHandle = u64;

/// Sentinel value representing an invalid or null handle.
pub const INVALID_HANDLE: FasterHandle = 0;

/// Thread-safe handle table mapping opaque `u64` handles to typed Rust objects.
///
/// # Handle Semantics
///
/// - Handles are monotonically increasing and never reused (prevents ABA bugs).
/// - Handle `0` is reserved as the invalid/null sentinel.
/// - Type safety is enforced at runtime via [`Any`] downcasting.
///
/// # Poisoned Lock Recovery
///
/// If a thread panics while holding the internal lock, subsequent operations
/// recover the lock (via [`into_inner`](std::sync::PoisonError::into_inner))
/// rather than propagating the panic. This is the correct policy for an FFI
/// boundary where panicking would be UB.
pub struct HandleTable {
    /// Monotonically increasing counter for handle generation.
    /// Starts at 1 so that handle 0 is never issued.
    next_handle: AtomicU64,
    /// The actual storage. `RwLock` allows concurrent reads.
    entries: RwLock<HashMap<FasterHandle, Box<dyn Any + Send + Sync>>>,
}

impl HandleTable {
    /// Create a new, empty handle table.
    pub fn new() -> Self {
        Self {
            next_handle: AtomicU64::new(1),
            entries: RwLock::new(HashMap::new()),
        }
    }

    /// Insert a value into the table, returning a unique handle.
    ///
    /// The handle is guaranteed to be non-zero and unique for the lifetime
    /// of this `HandleTable` instance (handles are never reused).
    pub fn insert<T: Any + Send + Sync>(&self, value: T) -> FasterHandle {
        // Relaxed is sufficient: we only need a unique monotonic value.
        // The RwLock write-lock below provides the happens-before ordering
        // that makes the HashMap insertion visible to subsequent readers.
        let handle = self.next_handle.fetch_add(1, Ordering::Relaxed);
        debug_assert_ne!(
            handle, 0,
            "handle counter wrapped to zero after 2^64 allocations"
        );

        let mut map = self.entries.write().unwrap_or_else(|e| e.into_inner());
        map.insert(handle, Box::new(value));
        handle
    }

    /// Access a stored object by handle via a closure.
    ///
    /// Returns `None` if the handle is invalid (`0`), not found, or the stored
    /// object's type does not match `T`.
    ///
    /// # Why a closure?
    ///
    /// The internal `RwLock` read-guard must be held while the reference is live.
    /// A closure-based API ensures the guard's lifetime is properly scoped without
    /// leaking lock internals to callers.
    pub fn with<T: Any + Send + Sync, R>(
        &self,
        handle: FasterHandle,
        f: impl FnOnce(&T) -> R,
    ) -> Option<R> {
        if handle == INVALID_HANDLE {
            return None;
        }
        let map = self.entries.read().unwrap_or_else(|e| e.into_inner());
        let boxed = map.get(&handle)?;
        let typed = boxed.downcast_ref::<T>()?;
        Some(f(typed))
    }

    /// Mutably access a stored object by handle via a closure.
    ///
    /// Returns `None` if the handle is invalid (`0`), not found, or the stored
    /// object's type does not match `T`.
    ///
    /// Takes a write-lock internally, so only one `with_mut` (or `insert`/`remove`)
    /// can execute at a time.
    pub fn with_mut<T: Any + Send + Sync, R>(
        &self,
        handle: FasterHandle,
        f: impl FnOnce(&mut T) -> R,
    ) -> Option<R> {
        if handle == INVALID_HANDLE {
            return None;
        }
        let mut map = self.entries.write().unwrap_or_else(|e| e.into_inner());
        let boxed = map.get_mut(&handle)?;
        let typed = boxed.downcast_mut::<T>()?;
        Some(f(typed))
    }

    /// Remove and return a stored object by handle.
    ///
    /// Returns `None` if the handle is invalid (`0`), not found, or the stored
    /// object's type does not match `T`. On type mismatch the object remains
    /// in the table — it is not lost.
    pub fn remove<T: Any + Send + Sync>(&self, handle: FasterHandle) -> Option<T> {
        if handle == INVALID_HANDLE {
            return None;
        }
        let mut map = self.entries.write().unwrap_or_else(|e| e.into_inner());
        // Check type before removing so a mismatch doesn't destroy the entry.
        if !map.get(&handle)?.is::<T>() {
            return None;
        }
        let boxed = map.remove(&handle)?;
        // downcast is infallible here — we just checked the type above while
        // holding the same write-lock (no TOCTOU).
        let typed = boxed.downcast::<T>().ok()?;
        Some(*typed)
    }

    /// Check whether a handle is currently valid (exists in the table).
    pub fn contains(&self, handle: FasterHandle) -> bool {
        if handle == INVALID_HANDLE {
            return false;
        }
        let map = self.entries.read().unwrap_or_else(|e| e.into_inner());
        map.contains_key(&handle)
    }

    /// Return the number of live objects in the table.
    pub fn len(&self) -> usize {
        let map = self.entries.read().unwrap_or_else(|e| e.into_inner());
        map.len()
    }

    /// Return whether the table is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Default for HandleTable {
    fn default() -> Self {
        Self::new()
    }
}

// SAFETY discussion: HandleTable is Send + Sync because:
// - AtomicU64 is Send + Sync
// - RwLock<HashMap<..., Box<dyn Any + Send + Sync>>> is Send + Sync
// The compiler derives this automatically; we assert it here for documentation.
const _: () = {
    const fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<HandleTable>();
};

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    // ── 1. Handle lifecycle ────────────────────────────────────────────────

    #[test]
    fn insert_get_remove_lifecycle() {
        let table = HandleTable::new();
        let h = table.insert(String::from("hello"));
        assert_ne!(h, INVALID_HANDLE);

        // get returns the value
        let val = table.with::<String, _>(h, |s| s.clone());
        assert_eq!(val, Some(String::from("hello")));

        // remove returns the value
        let removed = table.remove::<String>(h);
        assert_eq!(removed, Some(String::from("hello")));

        // get after remove returns None
        let gone = table.with::<String, _>(h, |s| s.clone());
        assert_eq!(gone, None);
    }

    #[test]
    fn handles_are_monotonically_increasing() {
        let table = HandleTable::new();
        let h1 = table.insert(1u32);
        let h2 = table.insert(2u32);
        let h3 = table.insert(3u32);
        assert!(h1 < h2);
        assert!(h2 < h3);
    }

    #[test]
    fn handles_are_never_zero() {
        let table = HandleTable::new();
        for _ in 0..100 {
            let h = table.insert(42u64);
            assert_ne!(h, INVALID_HANDLE);
        }
    }

    #[test]
    fn insert_returns_unique_handles() {
        let table = HandleTable::new();
        let mut handles = Vec::new();
        for i in 0..1000u32 {
            handles.push(table.insert(i));
        }
        let set: std::collections::HashSet<_> = handles.iter().collect();
        assert_eq!(set.len(), handles.len(), "all handles must be unique");
    }

    // ── 2. Invalid handle ──────────────────────────────────────────────────

    #[test]
    fn get_invalid_handle_zero() {
        let table = HandleTable::new();
        let result = table.with::<String, _>(INVALID_HANDLE, |s| s.clone());
        assert_eq!(result, None);
    }

    #[test]
    fn remove_invalid_handle_zero() {
        let table = HandleTable::new();
        let result = table.remove::<String>(INVALID_HANDLE);
        assert_eq!(result, None);
    }

    #[test]
    fn get_nonexistent_handle() {
        let table = HandleTable::new();
        let result = table.with::<String, _>(999, |s| s.clone());
        assert_eq!(result, None);
    }

    #[test]
    fn remove_nonexistent_handle() {
        let table = HandleTable::new();
        let result = table.remove::<String>(999);
        assert_eq!(result, None);
    }

    #[test]
    fn contains_invalid_handle() {
        let table = HandleTable::new();
        assert!(!table.contains(INVALID_HANDLE));
        assert!(!table.contains(42));
    }

    #[test]
    fn with_mut_invalid_handle() {
        let table = HandleTable::new();
        let result = table.with_mut::<String, _>(INVALID_HANDLE, |_| ());
        assert_eq!(result, None);
    }

    // ── 3. Type safety ─────────────────────────────────────────────────────

    #[test]
    fn type_mismatch_get_returns_none() {
        let table = HandleTable::new();
        let h = table.insert(String::from("hello"));

        // Try to get as u64 — should be None
        let result = table.with::<u64, _>(h, |v| *v);
        assert_eq!(result, None);

        // Original type still works
        let result = table.with::<String, _>(h, |s| s.clone());
        assert_eq!(result, Some(String::from("hello")));
    }

    #[test]
    fn type_mismatch_remove_returns_none_and_preserves_entry() {
        let table = HandleTable::new();
        let h = table.insert(String::from("preserved"));

        // Try to remove as wrong type
        let result = table.remove::<u64>(h);
        assert_eq!(result, None);

        // Entry is still there with correct type
        assert!(table.contains(h));
        let val = table.with::<String, _>(h, |s| s.clone());
        assert_eq!(val, Some(String::from("preserved")));
    }

    #[test]
    fn type_mismatch_with_mut_returns_none() {
        let table = HandleTable::new();
        let h = table.insert(42u64);

        let result = table.with_mut::<String, _>(h, |_| ());
        assert_eq!(result, None);

        // Original still intact
        let val = table.with::<u64, _>(h, |v| *v);
        assert_eq!(val, Some(42));
    }

    #[test]
    fn heterogeneous_types() {
        let table = HandleTable::new();
        let h1 = table.insert(String::from("text"));
        let h2 = table.insert(42u64);
        let h3 = table.insert(vec![1, 2, 3]);

        assert_eq!(
            table.with::<String, _>(h1, |s| s.clone()),
            Some(String::from("text"))
        );
        assert_eq!(table.with::<u64, _>(h2, |v| *v), Some(42));
        assert_eq!(
            table.with::<Vec<i32>, _>(h3, |v| v.clone()),
            Some(vec![1, 2, 3])
        );
    }

    // ── 4. Thread safety ───────────────────────────────────────────────────

    #[test]
    fn concurrent_insert_get_remove() {
        let table = Arc::new(HandleTable::new());
        let num_threads = 8;
        let ops_per_thread = 100;

        let handles: Vec<_> = (0..num_threads)
            .map(|t| {
                let table = Arc::clone(&table);
                std::thread::spawn(move || {
                    let mut local_handles = Vec::new();
                    for i in 0..ops_per_thread {
                        let val = format!("thread-{t}-item-{i}");
                        let h = table.insert(val.clone());
                        assert_ne!(h, INVALID_HANDLE);
                        local_handles.push((h, val));
                    }
                    // Verify all our handles
                    for (h, expected) in &local_handles {
                        let got = table.with::<String, _>(*h, |s| s.clone());
                        assert_eq!(got.as_deref(), Some(expected.as_str()));
                    }
                    // Remove all our handles
                    for (h, expected) in &local_handles {
                        let removed = table.remove::<String>(*h);
                        assert_eq!(removed.as_deref(), Some(expected.as_str()));
                    }
                    // Verify removal
                    for (h, _) in &local_handles {
                        assert!(!table.contains(*h));
                    }
                })
            })
            .collect();

        for h in handles {
            h.join().expect("thread panicked");
        }

        assert!(table.is_empty());
    }

    #[test]
    fn concurrent_mixed_operations() {
        let table = Arc::new(HandleTable::new());
        // Pre-insert some items
        let shared_handles: Vec<_> = (0..10)
            .map(|i| table.insert(format!("shared-{i}")))
            .collect();
        let shared_handles = Arc::new(shared_handles);

        let threads: Vec<_> = (0..4)
            .map(|t| {
                let table = Arc::clone(&table);
                let shared = Arc::clone(&shared_handles);
                std::thread::spawn(move || {
                    // Each thread reads shared handles and inserts/removes its own
                    for i in 0..50 {
                        // Read a shared handle (may or may not still exist)
                        let idx = (t * 50 + i) % shared.len();
                        let _ = table.with::<String, _>(shared[idx], |s| s.len());

                        // Insert and remove own handle
                        let h = table.insert(i as u64);
                        let _ = table.with::<u64, _>(h, |v| *v);
                        let _ = table.remove::<u64>(h);
                    }
                })
            })
            .collect();

        for t in threads {
            t.join().expect("thread panicked");
        }
    }

    // ── 5. Double-remove ───────────────────────────────────────────────────

    #[test]
    fn double_remove_returns_none() {
        let table = HandleTable::new();
        let h = table.insert(String::from("once"));

        let first = table.remove::<String>(h);
        assert_eq!(first, Some(String::from("once")));

        let second = table.remove::<String>(h);
        assert_eq!(second, None);
    }

    #[test]
    fn double_remove_does_not_affect_other_handles() {
        let table = HandleTable::new();
        let h1 = table.insert(1u32);
        let h2 = table.insert(2u32);

        table.remove::<u32>(h1);
        table.remove::<u32>(h1); // double remove

        // h2 is unaffected
        assert_eq!(table.with::<u32, _>(h2, |v| *v), Some(2));
    }

    // ── 6. with_mut ────────────────────────────────────────────────────────

    #[test]
    fn with_mut_modifies_in_place() {
        let table = HandleTable::new();
        let h = table.insert(String::from("before"));

        table.with_mut::<String, _>(h, |s| {
            s.clear();
            s.push_str("after");
        });

        let val = table.with::<String, _>(h, |s| s.clone());
        assert_eq!(val, Some(String::from("after")));
    }

    // ── 7. len / is_empty ──────────────────────────────────────────────────

    #[test]
    fn len_and_is_empty() {
        let table = HandleTable::new();
        assert!(table.is_empty());
        assert_eq!(table.len(), 0);

        let h1 = table.insert(1u32);
        assert!(!table.is_empty());
        assert_eq!(table.len(), 1);

        let h2 = table.insert(2u32);
        assert_eq!(table.len(), 2);

        table.remove::<u32>(h1);
        assert_eq!(table.len(), 1);

        table.remove::<u32>(h2);
        assert!(table.is_empty());
    }

    // ── 8. Default impl ────────────────────────────────────────────────────

    #[test]
    fn default_creates_empty_table() {
        let table = HandleTable::default();
        assert!(table.is_empty());
        let h = table.insert(42u64);
        assert_ne!(h, INVALID_HANDLE);
    }
}
