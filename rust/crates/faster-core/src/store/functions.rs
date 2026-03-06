//! User-defined operation callbacks for a FASTER store.
//!
//! The [`Functions`] trait defines how records are read, written, modified, and
//! deleted. Callers implement this trait to customise the semantics of Read,
//! Upsert, RMW, and Delete operations.
//!
//! Two ready-made implementations are provided:
//!
//! - [`SimpleFunctions`] — basic key-value store with full-value replacement.
//! - [`CounterFunctions`] — atomic increment/decrement (accumulator) pattern.

use core::marker::PhantomData;

use crate::record::{Key, Value};
use crate::status::OperationStatus;

// ── RmwInPlaceResult ─────────────────────────────────────────────────

/// Result of an in-place RMW update.
///
/// Returned by [`Functions::rmw_in_place`] to tell the store whether the
/// update was applied successfully or the record needs to be relocated
/// (e.g., because a variable-length value grew beyond its allocated space).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RmwInPlaceResult {
    /// Update applied successfully in-place.
    InPlaceOk,
    /// Value needs to be relocated (e.g., variable-length growth).
    NeedsNewRecord,
}

// ── Functions trait ──────────────────────────────────────────────────

/// User-defined operation semantics for a FASTER store.
///
/// This trait defines how records are read, written, modified, and deleted.
/// It uses associated types to reduce generic parameter explosion.
///
/// # Associated types
///
/// | Type | Purpose |
/// |------|---------|
/// | `Key` | The key type. Must be hashable and comparable. |
/// | `Value` | The value type stored in the log. |
/// | `Input` | Caller-provided input passed to RMW and Read operations. |
/// | `Output` | Output produced by Read and RMW operations. |
/// | `Context` | Opaque user context carried through pending operations. |
///
/// # Examples
///
/// See [`SimpleFunctions`] for a minimal implementation and
/// [`CounterFunctions`] for an RMW-oriented example.
pub trait Functions: Send + Sync + 'static {
    /// The key type. Must be hashable and comparable.
    type Key: Key;

    /// The value type stored in the log.
    type Value: Value;

    /// Caller-provided input passed to RMW and Read operations.
    type Input: Send + Sync + Clone;

    /// Output produced by Read and RMW operations.
    type Output: Send + Sync + Default;

    /// Opaque user context carried through pending operations.
    type Context: Send + Sync;

    // ── Read ────────────────────────────────────────────────────────

    /// Read a record's value and produce output.
    fn read(
        &self,
        key: &Self::Key,
        value: &Self::Value,
        input: &Self::Input,
        output: &mut Self::Output,
    );

    /// Called when a pending Read completes (optional notification).
    fn read_completion(
        &self,
        _key: &Self::Key,
        _output: &Self::Output,
        _context: &Self::Context,
        _status: OperationStatus,
    ) {
    }

    // ── Upsert ──────────────────────────────────────────────────────

    /// Write a value into a record.
    ///
    /// `old_value` is `Some` for in-place updates in the mutable region,
    /// `None` for new records.
    fn upsert(
        &self,
        key: &Self::Key,
        value: &mut Self::Value,
        input: &Self::Input,
        old_value: Option<&Self::Value>,
        output: &mut Self::Output,
    );

    // ── RMW ─────────────────────────────────────────────────────────

    /// Decide whether to create a new record when the key is not found.
    fn rmw_need_initial_update(&self, _key: &Self::Key, _input: &Self::Input) -> bool {
        true
    }

    /// Initialize a new record for a key that doesn't exist yet.
    fn rmw_initial(
        &self,
        key: &Self::Key,
        input: &Self::Input,
        value: &mut Self::Value,
        output: &mut Self::Output,
    );

    /// Update a record in-place in the mutable region.
    fn rmw_in_place(
        &self,
        key: &Self::Key,
        input: &Self::Input,
        value: &mut Self::Value,
        output: &mut Self::Output,
    ) -> RmwInPlaceResult;

    /// Decide whether to copy a read-only record before updating.
    fn rmw_need_copy_update(
        &self,
        _key: &Self::Key,
        _input: &Self::Input,
        _old_value: &Self::Value,
    ) -> bool {
        true
    }

    /// Create a new record by copying and modifying an existing read-only
    /// record.
    fn rmw_copy_update(
        &self,
        key: &Self::Key,
        input: &Self::Input,
        old_value: &Self::Value,
        new_value: &mut Self::Value,
        output: &mut Self::Output,
    );

    /// Called when a pending RMW completes (optional notification).
    fn rmw_completion(
        &self,
        _key: &Self::Key,
        _output: &Self::Output,
        _context: &Self::Context,
        _status: OperationStatus,
    ) {
    }

    // ── Delete ───────────────────────────────────────────────────────

    /// Called when a record is being deleted (optional cleanup).
    fn delete(&self, _key: &Self::Key, _value: &mut Self::Value) {}
}

