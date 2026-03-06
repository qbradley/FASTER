//! Internal CRUD operation implementations for a FASTER store.
//!
//! This module provides the core read/upsert/rmw/delete logic that ties
//! the hash index, hybrid log, record format, and session together.
//!
//! # FASTER Operation Protocol
//!
//! Every mutating operation follows a two-phase protocol:
//!
//! 1. **Tentative insert:** [`HashIndex::find_or_create`] CAS-inserts a
//!    tentative hash entry. Concurrent readers skip tentative entries.
//! 2. **Commit:** After the record is written to the log, a second CAS
//!    clears the tentative bit (and updates the address), making the
//!    entry visible to readers.
//!
//! If the log write fails, the tentative entry is CAS'd back to `EMPTY`
//! (abort path).
//!
//! # Region-based dispatch
//!
//! Every operation classifies the target address into one of the hybrid
//! log regions:
//!
//! | Region | Read | Write (Upsert/RMW) | Delete |
//! |--------|------|--------------------|--------|
//! | **Mutable** | read value directly | update in-place | set tombstone in-place |
//! | **Fuzzy / ReadOnly** | read value directly | copy to tail (RCU) | tombstone at tail |
//! | **OnDisk** | return `Pending` | return `Pending` | return `Pending` |
//!
//! # Fixed-size MVP
//!
//! This iteration assumes fixed-size keys and values. Variable-length
//! support (where the record size is not known at compile time) is a
//! future optimisation.

use crate::address::LogicalAddress;
use crate::hash::Hashable;
use crate::hash_bucket::HashBucketEntry;
use crate::hash_index::HashIndex;
use crate::hybrid_log::log_allocator::HybridLogAllocator;
use crate::hybrid_log::record_ops::{LogRecordReader, LogRecordWriter, MutableRecordAccessor};
use crate::hybrid_log::regions::AddressRegion;
use crate::record::{Key, RecordInfo, RecordLayout, Value};
use crate::status::OperationStatus;
use crate::store::functions::{Functions, RmwInPlaceResult};
use crate::store::session::{FasterSession, PendingOpType, PendingOperation};

// ── InternalContext ─────────────────────────────────────────────────

/// Shared references to the store's components needed by CRUD operations.
///
/// Passed to operation functions to avoid threading many parameters.
pub(crate) struct InternalContext<'a> {
    pub hash_index: &'a HashIndex,
    pub allocator: &'a HybridLogAllocator,
}

// ── Helpers ─────────────────────────────────────────────────────────

/// Compute the [`RecordLayout`] for a key/value pair using the fixed-size
/// approach: `std::mem::size_of::<V>()` for the value size.
///
/// For fixed-size types (`u64`, `i64`, etc.) this matches `Value::serialized_size`.
/// Variable-length types will need a different path in the future.
#[inline]
fn layout_for_fixed<K: Key, V: Value>(key: &K) -> RecordLayout {
    RecordLayout::compute(key.serialized_size(), std::mem::size_of::<V>())
}

/// Walk the version chain starting from `start_addr` looking for the
/// latest non-invalid record whose key matches `key`.
///
/// Returns `(address, RecordInfo)` of the first matching record, or
/// `None` if no match is found in the in-memory portion of the chain.
fn find_record_for_key<K: Key>(
    reader: &LogRecordReader<'_>,
    start_addr: LogicalAddress,
    key: &K,
    layout: &RecordLayout,
    allocator: &HybridLogAllocator,
) -> Option<(LogicalAddress, RecordInfo)> {
    let mut addr = start_addr;

    while addr.is_valid() {
        // If the record is not in memory we cannot check the key.
        if !allocator.is_in_memory(addr) {
            return None;
        }

        let ri = reader.read_record_info(addr)?;

        // Skip invalidated records — they have been superseded.
        if !ri.is_invalid() && reader.key_matches(addr, key, layout) {
            return Some((addr, ri));
        }

        // Walk the version chain.
        let prev = ri.previous_address();
        if prev == addr {
            // Self-loop guard.
            break;
        }

        // Only follow chain links whose record fits the same layout.
        // For the on-disk portion we stop — the caller must issue I/O.
        if !allocator.is_in_memory(prev) {
            break;
        }

        addr = prev;
    }

    None
}

