//! Custom FASTER Functions for campaign event aggregation.
//!
//! # Why a Custom Functions Implementation?
//!
//! FASTER's built-in [`SimpleFunctions`] and [`CounterFunctions`] handle
//! single-value stores (full replacement or i64 counters). Our ad-click
//! aggregator needs *multi-field* read-modify-write: each event increments
//! one of three counters (clicks, impressions, spend) inside a single
//! [`CampaignStats`] struct. That requires a custom [`Functions`] impl.
//!
//! # Data Model
//!
//! | Type | Definition | Purpose |
//! |------|-----------|---------|
//! | **Key** | `u64` | Campaign ID |
//! | **Value** | [`CampaignStats`] | Running totals per campaign |
//! | **Input** | [`EventInput`] | What to add: event type + amount |
//! | **Output** | [`Option<CampaignStats>`] | Read result (None if not found) |
//! | **Context** | `()` | No async context needed |
//!
//! # RMW Three-Phase Protocol
//!
//! FASTER's Read-Modify-Write has three phases, each handled by a different
//! callback. Understanding *when* each fires is critical:
//!
//! 1. **`rmw_initial`** — Key doesn't exist yet. Create the first record.
//! 2. **`rmw_in_place`** — Key exists in the *mutable* region. Update directly.
//! 3. **`rmw_copy_update`** — Key exists in the *read-only* region. Copy it
//!    to a new mutable record, then apply the update.
//!
//! All three must produce identical *logical* results for the same input.
//! This is the contract that makes FASTER's hybrid log transparent.
//!
//! # Shared Between Thread and Tokio Versions
//!
//! This module is identical to the thread-based `event-counter` sample's
//! functions.rs. The Functions trait operates synchronously inside FASTER's
//! internal callbacks — there is no async here. The async/sync boundary
//! is handled at a higher level (see main.rs and workload.rs).

use faster_core::record::{FixedSizeValue, Value};
use faster_core::status::OperationStatus;
use faster_core::store::{DeleteInfo, ReadInfo, RmwInfo, UpsertInfo};
use faster_core::store::{Functions, RmwInPlaceResult};

// ── Event Types ─────────────────────────────────────────────────────────

/// The type of ad event being recorded.
///
/// Each variant maps to a different counter field in [`CampaignStats`].
/// This keeps the RMW logic clean: match on event type, increment the
/// corresponding field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventType {
    /// User clicked an ad. Increments `CampaignStats::clicks`.
    Click,
    /// Ad was displayed to a user. Increments `CampaignStats::impressions`.
    Impression,
    /// Money was spent on the campaign. Adds to `CampaignStats::spend_cents`.
    Spend,
}

/// Input passed to FASTER's RMW operation.
///
/// Carries the event type and an amount. For Click/Impression the amount
/// is typically 1; for Spend it's the cost in cents. The amount field
/// exists for flexibility — you can batch-insert by setting amount > 1.
#[derive(Debug, Clone)]
pub struct EventInput {
    pub event_type: EventType,
    pub amount: u64,
}

// ── Campaign Stats ──────────────────────────────────────────────────────

/// Running totals for a single ad campaign.
///
/// This is the *Value* type stored in FASTER's hybrid log. Each field is
/// a monotonically increasing counter (we never decrement in this sample).
///
/// # Why `Clone + Default`?
///
/// FASTER requires `Value: Clone + Default + Send + Sync + 'static`.
/// - `Default` is used to zero-initialize new records in `rmw_initial`.
/// - `Clone` is used when copying records from read-only to mutable region.
/// - `Send + Sync` because the store is shared across threads.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CampaignStats {
    pub clicks: u64,
    pub impressions: u64,
    pub spend_cents: u64,
}

impl CampaignStats {
    /// Apply an event to these stats, mutating in place.
    ///
    /// Extracted as a helper so all three RMW phases use the same logic.
    /// This eliminates the risk of inconsistency between initial / in_place
    /// / copy_update — one of the most common FASTER Functions bugs.
    fn apply(&mut self, input: &EventInput) {
        match input.event_type {
            EventType::Click => self.clicks += input.amount,
            EventType::Impression => self.impressions += input.amount,
            EventType::Spend => self.spend_cents += input.amount,
        }
    }

    /// Total events across all types (for summary reporting).
    #[allow(dead_code)]
    pub fn total_events(&self) -> u64 {
        self.clicks + self.impressions + self.spend_cents
    }

    /// Serialized size: three u64 fields = 24 bytes, always fixed.
    const SERIALIZED_SIZE: usize = 3 * std::mem::size_of::<u64>();
}

// ── Value Trait Implementation ──────────────────────────────────────────
//
// FASTER requires stored values to implement the `Value` trait, which
// defines how to serialize/deserialize to the hybrid log's page buffers.
// For our fixed-size struct (3 × u64 = 24 bytes), this is straightforward
// little-endian encoding — no length prefix needed.

impl Value for CampaignStats {
    fn serialized_size(&self) -> usize {
        Self::SERIALIZED_SIZE
    }