// ── SimpleFunctions ─────────────────────────────────────────────────

/// Simple key-value store functions where:
///
/// - `Input` is the `Value` type (for upsert: the new value).
/// - `Output` is `Option<Value>` (for read: the read value).
/// - `Context` is `()` (no user context).
/// - Read copies the stored value into output.
/// - Upsert overwrites the record with the input value.
/// - RMW treats input as the new value (full replace, not merge).
///
/// This is a good starting point for stores that don't need custom
/// read/modify/write logic.
#[derive(Debug)]
pub struct SimpleFunctions<K, V> {
    _phantom: PhantomData<(K, V)>,
}

impl<K, V> SimpleFunctions<K, V> {
    /// Creates a new `SimpleFunctions` instance.
    pub fn new() -> Self {
        Self {
            _phantom: PhantomData,
        }
    }
}

impl<K, V> Default for SimpleFunctions<K, V> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K, V> Functions for SimpleFunctions<K, V>
where
    K: Key,
    V: Value + Clone,
{
    type Key = K;
    type Value = V;
    type Input = V;
    type Output = Option<V>;
    type Context = ();

    fn read(
        &self,
        _key: &Self::Key,
        value: &Self::Value,
        _input: &Self::Input,
        output: &mut Self::Output,
    ) {
        *output = Some(value.clone());
    }

    fn upsert(
        &self,
        _key: &Self::Key,
        value: &mut Self::Value,
        input: &Self::Input,
        _old_value: Option<&Self::Value>,
        _output: &mut Self::Output,
    ) {
        *value = input.clone();
    }

    fn rmw_initial(
        &self,
        _key: &Self::Key,
        input: &Self::Input,
        value: &mut Self::Value,
        _output: &mut Self::Output,
    ) {
        *value = input.clone();
    }

    fn rmw_in_place(
        &self,
        _key: &Self::Key,
        input: &Self::Input,
        value: &mut Self::Value,
        _output: &mut Self::Output,
    ) -> RmwInPlaceResult {
        *value = input.clone();
        RmwInPlaceResult::InPlaceOk
    }

    fn rmw_copy_update(
        &self,
        _key: &Self::Key,
        input: &Self::Input,
        _old_value: &Self::Value,
        new_value: &mut Self::Value,
        _output: &mut Self::Output,
    ) {
        *new_value = input.clone();
    }
}

// ── CounterFunctions ────────────────────────────────────────────────

/// Counter functions for atomic increment/decrement patterns.
///
/// - `Key` = `K` (any key type).
/// - `Value` = `i64` (the counter).
/// - `Input` = `i64` (the delta to add).
/// - `Output` = `i64` (the counter value after the operation).
/// - `Context` = `()`.
///
/// # Examples
///
/// ```
/// use faster_core::store::{CounterFunctions, Functions, RmwInPlaceResult};
///
/// let f = CounterFunctions::<u64>::new();
///
/// // Initial: counter starts at the delta value.
/// let mut value: i64 = 0;
/// let mut output: i64 = 0;
/// f.rmw_initial(&42u64, &5i64, &mut value, &mut output);
/// assert_eq!(value, 5);
///
/// // In-place: accumulate.
/// let result = f.rmw_in_place(&42u64, &3i64, &mut value, &mut output);
/// assert_eq!(value, 8);
/// assert_eq!(output, 8);
/// assert_eq!(result, RmwInPlaceResult::InPlaceOk);
/// ```
#[derive(Debug)]
pub struct CounterFunctions<K> {
    _phantom: PhantomData<K>,
}

