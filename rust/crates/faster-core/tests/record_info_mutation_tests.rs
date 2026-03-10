//! Mutation-testing gap-fill tests for `record/record_info.rs`.
//!
//! These tests were written during a cargo-mutants campaign to kill surviving
//! mutants. Each test targets a specific survivor identified in the campaign.

use faster_core::address::LogicalAddress;
use faster_core::record::{AtomicRecordInfo, RecordInfo};
use std::sync::atomic::Ordering;

// ===========================================================================
// with_invalid / with_tombstone / with_final — idempotency (| vs ^)
// ===========================================================================

/// Kill mutant: `with_invalid` — `|` → `^`.
///
/// If the invalid bit is already set and we call `with_invalid()` again,
/// `|` keeps the bit set while `^` would toggle it off.
#[test]
fn with_invalid_is_idempotent() {
    let info = RecordInfo::new(LogicalAddress::ZERO, 0, true, false, false);
    assert!(info.is_invalid(), "precondition: invalid bit should be set");
    let again = info.with_invalid();
    assert!(again.is_invalid(), "with_invalid must be idempotent (| not ^)");
}

/// Kill mutant: `with_tombstone` — `|` → `^`.
#[test]
fn with_tombstone_is_idempotent() {
    let info = RecordInfo::new(LogicalAddress::ZERO, 0, false, true, false);
    assert!(info.is_tombstone(), "precondition: tombstone bit should be set");
    let again = info.with_tombstone();
    assert!(
        again.is_tombstone(),
        "with_tombstone must be idempotent (| not ^)"
    );
}

/// Kill mutant: `with_final` — `|` → `^`.
#[test]
fn with_final_is_idempotent() {
    let info = RecordInfo::new(LogicalAddress::ZERO, 0, false, false, true);
    assert!(info.is_final(), "precondition: final bit should be set");
    let again = info.with_final();
    assert!(again.is_final(), "with_final must be idempotent (| not ^)");
}

// ===========================================================================
// with_previous_address — preserves existing address bits (| vs ^)
// ===========================================================================

/// Kill mutant: `with_previous_address` — `|` → `^`.
///
/// Set a previous address, then set the same address again. With `|`, the
/// value stays the same. With `^`, overlapping bits cancel out.
#[test]
fn with_previous_address_is_idempotent() {
    use faster_core::address::{Offset, Page};

    let addr = LogicalAddress::new(Page(5), Offset(128));
    let info = RecordInfo::new(addr, 42, false, false, false);
    assert_eq!(info.previous_address(), addr);

    // Apply the same address again — must be unchanged
    let again = info.with_previous_address(addr);
    assert_eq!(
        again.previous_address(),
        addr,
        "with_previous_address must be idempotent"
    );
    // Version should also be preserved
    assert_eq!(again.checkpoint_version(), 42);
}

/// Extra: setting a different previous address replaces correctly.
#[test]
fn with_previous_address_replaces_correctly() {
    use faster_core::address::{Offset, Page};

    let addr1 = LogicalAddress::new(Page(1), Offset(64));
    let addr2 = LogicalAddress::new(Page(99), Offset(512));
    let info = RecordInfo::new(addr1, 7, true, false, false)
        .with_previous_address(addr2);

    assert_eq!(info.previous_address(), addr2);
    assert_eq!(info.checkpoint_version(), 7);
    assert!(info.is_invalid(), "flag bits must be preserved");
}

// ===========================================================================
// with_checkpoint_version — preserves existing version bits (| vs ^)
// ===========================================================================

/// Kill mutant: `with_checkpoint_version` — `|` → `^`.
///
/// Set a version, then set the same version again. XOR would zero out the field.
#[test]
fn with_checkpoint_version_is_idempotent() {
    let info = RecordInfo::new(LogicalAddress::ZERO, 100, false, false, false);
    assert_eq!(info.checkpoint_version(), 100);

    let again = info.with_checkpoint_version(100);
    assert_eq!(
        again.checkpoint_version(),
        100,
        "with_checkpoint_version must be idempotent"
    );
}

/// Extra: replacing version preserves other fields.
#[test]
fn with_checkpoint_version_preserves_flags_and_address() {
    use faster_core::address::{Offset, Page};

    let addr = LogicalAddress::new(Page(3), Offset(256));
    let info = RecordInfo::new(addr, 50, true, true, false)
        .with_checkpoint_version(999);

    assert_eq!(info.checkpoint_version(), 999);
    assert_eq!(info.previous_address(), addr);
    assert!(info.is_invalid());
    assert!(info.is_tombstone());
}

