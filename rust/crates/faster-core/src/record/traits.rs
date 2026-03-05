//! Key and Value serialization traits.
//!
//! The [`Key`] and [`Value`] traits define what types can be stored in FASTER.
//! They provide a unified interface for both fixed-size types (`u32`, `u64`,
//! etc.) and variable-length types (`Vec<u8>`, `String`).
//!
//! # Fixed vs. Variable Length
//!
//! Types with a compile-time known size implement [`FixedSizeKey`] or
//! [`FixedSizeValue`]. The hybrid log uses this to pre-compute record sizes
//! without inspecting individual values.
//!
//! Variable-length types (`Vec<u8>`, `String`) use length-prefixed
//! serialization: a 4-byte little-endian `u32` length followed by the data
//! bytes.

use crate::hash::Hashable;

/// Trait for types that can be used as FASTER keys.
///
/// Keys must be hashable (for bucket assignment), comparable (for chain
/// traversal), cloneable (for pending operation contexts), and serializable
/// (for on-disk storage).
///
/// # Examples
///
/// ```
/// use faster_core::record::Key;
///
/// // u64 implements Key automatically
/// let key: u64 = 42;
/// assert_eq!(key.serialized_size(), 8);
///
/// let mut buf = vec![0u8; 8];
/// let written = key.serialize(&mut buf);
/// assert_eq!(written, 8);
/// let restored = u64::deserialize(&buf);
/// assert_eq!(restored, 42);
/// ```
pub trait Key: Hashable + Eq + Clone + Send + Sync + 'static {
    /// Returns the exact number of bytes needed to serialize this key.
    fn serialized_size(&self) -> usize;

    /// Serializes this key into `buf`.
    ///
    /// The buffer must be at least `self.serialized_size()` bytes.
    /// Returns the number of bytes written.
    fn serialize(&self, buf: &mut [u8]) -> usize;

    /// Deserializes a key from the start of `buf`.
    fn deserialize(buf: &[u8]) -> Self;
}

/// Trait for types that can be used as FASTER values.
///
/// Values must be cloneable and serializable. Unlike keys, values do not
/// need to be hashable or comparable.
///
/// # Examples
///
/// ```
/// use faster_core::record::Value;
///
/// let value: u64 = 999;
/// assert_eq!(value.serialized_size(), 8);
///
/// let mut buf = vec![0u8; 8];
/// let written = value.serialize(&mut buf);
/// assert_eq!(written, 8);
/// let restored = u64::deserialize(&buf);
/// assert_eq!(restored, 999);
/// ```
pub trait Value: Clone + Send + Sync + 'static {
    /// Returns the exact number of bytes needed to serialize this value.
    fn serialized_size(&self) -> usize;

    /// Serializes this value into `buf`.
    ///
    /// The buffer must be at least `self.serialized_size()` bytes.
    /// Returns the number of bytes written.
    fn serialize(&self, buf: &mut [u8]) -> usize;

    /// Deserializes a value from the start of `buf`.
    fn deserialize(buf: &[u8]) -> Self;
}

/// Marker trait for keys with a compile-time known serialized size.
///
/// The hybrid log uses this to pre-compute record sizes without inspecting
/// individual key instances, enabling faster allocation.
pub trait FixedSizeKey: Key {
    /// The constant serialized size in bytes.
    const SIZE: usize;
}

/// Marker trait for values with a compile-time known serialized size.
///
/// The hybrid log uses this to pre-compute record sizes without inspecting
/// individual value instances.
pub trait FixedSizeValue: Value {
    /// The constant serialized size in bytes.
    const SIZE: usize;
}

// ── Blanket implementations for numeric types ────────────────────────

macro_rules! impl_key_value_for_numeric {
    ($($t:ty),*) => {$(
        impl Key for $t {
            #[inline]
            fn serialized_size(&self) -> usize {
                core::mem::size_of::<$t>()
            }

            #[inline]
            fn serialize(&self, buf: &mut [u8]) -> usize {
                let bytes = self.to_le_bytes();
                buf[..bytes.len()].copy_from_slice(&bytes);
                bytes.len()
            }

            #[inline]
            fn deserialize(buf: &[u8]) -> Self {
                Self::from_le_bytes(
                    buf[..core::mem::size_of::<$t>()]
                        .try_into()
                        .expect("buffer too short for numeric Key::deserialize"),
                )
            }
        }

        impl FixedSizeKey for $t {
            const SIZE: usize = core::mem::size_of::<$t>();
        }

        impl Value for $t {
            #[inline]
            fn serialized_size(&self) -> usize {
                core::mem::size_of::<$t>()
            }

            #[inline]
            fn serialize(&self, buf: &mut [u8]) -> usize {
                let bytes = self.to_le_bytes();
                buf[..bytes.len()].copy_from_slice(&bytes);
                bytes.len()
            }

            #[inline]
            fn deserialize(buf: &[u8]) -> Self {
                Self::from_le_bytes(
                    buf[..core::mem::size_of::<$t>()]
                        .try_into()
                        .expect("buffer too short for numeric Value::deserialize"),
                )
            }
        }

        impl FixedSizeValue for $t {
            const SIZE: usize = core::mem::size_of::<$t>();
        }
    )*};
}