/// Allocate a new record at the log tail, handling page-boundary
/// crossing (advance to next page + single retry).
///
/// On success returns `(LogicalAddress, MutableRecordAccessor)`.
fn allocate_at_tail<K: Key, V: Value>(
    allocator: &HybridLogAllocator,
    key: &K,
    value: &V,
) -> Option<(LogicalAddress, MutableRecordAccessor)> {
    let writer = LogRecordWriter::new(allocator);

    // First attempt — may fail if the record would cross a page boundary.
    if let Some(result) = writer.allocate_record(key, value) {
        return Some(result);
    }

    // Page-boundary crossing: advance to the next page and retry once.
    allocator.advance_to_next_page()?;
    writer.allocate_record(key, value)
}

// ── Read ────────────────────────────────────────────────────────────

/// Internal Read operation.
///
/// Fast path: record is in mutable/read-only/fuzzy region → read directly.
/// Slow path: record is on disk → return `Pending`.
pub(crate) fn internal_read<F: Functions>(
    ctx: &InternalContext<'_>,
    session: &mut FasterSession<F>,
    functions: &F,
    key: &F::Key,
    input: &F::Input,
    output: &mut F::Output,
    context: F::Context,
) -> OperationStatus {
    let key_hash = key.hash();

    // 1. Look up committed entry in the hash index.
    let (entry, _slot) = match ctx.hash_index.find(key_hash) {
        Some(pair) => pair,
        None => return OperationStatus::NotFound,
    };

    let addr = entry.address();
    if !addr.is_valid() {
        return OperationStatus::NotFound;
    }

    // 2. Classify the address region.
    let info = ctx.allocator.snapshot();
    let region = info.classify(addr);

    let layout = layout_for_fixed::<F::Key, F::Value>(key);

    match region {
        AddressRegion::Mutable | AddressRegion::FuzzyRegion | AddressRegion::ReadOnly => {
            // Record is in memory — walk the chain for a key match.
            let reader = LogRecordReader::new(ctx.allocator);

            match find_record_for_key(&reader, addr, key, &layout, ctx.allocator) {
                Some((_found_addr, ri)) => {
                    if ri.is_tombstone() {
                        return OperationStatus::NotFound;
                    }

                    // Read the value and invoke the user callback.
                    let value: F::Value = reader
                        .read_value(_found_addr, &layout)
                        .expect("value must be readable for in-memory record");
                    functions.read(key, &value, input, output);
                    OperationStatus::Ok
                }
                None => OperationStatus::NotFound,
            }
        }
        AddressRegion::OnDisk => {
            // Record is on disk — enqueue a pending operation.
            session.enqueue_pending(PendingOperation {
                op_type: PendingOpType::Read,
                key: key.clone(),
                input: Some(input.clone()),
                context,
                address: addr,
                record_layout: layout,
                key_hash,
            });
            OperationStatus::Pending
        }
        _ => OperationStatus::NotFound,
    }
}

// ── Upsert ──────────────────────────────────────────────────────────

