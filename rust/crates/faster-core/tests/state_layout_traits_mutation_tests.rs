//! Mutation-testing gap-fill tests for `state/`, `record/layout.rs`, and
//! `record/traits.rs`.
//!
//! Each test targets a specific surviving mutant identified during the
//! cargo-mutants campaign on these modules.

use faster_core::record::{Key, LENGTH_PREFIX_SIZE, RecordLayout, Value, record_size_from_bytes};
use faster_core::state::{Phase, SystemState};

// ===========================================================================
// state/phase.rs — Phase::is_rest must return false for non-rest phases
// ===========================================================================

/// Kill mutant: `Phase::is_rest -> true`.
///
/// Non-rest phases must report `is_rest() == false`.
#[test]
fn phase_is_rest_false_for_non_rest() {
    assert!(Phase::Rest.is_rest());
    assert!(!Phase::Prepare.is_rest());
    assert!(!Phase::InProgress.is_rest());
    assert!(!Phase::WaitFlush.is_rest());
    assert!(!Phase::WaitCompletion.is_rest());
    assert!(!Phase::PersistenceCallback.is_rest());
    assert!(!Phase::PrepareGrow.is_rest());
    assert!(!Phase::InProgressGrow.is_rest());
    assert!(!Phase::WaitCompletionGrow.is_rest());
}

// ===========================================================================
// state/system_state.rs — make_intermediate idempotency (| vs ^)
// ===========================================================================

/// Kill mutant: `make_intermediate` — `|` → `^`.
///
/// Calling make_intermediate twice should be idempotent. With XOR, the
/// second call would clear the intermediate bit.
#[test]
fn make_intermediate_is_idempotent() {
    let state = SystemState::new(Phase::Prepare, 42);
    let inter1 = state.make_intermediate();
    assert!(inter1.is_intermediate());

    let inter2 = inter1.make_intermediate();
    assert!(
        inter2.is_intermediate(),
        "make_intermediate must be idempotent (| not ^)"
    );
    // Phase and version must be preserved
    assert_eq!(inter2.phase(), Phase::Prepare);
    assert_eq!(inter2.version(), 42);
}

/// Verify make_intermediate preserves phase identity.
#[test]
fn make_intermediate_preserves_phase() {
    for &phase in &[
        Phase::Rest,
        Phase::Prepare,
        Phase::InProgress,
        Phase::WaitFlush,
    ] {
        let state = SystemState::new(phase, 100);
        let inter = state.make_intermediate();
        assert!(inter.is_intermediate());
        assert_eq!(inter.phase(), phase);
        assert_eq!(inter.version(), 100);
    }
}

// ===========================================================================
// record/layout.rs — boundary condition in record_size_from_bytes (> vs >=)
// ===========================================================================

/// Kill mutant: `record_size_from_bytes` line 349 — `>` → `>=`.
///
/// When `value_offset + LENGTH_PREFIX_SIZE == remaining`, the current code
/// (with `>`) correctly proceeds. Changing to `>=` would reject this
/// boundary case as corrupted.
#[test]
fn record_size_from_bytes_boundary_exact_fit() {
    // For u64 Key (8 bytes fixed) and u64 Value (8 bytes fixed):
    // RecordInfo: 8 bytes at offset 0
    // Key: 8 bytes at offset 8 (aligned)
    // Value: 8 bytes at offset 16 (aligned)
    // Total: 24 bytes (aligned)
    let layout = RecordLayout::for_kv::<u64, u64>(&42u64, &99u64);
    let total = layout.total_size();

    // Create a page buffer exactly large enough
    let page_size = total;
    let mut page = vec![0u8; page_size];

    // Write a valid record at offset 0
    // RecordInfo header (8 bytes of zeros is fine for size computation)
    // Key: write u64 42 as little-endian at offset 8
    page[8..16].copy_from_slice(&42u64.to_le_bytes());
    // Value: write u64 99 as little-endian at offset 16
    page[16..24].copy_from_slice(&99u64.to_le_bytes());

    let result = record_size_from_bytes::<u64, u64>(&page, 0, page_size);
    assert!(
        result.is_ok(),
        "exact-fit page should succeed, got {:?}",
        result
    );
    assert_eq!(result.unwrap(), total);
}