// Types that have Hashable impls in hash.rs: u32, u64, i32, i64
impl_key_value_for_numeric!(u32, u64, i32, i64);

// ── Vec<u8> — variable-length, length-prefixed ──────────────────────

/// Length prefix size for variable-length types (4 bytes = u32).
const LENGTH_PREFIX_SIZE: usize = 4;

impl Key for Vec<u8> {
    #[inline]
    fn serialized_size(&self) -> usize {
        LENGTH_PREFIX_SIZE + self.len()
    }

    fn serialize(&self, buf: &mut [u8]) -> usize {
        let len = self.len() as u32;
        buf[..LENGTH_PREFIX_SIZE].copy_from_slice(&len.to_le_bytes());
        buf[LENGTH_PREFIX_SIZE..LENGTH_PREFIX_SIZE + self.len()].copy_from_slice(self);
        LENGTH_PREFIX_SIZE + self.len()
    }

    fn deserialize(buf: &[u8]) -> Self {
        let len = u32::from_le_bytes(
            buf[..LENGTH_PREFIX_SIZE]
                .try_into()
                .expect("buffer too short for Vec<u8> length prefix"),
        ) as usize;
        buf[LENGTH_PREFIX_SIZE..LENGTH_PREFIX_SIZE + len].to_vec()
    }
}

impl Value for Vec<u8> {
    #[inline]
    fn serialized_size(&self) -> usize {
        LENGTH_PREFIX_SIZE + self.len()
    }

    fn serialize(&self, buf: &mut [u8]) -> usize {
        let len = self.len() as u32;
        buf[..LENGTH_PREFIX_SIZE].copy_from_slice(&len.to_le_bytes());
        buf[LENGTH_PREFIX_SIZE..LENGTH_PREFIX_SIZE + self.len()].copy_from_slice(self);
        LENGTH_PREFIX_SIZE + self.len()
    }

    fn deserialize(buf: &[u8]) -> Self {
        let len = u32::from_le_bytes(
            buf[..LENGTH_PREFIX_SIZE]
                .try_into()
                .expect("buffer too short for Vec<u8> length prefix"),
        ) as usize;
        buf[LENGTH_PREFIX_SIZE..LENGTH_PREFIX_SIZE + len].to_vec()
    }
}

// ── String — variable-length, UTF-8, length-prefixed ────────────────

impl Key for String {
    #[inline]
    fn serialized_size(&self) -> usize {
        LENGTH_PREFIX_SIZE + self.len()
    }

    fn serialize(&self, buf: &mut [u8]) -> usize {
        let len = self.len() as u32;
        buf[..LENGTH_PREFIX_SIZE].copy_from_slice(&len.to_le_bytes());
        buf[LENGTH_PREFIX_SIZE..LENGTH_PREFIX_SIZE + self.len()].copy_from_slice(self.as_bytes());
        LENGTH_PREFIX_SIZE + self.len()
    }

    fn deserialize(buf: &[u8]) -> Self {
        let len = u32::from_le_bytes(
            buf[..LENGTH_PREFIX_SIZE]
                .try_into()
                .expect("buffer too short for String length prefix"),
        ) as usize;
        String::from_utf8(buf[LENGTH_PREFIX_SIZE..LENGTH_PREFIX_SIZE + len].to_vec())
            .expect("invalid UTF-8 in deserialized String")
    }
}

impl Value for String {
    #[inline]
    fn serialized_size(&self) -> usize {
        LENGTH_PREFIX_SIZE + self.len()
    }