/// Internal Upsert operation.
///
/// - Key exists in **mutable** region → update in place → `InPlaceUpdated`.
/// - Key exists in **read-only / fuzzy** → copy to tail (RCU) → `CopyUpdated`.
/// - Key does not exist → insert new record at tail → `Created`.
/// - Key on disk → `Pending`.
///
/// `input` is passed through to [`Functions::upsert`], which writes the
/// desired value into the record's value slot.
pub(crate) fn internal_upsert<F: Functions>(
    ctx: &InternalContext<'_>,
    session: &mut FasterSession<F>,
    functions: &F,
    key: &F::Key,
    input: &F::Input,
    context: F::Context,
) -> OperationStatus {
    let key_hash = key.hash();
    let layout = layout_for_fixed::<F::Key, F::Value>(key);

    // Phase 1: Probe the hash index.  `find_or_create` either finds an
    // existing committed entry or CAS-inserts a tentative one.
    let result = ctx
        .hash_index
        .find_or_create(key_hash, LogicalAddress::INVALID);

    if result.created {
        // ── New key — allocate a record at the tail ────────────────

        // Let the callback initialise the value into a zeroed slot.
        // SAFETY: For fixed-size numeric Value types (u64, i64, etc.), a
        // zeroed representation is valid. The callback immediately overwrites
        // the value before it is read. Variable-length values will need a
        // different initialisation strategy (future work).
        let mut value: F::Value = unsafe { std::mem::zeroed() };
        let mut output = F::Output::default();
        functions.upsert(key, &mut value, input, None, &mut output);

        let (new_addr, accessor) = match allocate_at_tail(ctx.allocator, key, &value) {
            Some(pair) => pair,
            None => {
                // Abort: CAS the tentative entry back to EMPTY.
                let _ = ctx
                    .hash_index
                    .update(result.slot, result.entry, HashBucketEntry::EMPTY);
                return OperationStatus::Aborted;
            }
        };

        // Write the full record.
        let ri = RecordInfo::new(LogicalAddress::ZERO, 0, false, false, false);
        accessor.write_full_record(&ri, key, &value, &layout);

        // Phase 2: Commit — CAS the tentative entry to a committed one
        // pointing at the new record address.
        let committed = HashBucketEntry::new(result.entry.tag(), new_addr, false);
        if !ctx.hash_index.update(result.slot, result.entry, committed) {
            return OperationStatus::Aborted;
        }

        return OperationStatus::Created;
    }

    // ── Existing key — check which region the record is in ─────────
    let addr = result.entry.address();
    if !addr.is_valid() {
        return OperationStatus::NotFound;
    }

    let snap = ctx.allocator.snapshot();
    let region = snap.classify(addr);

    match region {
        AddressRegion::Mutable => {
            // In-place update: verify key matches, then overwrite value.
            let reader = LogRecordReader::new(ctx.allocator);
            match find_record_for_key(&reader, addr, key, &layout, ctx.allocator) {
                Some((found_addr, ri)) => {
                    if ri.is_tombstone() {
                        // Key was deleted — treat as new insert via RCU path.
                        return upsert_copy_to_tail(
                            ctx,
                            functions,
                            key,
                            input,
                            &layout,
                            result.entry,
                            result.slot,
                            found_addr,
                        );
                    }

                    // Read old value, write new value in-place.
                    let ptr = ctx.allocator.get_physical_address(found_addr);
                    if let Some(ptr) = ptr {
                        let record_size = layout.total_size() as u32;
                        // SAFETY: record is in the mutable region and we hold
                        // epoch protection, so the page frame won't be evicted.
                        let accessor = unsafe { MutableRecordAccessor::new(ptr, record_size) };

                        let old_value: F::Value = accessor.value(&layout);
                        let mut output = F::Output::default();

                        // Let the user callback decide the final value.
                        let mut new_val = old_value.clone();
                        functions.upsert(key, &mut new_val, input, Some(&old_value), &mut output);
                        accessor.write_value(&new_val, &layout);

                        OperationStatus::InPlaceUpdated
                    } else {
                        OperationStatus::Aborted
                    }
                }
                None => {
                    // Key didn't match any record in chain — this is a hash
                    // collision. We need to insert a new record.
                    upsert_copy_to_tail(
                        ctx,
                        functions,
                        key,
                        input,
                        &layout,
                        result.entry,
                        result.slot,
                        addr,
                    )
                }
            }
        }
        AddressRegion::FuzzyRegion | AddressRegion::ReadOnly => {
            // Copy to tail (RCU update).
            upsert_copy_to_tail(
                ctx,
                functions,
                key,
                input,
                &layout,
                result.entry,
                result.slot,
                addr,
            )
        }
        AddressRegion::OnDisk => {
            session.enqueue_pending(PendingOperation {
                op_type: PendingOpType::Upsert,
                key: key.clone(),
                input: None,
                context,
                address: addr,
                record_layout: layout,
                key_hash,
            });
            OperationStatus::Pending
        }
        _ => OperationStatus::Aborted,
    }
}

/// Upsert helper: allocate a new record at the tail and CAS the hash
/// entry to point to it (RCU path for read-only / new-key-from-chain).
fn upsert_copy_to_tail<F: Functions>(
    ctx: &InternalContext<'_>,
    functions: &F,
    key: &F::Key,
    input: &F::Input,
    layout: &RecordLayout,
    old_entry: HashBucketEntry,
    slot: &crate::hash_bucket::AtomicHashBucketEntry,
    previous_addr: LogicalAddress,
) -> OperationStatus {
    // Let the callback produce the final value.
    // SAFETY: For fixed-size numeric Value types (u64, i64, etc.), a zeroed
    // representation is valid. The callback immediately overwrites the value
    // before it is read.
    let mut new_val: F::Value = unsafe { std::mem::zeroed() };
    let mut output = F::Output::default();
    functions.upsert(key, &mut new_val, input, None, &mut output);

    let (new_addr, accessor) = match allocate_at_tail(ctx.allocator, key, &new_val) {
        Some(pair) => pair,
        None => return OperationStatus::Aborted,
    };

    // Write the record — link back to the previous address for chain.
    let ri = RecordInfo::new(previous_addr, 0, false, false, false);
    accessor.write_full_record(&ri, key, &new_val, layout);

    // CAS the hash entry to point to the new record.
    let committed = HashBucketEntry::new(old_entry.tag(), new_addr, false);
    if ctx.hash_index.update(slot, old_entry, committed) {
        OperationStatus::CopyUpdated
    } else {
        // CAS failed — concurrent modification.  For now, the record we
        // wrote is "orphaned" in the log (harmless, will be reclaimed by
        // compaction).
        OperationStatus::Aborted
    }
}

