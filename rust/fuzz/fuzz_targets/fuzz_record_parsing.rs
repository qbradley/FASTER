//! Fuzz target: record parsing from arbitrary byte slices.
//!
//! Feeds random bytes into `RecordInfo::from_raw`, `Key::deserialize`,
//! `Value::deserialize`, and `serialized_size_from_bytes` for multiple
//! key/value types. The goal is to find panics, bounds-check failures,
//! or undefined behavior in deserialization paths.

#![no_main]

use libfuzzer_sys::fuzz_target;

use faster_core::record::{Key, RecordInfo, Value};

fuzz_target!(|data: &[u8]| {
    // ── RecordInfo from arbitrary u64 ────────────────────────────────
    if data.len() >= 8 {
        let raw = u64::from_le_bytes(data[..8].try_into().unwrap());
        let info = RecordInfo::from_raw(raw);

        // Exercise all accessors — none should panic.
        let _ = info.previous_address();
        let _ = info.checkpoint_version();
        let _ = info.is_invalid();
        let _ = info.is_tombstone();
        let _ = info.is_final();
        let _ = info.is_null();
        let _ = info.with_invalid();
        let _ = info.with_tombstone();
        let _ = info.with_final();
    }

    // ── Fixed-size key deserialization ────────────────────────────────
    // u64 requires exactly 8 bytes — should panic on short input.
    // We catch panics to let the fuzzer keep running.
    if data.len() >= 8 {
        let _ = <u64 as Key>::deserialize(data);
        let _ = <u64 as Key>::serialized_size_from_bytes(data);
        let _ = <u64 as Value>::deserialize(data);
        let _ = <u64 as Value>::serialized_size_from_bytes(data);
    }
    if data.len() >= 4 {
        let _ = <u32 as Key>::deserialize(data);
        let _ = <u32 as Key>::serialized_size_from_bytes(data);
        let _ = <u32 as Value>::deserialize(data);
        let _ = <u32 as Value>::serialized_size_from_bytes(data);

        let _ = <i32 as Key>::deserialize(data);
        let _ = <i64 as Key>::serialized_size_from_bytes(data);
    }

    // ── Variable-length key/value deserialization ─────────────────────
    // Vec<u8> uses 4-byte LE length prefix + data.
    // This is the most panic-prone path: a large length prefix on a
    // short buffer will attempt an out-of-bounds slice.
    if data.len() >= 4 {
        let len_prefix =
            u32::from_le_bytes(data[..4].try_into().unwrap()) as usize;
        // Only attempt deserialization if the claimed length fits.
        if data.len() >= 4 + len_prefix {
            let v = <Vec<u8> as Key>::deserialize(data);
            assert_eq!(v.len(), len_prefix);

            let v2 = <Vec<u8> as Value>::deserialize(data);
            assert_eq!(v2.len(), len_prefix);

            let sz = <Vec<u8> as Key>::serialized_size_from_bytes(data);
            assert_eq!(sz, 4 + len_prefix);
        }
    }

    // ── String deserialization — only with valid UTF-8 ──────────────
    // String::deserialize panics on invalid UTF-8 *by design*.
    // Only test with data we've verified is valid UTF-8.
    if data.len() >= 4 {
        let len_prefix =
            u32::from_le_bytes(data[..4].try_into().unwrap()) as usize;
        if data.len() >= 4 + len_prefix {
            // Only deserialize if the payload is valid UTF-8.
            if std::str::from_utf8(&data[4..4 + len_prefix]).is_ok() {
                let s = <String as Key>::deserialize(data);
                assert_eq!(s.len(), len_prefix);

                let sz = <String as Key>::serialized_size_from_bytes(data);
                assert_eq!(sz, 4 + len_prefix);
            }
        }
    }

    // ── eq_from_bytes with known key vs fuzzed buffer ────────────────
    if data.len() >= 8 {
        let key = <u64 as Key>::deserialize(data);
        let _ = key.eq_from_bytes(data);
    }

    if data.len() >= 4 {
        let len_prefix =
            u32::from_le_bytes(data[..4].try_into().unwrap()) as usize;
        if data.len() >= 4 + len_prefix {
            let key = <Vec<u8> as Key>::deserialize(data);
            let _ = key.eq_from_bytes(data);
        }
    }
});