    fn serialize(&self, buf: &mut [u8]) -> usize {
        let len = self.len() as u32;
        buf[..LENGTH_PREFIX_SIZE].copy_from_slice(&len.to_le_bytes());
        buf[LENGTH_PREFIX_SIZE..LENGTH_PREFIX_SIZE + self.len()].copy_from_slice(self.as_bytes());
        LENGTH_PREFIX_SIZE + self.len()
    }

    fn deserialize(buf: &[u8]) -> Self {
        let len = u32::from_le_bytes(
            buf[..LENGTH_PREFIX_SIZE]
                .try_into()
                .expect("buffer too short for String length prefix"),
        ) as usize;
        String::from_utf8(buf[LENGTH_PREFIX_SIZE..LENGTH_PREFIX_SIZE + len].to_vec())
            .expect("invalid UTF-8 in deserialized String")
    }
}

// ── Tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Numeric key/value round-trips ───────────────────────────────

    #[test]
    fn u32_key_round_trip() {
        let key: u32 = 0xDEAD_BEEF;
        assert_eq!(Key::serialized_size(&key), 4);
        let mut buf = vec![0u8; 4];
        Key::serialize(&key, &mut buf);
        let restored = <u32 as Key>::deserialize(&buf);
        assert_eq!(restored, key);
    }

    #[test]
    fn u64_key_round_trip() {
        let key: u64 = 0xCAFE_BABE_DEAD_BEEF;
        assert_eq!(Key::serialized_size(&key), 8);
        let mut buf = vec![0u8; 8];
        Key::serialize(&key, &mut buf);
        let restored = <u64 as Key>::deserialize(&buf);
        assert_eq!(restored, key);
    }

    #[test]
    fn i32_key_round_trip() {
        let key: i32 = -12345;
        assert_eq!(Key::serialized_size(&key), 4);
        let mut buf = vec![0u8; 4];
        Key::serialize(&key, &mut buf);
        let restored = <i32 as Key>::deserialize(&buf);
        assert_eq!(restored, key);
    }

    #[test]
    fn i64_key_round_trip() {
        let key: i64 = i64::MIN;
        assert_eq!(Key::serialized_size(&key), 8);
        let mut buf = vec![0u8; 8];
        Key::serialize(&key, &mut buf);
        let restored = <i64 as Key>::deserialize(&buf);
        assert_eq!(restored, key);
    }

    #[test]
    fn u64_value_round_trip() {
        let value: u64 = 42;
        assert_eq!(Value::serialized_size(&value), 8);
        let mut buf = vec![0u8; 8];
        Value::serialize(&value, &mut buf);
        let restored = <u64 as Value>::deserialize(&buf);
        assert_eq!(restored, value);
    }

    // ── Fixed-size marker traits ────────────────────────────────────

    #[test]
    fn fixed_size_key_constants() {
        assert_eq!(<u32 as FixedSizeKey>::SIZE, 4);
        assert_eq!(<u64 as FixedSizeKey>::SIZE, 8);
        assert_eq!(<i32 as FixedSizeKey>::SIZE, 4);
        assert_eq!(<i64 as FixedSizeKey>::SIZE, 8);
    }

    #[test]
    fn fixed_size_value_constants() {
        assert_eq!(<u32 as FixedSizeValue>::SIZE, 4);
        assert_eq!(<u64 as FixedSizeValue>::SIZE, 8);
        assert_eq!(<i32 as FixedSizeValue>::SIZE, 4);
        assert_eq!(<i64 as FixedSizeValue>::SIZE, 8);
    }

    // ── Vec<u8> key/value round-trips ───────────────────────────────

    #[test]
    fn vec_u8_key_round_trip() {
        let key: Vec<u8> = vec![1, 2, 3, 4, 5];
        assert_eq!(Key::serialized_size(&key), 4 + 5); // length prefix + data
        let mut buf = vec![0u8; Key::serialized_size(&key)];
        Key::serialize(&key, &mut buf);
        let restored = <Vec<u8> as Key>::deserialize(&buf);
        assert_eq!(restored, key);
    }

    #[test]
    fn vec_u8_empty() {
        let key: Vec<u8> = vec![];
        assert_eq!(Key::serialized_size(&key), 4);
        let mut buf = vec![0u8; 4];
        Key::serialize(&key, &mut buf);
        let restored = <Vec<u8> as Key>::deserialize(&buf);
        assert!(restored.is_empty());
    }

    #[test]
    fn vec_u8_value_round_trip() {
        let value: Vec<u8> = vec![0xAA; 256];
        let mut buf = vec![0u8; Value::serialized_size(&value)];
        Value::serialize(&value, &mut buf);
        let restored = <Vec<u8> as Value>::deserialize(&buf);
        assert_eq!(restored, value);
    }

    // ── String key/value round-trips ────────────────────────────────

    #[test]
    fn string_key_round_trip() {
        let key = String::from("hello world");
        assert_eq!(Key::serialized_size(&key), 4 + 11);
        let mut buf = vec![0u8; Key::serialized_size(&key)];
        Key::serialize(&key, &mut buf);
        let restored = <String as Key>::deserialize(&buf);
        assert_eq!(restored, key);
    }

    #[test]
    fn string_empty() {
        let key = String::new();
        assert_eq!(Key::serialized_size(&key), 4);
        let mut buf = vec![0u8; 4];
        Key::serialize(&key, &mut buf);
        let restored = <String as Key>::deserialize(&buf);
        assert!(restored.is_empty());
    }

    #[test]
    fn string_unicode() {
        let key = String::from("日本語テスト 🎉");
        let mut buf = vec![0u8; Key::serialized_size(&key)];
        Key::serialize(&key, &mut buf);
        let restored = <String as Key>::deserialize(&buf);
        assert_eq!(restored, key);
    }

    #[test]
    fn string_value_round_trip() {
        let value = String::from("value data");
        let mut buf = vec![0u8; Value::serialized_size(&value)];
        Value::serialize(&value, &mut buf);
        let restored = <String as Value>::deserialize(&buf);
        assert_eq!(restored, value);
    }

    // ── Little-endian encoding ──────────────────────────────────────

    #[test]
    fn u32_serializes_as_little_endian() {
        let key: u32 = 0x01020304;
        let mut buf = [0u8; 4];
        Key::serialize(&key, &mut buf);
        assert_eq!(buf, [0x04, 0x03, 0x02, 0x01]);
    }

    #[test]
    fn vec_u8_length_prefix_is_little_endian() {
        let key: Vec<u8> = vec![0xAA; 0x0100]; // 256 bytes
        let mut buf = vec![0u8; Key::serialized_size(&key)];
        Key::serialize(&key, &mut buf);
        // Length prefix: 256 as u32 little-endian = [0x00, 0x01, 0x00, 0x00]
        assert_eq!(&buf[..4], &[0x00, 0x01, 0x00, 0x00]);
    }

    // ── Trait bound verification ────────────────────────────────────

    #[test]
    fn key_requires_hashable() {
        fn use_key<K: Key>(k: &K) -> usize {
            k.serialized_size()
        }
        let k: u64 = 42;
        assert_eq!(use_key(&k), 8);
    }

    // ── Property tests ──────────────────────────────────────────────

    mod proptests {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            #[test]
            fn u64_key_prop_round_trip(v in any::<u64>()) {
                let mut buf = vec![0u8; 8];
                Key::serialize(&v, &mut buf);
                let restored = <u64 as Key>::deserialize(&buf);
                prop_assert_eq!(restored, v);
            }

            #[test]
            fn u32_key_prop_round_trip(v in any::<u32>()) {
                let mut buf = vec![0u8; 4];
                Key::serialize(&v, &mut buf);
                let restored = <u32 as Key>::deserialize(&buf);
                prop_assert_eq!(restored, v);
            }

            #[test]
            fn i64_key_prop_round_trip(v in any::<i64>()) {
                let mut buf = vec![0u8; 8];
                Key::serialize(&v, &mut buf);
                let restored = <i64 as Key>::deserialize(&buf);
                prop_assert_eq!(restored, v);
            }

            #[test]
            fn i32_key_prop_round_trip(v in any::<i32>()) {
                let mut buf = vec![0u8; 4];
                Key::serialize(&v, &mut buf);
                let restored = <i32 as Key>::deserialize(&buf);
                prop_assert_eq!(restored, v);
            }

            #[test]
            fn vec_u8_key_prop_round_trip(v in proptest::collection::vec(any::<u8>(), 0..1024)) {
                let mut buf = vec![0u8; Key::serialized_size(&v)];
                Key::serialize(&v, &mut buf);
                let restored = <Vec<u8> as Key>::deserialize(&buf);
                prop_assert_eq!(restored, v);
            }

            #[test]
            fn string_key_prop_round_trip(v in ".*") {
                let mut buf = vec![0u8; Key::serialized_size(&v)];
                Key::serialize(&v, &mut buf);
                let restored = <String as Key>::deserialize(&buf);
                prop_assert_eq!(restored, v);
            }
        }
    }
}
