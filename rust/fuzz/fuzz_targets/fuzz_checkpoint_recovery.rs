//! Fuzz target: checkpoint metadata parsing and index recovery.
//!
//! Exercises the binary index checkpoint file parser
//! (`IndexCheckpointReader::open`, `verify`, `read_header`) and the
//! JSON checkpoint metadata deserializer with arbitrary bytes.
//!
//! The index checkpoint format uses a 28-byte header (magic "FXIX" +
//! version + bucket metadata) followed by raw bucket data and a CRC-32
//! footer. Corrupted headers, bad CRC values, truncated files, and
//! oversized bucket counts should all be rejected gracefully.

#![no_main]

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use std::io::Write;

use faster_core::checkpoint::index_writer::IndexCheckpointReader;
use faster_core::checkpoint::{
    CheckpointMetadata, CheckpointToken, CheckpointType, IndexRecoveryInfo, LogRecoveryInfo,
    SessionRecoveryInfo,
};

/// Fuzzer input: raw bytes for an index checkpoint file and a JSON string
/// for metadata parsing.
#[derive(Arbitrary, Debug)]
struct CheckpointInput {
    /// Raw bytes to interpret as an index checkpoint binary file.
    index_file_bytes: Vec<u8>,
    /// Raw bytes to interpret as checkpoint metadata JSON.
    json_bytes: Vec<u8>,
}

fuzz_target!(|input: CheckpointInput| {
    // Limit input size to avoid OOM in tempfile writes.
    if input.index_file_bytes.len() > 16 * 1024 * 1024 {
        return;
    }

    // ── Binary index checkpoint file parsing ─────────────────────────
    // IndexCheckpointReader::open() parses the 28-byte header, validates
    // magic bytes and format version. Must never panic on any input.
    if !input.index_file_bytes.is_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("test.index");
        {
            let mut f = std::fs::File::create(&path).expect("create");
            f.write_all(&input.index_file_bytes).expect("write");
        }

        match IndexCheckpointReader::open(&path) {
            Ok(reader) => {
                // Header parsed successfully — exercise all accessors.
                let _header = reader.read_header();
                let _buckets = reader.num_buckets();
                let _bits = reader.table_size_bits();
                let _count = reader.entry_count();

                // CRC verification — must return Ok or Err, never panic.
                let _ = reader.verify();
            }
            Err(_) => {
                // Expected for most fuzzed inputs — bad magic, bad version,
                // truncated header, etc.
            }
        }
    }

    // ── JSON checkpoint metadata deserialization ─────────────────────
    // These types all derive serde::Deserialize. Malformed JSON must
    // produce Err, never panic.
    if let Ok(json_str) = std::str::from_utf8(&input.json_bytes) {
        // Full checkpoint metadata.
        let _ = serde_json::from_str::<CheckpointMetadata>(json_str);

        // Individual recovery info types — the smallest attack surfaces.
        let _ = serde_json::from_str::<IndexRecoveryInfo>(json_str);
        let _ = serde_json::from_str::<LogRecoveryInfo>(json_str);
        let _ = serde_json::from_str::<SessionRecoveryInfo>(json_str);
        let _ = serde_json::from_str::<CheckpointToken>(json_str);
        let _ = serde_json::from_str::<CheckpointType>(json_str);
    }

    // ── Structured JSON with valid-ish fields ────────────────────────
    // Also try well-formed JSON with random field values, which exercises
    // deeper validation paths inside the metadata types.
    if input.json_bytes.len() >= 16 {
        let token_val = u128::from_le_bytes(
            input.json_bytes[..16].try_into().unwrap(),
        );
        let json = format!(
            r#"{{
                "token": {token_val},
                "checkpoint_type": "FoldOver",
                "index_info": {{
                    "format_version": 3,
                    "version": 1,
                    "table_size": 16,
                    "num_ht_bytes": 1024,
                    "num_ofb_bytes": 0,
                    "num_buckets": 16,
                    "start_logical_address": 0,
                    "final_logical_address": 0
                }},
                "log_info": {{
                    "format_version": 3,
                    "version": 1,
                    "checkpoint_type": "FoldOver",
                    "begin_address": 0,
                    "flushed_until_address": 0,
                    "final_address": 0,
                    "head_address": 0,
                    "snapshot_start_address": 0,
                    "snapshot_final_address": 0,
                    "use_snapshot_file": false,
                    "object_log_segment_count": 0
                }},
                "session_infos": [],
                "created_at": "2026-01-01T00:00:00Z"
            }}"#,
        );
        let _ = serde_json::from_str::<CheckpointMetadata>(&json);
    }
});