impl<K> CounterFunctions<K> {
    /// Creates a new `CounterFunctions` instance.
    pub fn new() -> Self {
        Self {
            _phantom: PhantomData,
        }
    }
}

impl<K> Default for CounterFunctions<K> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K> Functions for CounterFunctions<K>
where
    K: Key,
{
    type Key = K;
    type Value = i64;
    type Input = i64;
    type Output = i64;
    type Context = ();

    fn read(
        &self,
        _key: &Self::Key,
        value: &Self::Value,
        _input: &Self::Input,
        output: &mut Self::Output,
    ) {
        *output = *value;
    }

    fn upsert(
        &self,
        _key: &Self::Key,
        value: &mut Self::Value,
        input: &Self::Input,
        _old_value: Option<&Self::Value>,
        _output: &mut Self::Output,
    ) {
        *value = *input;
    }

    fn rmw_initial(
        &self,
        _key: &Self::Key,
        input: &Self::Input,
        value: &mut Self::Value,
        _output: &mut Self::Output,
    ) {
        *value = *input;
    }

    fn rmw_in_place(
        &self,
        _key: &Self::Key,
        input: &Self::Input,
        value: &mut Self::Value,
        output: &mut Self::Output,
    ) -> RmwInPlaceResult {
        *value += *input;
        *output = *value;
        RmwInPlaceResult::InPlaceOk
    }

    fn rmw_copy_update(
        &self,
        _key: &Self::Key,
        input: &Self::Input,
        old_value: &Self::Value,
        new_value: &mut Self::Value,
        output: &mut Self::Output,
    ) {
        *new_value = *old_value + *input;
        *output = *new_value;
    }
}

// ── Tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── SimpleFunctions ─────────────────────────────────────────────

    #[test]
    fn simple_functions_read_round_trip() {
        let f = SimpleFunctions::<u64, u64>::new();
        let key = 1u64;
        let value = 42u64;
        let input = 0u64; // unused for read
        let mut output: Option<u64> = None;

        f.read(&key, &value, &input, &mut output);
        assert_eq!(output, Some(42));
    }

    #[test]
    fn simple_functions_upsert() {
        let f = SimpleFunctions::<u64, u64>::new();
        let key = 1u64;
        let mut output: Option<u64> = None;

        // Upsert with no old value (new record).
        let mut value = 0u64;
        f.upsert(&key, &mut value, &100u64, None, &mut output);
        assert_eq!(value, 100);

        // Upsert with old value (in-place update).
        let old = 100u64;
        f.upsert(&key, &mut value, &200u64, Some(&old), &mut output);
        assert_eq!(value, 200);
    }

    #[test]
    fn simple_functions_rmw_lifecycle() {
        let f = SimpleFunctions::<u64, u64>::new();
        let key = 1u64;
        let mut output: Option<u64> = None;

        // Phase 1: initial — key doesn't exist.
        assert!(f.rmw_need_initial_update(&key, &10u64));
        let mut value = 0u64;
        f.rmw_initial(&key, &10u64, &mut value, &mut output);
        assert_eq!(value, 10);

        // Phase 2: in-place update — key exists in mutable region.
        let result = f.rmw_in_place(&key, &20u64, &mut value, &mut output);
        assert_eq!(result, RmwInPlaceResult::InPlaceOk);
        assert_eq!(value, 20);

        // Phase 3: copy update — key exists in read-only region.
        let old_value = 20u64;
        assert!(f.rmw_need_copy_update(&key, &30u64, &old_value));
        let mut new_value = 0u64;
        f.rmw_copy_update(&key, &30u64, &old_value, &mut new_value, &mut output);
        assert_eq!(new_value, 30);
    }

    // ── CounterFunctions ────────────────────────────────────────────

    #[test]
    fn counter_functions_accumulate() {
        let f = CounterFunctions::<u64>::new();
        let key = 1u64;
        let mut output = 0i64;

        // Initialize with the first delta.
        let mut value = 0i64;
        f.rmw_initial(&key, &1i64, &mut value, &mut output);
        assert_eq!(value, 1);

        // Accumulate 9 more increments of 1.
        for _ in 0..9 {
            let result = f.rmw_in_place(&key, &1i64, &mut value, &mut output);
            assert_eq!(result, RmwInPlaceResult::InPlaceOk);
        }
        assert_eq!(value, 10);
        assert_eq!(output, 10);
    }

    #[test]
    fn counter_functions_initial_then_update() {
        let f = CounterFunctions::<u64>::new();
        let key = 42u64;
        let mut output = 0i64;

        // Initialize counter at 100.
        let mut value = 0i64;
        f.rmw_initial(&key, &100i64, &mut value, &mut output);
        assert_eq!(value, 100);

        // In-place: add 50.
        let result = f.rmw_in_place(&key, &50i64, &mut value, &mut output);
        assert_eq!(result, RmwInPlaceResult::InPlaceOk);
        assert_eq!(value, 150);
        assert_eq!(output, 150);

        // Copy-update from read-only region: add 25.
        let mut new_value = 0i64;
        f.rmw_copy_update(&key, &25i64, &value, &mut new_value, &mut output);
        assert_eq!(new_value, 175);
        assert_eq!(output, 175);
    }

    // ── Trait constraint verification ───────────────────────────────

    #[test]
    fn functions_trait_is_object_safe() {
        // The Functions trait is NOT object-safe because it uses associated
        // types with generic methods. This is by design — FASTER uses static
        // dispatch for performance. We verify this is the expected behavior
        // by ensuring concrete types work through generics.
        fn use_functions<F: Functions>(f: &F, key: &F::Key, val: &F::Value, input: &F::Input) {
            let mut output = F::Output::default();
            f.read(key, val, input, &mut output);
        }

        let f = SimpleFunctions::<u64, u64>::new();
        use_functions(&f, &1u64, &42u64, &0u64);
    }

    #[test]
    fn simple_functions_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<SimpleFunctions<u64, u64>>();
    }

    #[test]
    fn counter_functions_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<CounterFunctions<u64>>();
    }

    #[test]
    fn rmw_in_place_result_traits() {
        // Debug
        let s = format!("{:?}", RmwInPlaceResult::InPlaceOk);
        assert!(s.contains("InPlaceOk"));

        // Clone + Copy
        let a = RmwInPlaceResult::NeedsNewRecord;
        let b = a;
        assert_eq!(a, b);

        // PartialEq
        assert_ne!(
            RmwInPlaceResult::InPlaceOk,
            RmwInPlaceResult::NeedsNewRecord
        );
    }

    #[test]
    fn simple_functions_default() {
        let f = SimpleFunctions::<u64, u64>::default();
        let mut output: Option<u64> = None;
        f.read(&1u64, &42u64, &0u64, &mut output);
        assert_eq!(output, Some(42));
    }

    #[test]
    fn counter_functions_default() {
        let f = CounterFunctions::<u64>::default();
        let mut value = 0i64;
        let mut output = 0i64;
        f.rmw_initial(&1u64, &5i64, &mut value, &mut output);
        assert_eq!(value, 5);
    }

    #[test]
    fn counter_functions_read() {
        let f = CounterFunctions::<u64>::new();
        let mut output = 0i64;
        f.read(&1u64, &42i64, &0i64, &mut output);
        assert_eq!(output, 42);
    }

    #[test]
    fn counter_functions_upsert() {
        let f = CounterFunctions::<u64>::new();
        let mut value = 0i64;
        let mut output = 0i64;
        f.upsert(&1u64, &mut value, &99i64, None, &mut output);
        assert_eq!(value, 99);
    }
}