// ===========================================================================
// AtomicRecordInfo::is_sealed — must return false when not sealed
// ===========================================================================

/// Kill mutant: `AtomicRecordInfo::is_sealed` → `true`.
///
/// A freshly created AtomicRecordInfo must report `is_sealed() == false`.
#[test]
fn atomic_is_sealed_returns_false_when_not_sealed() {
    let info = RecordInfo::new(LogicalAddress::ZERO, 0, false, false, false);
    let atomic = AtomicRecordInfo::new(info);
    assert!(
        !atomic.is_sealed(),
        "unsealed record must report is_sealed == false"
    );
}

/// After sealing, is_sealed must return true.
#[test]
fn atomic_is_sealed_returns_true_after_seal() {
    let info = RecordInfo::new(LogicalAddress::ZERO, 0, false, false, false);
    let atomic = AtomicRecordInfo::new(info);
    atomic.seal();
    assert!(atomic.is_sealed(), "sealed record must report is_sealed == true");
}

// ===========================================================================
// AtomicRecordInfo::compare_exchange_weak — must be able to fail
// ===========================================================================

/// Kill mutant: `compare_exchange_weak` → `Ok(Default::default())`.
///
/// If the expected value doesn't match, CAS must fail with `Err(actual)`.
#[test]
fn compare_exchange_weak_fails_on_mismatch() {
    let info = RecordInfo::new(LogicalAddress::ZERO, 0, false, false, false);
    let atomic = AtomicRecordInfo::new(info);

    // Seal the record to change its state
    atomic.seal();

    // Try CAS with the original (unsealed) value — should fail
    let wrong_expected = info; // original, without sealed bit
    let desired = RecordInfo::new(LogicalAddress::ZERO, 99, false, false, false);

    let result = atomic.compare_exchange_weak(
        wrong_expected,
        desired,
        Ordering::AcqRel,
        Ordering::Acquire,
    );

    assert!(
        result.is_err(),
        "CAS with wrong expected value must fail"
    );

    // The error should contain the actual (sealed) value
    let actual = result.unwrap_err();
    assert!(actual.is_sealed(), "error should contain actual sealed value");
}

/// CAS weak succeeds when expected matches actual.
#[test]
fn compare_exchange_weak_succeeds_on_match() {
    let info = RecordInfo::new(LogicalAddress::ZERO, 0, false, false, false);
    let atomic = AtomicRecordInfo::new(info);

    let desired = RecordInfo::new(LogicalAddress::ZERO, 42, false, false, false);

    // Weak CAS may spuriously fail, so try in a loop
    let mut succeeded = false;
    for _ in 0..100 {
        match atomic.compare_exchange_weak(info, desired, Ordering::AcqRel, Ordering::Acquire) {
            Ok(prev) => {
                assert_eq!(prev.raw(), info.raw());
                succeeded = true;
                break;
            }
            Err(_) => continue, // spurious failure, retry
        }
    }
    assert!(succeeded, "CAS weak should succeed within 100 attempts");
    assert_eq!(
        atomic.load(Ordering::Acquire).checkpoint_version(),
        42,
        "stored value should reflect the desired"
    );
}

// ===========================================================================
// Composite tests — multiple flags and fields together
// ===========================================================================

/// Verify all flags can coexist without interference.
#[test]
fn all_flags_coexist() {
    use faster_core::address::{Offset, Page};

    let addr = LogicalAddress::new(Page(100), Offset(2048));
    let info = RecordInfo::new(addr, 4095, true, true, true).with_sealed();

    assert!(info.is_invalid());
    assert!(info.is_tombstone());
    assert!(info.is_final());
    assert!(info.is_sealed());
    assert_eq!(info.checkpoint_version(), 4095);
    assert_eq!(info.previous_address(), addr);
}

/// Verify flag operations compose correctly.
#[test]
fn flag_operations_compose() {
    let info = RecordInfo::default()
        .with_invalid()
        .with_tombstone()
        .with_final()
        .with_sealed()
        .with_checkpoint_version(1000);

    assert!(info.is_invalid());
    assert!(info.is_tombstone());
    assert!(info.is_final());
    assert!(info.is_sealed());
    assert_eq!(info.checkpoint_version(), 1000);

    // Clear sealed, verify others remain
    let unsealed = info.with_sealed_cleared();
    assert!(!unsealed.is_sealed());
    assert!(unsealed.is_invalid());
    assert!(unsealed.is_tombstone());
    assert!(unsealed.is_final());
    assert_eq!(unsealed.checkpoint_version(), 1000);
}
