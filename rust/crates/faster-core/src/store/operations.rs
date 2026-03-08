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
//! # Variable-length value support
//!
//! For fixed-size types (`u64`, `i64`, etc.) the record layout can be
//! determined at compile time. For variable-length types (`Vec<u8>`,
//! `String`) the serialized value size is only known at write time, so
//! **read paths use page-bounded record sizing** rather than relying on
//! `layout_for_fixed`. The key_offset / value_offset from
//! `layout_for_fixed` are still correct (they depend only on key size),
//! but `total_size()` must not be used as the authoritative record size
//! when reading from the log.

use crate::address::{LogicalAddress, OFFSET_BITS};
use crate::hash::Hashable;
use crate::hash::bucket::HashBucketEntry;
use crate::hash::index::HashIndex;
use crate::hash::prefetch;
use crate::hybrid_log::log_allocator::HybridLogAllocator;
use crate::hybrid_log::record_ops::{LogRecordReader, LogRecordWriter, MutableRecordAccessor};
use crate::hybrid_log::regions::{AddressInfo, AddressRegion};
use crate::record::{Key, RecordInfo, RecordLayout, Value};
use crate::status::OperationStatus;
use crate::store::functions::{DeleteInfo, Functions, ReadInfo, RmwInfo, RmwInPlaceResult, UpsertInfo};
use crate::store::session::{FasterSession, PendingOpType, PendingOperation};

/// Maximum number of version chain hops before giving up.
///
/// Prevents infinite traversal on corrupted data (e.g., a 2-node cycle
/// A→B→A that the self-loop guard doesn't catch). At 32 MB per page,
/// 4096 records per chain is well beyond any realistic workload.
const MAX_CHAIN_DEPTH: usize = 4096;

// ── InternalContext ─────────────────────────────────────────────────

/// Shared references to the store's components needed by CRUD operations.
///
/// Passed to operation functions to avoid threading many parameters.
pub(crate) struct InternalContext<'a> {
    pub hash_index: &'a HashIndex,
    pub allocator: &'a HybridLogAllocator,
}

// ── Helpers ─────────────────────────────────────────────────────────

/// Issue a Level-2 prefetch for the record at `addr`.
///
/// After the hash bucket lookup returns a record address, this function
/// prefetches the record's cache line(s) so the data is in L1 by the
/// time we walk the version chain and access the record.
///
/// This is the "second level" of the two-level prefetch pipeline:
///   L1: hash bucket prefetch (issued before find/find_or_create)
///   L2: record prefetch (issued after find returns the entry address)
///
/// For writes (`is_write = true`) we use `prefetch_write` to bring the
/// line into exclusive MESI state, avoiding a read-for-ownership stall.
#[inline(always)]
fn prefetch_record(allocator: &HybridLogAllocator, addr: LogicalAddress, is_write: bool) {
    if !addr.is_valid() {
        return;
    }
    if let Some(ptr) = allocator.get_physical_address(addr) {
        if is_write {
            prefetch::prefetch_write(ptr);
        } else {
            prefetch::prefetch_read(ptr as *const u8);
        }
    }
}

/// Compute the [`RecordLayout`] for a key/value pair using the fixed-size
/// approach: `std::mem::size_of::<V>()` for the value size.
///
/// For fixed-size types (`u64`, `i64`, etc.) this matches `Value::serialized_size`.
///
/// **Important:** For variable-length types (e.g. `Vec<u8>`), only
/// `key_offset()` and `value_offset()` are reliable — the `total_size()`
/// will be incorrect. Use [`RecordLayout::for_kv`] with the actual value
/// instance when computing allocation sizes or verifying write bounds.
#[inline]
fn layout_for_fixed<K: Key, V: Value>(key: &K) -> RecordLayout {
    RecordLayout::compute(key.serialized_size(), std::mem::size_of::<V>())
}

/// Compute a safe record_size for reading a record at the given address.
///
/// Records never span page boundaries, so the remaining space within the
/// current page is a safe upper bound. This is necessary for variable-length
/// records where [`layout_for_fixed`] may underestimate the value size.
///
/// Falls back to `min_size` if the computed page remainder is somehow
/// smaller (e.g., the address is very close to the page end, but records
/// near the end would have been sized by the allocator to fit).
#[inline]
fn safe_read_record_size(addr: LogicalAddress, min_size: u32) -> u32 {
    let page_size = 1u32 << OFFSET_BITS;
    let offset = addr.offset().0;
    let remaining = page_size.saturating_sub(offset);
    remaining.max(min_size)
}

