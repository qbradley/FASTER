# Decision: Hash Table Core Design

**Agent:** Aragorn (Rust Expert)
**Date:** 2025-07-18
**Task:** 2d — Hash Table Core
**Status:** Implemented

## Decision 1: Tentative Duplicate Detection is Tag-Level Only

**Choice:** `find_or_create_entry` re-scans for *committed* duplicates after tentative CAS. It does NOT detect concurrent tentative duplicates.

**Rationale:**
- Two threads can both create tentative entries with the same tag in different bucket slots. This is correct behavior matching C++ FASTER.
- The hash table is a tag-based filter, not a unique-key store. Tag collision (same tag, different key) is expected.
- True key-level deduplication happens at the log layer: the caller reads the record at the address and compares the actual key.
- Attempting to detect tentative duplicates would require waiting for the other thread to commit or abort, breaking lock-freedom.

**Impact:** Callers (FasterKv operations) must handle the case where two entries with the same tag exist in the bucket chain. The log-level key comparison resolves this.

## Decision 2: `get_unchecked` for Bucket Lookup

**Choice:** Used `unsafe { self.buckets.get_unchecked(idx) }` in `bucket()` instead of safe indexing.

**Rationale:**
- `bucket()` is THE hot path — called on every hash table operation.
- The index is always `hash & (num_buckets - 1)` which is mathematically guaranteed to be `< num_buckets`.
- Eliminating the bounds check avoids a branch on every probe.
- Safety invariant is trivial and well-documented: `buckets.len() == num_buckets` (set in constructor), `hash.index(n)` returns `hash & (n-1)` which is always `< n` for `n > 0`.

**Impact:** This is the only `unsafe` block in the hash table module. Galadriel should verify the invariant during unsafe audit.

## Decision 3: FindOrCreateResult Returns Slot Reference

**Choice:** `find_or_create_entry` returns `FindOrCreateResult { entry, slot: &AtomicHashBucketEntry, created }` — a reference to the atomic slot, not the bucket+index.

**Rationale:**
- The caller needs the slot reference for subsequent CAS operations (commit tentative, update address, abort).
- Returning `&AtomicHashBucketEntry` directly is more ergonomic than returning `(bucket_ref, slot_index)` and forcing the caller to re-index.
- The lifetime ties the result to the hash table's lifetime, preventing use-after-free.

**Impact:** All callers interact with the slot via `table.update_entry(result.slot, old, new)`. No need to re-look up the bucket.

## Decision 4: No Separate `delete_entry` API

**Choice:** Deletion is handled via `update_entry(slot, entry, EMPTY)` rather than a dedicated `delete_entry` method.

**Rationale:**
- In C++ FASTER, deletion sets a tombstone in the *log record*, not in the hash table entry. The hash entry stays pointing to the tombstoned record.
- Entry invalidation (CAS to EMPTY) is used for aborting tentative inserts, not for key deletion.
- A dedicated delete API would be misleading — it suggests key-level deletion, which is a higher-level operation.

**Impact:** Task 2e (concurrent ops) and the FasterKv layer handle deletion semantics via record tombstones.