// ── RMW ─────────────────────────────────────────────────────────────

/// Internal RMW (Read-Modify-Write) operation.
///
/// - Key exists in **mutable** region → try in-place update; if
///   `NeedsNewRecord`, fall through to copy-to-tail.
/// - Key exists in **read-only / fuzzy** → copy to tail with modification.
/// - Key does not exist → create with initial value (`rmw_initial`).
/// - Key on disk → `Pending`.
pub(crate) fn internal_rmw<F: Functions>(
    ctx: &InternalContext<'_>,
    session: &mut FasterSession<F>,
    functions: &F,
    key: &F::Key,
    input: &F::Input,
    output: &mut F::Output,
    context: F::Context,
) -> OperationStatus {
    let key_hash = key.hash();
    let layout = layout_for_fixed::<F::Key, F::Value>(key);

    let result = ctx
        .hash_index
        .find_or_create(key_hash, LogicalAddress::INVALID);

    if result.created {
        // ── Key not found — create initial value ───────────────────
        if !functions.rmw_need_initial_update(key, input) {
            // User declined to create a new record — abort.
            let _ = ctx
                .hash_index
                .update(result.slot, result.entry, HashBucketEntry::EMPTY);
            return OperationStatus::NotFound;
        }

        // Create a zeroed value and let the callback initialise it.
        // SAFETY: For fixed-size numeric Value types (u64, i64, etc.), a zeroed
        // representation is valid. The callback immediately overwrites the value.
        let mut value = unsafe { std::mem::zeroed::<F::Value>() };
        functions.rmw_initial(key, input, &mut value, output);

        let (new_addr, accessor) = match allocate_at_tail(ctx.allocator, key, &value) {
            Some(pair) => pair,
            None => {
                let _ = ctx
                    .hash_index
                    .update(result.slot, result.entry, HashBucketEntry::EMPTY);
                return OperationStatus::Aborted;
            }
        };

        let ri = RecordInfo::new(LogicalAddress::ZERO, 0, false, false, false);
        accessor.write_full_record(&ri, key, &value, &layout);

        let committed = HashBucketEntry::new(result.entry.tag(), new_addr, false);
        if !ctx.hash_index.update(result.slot, result.entry, committed) {
            return OperationStatus::Aborted;
        }

        return OperationStatus::Created;
    }

    // ── Key exists — dispatch by region ────────────────────────────
    let addr = result.entry.address();
    if !addr.is_valid() {
        return OperationStatus::NotFound;
    }

    let snap = ctx.allocator.snapshot();
    let region = snap.classify(addr);

    match region {
        AddressRegion::Mutable => {
            let reader = LogRecordReader::new(ctx.allocator);
            match find_record_for_key(&reader, addr, key, &layout, ctx.allocator) {
                Some((found_addr, ri)) => {
                    if ri.is_tombstone() {
                        // Deleted key — treat as initial.
                        return rmw_create_at_tail(
                            ctx,
                            functions,
                            key,
                            input,
                            output,
                            &layout,
                            result.entry,
                            result.slot,
                            found_addr,
                        );
                    }

                    // Try in-place update.
                    let ptr = match ctx.allocator.get_physical_address(found_addr) {
                        Some(p) => p,
                        None => return OperationStatus::Aborted,
                    };

                    let record_size = layout.total_size() as u32;
                    // SAFETY: record is in mutable region, epoch guard held.
                    let accessor = unsafe { MutableRecordAccessor::new(ptr, record_size) };

                    let mut value: F::Value = accessor.value(&layout);
                    let rmw_result = functions.rmw_in_place(key, input, &mut value, output);

                    match rmw_result {
                        RmwInPlaceResult::InPlaceOk => {
                            accessor.write_value(&value, &layout);
                            OperationStatus::InPlaceUpdated
                        }
                        RmwInPlaceResult::NeedsNewRecord => {
                            // Fall through to copy-to-tail.
                            rmw_copy_to_tail(
                                ctx,
                                functions,
                                key,
                                input,
                                &value,
                                output,
                                &layout,
                                result.entry,
                                result.slot,
                                found_addr,
                            )
                        }
                    }
                }
                None => {
                    // Hash collision — key not in chain. Create initial.
                    rmw_create_at_tail(
                        ctx,
                        functions,
                        key,
                        input,
                        output,
                        &layout,
                        result.entry,
                        result.slot,
                        addr,
                    )
                }
            }
        }
        AddressRegion::FuzzyRegion | AddressRegion::ReadOnly => {
            let reader = LogRecordReader::new(ctx.allocator);
            match find_record_for_key(&reader, addr, key, &layout, ctx.allocator) {
                Some((found_addr, ri)) => {
                    if ri.is_tombstone() {
                        return rmw_create_at_tail(
                            ctx,
                            functions,
                            key,
                            input,
                            output,
                            &layout,
                            result.entry,
                            result.slot,
                            found_addr,
                        );
                    }

                    let old_value: F::Value = reader
                        .read_value(found_addr, &layout)
                        .expect("readable in-memory record");

                    if !functions.rmw_need_copy_update(key, input, &old_value) {
                        return OperationStatus::InPlaceUpdated;
                    }

                    rmw_copy_to_tail(
                        ctx,
                        functions,
                        key,
                        input,
                        &old_value,
                        output,
                        &layout,
                        result.entry,
                        result.slot,
                        found_addr,
                    )
                }
                None => rmw_create_at_tail(
                    ctx,
                    functions,
                    key,
                    input,
                    output,
                    &layout,
                    result.entry,
                    result.slot,
                    addr,
                ),
            }
        }
        AddressRegion::OnDisk => {
            session.enqueue_pending(PendingOperation {
                op_type: PendingOpType::Rmw,
                key: key.clone(),
                input: Some(input.clone()),
                context,
                address: addr,
                record_layout: layout,
                key_hash,
            });
            OperationStatus::Pending
        }
        _ => OperationStatus::Aborted,
    }
}

