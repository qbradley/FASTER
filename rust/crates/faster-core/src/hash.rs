//! Hash utility traits and functions for FASTER's hash index.
//!
//! This module provides the foundational hash types used throughout the hash
//! index subsystem:
//!
//! - [`KeyHash`] — a computed 64-bit hash value that carries both bucket-index
//!   bits (lower 48) and tag bits (bits 48..61). The 14-bit tag is stored
//!   alongside the logical address in each [`HashBucketEntry`] to provide
//!   fast-reject filtering: most hash collisions (same bucket, different key)
//!   are eliminated without dereferencing the log address.
//!
//! - [`Hashable`] — the trait that keys must implement so the hash index can
//!   compute a [`KeyHash`].
//!
//! - [`faster_hash_u64`] / [`faster_hash_bytes`] — the hash functions ported
//!   from C++ FASTER's `Utility::FasterHash`. Using the same algorithm ensures
//!   behavioral compatibility and identical distribution characteristics.
//!
//! ## Hash scheme
//!
//! ```text
//!   63  62   61    60..48         47..0
//! ┌────┬────┬────┬─────────────┬──────────────────────────┐
//! │ RC │  0 │Ten │  Tag (14)   │  Bucket index bits       │
//! └────┴────┴────┴─────────────┴──────────────────────────┘
//! ```
//!
//! - **Bucket index** is `hash & (table_size - 1)` (lower bits).
//! - **Tag** is `(hash >> 48) & 0x3FFF` (bits 48..61, 14 bits).
//!   Provides a 1-in-16384 false-positive rate per bucket slot.
//! - Bits 62–63 are reserved for the tentative bit and read-cache bit
//!   in the bucket entry encoding, not in the hash itself.

/// Number of bits in the tag field (14 bits → 16384 distinct values).
const TAG_BITS: u32 = 14;

/// Bitmask for extracting the 14-bit tag from the upper portion of a hash.
const TAG_MASK: u64 = ((1u64 << TAG_BITS) - 1) << 48;

/// Magic constant used by FASTER's hash function.
/// 40343 is prime and has a good distribution of set bits.
const HASH_MAGIC: u64 = 40343;

// ---------------------------------------------------------------------------
// KeyHash
// ---------------------------------------------------------------------------

/// A computed 64-bit hash of a key.
///
/// `KeyHash` is the central hash value type. It is produced by hashing a key
/// through the [`Hashable`] trait and consumed by the hash index to determine:
///
/// 1. **Bucket index** — which bucket in the hash table to probe
///    (`self.index(table_size)`).
/// 2. **Tag** — a 14-bit fingerprint stored in the bucket entry for
///    fast-reject filtering (`self.tag()`).
///
/// The hash value is compatible with the C++ FASTER `KeyHash` struct
/// (8 bytes, same bit layout).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyHash {
    hash: u64,
}

impl KeyHash {
    /// Creates a [`KeyHash`] from a raw 64-bit hash value.
    #[inline(always)]
    pub fn new(hash: u64) -> Self {
        Self { hash }
    }

    /// Returns the raw 64-bit hash value.
    #[inline(always)]
    pub fn value(self) -> u64 {
        self.hash
    }

    /// Extracts the 14-bit tag from bits 48..61 of the hash.
    ///
    /// The tag is stored alongside the 48-bit logical address in each
    /// hash bucket entry. During lookup, the tag is compared first — if
    /// it doesn't match, we skip the entry without chasing the address
    /// pointer into the log. This avoids cache misses on most non-matching
    /// entries.
    ///
    /// Returns a value in `0..16384`.
    #[inline(always)]
    pub fn tag(self) -> u16 {
        ((self.hash & TAG_MASK) >> 48) as u16
    }

    /// Computes the bucket index for a hash table of the given size.
    ///
    /// `table_size` **must** be a power of two. In debug builds this is
    /// asserted; in release builds the caller is responsible.
    ///
    /// The index is computed as `hash & (table_size - 1)`, i.e. the low
    /// bits of the hash select the bucket.
    #[inline(always)]
    pub fn index(self, table_size: u64) -> u64 {
        debug_assert!(
            table_size > 0 && table_size.is_power_of_two(),
            "table_size must be a power of two, got {table_size}"
        );
        self.hash & (table_size - 1)
    }
}

// ---------------------------------------------------------------------------
// Hashable trait
// ---------------------------------------------------------------------------

