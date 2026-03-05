# Design Decisions — HashIndex (Work Item 2e)

## 1. Epoch Discipline: Caller-Managed Guards

**Decision:** `find`, `find_or_create`, and `update` do NOT acquire epoch guards internally.
They document that the caller must hold an `EpochGuard`.

**Rationale:** FASTER's session model acquires epoch protection once per operation batch,
not per-operation. Acquiring per-operation would be wasteful and break the reentrant
semantics — the caller's guard already covers the entire critical section.

## 2. `invalidate_entries_in_range` Does NOT Require Epoch Protection

**Decision:** This method is called from epoch drain callbacks (after safe-epoch advances),
so it runs outside epoch protection. It CAS's entries to EMPTY; failures are harmless
(another thread modified the entry concurrently).

**Rationale:** Matches C++ `InternalHashTable::InvalidateEntries()` which is invoked
from page eviction callbacks. No epoch guard needed because:
- We're not following addresses into the log — just zeroing hash index entries.
- CAS failure is benign (entry was concurrently modified, which is fine).

## 3. HashIndex Owns HashTable and EpochTable by Value

**Decision:** `HashIndex { table: HashTable, epoch: Arc<EpochTable> }`. The epoch table
is `Arc`-wrapped because `EpochTable::register()` requires `&Arc<Self>`.

**Rationale:** `register()` returns `EpochThread` which holds `Arc<EpochTable>`.
Storing the epoch as `Arc<EpochTable>` directly avoids a second Arc wrap at every
`register_thread()` call.

## 4. `cleanup_tentative_entries` Is O(n) Maintenance

**Decision:** Scan all buckets + overflow, CAS any tentative entry to EMPTY.
Not on the hot path. Called during recovery or periodic maintenance.

**Rationale:** Stale tentative entries from crashed inserts must be cleaned up.
In C++ FASTER, this is done during recovery. We expose it as a public method
for flexibility.