/// RMW helper: copy a read-only record to the tail with a modification.
fn rmw_copy_to_tail<F: Functions>(
    ctx: &InternalContext<'_>,
    functions: &F,
    key: &F::Key,
    input: &F::Input,
    old_value: &F::Value,
    output: &mut F::Output,
    layout: &RecordLayout,
    old_entry: HashBucketEntry,
    slot: &crate::hash_bucket::AtomicHashBucketEntry,
    previous_addr: LogicalAddress,
) -> OperationStatus {
    let mut new_value = old_value.clone();
    functions.rmw_copy_update(key, input, old_value, &mut new_value, output);

    let (new_addr, accessor) = match allocate_at_tail(ctx.allocator, key, &new_value) {
        Some(pair) => pair,
        None => return OperationStatus::Aborted,
    };

    let ri = RecordInfo::new(previous_addr, 0, false, false, false);
    accessor.write_full_record(&ri, key, &new_value, layout);

    let committed = HashBucketEntry::new(old_entry.tag(), new_addr, false);
    if ctx.hash_index.update(slot, old_entry, committed) {
        OperationStatus::CopyUpdated
    } else {
        OperationStatus::Aborted
    }
}

/// RMW helper: create a new initial-value record at the tail.
fn rmw_create_at_tail<F: Functions>(
    ctx: &InternalContext<'_>,
    functions: &F,
    key: &F::Key,
    input: &F::Input,
    output: &mut F::Output,
    layout: &RecordLayout,
    old_entry: HashBucketEntry,
    slot: &crate::hash_bucket::AtomicHashBucketEntry,
    previous_addr: LogicalAddress,
) -> OperationStatus {
    if !functions.rmw_need_initial_update(key, input) {
        return OperationStatus::NotFound;
    }

    // SAFETY: For fixed-size numeric Value types (u64, i64, etc.), a zeroed
    // representation is valid. The callback immediately overwrites the value.
    let mut value = unsafe { std::mem::zeroed::<F::Value>() };
    functions.rmw_initial(key, input, &mut value, output);

    let (new_addr, accessor) = match allocate_at_tail(ctx.allocator, key, &value) {
        Some(pair) => pair,
        None => return OperationStatus::Aborted,
    };

    let ri = RecordInfo::new(previous_addr, 0, false, false, false);
    accessor.write_full_record(&ri, key, &value, layout);

    let committed = HashBucketEntry::new(old_entry.tag(), new_addr, false);
    if ctx.hash_index.update(slot, old_entry, committed) {
        OperationStatus::Created
    } else {
        OperationStatus::Aborted
    }
}

// ── Delete ──────────────────────────────────────────────────────────