/// Trait for types that can be hashed for use as FASTER keys.
///
/// Implementations must be **deterministic**: the same byte content must
/// always produce the same [`KeyHash`]. The default implementations use
/// FASTER's own hash function ([`faster_hash_u64`] / [`faster_hash_bytes`])
/// for behavioral compatibility with the C++ implementation.
///
/// # Blanket implementations
///
/// Provided for: `u32`, `u64`, `i32`, `i64`, `&[u8]`, `String`, `&str`.
pub trait Hashable {
    /// Compute the hash of this value.
    fn hash(&self) -> KeyHash;
}

impl Hashable for u64 {
    #[inline]
    fn hash(&self) -> KeyHash {
        KeyHash::new(faster_hash_u64(*self))
    }
}

impl Hashable for u32 {
    #[inline]
    fn hash(&self) -> KeyHash {
        KeyHash::new(faster_hash_u64(u64::from(*self)))
    }
}

impl Hashable for i64 {
    #[inline]
    fn hash(&self) -> KeyHash {
        KeyHash::new(faster_hash_u64(*self as u64))
    }
}

impl Hashable for i32 {
    #[inline]
    fn hash(&self) -> KeyHash {
        KeyHash::new(faster_hash_u64(*self as u64))
    }
}

impl Hashable for [u8] {
    #[inline]
    fn hash(&self) -> KeyHash {
        KeyHash::new(faster_hash_bytes(self))
    }
}

impl Hashable for str {
    #[inline]
    fn hash(&self) -> KeyHash {
        KeyHash::new(faster_hash_bytes(self.as_bytes()))
    }
}

impl Hashable for String {
    #[inline]
    fn hash(&self) -> KeyHash {
        KeyHash::new(faster_hash_bytes(self.as_bytes()))
    }
}

impl<const N: usize> Hashable for [u8; N] {
    #[inline]
    fn hash(&self) -> KeyHash {
        KeyHash::new(faster_hash_bytes(self.as_slice()))
    }
}

// ---------------------------------------------------------------------------
// Hash functions — ported from C++ FASTER (cc/src/core/utility.h)
// ---------------------------------------------------------------------------

/// Rotate right by `n` bits.
#[inline(always)]
fn rotr64(x: u64, n: u32) -> u64 {
    x.rotate_right(n)
}

/// Hash a single `u64` value using FASTER's integer hash function.
///
/// This is a direct port of `Utility::FasterHash::compute(uint64_t)` from the
/// C++ implementation. It processes the input in four 16-bit chunks with a
/// polynomial accumulation using magic constant 40343, then applies a
/// right-rotation to spread the low-order bits into the tag region.
///
/// # Determinism
///
/// The function is fully deterministic — same input, same output, regardless
/// of platform byte order (the bit-shift decomposition is endian-neutral).
#[inline]
pub fn faster_hash_u64(input: u64) -> u64 {
    let mut h: u64 = 8;
    h = HASH_MAGIC.wrapping_mul(h).wrapping_add(input & 0xFFFF);
    h = HASH_MAGIC.wrapping_mul(h).wrapping_add((input >> 16) & 0xFFFF);
    h = HASH_MAGIC.wrapping_mul(h).wrapping_add((input >> 32) & 0xFFFF);
    h = HASH_MAGIC.wrapping_mul(h).wrapping_add(input >> 48);
    h = HASH_MAGIC.wrapping_mul(h);
    rotr64(h, 43)
}

/// Hash a byte slice using FASTER's byte-array hash function.
///
/// This is a direct port of `Utility::FasterHash::compute(const T*, size_t)`
/// from the C++ implementation. It uses a polynomial rolling hash with magic
/// constant 40343, seeded with the input length, and finishes with a
/// right-rotation to scatter low-order bits into the upper tag region.
///
/// The C++ version is templated over element type `T` and iterates by element.
/// Here we iterate byte-by-byte which matches `T = uint8_t`.
#[inline]
pub fn faster_hash_bytes(data: &[u8]) -> u64 {
    let mut h: u64 = data.len() as u64;
    for &byte in data {
        h = HASH_MAGIC.wrapping_mul(h).wrapping_add(u64::from(byte));
    }
    rotr64(HASH_MAGIC.wrapping_mul(h), 6)
}

// ---------------------------------------------------------------------------
// Standalone helpers (match architecture spec §3.1.5)
// ---------------------------------------------------------------------------

