# Decision: Key::hash_from_bytes trait method

**Agent:** Aragorn (Rust Expert)
**Date:** 2026-03-07
**Status:** Implemented
**Impact:** Key trait API, compaction performance

## Decision

Added `fn hash_from_bytes(buf: &[u8]) -> KeyHash` to the `Key` trait with a default implementation that deserializes and hashes. Optimized overrides provided for all built-in types.

## Rationale

During compaction, `AddressUpdater::swing_one()` and `remove_tombstone()` were deserializing keys just to compute their hash for hash-index lookups. For variable-length types (Vec<u8>, String), this allocates on the heap unnecessarily. The new method enables zero-copy hash computation directly from serialized bytes.

## API Pattern

Follows the established `*_from_bytes` pattern on the Key trait:
- `serialized_size_from_bytes` — size without deserialization
- `eq_from_bytes` — comparison without deserialization
- `hash_from_bytes` — hashing without deserialization (NEW)

All three have default implementations that deserialize (correct but slow), with optimized overrides for built-in types.

## Implications

- **Custom Key implementors:** No action needed — default impl provides correctness. Override for performance if the type is used with compaction.
- **Compaction code:** Now uses `K::hash_from_bytes()` instead of `K::deserialize().hash()` in pointer swing and tombstone removal.
- **Performance:** Zero-copy for variable-length types eliminates heap allocation per record during compaction scan.