    fn serialize(&self, buf: &mut [u8]) -> usize {
        buf[0..8].copy_from_slice(&self.clicks.to_le_bytes());
        buf[8..16].copy_from_slice(&self.impressions.to_le_bytes());
        buf[16..24].copy_from_slice(&self.spend_cents.to_le_bytes());
        Self::SERIALIZED_SIZE
    }

    fn deserialize(buf: &[u8]) -> Self {
        Self {
            clicks: u64::from_le_bytes(buf[0..8].try_into().unwrap()),
            impressions: u64::from_le_bytes(buf[8..16].try_into().unwrap()),
            spend_cents: u64::from_le_bytes(buf[16..24].try_into().unwrap()),
        }
    }

    fn serialized_size_from_bytes(_buf: &[u8]) -> usize {
        Self::SERIALIZED_SIZE
    }
}

impl FixedSizeValue for CampaignStats {
    const SIZE: usize = CampaignStats::SERIALIZED_SIZE;
}

// ── CampaignFunctions ───────────────────────────────────────────────────

/// FASTER Functions implementation for the ad-click aggregator.
///
/// # KEY DIFFERENCE from SimpleFunctions:
///
/// `SimpleFunctions` does full-value replacement on RMW (it overwrites the
/// old value). `CampaignFunctions` does *additive* RMW — it reads the
/// existing counters and adds the input delta. This is the pattern that
/// makes FASTER shine for real-time aggregation.
///
/// # Thread Safety
///
/// The Functions trait requires `Send + Sync`, but the methods themselves
/// are called from within FASTER's internal lock-free record access path.
/// FASTER guarantees that only one thread accesses a given record at a time
/// during RMW (via epoch protection and record ownership), so the mutable
/// `&mut Value` references are safe despite the shared store.
pub struct CampaignFunctions;

impl Functions for CampaignFunctions {
    type Key = u64;
    type Value = CampaignStats;
    type Input = EventInput;
    type Output = Option<CampaignStats>;
    type Context = ();

    // ── Read ────────────────────────────────────────────────────────

    /// Read a campaign's stats.
    ///
    /// Called when `store.read()` finds the key. Clones the value into
    /// the output. The clone is necessary because the record lives in
    /// FASTER's managed memory and we need an owned copy.
    fn read(
        &self,
        _key: &u64,
        value: &CampaignStats,
        _input: &EventInput,
        output: &mut Option<CampaignStats>,
        _info: &ReadInfo,
    ) {
        *output = Some(value.clone());
    }

    // ── Upsert ──────────────────────────────────────────────────────

    /// Upsert (blind write) a campaign's stats.
    ///
    /// Used during bulk-loading or direct value replacement. In this sample,
    /// upsert is used sparingly — most writes go through RMW. But it's
    /// exercised during verification to confirm read-after-write consistency.
    fn upsert(
        &self,
        _key: &u64,
        value: &mut CampaignStats,
        input: &EventInput,
        _old_value: Option<&CampaignStats>,
        _output: &mut Option<CampaignStats>,
        _info: &UpsertInfo,
    ) {
        // Upsert treats the input as a full replacement: create stats
        // from just this one event. This matches the semantics of "set
        // the value" rather than "accumulate".
        let mut stats = CampaignStats::default();
        stats.apply(input);
        *value = stats;
    }

    // ── RMW Phase 1: Initial ────────────────────────────────────────

    /// Create the first record for a campaign that doesn't exist yet.
    ///
    /// # WHEN THIS FIRES:
    /// The key was not found in the hash index. FASTER allocates a new
    /// record and calls this to initialize it. The value starts as
    /// `Default::default()` (all zeros), then we apply the first event.
    ///
    /// # WHY NOT just set value = input?
    /// Because `EventInput` is a delta, not a full value. We start from
    /// zero and apply the delta, keeping the same code path as in_place.
    fn rmw_initial(
        &self,
        _key: &u64,
        input: &EventInput,
        value: &mut CampaignStats,
        _output: &mut Option<CampaignStats>,
        _info: &RmwInfo,
    ) {
        // Value is already Default (all zeros) — apply the first event.
        value.apply(input);
    }

    // ── RMW Phase 2: In-Place ───────────────────────────────────────

    /// Update an existing record in the mutable region.
    ///
    /// # WHEN THIS FIRES:
    /// The key exists and its record is in FASTER's mutable region (the
    /// hot tail of the hybrid log). We can modify it directly — no copy
    /// needed. This is the fast path and handles the vast majority of
    /// RMW operations for hot keys.
    ///
    /// # RETURN VALUE:
    /// `InPlaceOk` tells FASTER the update succeeded in place. If we
    /// needed more space (e.g., variable-length value growth), we'd
    /// return `NeedsNewRecord` to trigger a copy to a larger record.
    /// Our fixed-size `CampaignStats` never needs relocation.
    fn rmw_in_place(
        &self,
        _key: &u64,
        input: &EventInput,
        value: &mut CampaignStats,
        _output: &mut Option<CampaignStats>,
        _info: &RmwInfo,
    ) -> RmwInPlaceResult {
        value.apply(input);
        RmwInPlaceResult::InPlaceOk
    }