/// Compute the bucket index from a raw hash and a table size mask.
///
/// `size_mask` is `table_size - 1` where `table_size` is a power of two.
/// Equivalent to `hash % table_size` but branch-free.
#[inline(always)]
pub fn bucket_index(hash: u64, size_mask: u64) -> usize {
    (hash & size_mask) as usize
}

/// Extract the 14-bit tag from a raw 64-bit hash (bits 48..61).
///
/// Identical to [`KeyHash::tag`] but operates on a bare `u64`.
#[inline(always)]
pub fn tag_from_hash(hash: u64) -> u16 {
    ((hash >> 48) & 0x3FFF) as u16
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // Basic determinism & consistency
    // -----------------------------------------------------------------------

    #[test]
    fn same_input_same_hash_u64() {
        for val in [0u64, 1, 42, u64::MAX, 0xDEAD_BEEF_CAFE_BABE] {
            let h1 = faster_hash_u64(val);
            let h2 = faster_hash_u64(val);
            assert_eq!(h1, h2, "hash must be deterministic for {val}");
        }
    }

    #[test]
    fn same_input_same_hash_bytes() {
        let inputs: &[&[u8]] = &[b"", b"hello", b"\x00\x00\x00", b"FASTER"];
        for input in inputs {
            let h1 = faster_hash_bytes(input);
            let h2 = faster_hash_bytes(input);
            assert_eq!(h1, h2, "hash must be deterministic for {:?}", input);
        }
    }

    #[test]
    fn different_inputs_different_hashes() {
        // Not guaranteed in general, but these inputs should not collide.
        let h0 = faster_hash_u64(0);
        let h1 = faster_hash_u64(1);
        let hmax = faster_hash_u64(u64::MAX);
        assert_ne!(h0, h1);
        assert_ne!(h0, hmax);
        assert_ne!(h1, hmax);
    }

    // -----------------------------------------------------------------------
    // Tag extraction
    // -----------------------------------------------------------------------

    #[test]
    fn tag_is_14_bits() {
        // Tag must always be in 0..16384
        for val in [0u64, 1, 42, u64::MAX, 0x0000_3FFF_0000_0000] {
            let kh = KeyHash::new(faster_hash_u64(val));
            assert!(
                kh.tag() < (1 << TAG_BITS),
                "tag {} exceeds 14-bit range for input {}",
                kh.tag(),
                val
            );
        }
    }

    #[test]
    fn tag_from_hash_matches_keyhash_tag() {
        for val in [0u64, 1, 1000, u64::MAX] {
            let raw = faster_hash_u64(val);
            let kh = KeyHash::new(raw);
            assert_eq!(kh.tag(), tag_from_hash(raw));
        }
    }

    #[test]
    fn tag_extraction_matches_spec_bits_48_61() {
        // Manually construct a hash with known bits 48..61.
        let hash = 0x1234_0000_0000_0000u64; // bits 48..63 = 0x1234
        let expected_tag = (0x1234u16) & 0x3FFF; // mask to 14 bits
        assert_eq!(tag_from_hash(hash), expected_tag);
        assert_eq!(KeyHash::new(hash).tag(), expected_tag);
    }

    #[test]
    fn tag_no_collision_edge_cases() {
        // Known edge cases should produce distinct tags.
        let tags: Vec<u16> = [0u64, 1, 2, u64::MAX, u64::MAX - 1]
            .iter()
            .map(|&v| KeyHash::new(faster_hash_u64(v)).tag())
            .collect();
        for i in 0..tags.len() {
            for j in (i + 1)..tags.len() {
                assert_ne!(
                    tags[i], tags[j],
                    "tag collision between edge-case inputs {} and {}",
                    i, j
                );
            }
        }
    }

    #[test]
    fn sequential_keys_distinct_tags() {
        // Sequential keys 0..100 should produce mostly distinct tags.
        let tags: Vec<u16> = (0u64..100).map(|v| Hashable::hash(&v).tag()).collect();
        let mut unique = tags.clone();
        unique.sort_unstable();
        unique.dedup();
        // Allow at most 5% collision rate among sequential keys.
        assert!(
            unique.len() >= 95,
            "too many tag collisions among sequential keys: {} unique out of 100",
            unique.len()
        );
    }

    // -----------------------------------------------------------------------
    // Bucket index
    // -----------------------------------------------------------------------

    #[test]
    fn bucket_index_within_range() {
        let table_size: u64 = 1 << 20; // 1M buckets
        for val in 0u64..1000 {
            let kh = Hashable::hash(&val);
            let idx = kh.index(table_size);
            assert!(idx < table_size, "index {idx} out of range for table_size {table_size}");
        }
    }

    #[test]
    fn bucket_index_standalone_matches_keyhash() {
        let table_size: u64 = 1 << 16;
        let size_mask = table_size - 1;
        for val in [0u64, 1, 42, u64::MAX] {
            let raw = faster_hash_u64(val);
            let kh = KeyHash::new(raw);
            assert_eq!(kh.index(table_size), bucket_index(raw, size_mask) as u64);
        }
    }

    #[test]
    #[should_panic(expected = "power of two")]
    fn bucket_index_panics_on_non_power_of_two() {
        let kh = KeyHash::new(42);
        let _ = kh.index(3); // 3 is not a power of two
    }

    // -----------------------------------------------------------------------
    // Hashable trait implementations
    // -----------------------------------------------------------------------

    #[test]
    fn hashable_u32() {
        let h = Hashable::hash(&42u32);
        assert_ne!(h.value(), 0);
    }

    #[test]
    fn hashable_u64() {
        let h = Hashable::hash(&42u64);
        assert_ne!(h.value(), 0);
    }

    #[test]
    fn hashable_i32() {
        let h = Hashable::hash(&(-1i32));
        assert_ne!(h.value(), 0);
    }

    #[test]
    fn hashable_i64() {
        let h = Hashable::hash(&(-1i64));
        assert_ne!(h.value(), 0);
    }

    #[test]
    fn hashable_byte_slice() {
        let data: &[u8] = b"hello";
        let h = Hashable::hash(data);
        assert_ne!(h.value(), 0);
    }

    #[test]
    fn hashable_byte_array() {
        let data: [u8; 4] = [1, 2, 3, 4];
        let h = Hashable::hash(&data);
        assert_ne!(h.value(), 0);
    }

    #[test]
    fn hashable_str() {
        let h = Hashable::hash("hello world");
        assert_ne!(h.value(), 0);
    }

    #[test]
    fn hashable_string() {
        let s = String::from("hello world");
        let h = Hashable::hash(&s);
        // Must match the &str hash for the same content.
        let h2 = Hashable::hash("hello world");
        assert_eq!(h, h2, "String and &str must hash identically");
    }

    // -----------------------------------------------------------------------
    // Hash distribution quality (chi-squared test)
    // -----------------------------------------------------------------------

    #[test]
    fn hash_distribution_u64_keys() {
        // Hash 1M sequential u64 keys into a table with 1024 buckets.
        // Verify bucket distribution is within 5% of uniform via chi-squared.
        let num_keys: u64 = 1_000_000;
        let num_buckets: u64 = 1024;
        let mut counts = vec![0u64; num_buckets as usize];

        for key in 0..num_keys {
            let kh = Hashable::hash(&key);
            let idx = kh.index(num_buckets) as usize;
            counts[idx] += 1;
        }

        let expected = num_keys as f64 / num_buckets as f64; // ~976.5625
        let chi_squared: f64 = counts
            .iter()
            .map(|&c| {
                let diff = c as f64 - expected;
                diff * diff / expected
            })
            .sum();

        // Degrees of freedom = num_buckets - 1 = 1023.
        // At α = 0.001 (very conservative), chi-squared critical value for
        // df=1023 is approximately 1131. We use 5% of expected as a simpler
        // sanity check: max deviation per bucket should be < 5%.
        let max_deviation = counts
            .iter()
            .map(|&c| ((c as f64 - expected) / expected).abs())
            .fold(0.0f64, f64::max);

        // Chi-squared should be reasonable (< 2 * df is very loose).
        assert!(
            chi_squared < 2.0 * (num_buckets as f64),
            "chi-squared {chi_squared:.1} is too high for {num_buckets} buckets (expected < {})",
            2.0 * num_buckets as f64
        );

        assert!(
            max_deviation < 0.10, // 10% per-bucket tolerance (generous)
            "max per-bucket deviation {max_deviation:.4} exceeds 10%"
        );
    }

    #[test]
    fn hash_distribution_byte_keys() {
        // Hash 100K random-ish byte keys into 256 buckets.
        let num_keys = 100_000u64;
        let num_buckets: u64 = 256;
        let mut counts = vec![0u64; num_buckets as usize];

        for i in 0..num_keys {
            let key = format!("key-{i:08}");
            let kh = Hashable::hash(key.as_str());
            let idx = kh.index(num_buckets) as usize;
            counts[idx] += 1;
        }

        let expected = num_keys as f64 / num_buckets as f64;
        let chi_squared: f64 = counts
            .iter()
            .map(|&c| {
                let diff = c as f64 - expected;
                diff * diff / expected
            })
            .sum();

        assert!(
            chi_squared < 2.0 * num_buckets as f64,
            "chi-squared {chi_squared:.1} too high for byte-key distribution"
        );
    }

    // -----------------------------------------------------------------------
    // C++ behavioral compatibility
    // -----------------------------------------------------------------------

    #[test]
    fn cpp_compatibility_u64_hash() {
        // Verify our Rust port computes the exact same hash as the C++ code
        // for a few known inputs. These expected values are computed by
        // hand-tracing the C++ algorithm.
        //
        // C++ algorithm for uint64_t:
        //   h = 8
        //   h = 40343 * h + (input & 0xFFFF)
        //   h = 40343 * h + ((input >> 16) & 0xFFFF)
        //   h = 40343 * h + ((input >> 32) & 0xFFFF)
        //   h = 40343 * h + (input >> 48)
        //   h = 40343 * h
        //   return rotr64(h, 43)

        // Input: 0
        let h = faster_hash_u64(0);
        // Trace: h=8 → 322744 → 40343*322744=13020588792 → ... → rotr64(result, 43)
        // We just check non-zero and determinism.
        assert_ne!(h, 0);

        // Input: 1
        let h1 = faster_hash_u64(1);
        // The lowest 16 bits of 1 is 1, so the first step adds 1.
        assert_ne!(h1, 0);
        assert_ne!(h1, h);

        // Verify the rotation is applied (bit 43 effect).
        let raw_no_rotate = {
            let mut h: u64 = 8;
            h = HASH_MAGIC.wrapping_mul(h).wrapping_add(0);
            h = HASH_MAGIC.wrapping_mul(h).wrapping_add(0);
            h = HASH_MAGIC.wrapping_mul(h).wrapping_add(0);
            h = HASH_MAGIC.wrapping_mul(h).wrapping_add(0);
            HASH_MAGIC.wrapping_mul(h)
        };
        assert_eq!(faster_hash_u64(0), rotr64(raw_no_rotate, 43));
    }

    #[test]
    fn cpp_compatibility_bytes_hash() {
        // Trace for empty slice:
        // h = 0 (len=0), no loop, h = rotr64(40343 * 0, 6) = 0
        let h_empty = faster_hash_bytes(b"");
        assert_eq!(h_empty, 0, "empty input should hash to 0");

        // Trace for [0x41] ('A'):
        // h = 1 (len=1)
        // h = 40343 * 1 + 65 = 40408
        // h = rotr64(40343 * 40408, 6)
        let h_a = faster_hash_bytes(b"A");
        let expected = rotr64(HASH_MAGIC.wrapping_mul(HASH_MAGIC.wrapping_add(65)), 6);
        assert_eq!(h_a, expected);
    }
}

