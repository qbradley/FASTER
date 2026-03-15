# Decision: Fuzz Targets for Recovery/Checkpoint Deserialization (A9)

**Agent:** Galadriel (Security Expert)
**Date:** 2026-03-11
**Branch:** `galadriel/fuzz-recovery-paths`
**Status:** Implemented, awaiting CI integration

## Summary

Added 3 new cargo-fuzz targets to close the recovery/checkpoint deserialization fuzz gap identified in the retrospective (A9). The fuzz crate now has 8 targets total, covering all major attack surfaces.

## New Targets

| Target | Attack Surface | Key Entry Points |
|--------|---------------|-----------------|
| `fuzz_page_trailer` | CRC computation/verification | `PageTrailer::from_bytes`, `from_slice`, `write_size`, `crc_range` |
| `fuzz_checkpoint_recovery` | Binary index file + JSON metadata | `IndexCheckpointReader::open/verify`, `serde_json::from_str` for all metadata types |
| `fuzz_log_recovery` | Full log recovery pipeline | `LogRecoveryEngine::recover_fold_over()` with arbitrary segment files |

## Dependencies Added (fuzz crate only)

- `serde_json = "1"` — JSON deserialization
- `serde = "1"` — Deserialize trait
- `tempfile = "3"` — temp directories for file-based targets
- `crc32fast = "1"` — CRC roundtrip verification

## CI Integration Required

For the 10-min-per-run budget from A9:
```bash
cargo +nightly fuzz run fuzz_page_trailer -- -max_total_time=200
cargo +nightly fuzz run fuzz_checkpoint_recovery -- -max_total_time=200
cargo +nightly fuzz run fuzz_log_recovery -- -max_total_time=200
```

## Team Impact

- **Frodo:** Add to CI fuzzing job (requires nightly toolchain)
- **Éowyn:** Complementary to DST — fuzzing covers byte-level mutations that DST's crash injection doesn't
- **Boromir:** Coverage matrix (A11) now has fuzz coverage for recovery/checkpoint column
- **Aragorn:** If adding new deserialization paths in recovery, add corresponding fuzz target