    // ── RMW Phase 3: Copy-Update ────────────────────────────────────

    /// Copy a read-only record to the mutable region and apply the update.
    ///
    /// # WHEN THIS FIRES:
    /// The key exists but its record has been flushed to the read-only
    /// region (or to disk). FASTER copies the old value to a new mutable
    /// record, then calls this to apply the pending update.
    ///
    /// # KEY INSIGHT:
    /// This is where FASTER's hybrid log shines. Cold records are
    /// automatically promoted back to the hot region when they're
    /// modified. The old record becomes a tombstone in the read-only log.
    fn rmw_copy_update(
        &self,
        _key: &u64,
        input: &EventInput,
        old_value: &CampaignStats,
        new_value: &mut CampaignStats,
        _output: &mut Option<CampaignStats>,
        _info: &RmwInfo,
    ) {
        // Start with the old value's counters, then apply the new event.
        *new_value = old_value.clone();
        new_value.apply(input);
    }

    // ── Delete ──────────────────────────────────────────────────────

    /// Called when a record is deleted.
    ///
    /// FASTER writes a tombstone record. We don't need to do any cleanup
    /// for our value type, so this is a no-op (default impl would suffice,
    /// but we include it explicitly for documentation completeness).
    fn delete(&self, _key: &u64, _value: &mut CampaignStats, _info: &DeleteInfo) {
        // No cleanup needed for CampaignStats — it's plain data.
    }

    // ── Completion Callbacks ────────────────────────────────────────

    /// Called when a pending Read completes (I/O returned from disk).
    ///
    /// In this sample, we don't use the completion callback because we
    /// call `complete_pending()` explicitly after operations. The output
    /// is already populated by `read()` above.
    fn read_completion(
        &self,
        _key: &u64,
        _output: &Option<CampaignStats>,
        _context: &(),
        _status: OperationStatus,
    ) {
        // No-op: we collect results via complete_pending().
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use faster_core::address::LogicalAddress;
    use faster_core::record::RecordInfo;

    fn dummy_read_info() -> ReadInfo {
        ReadInfo::new(0, LogicalAddress::INVALID, RecordInfo::default())
    }
    fn dummy_rmw_info() -> RmwInfo {
        RmwInfo::new(0, LogicalAddress::INVALID, RecordInfo::default(), false)
    }

    #[test]
    fn rmw_initial_creates_correct_stats() {
        let f = CampaignFunctions;
        let mut value = CampaignStats::default();
        let input = EventInput {
            event_type: EventType::Click,
            amount: 5,
        };
        f.rmw_initial(&1, &input, &mut value, &mut None, &dummy_rmw_info());
        assert_eq!(value.clicks, 5);
        assert_eq!(value.impressions, 0);
        assert_eq!(value.spend_cents, 0);
    }

    #[test]
    fn rmw_in_place_accumulates() {
        let f = CampaignFunctions;
        let mut value = CampaignStats {
            clicks: 10,
            impressions: 20,
            spend_cents: 100,
        };
        let input = EventInput {
            event_type: EventType::Impression,
            amount: 3,
        };
        let result = f.rmw_in_place(&1, &input, &mut value, &mut None, &dummy_rmw_info());
        assert_eq!(result, RmwInPlaceResult::InPlaceOk);
        assert_eq!(value.clicks, 10);
        assert_eq!(value.impressions, 23);
        assert_eq!(value.spend_cents, 100);
    }

    #[test]
    fn rmw_copy_update_preserves_old_and_applies() {
        let f = CampaignFunctions;
        let old = CampaignStats {
            clicks: 5,
            impressions: 10,
            spend_cents: 50,
        };
        let mut new_val = CampaignStats::default();
        let input = EventInput {
            event_type: EventType::Spend,
            amount: 25,
        };
        f.rmw_copy_update(&1, &input, &old, &mut new_val, &mut None, &dummy_rmw_info());
        assert_eq!(new_val.clicks, 5);
        assert_eq!(new_val.impressions, 10);
        assert_eq!(new_val.spend_cents, 75);
    }

    #[test]
    fn read_clones_value() {
        let f = CampaignFunctions;
        let value = CampaignStats {
            clicks: 1,
            impressions: 2,
            spend_cents: 3,
        };
        let input = EventInput {
            event_type: EventType::Click,
            amount: 0,
        };
        let mut output = None;
        f.read(&42, &value, &input, &mut output, &dummy_read_info());
        assert_eq!(output, Some(value));
    }

    #[test]
    fn all_event_types_apply_correctly() {
        let mut stats = CampaignStats::default();
        stats.apply(&EventInput {
            event_type: EventType::Click,
            amount: 1,
        });
        stats.apply(&EventInput {
            event_type: EventType::Impression,
            amount: 2,
        });
        stats.apply(&EventInput {
            event_type: EventType::Spend,
            amount: 300,
        });
        assert_eq!(stats.clicks, 1);
        assert_eq!(stats.impressions, 2);
        assert_eq!(stats.spend_cents, 300);
        assert_eq!(stats.total_events(), 303);
    }
}