/// Just one byte short should fail.
#[test]
fn record_size_from_bytes_one_byte_short_fails() {
    let layout = RecordLayout::for_kv::<u64, u64>(&42u64, &99u64);
    let total = layout.total_size();

    // One byte smaller than needed
    let page_size = total - 1;
    let page = vec![0u8; page_size];

    let result = record_size_from_bytes::<u64, u64>(&page, 0, page_size);
    assert!(result.is_err(), "too-small page should fail");
}

// ===========================================================================
// record/traits.rs — Key/Value trait implementations
// ===========================================================================

// --- Default Key::serialized_size_from_bytes ---

/// Kill mutant: `Key::serialized_size_from_bytes -> 0` and `-> 1`.
///
/// Verify that the default impl for fixed-size keys returns the correct size.
#[test]
fn key_serialized_size_from_bytes_u64() {
    let key: u64 = 0xDEAD_BEEF;
    let mut buf = vec![0u8; 64];
    let written = <u64 as Key>::serialize(&key, &mut buf);
    assert_eq!(written, 8);

    let size = <u64 as Key>::serialized_size_from_bytes(&buf);
    assert_eq!(size, 8, "u64 key should report serialized size of 8");
}

/// Kill mutant: `Key::serialized_size_from_bytes -> 0` for variable-length.
#[test]
fn key_serialized_size_from_bytes_vec_u8() {
    let key: Vec<u8> = vec![1, 2, 3, 4, 5];
    let mut buf = vec![0u8; 64];
    let written = <Vec<u8> as Key>::serialize(&key, &mut buf);
    assert_eq!(written, LENGTH_PREFIX_SIZE + 5);

    let size = <Vec<u8> as Key>::serialized_size_from_bytes(&buf);
    assert_eq!(
        size,
        LENGTH_PREFIX_SIZE + 5,
        "Vec<u8> key should report prefix + data length"
    );
}

// --- Default Key::eq_from_bytes ---

/// Kill mutant: `Key::eq_from_bytes -> true` and `-> false`.
#[test]
fn key_eq_from_bytes_equal() {
    let key: u64 = 42;
    let mut buf = vec![0u8; 16];
    <u64 as Key>::serialize(&key, &mut buf);
    assert!(
        <u64 as Key>::eq_from_bytes(&key, &buf),
        "equal key must return true"
    );
}

#[test]
fn key_eq_from_bytes_not_equal() {
    let key: u64 = 42;
    let different: u64 = 99;
    let mut buf = vec![0u8; 16];
    <u64 as Key>::serialize(&different, &mut buf);
    assert!(
        !<u64 as Key>::eq_from_bytes(&key, &buf),
        "different key must return false"
    );
}

/// Kill mutant: `== with !=` in eq_from_bytes.
#[test]
fn key_eq_from_bytes_negation() {
    let key: Vec<u8> = vec![10, 20, 30];
    let mut buf = vec![0u8; 64];
    <Vec<u8> as Key>::serialize(&key, &mut buf);

    assert!(<Vec<u8> as Key>::eq_from_bytes(&key, &buf));

    // Different key
    let other: Vec<u8> = vec![10, 20, 31]; // one byte different
    assert!(!<Vec<u8> as Key>::eq_from_bytes(&other, &buf));
}

// --- Value::serialized_size_from_bytes ---

/// Kill mutant: `Value::serialized_size_from_bytes -> 0`.
#[test]
fn value_serialized_size_from_bytes_u64() {
    let val: u64 = 0xCAFE;
    let mut buf = vec![0u8; 64];
    let written = <u64 as Value>::serialize(&val, &mut buf);
    assert_eq!(written, 8);

    let size = <u64 as Value>::serialized_size_from_bytes(&buf);
    assert_eq!(size, 8, "u64 value should report serialized size of 8");
}

#[test]
fn value_serialized_size_from_bytes_vec_u8() {
    let val: Vec<u8> = vec![0xAA; 10];
    let mut buf = vec![0u8; 64];
    <Vec<u8> as Value>::serialize(&val, &mut buf);

    let size = <Vec<u8> as Value>::serialized_size_from_bytes(&buf);
    assert_eq!(
        size,
        LENGTH_PREFIX_SIZE + 10,
        "Vec<u8> value should report prefix + data length"
    );
}