/// Internal Delete operation.
///
/// - Key exists in **mutable** region → set tombstone flag in-place → `Deleted`.
/// - Key exists in **read-only / fuzzy** → write tombstone record at tail → `Deleted`.
/// - Key does not exist → `NotFound`.
/// - Key on disk → `Pending`.
pub(crate) fn internal_delete<F: Functions>(
    ctx: &InternalContext<'_>,
    session: &mut FasterSession<F>,
    functions: &F,
    key: &F::Key,
    context: F::Context,
) -> OperationStatus {
    let key_hash = key.hash();
    let layout = layout_for_fixed::<F::Key, F::Value>(key);

    // Look up an existing entry — delete does not create new entries.
    let (entry, slot) = match ctx.hash_index.find(key_hash) {
        Some(pair) => pair,
        None => return OperationStatus::NotFound,
    };

    let addr = entry.address();
    if !addr.is_valid() {
        return OperationStatus::NotFound;
    }

    let snap = ctx.allocator.snapshot();
    let region = snap.classify(addr);

    match region {
        AddressRegion::Mutable => {
            let reader = LogRecordReader::new(ctx.allocator);
            match find_record_for_key(&reader, addr, key, &layout, ctx.allocator) {
                Some((found_addr, ri)) => {
                    if ri.is_tombstone() {
                        return OperationStatus::NotFound;
                    }

                    // Set tombstone in-place via atomic CAS on the RecordInfo.
                    let ptr = match ctx.allocator.get_physical_address(found_addr) {
                        Some(p) => p,
                        None => return OperationStatus::Aborted,
                    };

                    let record_size = layout.total_size() as u32;
                    // SAFETY: record is in mutable region, epoch guard held.
                    let accessor = unsafe { MutableRecordAccessor::new(ptr, record_size) };

                    // Invoke user callback for cleanup.
                    let mut value: F::Value = accessor.value(&layout);
                    functions.delete(key, &mut value);

                    // Write the tombstone header.
                    let new_ri = ri.with_tombstone();
                    accessor.write_record_info(&new_ri);

                    OperationStatus::Deleted
                }
                None => OperationStatus::NotFound,
            }
        }
        AddressRegion::FuzzyRegion | AddressRegion::ReadOnly => {
            // Write a tombstone record at the tail.
            let reader = LogRecordReader::new(ctx.allocator);
            match find_record_for_key(&reader, addr, key, &layout, ctx.allocator) {
                Some((_found_addr, ri)) => {
                    if ri.is_tombstone() {
                        return OperationStatus::NotFound;
                    }

                    // Allocate a tombstone record at tail.
                    let dummy_value: F::Value = reader
                        .read_value(_found_addr, &layout)
                        .expect("readable in-memory record");
                    let (new_addr, accessor) =
                        match allocate_at_tail(ctx.allocator, key, &dummy_value) {
                            Some(pair) => pair,
                            None => return OperationStatus::Aborted,
                        };

                    let tombstone_ri = RecordInfo::new(addr, 0, false, true, false);
                    accessor.write_full_record(&tombstone_ri, key, &dummy_value, &layout);

                    // CAS the hash entry to point to the tombstone.
                    let committed = HashBucketEntry::new(entry.tag(), new_addr, false);
                    if ctx.hash_index.update(slot, entry, committed) {
                        OperationStatus::Deleted
                    } else {
                        OperationStatus::Aborted
                    }
                }
                None => OperationStatus::NotFound,
            }
        }
        AddressRegion::OnDisk => {
            session.enqueue_pending(PendingOperation {
                op_type: PendingOpType::Delete,
                key: key.clone(),
                input: None,
                context,
                address: addr,
                record_layout: layout,
                key_hash,
            });
            OperationStatus::Pending
        }
        _ => OperationStatus::NotFound,
    }
}

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash_index::HashIndex;
    use crate::hybrid_log::log_allocator::HybridLogAllocator;
    use crate::store::functions::SimpleFunctions;

    type TestFunctions = SimpleFunctions<u64, u64>;

    /// Create a minimal store context for testing.
    ///
    /// Uses 256 hash buckets and 4 pages at 90% mutable fraction so that
    /// virtually all records land in the mutable region.
    fn test_context() -> (HashIndex, HybridLogAllocator, TestFunctions) {
        let hash_index = HashIndex::new(8); // 256 buckets
        let allocator = HybridLogAllocator::new(4, 0.9, 512);
        // Skip past sentinel addresses (0 = ZERO, 1 = INVALID) so that the
        // first real record lives at an address where is_valid() returns true.
        allocator.try_allocate(8).expect("skip sentinel addresses");
        let functions = SimpleFunctions::default();
        (hash_index, allocator, functions)
    }

    /// Helper: create an InternalContext from the test components.
    fn ctx<'a>(
        hash_index: &'a HashIndex,
        allocator: &'a HybridLogAllocator,
    ) -> InternalContext<'a> {
        InternalContext {
            hash_index,
            allocator,
        }
    }

    /// Helper: create a session with epoch registration.
    fn test_session(hash_index: &HashIndex) -> FasterSession<TestFunctions> {
        let epoch_table = hash_index.epoch_arc();
        let epoch_thread = epoch_table.register().expect("register thread");
        FasterSession::new(epoch_thread, epoch_table)
    }

    // ────────────────────────────────────────────────────────────────
    // 1. Upsert creates a new record
    // ────────────────────────────────────────────────────────────────

    #[test]
    fn upsert_creates_new_record() {
        let (hi, alloc, funcs) = test_context();
        let mut session = test_session(&hi);
        let mut guard = session.begin_unsafe();

        let ic = ctx(&hi, &alloc);
        let status = internal_upsert(&ic, guard.session_mut(), &funcs, &42u64, &100u64, ());
        assert_eq!(status, OperationStatus::Created);

        // Verify the hash entry now exists.
        let key_hash = 42u64.hash();
        let (entry, _) = hi.find(key_hash).expect("entry must exist");
        assert!(entry.address().is_valid());
    }

    // ────────────────────────────────────────────────────────────────
    // 2. Upsert updates an existing record in the mutable region
    // ────────────────────────────────────────────────────────────────

    #[test]
    fn upsert_updates_existing_in_mutable() {
        let (hi, alloc, funcs) = test_context();
        let mut session = test_session(&hi);
        let mut guard = session.begin_unsafe();

        let ic = ctx(&hi, &alloc);

        // First upsert: create.
        let s1 = internal_upsert(&ic, guard.session_mut(), &funcs, &1u64, &10u64, ());
        assert_eq!(s1, OperationStatus::Created);

        // Second upsert: in-place update.
        let s2 = internal_upsert(&ic, guard.session_mut(), &funcs, &1u64, &20u64, ());
        assert_eq!(s2, OperationStatus::InPlaceUpdated);
    }

    // ────────────────────────────────────────────────────────────────
    // 3. Read an existing record
    // ────────────────────────────────────────────────────────────────

    #[test]
    fn read_existing_record() {
        let (hi, alloc, funcs) = test_context();
        let mut session = test_session(&hi);
        let mut guard = session.begin_unsafe();

        let ic = ctx(&hi, &alloc);
        let _ = internal_upsert(&ic, guard.session_mut(), &funcs, &7u64, &777u64, ());

        let mut output: Option<u64> = None;
        let status = internal_read(
            &ic,
            guard.session_mut(),
            &funcs,
            &7u64,
            &0u64, // input (unused for SimpleFunctions read)
            &mut output,
            (),
        );
        assert_eq!(status, OperationStatus::Ok);
        assert_eq!(output, Some(777));
    }

    // ────────────────────────────────────────────────────────────────
    // 4. Read a nonexistent key returns NotFound
    // ────────────────────────────────────────────────────────────────

    #[test]
    fn read_nonexistent_returns_not_found() {
        let (hi, alloc, funcs) = test_context();
        let mut session = test_session(&hi);
        let mut guard = session.begin_unsafe();

        let ic = ctx(&hi, &alloc);
        let mut output: Option<u64> = None;
        let status = internal_read(
            &ic,
            guard.session_mut(),
            &funcs,
            &999u64,
            &0u64,
            &mut output,
            (),
        );
        assert_eq!(status, OperationStatus::NotFound);
        assert_eq!(output, None);
    }

    // ────────────────────────────────────────────────────────────────
    // 5. Delete an existing record
    // ────────────────────────────────────────────────────────────────

    #[test]
    fn delete_existing_record() {
        let (hi, alloc, funcs) = test_context();
        let mut session = test_session(&hi);
        let mut guard = session.begin_unsafe();

        let ic = ctx(&hi, &alloc);
        let _ = internal_upsert(&ic, guard.session_mut(), &funcs, &5u64, &50u64, ());

        let status = internal_delete(&ic, guard.session_mut(), &funcs, &5u64, ());
        assert_eq!(status, OperationStatus::Deleted);
    }

    // ────────────────────────────────────────────────────────────────
    // 6. Read after delete returns NotFound
    // ────────────────────────────────────────────────────────────────

    #[test]
    fn read_after_delete_returns_not_found() {
        let (hi, alloc, funcs) = test_context();
        let mut session = test_session(&hi);
        let mut guard = session.begin_unsafe();

        let ic = ctx(&hi, &alloc);
        let _ = internal_upsert(&ic, guard.session_mut(), &funcs, &8u64, &80u64, ());
        let _ = internal_delete(&ic, guard.session_mut(), &funcs, &8u64, ());

        let mut output: Option<u64> = None;
        let status = internal_read(
            &ic,
            guard.session_mut(),
            &funcs,
            &8u64,
            &0u64,
            &mut output,
            (),
        );
        assert_eq!(status, OperationStatus::NotFound);
        assert_eq!(output, None);
    }

    // ────────────────────────────────────────────────────────────────
    // 7. RMW creates initial value for a nonexistent key
    // ────────────────────────────────────────────────────────────────

    #[test]
    fn rmw_creates_initial_value() {
        let (hi, alloc, funcs) = test_context();
        let mut session = test_session(&hi);
        let mut guard = session.begin_unsafe();

        let ic = ctx(&hi, &alloc);
        let mut output: Option<u64> = None;
        let status = internal_rmw(
            &ic,
            guard.session_mut(),
            &funcs,
            &10u64,
            &42u64, // input = desired initial value for SimpleFunctions
            &mut output,
            (),
        );
        assert_eq!(status, OperationStatus::Created);

        // Verify via read.
        let mut read_output: Option<u64> = None;
        let read_status = internal_read(
            &ic,
            guard.session_mut(),
            &funcs,
            &10u64,
            &0u64,
            &mut read_output,
            (),
        );
        assert_eq!(read_status, OperationStatus::Ok);
        assert_eq!(read_output, Some(42));
    }

    // ────────────────────────────────────────────────────────────────
    // 8. RMW modifies existing mutable record in-place
    // ────────────────────────────────────────────────────────────────

    #[test]
    fn rmw_modifies_existing_in_place() {
        let (hi, alloc, funcs) = test_context();
        let mut session = test_session(&hi);
        let mut guard = session.begin_unsafe();

        let ic = ctx(&hi, &alloc);

        // Create initial value via upsert.
        let _ = internal_upsert(&ic, guard.session_mut(), &funcs, &20u64, &100u64, ());

        // RMW should update in-place (SimpleFunctions replaces the value).
        let mut output: Option<u64> = None;
        let status = internal_rmw(
            &ic,
            guard.session_mut(),
            &funcs,
            &20u64,
            &200u64,
            &mut output,
            (),
        );
        assert_eq!(status, OperationStatus::InPlaceUpdated);

        // Verify via read.
        let mut read_output: Option<u64> = None;
        let _ = internal_read(
            &ic,
            guard.session_mut(),
            &funcs,
            &20u64,
            &0u64,
            &mut read_output,
            (),
        );
        assert_eq!(read_output, Some(200));
    }

    // ────────────────────────────────────────────────────────────────
    // 9. Upsert multiple keys, read them all back
    // ────────────────────────────────────────────────────────────────

    #[test]
    fn upsert_multiple_keys() {
        let (hi, alloc, funcs) = test_context();
        let mut session = test_session(&hi);
        let mut guard = session.begin_unsafe();

        let ic = ctx(&hi, &alloc);

        // Insert 100 keys.
        for i in 0u64..100 {
            let status = internal_upsert(&ic, guard.session_mut(), &funcs, &i, &(i * 10), ());
            assert!(
                status == OperationStatus::Created || status == OperationStatus::CopyUpdated,
                "key {i}: unexpected status {status}"
            );
        }

        // Read them all back.
        for i in 0u64..100 {
            let mut output: Option<u64> = None;
            let status =
                internal_read(&ic, guard.session_mut(), &funcs, &i, &0u64, &mut output, ());
            assert_eq!(status, OperationStatus::Ok, "read key {i} failed");
            assert_eq!(output, Some(i * 10), "value mismatch for key {i}");
        }
    }

    // ────────────────────────────────────────────────────────────────
    // 10. Upsert overwrites value
    // ────────────────────────────────────────────────────────────────

    #[test]
    fn upsert_overwrite_value() {
        let (hi, alloc, funcs) = test_context();
        let mut session = test_session(&hi);
        let mut guard = session.begin_unsafe();

        let ic = ctx(&hi, &alloc);

        // Insert with value A.
        let _ = internal_upsert(&ic, guard.session_mut(), &funcs, &99u64, &111u64, ());

        // Overwrite with value B.
        let _ = internal_upsert(&ic, guard.session_mut(), &funcs, &99u64, &222u64, ());

        // Read → should return B.
        let mut output: Option<u64> = None;
        let _ = internal_read(
            &ic,
            guard.session_mut(),
            &funcs,
            &99u64,
            &0u64,
            &mut output,
            (),
        );
        assert_eq!(output, Some(222));
    }
}