/// Walk the version chain starting from `start_addr` looking for the
/// latest non-invalid record whose key matches `key`.
///
/// Returns `(address, RecordInfo)` of the first matching record, or
/// `None` if no match is found in the in-memory portion of the chain.
///
/// Uses the caller's [`AddressInfo`] snapshot to classify addresses,
/// avoiding redundant atomic loads on the hot path. The snapshot is
/// safe because epoch protection guarantees pages won't be evicted
/// during an operation.
///
/// `read_header_and_match_key` is used to merge the `RecordInfo` read
/// and key comparison into a single physical-address lookup per hop.
fn find_record_for_key<K: Key>(
    reader: &LogRecordReader<'_>,
    start_addr: LogicalAddress,
    key: &K,
    layout: &RecordLayout,
    info: &AddressInfo,
) -> Option<(LogicalAddress, RecordInfo)> {
    let mut addr = start_addr;
    let mut depth = 0usize;

    while addr.is_valid() {
        // SF-1: Guard against cycles and pathologically long chains.
        if depth >= MAX_CHAIN_DEPTH {
            debug_assert!(
                false,
                "version chain exceeded MAX_CHAIN_DEPTH ({MAX_CHAIN_DEPTH}) — possible cycle or corruption"
            );
            return None;
        }
        depth += 1;

        // If the record is not in memory we cannot check the key.
        if !info.classify(addr).is_in_memory() {
            return None;
        }

        // Read header + check key in a single physical-address lookup.
        let (ri, matched) = reader.read_header_and_match_key(addr, key, layout)?;

        // Skip invalidated records — they have been superseded.
        if !ri.is_invalid() && matched {
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
        if !info.classify(prev).is_in_memory() {
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
pub(crate) fn allocate_at_tail<K: Key, V: Value>(
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

    // Prefetch the hash bucket — the CPU begins fetching the cache line
    // while we reach the find() call below.
    ctx.hash_index.prefetch(key_hash);

    // 1. Look up committed entry in the hash index.
    let (entry, _slot) = match ctx.hash_index.find(key_hash) {
        Some(pair) => pair,
        None => return OperationStatus::NotFound,
    };

    let addr = entry.address();
    if !addr.is_valid() {
        return OperationStatus::NotFound;
    }

    // L2 prefetch: bring the record cache line into L1 while we
    // compute the snapshot and classify the address region.
    prefetch_record(ctx.allocator, addr, false);

    // 2. Classify the address region.
    let info = ctx.allocator.snapshot();
    let region = info.classify(addr);

    let layout = layout_for_fixed::<F::Key, F::Value>(key);

    match region {
        AddressRegion::Mutable | AddressRegion::FuzzyRegion | AddressRegion::ReadOnly => {
            // Record is in memory — walk the chain for a key match.
            let reader = LogRecordReader::new(ctx.allocator);

            match find_record_for_key(&reader, addr, key, &layout, &info) {
                Some((_found_addr, ri)) => {
                    if ri.is_tombstone() {
                        return OperationStatus::NotFound;
                    }

                    // Read the value and invoke the user callback.
                    let safe_size = safe_read_record_size(_found_addr, layout.total_size() as u32);
                    let value: F::Value = reader
                        .get_record(_found_addr, safe_size)
                        .map(|acc| acc.value::<F::Value>(&layout))
                        .expect("value must be readable for in-memory record");
                    functions.read(key, &value, input, output, &ReadInfo::new(0, _found_addr, ri));
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

    // Prefetch the hash bucket — layout computation provides latency cover.
    ctx.hash_index.prefetch(key_hash);

    // Phase 1: Probe the hash index.  `find_or_create` either finds an
    // existing committed entry or CAS-inserts a tentative one.
    let result = ctx
        .hash_index
        .find_or_create(key_hash, LogicalAddress::INVALID);

    if result.created {
        // ── New key — allocate a record at the tail ────────────────

        // Let the callback initialise the value into a default-constructed slot.
        let mut value: F::Value = F::Value::default();
        let mut output = F::Output::default();
        functions.upsert(key, &mut value, input, None, &mut output, &UpsertInfo::new(0, LogicalAddress::INVALID, RecordInfo::default()));

        let (new_addr, mut accessor) = match allocate_at_tail(ctx.allocator, key, &value) {
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
        let write_layout = RecordLayout::for_kv(key, &value);
        accessor.write_full_record(&ri, key, &value, &write_layout);

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

    // L2 prefetch: bring the record into L1 in exclusive state for
    // the likely in-place write while we snapshot and classify.
    prefetch_record(ctx.allocator, addr, true);

    let snap = ctx.allocator.snapshot();
    let region = snap.classify(addr);

    match region {
        AddressRegion::Mutable => {
            // In-place update: verify key matches, then overwrite value.
            let reader = LogRecordReader::new(ctx.allocator);
            match find_record_for_key(&reader, addr, key, &layout, &snap) {
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

                    // Sealed records are write-protected: fall back to
                    // copy-to-tail (RCU) instead of in-place mutation.
                    if ri.is_sealed() {
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

                    // In-place update via raw or standard path.
                    let ptr = ctx.allocator.get_physical_address(found_addr);
                    if let Some(ptr) = ptr {
                        // SF-15: Verify the address is still in memory after
                        // obtaining the physical pointer. Epoch protection
                        // should prevent eviction, but this catches bugs.
                        debug_assert!(
                            ctx.allocator.is_in_memory(found_addr),
                            "SF-15: address evicted between classify and access"
                        );
                        let record_size =
                            safe_read_record_size(found_addr, layout.total_size() as u32);
                        // SAFETY: record is in the mutable region and we hold
                        // epoch protection, so the page frame won't be evicted.
                        let mut accessor = unsafe { MutableRecordAccessor::new(ptr, record_size) };

                        if F::SUPPORTS_RAW_IN_PLACE {
                            let value_ptr = accessor.value_mut_ptr(&layout);
                            let value_len = std::mem::size_of::<F::Value>();
                            let mut output = F::Output::default();
                            // SAFETY: value_ptr points into a mutable-region
                            // page frame under epoch protection.
                            unsafe {
                                functions.upsert_in_place_raw(
                                    key,
                                    value_ptr,
                                    value_len,
                                    input,
                                    &mut output,
                                    &UpsertInfo::new(0, found_addr, ri),
                                );
                            }
                        } else {
                            let old_value: F::Value = accessor.value(&layout);
                            let mut output = F::Output::default();
                            let mut new_val = old_value.clone();
                            functions.upsert(
                                key,
                                &mut new_val,
                                input,
                                Some(&old_value),
                                &mut output,
                                &UpsertInfo::new(0, found_addr, ri),
                            );
                            // For variable-length values, check if the new
                            // value fits in the existing record's allocation.
                            if new_val.serialized_size() > old_value.serialized_size() {
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
                            let write_layout = RecordLayout::for_kv(key, &new_val);
                            accessor.write_value(&new_val, &write_layout);
                        }

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

/// Upsert helper: allocate a new record at the tail and CAS the hash
/// entry to point to it (RCU path for read-only / new-key-from-chain).
///
/// **Note (SF-3):** The RCU path does not read the old value from
/// `previous_addr`, passing `None` to `Functions::upsert`. For
/// `SimpleFunctions` (blind overwrite) this is correct. Merge-on-upsert
/// semantics require reading the previous record first — intentionally
/// deferred to a dedicated merge-semantics iteration.
fn upsert_copy_to_tail<F: Functions>(
    ctx: &InternalContext<'_>,
    functions: &F,
    key: &F::Key,
    input: &F::Input,
    _layout: &RecordLayout,
    old_entry: HashBucketEntry,
    slot: &crate::hash::bucket::AtomicHashBucketEntry,
    previous_addr: LogicalAddress,
) -> OperationStatus {
    // Let the callback produce the final value.
    let mut new_val: F::Value = F::Value::default();
    let mut output = F::Output::default();
    functions.upsert(key, &mut new_val, input, None, &mut output, &UpsertInfo::new(0, previous_addr, RecordInfo::default()));

    let (new_addr, mut accessor) = match allocate_at_tail(ctx.allocator, key, &new_val) {
        Some(pair) => pair,
        None => return OperationStatus::Aborted,
    };

    // Write the record — link back to the previous address for chain.
    let ri = RecordInfo::new(previous_addr, 0, false, false, false);
    let write_layout = RecordLayout::for_kv(key, &new_val);
    accessor.write_full_record(&ri, key, &new_val, &write_layout);

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

    // Prefetch the hash bucket — layout computation provides latency cover.
    ctx.hash_index.prefetch(key_hash);

    let result = ctx
        .hash_index
        .find_or_create(key_hash, LogicalAddress::INVALID);

    if result.created {
        // ── Key not found — create initial value ───────────────────
        if !functions.rmw_need_initial_update(key, input, &RmwInfo::new(0, LogicalAddress::INVALID, RecordInfo::default(), false)) {
            // User declined to create a new record — abort.
            let _ = ctx
                .hash_index
                .update(result.slot, result.entry, HashBucketEntry::EMPTY);
            return OperationStatus::NotFound;
        }

        // Create a default value and let the callback initialise it.
        let mut value = F::Value::default();
        functions.rmw_initial(key, input, &mut value, output, &RmwInfo::new(0, LogicalAddress::INVALID, RecordInfo::default(), false));

        let (new_addr, mut accessor) = match allocate_at_tail(ctx.allocator, key, &value) {
            Some(pair) => pair,
            None => {
                let _ = ctx
                    .hash_index
                    .update(result.slot, result.entry, HashBucketEntry::EMPTY);
                return OperationStatus::Aborted;
            }
        };

        let ri = RecordInfo::new(LogicalAddress::ZERO, 0, false, false, false);
        let write_layout = RecordLayout::for_kv(key, &value);
        accessor.write_full_record(&ri, key, &value, &write_layout);

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

    // L2 prefetch: bring the record into L1 in exclusive state
    // (RMW will likely modify in-place).
    prefetch_record(ctx.allocator, addr, true);

    let snap = ctx.allocator.snapshot();
    let region = snap.classify(addr);

    match region {
        AddressRegion::Mutable => {
            let reader = LogRecordReader::new(ctx.allocator);
            match find_record_for_key(&reader, addr, key, &layout, &snap) {
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

                    // Sealed records are write-protected: read current
                    // value then copy-to-tail (RCU).
                    if ri.is_sealed() {
                        let reader = LogRecordReader::new(ctx.allocator);
                        let value: F::Value = match reader.read_value(found_addr, &layout) {
                            Some(v) => v,
                            None => return OperationStatus::Aborted,
                        };
                        return rmw_copy_to_tail(
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
                        );
                    }

                    // Try in-place update.
                    let ptr = match ctx.allocator.get_physical_address(found_addr) {
                        Some(p) => p,
                        None => return OperationStatus::Aborted,
                    };

                    // SF-15: Verify the address is still in memory.
                    debug_assert!(
                        ctx.allocator.is_in_memory(found_addr),
                        "SF-15: address evicted between classify and access"
                    );
                    let record_size = safe_read_record_size(found_addr, layout.total_size() as u32);
                    // SAFETY: record is in mutable region, epoch guard held.
                    let mut accessor = unsafe { MutableRecordAccessor::new(ptr, record_size) };

                    if F::SUPPORTS_RAW_IN_PLACE {
                        let value_ptr = accessor.value_mut_ptr(&layout);
                        let value_len = std::mem::size_of::<F::Value>();
                        // SAFETY: value_ptr in mutable-region page under epoch.
                        let rmw_result = unsafe {
                            functions.rmw_in_place_raw(key, value_ptr, value_len, input, output, &RmwInfo::new(0, found_addr, ri, false))
                        };
                        match rmw_result {
                            RmwInPlaceResult::InPlaceOk => OperationStatus::InPlaceUpdated,
                            RmwInPlaceResult::NeedsNewRecord => {
                                let value: F::Value = accessor.value(&layout);
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
                    } else {
                        let mut value: F::Value = accessor.value(&layout);
                        let rmw_result = functions.rmw_in_place(key, input, &mut value, output, &RmwInfo::new(0, found_addr, ri, false));
                        match rmw_result {
                            RmwInPlaceResult::InPlaceOk => {
                                let write_layout = RecordLayout::for_kv(key, &value);
                                accessor.write_value(&value, &write_layout);
                                OperationStatus::InPlaceUpdated
                            }
                            RmwInPlaceResult::NeedsNewRecord => rmw_copy_to_tail(
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
                            ),
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
            match find_record_for_key(&reader, addr, key, &layout, &snap) {
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

                    let safe_size = safe_read_record_size(found_addr, layout.total_size() as u32);
                    let old_value: F::Value = reader
                        .get_record(found_addr, safe_size)
                        .map(|acc| acc.value::<F::Value>(&layout))
                        .expect("readable in-memory record");

                    if !functions.rmw_need_copy_update(key, input, &old_value, &RmwInfo::new(0, found_addr, ri, true)) {
                        return OperationStatus::Ok;
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
    _layout: &RecordLayout,
    old_entry: HashBucketEntry,
    slot: &crate::hash::bucket::AtomicHashBucketEntry,
    previous_addr: LogicalAddress,
) -> OperationStatus {
    let mut new_value = old_value.clone();
    functions.rmw_copy_update(key, input, old_value, &mut new_value, output, &RmwInfo::new(0, previous_addr, RecordInfo::default(), true));

    let (new_addr, mut accessor) = match allocate_at_tail(ctx.allocator, key, &new_value) {
        Some(pair) => pair,
        None => return OperationStatus::Aborted,
    };

    let ri = RecordInfo::new(previous_addr, 0, false, false, false);
    let write_layout = RecordLayout::for_kv(key, &new_value);
    accessor.write_full_record(&ri, key, &new_value, &write_layout);

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
    _layout: &RecordLayout,
    old_entry: HashBucketEntry,
    slot: &crate::hash::bucket::AtomicHashBucketEntry,
    previous_addr: LogicalAddress,
) -> OperationStatus {
    if !functions.rmw_need_initial_update(key, input, &RmwInfo::new(0, LogicalAddress::INVALID, RecordInfo::default(), false)) {
        return OperationStatus::NotFound;
    }

    let mut value = F::Value::default();
    functions.rmw_initial(key, input, &mut value, output, &RmwInfo::new(0, LogicalAddress::INVALID, RecordInfo::default(), false));

    let (new_addr, mut accessor) = match allocate_at_tail(ctx.allocator, key, &value) {
        Some(pair) => pair,
        None => return OperationStatus::Aborted,
    };

    let ri = RecordInfo::new(previous_addr, 0, false, false, false);
    let write_layout = RecordLayout::for_kv(key, &value);
    accessor.write_full_record(&ri, key, &value, &write_layout);

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

    // Prefetch the hash bucket — layout computation provides latency cover.
    ctx.hash_index.prefetch(key_hash);

    // Look up an existing entry — delete does not create new entries.
    let (entry, slot) = match ctx.hash_index.find(key_hash) {
        Some(pair) => pair,
        None => return OperationStatus::NotFound,
    };

    let addr = entry.address();
    if !addr.is_valid() {
        return OperationStatus::NotFound;
    }

    // L2 prefetch: bring the record into L1 in exclusive state
    // (delete writes a tombstone flag).
    prefetch_record(ctx.allocator, addr, true);

    let snap = ctx.allocator.snapshot();
    let region = snap.classify(addr);

    match region {
        AddressRegion::Mutable => {
            let reader = LogRecordReader::new(ctx.allocator);
            match find_record_for_key(&reader, addr, key, &layout, &snap) {
                Some((found_addr, ri)) => {
                    if ri.is_tombstone() {
                        return OperationStatus::NotFound;
                    }

                    // Set tombstone in-place via atomic CAS on the RecordInfo.
                    let ptr = match ctx.allocator.get_physical_address(found_addr) {
                        Some(p) => p,
                        None => return OperationStatus::Aborted,
                    };

                    let record_size = safe_read_record_size(found_addr, layout.total_size() as u32);
                    // SAFETY: record is in mutable region, epoch guard held.
                    let mut accessor = unsafe { MutableRecordAccessor::new(ptr, record_size) };

                    // Invoke user callback for cleanup.
                    let mut value: F::Value = accessor.value(&layout);
                    functions.delete(key, &mut value, &DeleteInfo::new(0, found_addr, ri));

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
            match find_record_for_key(&reader, addr, key, &layout, &snap) {
                Some((_found_addr, ri)) => {
                    if ri.is_tombstone() {
                        return OperationStatus::NotFound;
                    }

                    // Allocate a tombstone record at tail.
                    let safe_size = safe_read_record_size(_found_addr, layout.total_size() as u32);
                    let dummy_value: F::Value = reader
                        .get_record(_found_addr, safe_size)
                        .map(|acc| acc.value::<F::Value>(&layout))
                        .expect("readable in-memory record");
                    let (new_addr, mut accessor) =
                        match allocate_at_tail(ctx.allocator, key, &dummy_value) {
                            Some(pair) => pair,
                            None => return OperationStatus::Aborted,
                        };

                    let tombstone_ri = RecordInfo::new(addr, 0, false, true, false);
                    let write_layout = RecordLayout::for_kv(key, &dummy_value);
                    accessor.write_full_record(&tombstone_ri, key, &dummy_value, &write_layout);

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
    use crate::hash::index::HashIndex;
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