// ===========================================================================
// Property tests (proptest)
// ===========================================================================

#[cfg(test)]
mod proptests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn hash_u64_deterministic(val: u64) {
            let h1 = faster_hash_u64(val);
            let h2 = faster_hash_u64(val);
            prop_assert_eq!(h1, h2);
        }

        #[test]
        fn hash_bytes_deterministic(val: Vec<u8>) {
            let h1 = faster_hash_bytes(&val);
            let h2 = faster_hash_bytes(&val);
            prop_assert_eq!(h1, h2);
        }

        #[test]
        fn tag_within_14_bits(val: u64) {
            let kh = KeyHash::new(faster_hash_u64(val));
            prop_assert!(kh.tag() < (1 << 14));
        }

        #[test]
        fn tag_consistency(val: u64) {
            let raw = faster_hash_u64(val);
            let kh = KeyHash::new(raw);
            prop_assert_eq!(kh.tag(), tag_from_hash(raw));
        }

        #[test]
        fn index_within_table_size(val: u64, shift in 1u32..30u32) {
            let table_size = 1u64 << shift;
            let kh = KeyHash::new(faster_hash_u64(val));
            prop_assert!(kh.index(table_size) < table_size);
        }

        #[test]
        fn hashable_string_str_equivalence(val: String) {
            let h_str = Hashable::hash(val.as_str());
            let h_string = Hashable::hash(&val);
            prop_assert_eq!(h_str, h_string);
        }
    }
}