// --- serialize roundtrip for Vec<u8> and String (+ with * mutants) ---

/// Kill mutant: `+ with *` in `<impl Key for Vec<u8>>::serialize` (line 384).
///
/// If `LENGTH_PREFIX_SIZE + self.len()` becomes `LENGTH_PREFIX_SIZE * self.len()`,
/// the return value is wrong and roundtrip deserialization breaks.
#[test]
fn vec_u8_key_serialize_roundtrip() {
    let key: Vec<u8> = vec![1, 2, 3, 4, 5];
    let mut buf = vec![0u8; 64];
    let written = <Vec<u8> as Key>::serialize(&key, &mut buf);
    assert_eq!(
        written,
        LENGTH_PREFIX_SIZE + key.len(),
        "serialize must return prefix + data length"
    );

    let deserialized = <Vec<u8> as Key>::deserialize(&buf);
    assert_eq!(deserialized, key, "roundtrip must preserve data");
}

/// Kill mutant: `+ with *` in `<impl Value for Vec<u8>>::serialize` (line 441).
#[test]
fn vec_u8_value_serialize_roundtrip() {
    let val: Vec<u8> = vec![10, 20, 30];
    let mut buf = vec![0u8; 64];
    let written = <Vec<u8> as Value>::serialize(&val, &mut buf);
    assert_eq!(written, LENGTH_PREFIX_SIZE + val.len());

    let deserialized = <Vec<u8> as Value>::deserialize(&buf);
    assert_eq!(deserialized, val);
}

/// Kill mutant: `+ with *` in `<impl Key for String>::serialize` (line 476).
#[test]
fn string_key_serialize_roundtrip() {
    let key = String::from("hello world");
    let mut buf = vec![0u8; 64];
    let written = <String as Key>::serialize(&key, &mut buf);
    assert_eq!(written, LENGTH_PREFIX_SIZE + key.len());

    let deserialized = <String as Key>::deserialize(&buf);
    assert_eq!(deserialized, key);
}

/// Kill mutant: `+ with *` in `<impl Value for String>::serialize` (line 535).
#[test]
fn string_value_serialize_roundtrip() {
    let val = String::from("test data 123");
    let mut buf = vec![0u8; 64];
    let written = <String as Value>::serialize(&val, &mut buf);
    assert_eq!(written, LENGTH_PREFIX_SIZE + val.len());

    let deserialized = <String as Value>::deserialize(&buf);
    assert_eq!(deserialized, val);
}

/// Verify serialize return value specifically catches the + vs * error.
/// When LENGTH_PREFIX_SIZE (4) * len (5) = 20, but correct is 4 + 5 = 9.
#[test]
fn serialize_return_value_is_addition_not_multiplication() {
    let key: Vec<u8> = vec![0; 5];
    let mut buf = vec![0u8; 64];
    let written = <Vec<u8> as Key>::serialize(&key, &mut buf);
    // With +: 4 + 5 = 9. With *: 4 * 5 = 20.
    assert_eq!(written, 9, "serialize must return 4 + 5, not 4 * 5");
}

// ===========================================================================
// Additional: empty and edge-case serialization
// ===========================================================================

/// Empty Vec<u8> key: serialize returns just the prefix size.
#[test]
fn empty_vec_key_serialize() {
    let key: Vec<u8> = vec![];
    let mut buf = vec![0u8; 16];
    let written = <Vec<u8> as Key>::serialize(&key, &mut buf);
    assert_eq!(written, LENGTH_PREFIX_SIZE);
    let size = <Vec<u8> as Key>::serialized_size_from_bytes(&buf);
    assert_eq!(size, LENGTH_PREFIX_SIZE);
}

/// Empty String value: serialize returns just the prefix size.
#[test]
fn empty_string_value_serialize() {
    let val = String::new();
    let mut buf = vec![0u8; 16];
    let written = <String as Value>::serialize(&val, &mut buf);
    assert_eq!(written, LENGTH_PREFIX_SIZE);
    let deser = <String as Value>::deserialize(&buf);
    assert_eq!(deser, val);
}
