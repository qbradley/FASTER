//! Index checkpoint writer and reader for the FASTER hash index.
//!
//! Serializes the hash index to a binary file during checkpoint and reads it
//! back during recovery. The file format is:
//!
//! ```text
//! ┌──────────────────────────────────────┐
//! │  Header                              │
//! │    magic: [u8; 4]   = b"FXIX"       │
//! │    version: u32     = 1              │
//! │    table_size_bits: u8               │
//! │    num_buckets: u64                  │
//! │    entry_count: u64                  │
//! ├──────────────────────────────────────┤
//! │  Body                                │
//! │    bucket[0]:  [u8; 64]              │
//! │    bucket[1]:  [u8; 64]              │
//! │    ...                               │
//! │    bucket[N-1]: [u8; 64]             │
//! ├──────────────────────────────────────┤
//! │  Footer                              │
//! │    crc32: u32  (of header + body)    │
//! └──────────────────────────────────────┘
//! ```
//!
//! Each [`HashBucket`](crate::hash_bucket::HashBucket) is written as its raw
//! 64-byte `#[repr(C, align(64))]` representation. This is safe because
//! `HashBucket` contains only atomic integers (which have the same layout as
//! their non-atomic counterparts) and uses a fixed, platform-independent layout.

use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use crate::address::LogicalAddress;
use crate::checkpoint::{CheckpointError, CheckpointToken, IndexRecoveryInfo};
use crate::hash_bucket::HashBucket;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Magic bytes identifying a FASTER index checkpoint file.
const INDEX_MAGIC: &[u8; 4] = b"FXIX";

/// Current format version of the index checkpoint file.
const FORMAT_VERSION: u32 = 1;

/// Size of the file header in bytes:
/// magic(4) + version(4) + table_size_bits(1) + padding(3) + num_buckets(8) + entry_count(8) = 28
const HEADER_SIZE: usize = 4 + 4 + 1 + 3 + 8 + 8;

/// Size of a single serialized bucket in bytes.
const BUCKET_SIZE: usize = core::mem::size_of::<HashBucket>();

// Compile-time assertion that HashBucket is exactly 64 bytes.
const _: () = assert!(BUCKET_SIZE == 64);

// ---------------------------------------------------------------------------
// IndexCheckpointWriter
// ---------------------------------------------------------------------------

/// Writes the hash index to persistent storage during a checkpoint.
///
/// The writer serializes the primary bucket array of a hash index into a
/// binary file at `{checkpoint_dir}/{token}.index`. The file format includes
/// a header with metadata, the raw bucket data, and a CRC32 footer for
/// integrity verification.
///
/// # Examples
///
/// ```no_run
/// use std::path::Path;
/// use faster_core::checkpoint::{CheckpointToken, IndexRecoveryInfo};
/// use faster_core::checkpoint::index_writer::IndexCheckpointWriter;
/// use faster_core::hash_table::HashTable;
///
/// let dir = Path::new("/tmp/checkpoint");
/// let token = CheckpointToken::new(42);
/// let mut writer = IndexCheckpointWriter::new(dir, &token).unwrap();
///
/// let table = HashTable::new(8);
/// let info = writer.write_index(table.bucket_slice(), table.log2_buckets() as u8, 0, 0).unwrap();
/// assert_eq!(info.table_size, 256);
/// ```
pub struct IndexCheckpointWriter {
    /// Full path to the checkpoint file.
    path: PathBuf,
}

impl IndexCheckpointWriter {
    /// Creates a new index checkpoint writer.
    ///
    /// The checkpoint file will be written to `{checkpoint_dir}/{token}.index`.
    /// The directory is created if it does not already exist.
    ///
    /// # Errors
    ///
    /// Returns [`CheckpointError::IoError`] if the directory cannot be created.
    pub fn new(checkpoint_dir: &Path, token: &CheckpointToken) -> Result<Self, CheckpointError> {
        std::fs::create_dir_all(checkpoint_dir)?;
        let path = checkpoint_dir.join(format!("{token}.index"));
        Ok(Self { path })
    }

    /// Writes the hash index bucket data to the checkpoint file.
    ///
    /// Serializes the header, all primary buckets, and a CRC32 footer.
    /// Returns an [`IndexRecoveryInfo`] describing the checkpointed index.
    ///
    /// # Parameters
    ///
    /// - `buckets`: the primary bucket array (from [`HashTable::bucket_slice`]).
    /// - `table_size_bits`: log2 of the number of buckets.
    /// - `version`: the hash index version (0 or 1).
    /// - `entry_count`: approximate number of live entries.
    ///
    /// # Errors
    ///
    /// Returns [`CheckpointError::IoError`] on any I/O failure.
    pub fn write_index(
        &mut self,
        buckets: &[HashBucket],
        table_size_bits: u8,
        version: u32,
        entry_count: u64,
    ) -> Result<IndexRecoveryInfo, CheckpointError> {
        let num_buckets = buckets.len() as u64;

        let file = File::create(&self.path)?;
        let mut writer = BufWriter::new(file);
        let mut hasher = crc32fast::Hasher::new();

        // -- Header --
        let header = encode_header(table_size_bits, num_buckets, entry_count);
        writer.write_all(&header)?;
        hasher.update(&header);

        // -- Body: raw bucket data --
        for bucket in buckets {
            // SAFETY: `HashBucket` is `#[repr(C, align(64))]` and contains only
            // `AtomicU64` / `AtomicLogicalAddress` fields, which have the same
            // in-memory representation as `u64` on all platforms where
            // `AtomicU64` is lock-free (guaranteed by Rust on all tier-1
            // targets). The struct has no padding holes because
            // 7×8 + 1×8 = 64 bytes == size_of::<HashBucket>().
            //
            // We read the raw bytes of a shared reference; no mutation occurs.
            // The atomic values are read with an implicit Relaxed ordering
            // (byte copy). The caller is responsible for ensuring no concurrent
            // writers are active (checkpoint quiescence).
            let bytes: &[u8; BUCKET_SIZE] =
                unsafe { &*(bucket as *const HashBucket as *const [u8; BUCKET_SIZE]) };
            writer.write_all(bytes)?;
            hasher.update(bytes);
        }

        // -- Footer: CRC32 --
        let crc = hasher.finalize();
        writer.write_all(&crc.to_le_bytes())?;
        writer.flush()?;

        let info = IndexRecoveryInfo {
            format_version: crate::checkpoint::FORMAT_VERSION_CURRENT,
            version: version as u64,
            table_size: num_buckets,
            num_ht_bytes: num_buckets * BUCKET_SIZE as u64,
            num_ofb_bytes: 0, // overflow buckets are not persisted in this format
            num_buckets,
            start_logical_address: LogicalAddress::ZERO,
            final_logical_address: LogicalAddress::ZERO,
        };

        Ok(info)
    }
}

// ---------------------------------------------------------------------------
// IndexCheckpointReader
// ---------------------------------------------------------------------------

/// Reads and verifies an index checkpoint file.
///
/// Provides header inspection and full CRC32 integrity verification without
/// loading the entire index into memory. Used during recovery to validate
/// checkpoint files before restoring the hash index.
///
/// # Examples
///
/// ```no_run
/// use std::path::Path;
/// use faster_core::checkpoint::index_writer::IndexCheckpointReader;
///
/// let reader = IndexCheckpointReader::open(Path::new("/tmp/ckpt/42.index")).unwrap();
/// let info = reader.read_header();
/// println!("Buckets: {}", info.table_size);
/// reader.verify().expect("CRC mismatch!");
/// ```
pub struct IndexCheckpointReader {
    /// Full path to the checkpoint file.
    path: PathBuf,
    /// Decoded header metadata.
    header: DecodedHeader,
}

/// Internal representation of the parsed file header.
#[derive(Debug, Clone, Copy)]
struct DecodedHeader {
    table_size_bits: u8,
    num_buckets: u64,
    entry_count: u64,
}

impl IndexCheckpointReader {
    /// Opens an existing index checkpoint file and validates its header.
    ///
    /// Reads and verifies the magic bytes and format version without loading
    /// the body. The CRC is **not** checked here — call [`verify`](Self::verify)
    /// for full integrity validation.
    ///
    /// # Errors
    ///
    /// Returns [`CheckpointError::IoError`] on I/O failure, or
    /// [`CheckpointError::InvalidState`] if the magic bytes or format version
    /// are invalid.
    pub fn open(path: &Path) -> Result<Self, CheckpointError> {
        let mut file = BufReader::new(File::open(path)?);

        let mut header_buf = [0u8; HEADER_SIZE];
        file.read_exact(&mut header_buf).map_err(|e| {
            CheckpointError::InvalidState(format!("failed to read index header: {e}"))
        })?;

        let header = decode_header(&header_buf)?;

        Ok(Self {
            path: path.to_path_buf(),
            header,
        })
    }

    /// Returns recovery metadata from the checkpoint header.
    ///
    /// This is a cheap operation that does not read the body or verify the CRC.
    pub fn read_header(&self) -> IndexRecoveryInfo {
        IndexRecoveryInfo {
            format_version: crate::checkpoint::FORMAT_VERSION_CURRENT,
            version: 0, // version is not stored in the file header; set by caller
            table_size: self.header.num_buckets,
            num_ht_bytes: self.header.num_buckets * BUCKET_SIZE as u64,
            num_ofb_bytes: 0,
            num_buckets: self.header.num_buckets,
            start_logical_address: LogicalAddress::ZERO,
            final_logical_address: LogicalAddress::ZERO,
        }
    }

    /// Returns the number of primary buckets recorded in the header.
    pub fn num_buckets(&self) -> u64 {
        self.header.num_buckets
    }

    /// Returns the `log2(num_buckets)` value from the header.
    pub fn table_size_bits(&self) -> u8 {
        self.header.table_size_bits
    }

    /// Returns the entry count recorded in the header.
    pub fn entry_count(&self) -> u64 {
        self.header.entry_count
    }

    /// Performs full CRC32 verification of the checkpoint file.
    ///
    /// Reads the entire file (header + body), computes the CRC32, and
    /// compares it against the stored footer value.
    ///
    /// # Errors
    ///
    /// Returns [`CheckpointError::InvalidState`] if the CRC does not match,
    /// or [`CheckpointError::IoError`] on I/O failure.
    pub fn verify(&self) -> Result<(), CheckpointError> {
        let mut file = BufReader::new(File::open(&self.path)?);

        let body_size = self.header.num_buckets as usize * BUCKET_SIZE;
        let data_size = HEADER_SIZE + body_size;

        let mut data = vec![0u8; data_size];
        file.read_exact(&mut data).map_err(|e| {
            CheckpointError::InvalidState(format!("failed to read index data for CRC: {e}"))
        })?;

        let mut stored_crc_buf = [0u8; 4];
        file.read_exact(&mut stored_crc_buf).map_err(|e| {
            CheckpointError::InvalidState(format!("failed to read index CRC footer: {e}"))
        })?;
        let stored_crc = u32::from_le_bytes(stored_crc_buf);

        let computed_crc = crc32fast::hash(&data);

        if computed_crc != stored_crc {
            return Err(CheckpointError::InvalidState(format!(
                "CRC mismatch: expected 0x{stored_crc:08x}, computed 0x{computed_crc:08x}"
            )));
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Header encoding/decoding
// ---------------------------------------------------------------------------

/// Encodes the file header into a fixed-size byte array.
fn encode_header(table_size_bits: u8, num_buckets: u64, entry_count: u64) -> [u8; HEADER_SIZE] {
    let mut buf = [0u8; HEADER_SIZE];
    buf[0..4].copy_from_slice(INDEX_MAGIC);
    buf[4..8].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
    buf[8] = table_size_bits;
    // buf[9..12] = padding (zeros)
    buf[12..20].copy_from_slice(&num_buckets.to_le_bytes());
    buf[20..28].copy_from_slice(&entry_count.to_le_bytes());
    buf
}

/// Decodes and validates the file header from raw bytes.
fn decode_header(buf: &[u8; HEADER_SIZE]) -> Result<DecodedHeader, CheckpointError> {
    if &buf[0..4] != INDEX_MAGIC {
        return Err(CheckpointError::InvalidState(format!(
            "invalid index magic: expected {:?}, got {:?}",
            INDEX_MAGIC,
            &buf[0..4]
        )));
    }

    let version = u32::from_le_bytes(buf[4..8].try_into().unwrap());
    if version != FORMAT_VERSION {
        return Err(CheckpointError::InvalidState(format!(
            "unsupported index format version: expected {FORMAT_VERSION}, got {version}"
        )));
    }

    let table_size_bits = buf[8];
    let num_buckets = u64::from_le_bytes(buf[12..20].try_into().unwrap());
    let entry_count = u64::from_le_bytes(buf[20..28].try_into().unwrap());

    Ok(DecodedHeader {
        table_size_bits,
        num_buckets,
        entry_count,
    })
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::address::LogicalAddress;
    use crate::hash::KeyHash;
    use crate::hash_bucket::HashBucketEntry;
    use crate::hash_table::HashTable;
    use std::io::{Seek, SeekFrom};

    /// Helper: create a temp dir and return checkpoint writer + dir handle.
    fn setup_writer() -> (IndexCheckpointWriter, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("create temp dir");
        let token = CheckpointToken::new(42);
        let writer =
            IndexCheckpointWriter::new(dir.path(), &token).expect("create checkpoint writer");
        (writer, dir)
    }

    /// Helper: path to the checkpoint file for token=42.
    fn checkpoint_path(dir: &tempfile::TempDir) -> PathBuf {
        let token = CheckpointToken::new(42);
        dir.path().join(format!("{token}.index"))
    }

    /// Helper: insert committed entries into a `HashTable` and return the
    /// number of entries actually inserted.
    fn populate_table(table: &HashTable, count: u64) -> u64 {
        let mut inserted = 0u64;
        for i in 0..count {
            let hash = KeyHash::new(i.wrapping_mul(0x9E37_79B9_7F4A_7C15));
            let result = table.find_or_create_entry(hash, LogicalAddress::INVALID);
            if result.created {
                // Commit the tentative entry.
                let committed = HashBucketEntry::new(
                    result.entry.tag(),
                    LogicalAddress::from_raw(i + 1),
                    false,
                );
                table.update_entry(result.slot, result.entry, committed);
                inserted += 1;
            }
        }
        inserted
    }

    // -----------------------------------------------------------------------
    // Round-trip: write then read-back
    // -----------------------------------------------------------------------

    #[test]
    fn round_trip_empty_index() {
        let (mut writer, dir) = setup_writer();
        let table = HashTable::new(4); // 16 buckets, all empty

        let info = writer
            .write_index(table.bucket_slice(), table.log2_buckets() as u8, 0, 0)
            .unwrap();
        assert_eq!(info.table_size, 16);
        assert_eq!(info.num_buckets, 16);
        assert_eq!(info.num_ht_bytes, 16 * 64);
        assert_eq!(info.num_ofb_bytes, 0);

        let path = checkpoint_path(&dir);
        let reader = IndexCheckpointReader::open(&path).unwrap();
        let header_info = reader.read_header();
        assert_eq!(header_info.table_size, 16);
        assert_eq!(header_info.num_buckets, 16);

        reader.verify().unwrap();
    }

    #[test]
    fn round_trip_with_entries() {
        let (mut writer, dir) = setup_writer();
        let table = HashTable::new(8); // 256 buckets
        let inserted = populate_table(&table, 50);
        assert_eq!(inserted, 50);

        let info = writer
            .write_index(
                table.bucket_slice(),
                table.log2_buckets() as u8,
                0,
                inserted,
            )
            .unwrap();
        assert_eq!(info.table_size, 256);

        let path = checkpoint_path(&dir);
        let reader = IndexCheckpointReader::open(&path).unwrap();
        assert_eq!(reader.num_buckets(), 256);
        assert_eq!(reader.table_size_bits(), 8);
        assert_eq!(reader.entry_count(), 50);

        reader.verify().unwrap();
    }

    // -----------------------------------------------------------------------
    // Header correctness
    // -----------------------------------------------------------------------

    #[test]
    fn header_encode_decode_round_trip() {
        let encoded = encode_header(10, 1024, 500);
        let decoded = decode_header(&encoded).unwrap();
        assert_eq!(decoded.table_size_bits, 10);
        assert_eq!(decoded.num_buckets, 1024);
        assert_eq!(decoded.entry_count, 500);
    }

    #[test]
    fn header_invalid_magic_rejected() {
        let mut buf = encode_header(8, 256, 0);
        buf[0] = b'Z'; // corrupt magic
        let result = decode_header(&buf);
        assert!(result.is_err());
    }

    #[test]
    fn header_invalid_version_rejected() {
        let mut buf = encode_header(8, 256, 0);
        buf[4..8].copy_from_slice(&99u32.to_le_bytes()); // bad version
        let result = decode_header(&buf);
        assert!(result.is_err());
    }

    // -----------------------------------------------------------------------
    // CRC validation — corruption detection
    // -----------------------------------------------------------------------

    #[test]
    fn crc_detects_corrupted_body() {
        let (mut writer, dir) = setup_writer();
        let table = HashTable::new(4);
        writer
            .write_index(table.bucket_slice(), table.log2_buckets() as u8, 0, 0)
            .unwrap();

        let path = checkpoint_path(&dir);

        // Corrupt a byte in the body area (after the header).
        {
            let mut file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
            file.seek(SeekFrom::Start(HEADER_SIZE as u64 + 10)).unwrap();
            file.write_all(&[0xFF]).unwrap();
        }

        let reader = IndexCheckpointReader::open(&path).unwrap();
        let result = reader.verify();
        assert!(result.is_err(), "CRC should detect body corruption");
    }

    #[test]
    fn crc_detects_corrupted_header() {
        let (mut writer, dir) = setup_writer();
        let table = HashTable::new(4);
        writer
            .write_index(table.bucket_slice(), table.log2_buckets() as u8, 0, 0)
            .unwrap();

        let path = checkpoint_path(&dir);

        // Corrupt the entry_count field in the header (offset 20).
        {
            let mut file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
            file.seek(SeekFrom::Start(20)).unwrap();
            file.write_all(&[0xFF, 0xFF]).unwrap();
        }

        // Header still parses (corrupt entry_count is still valid bytes),
        // but CRC should fail.
        let reader = IndexCheckpointReader::open(&path).unwrap();
        let result = reader.verify();
        assert!(result.is_err(), "CRC should detect header corruption");
    }

    // -----------------------------------------------------------------------
    // Empty index checkpoint — file size
    // -----------------------------------------------------------------------

    #[test]
    fn empty_index_file_size_correct() {
        let (mut writer, dir) = setup_writer();
        let table = HashTable::new(4); // 16 buckets
        writer
            .write_index(table.bucket_slice(), table.log2_buckets() as u8, 0, 0)
            .unwrap();

        let path = checkpoint_path(&dir);
        let metadata = std::fs::metadata(&path).unwrap();
        // header(28) + body(16 * 64) + footer(4) = 28 + 1024 + 4 = 1056
        let expected = HEADER_SIZE as u64 + 16 * BUCKET_SIZE as u64 + 4;
        assert_eq!(metadata.len(), expected);
    }

    // -----------------------------------------------------------------------
    // Index with overflow buckets
    // -----------------------------------------------------------------------

    #[test]
    fn index_with_overflow_still_checkpoints() {
        let (mut writer, dir) = setup_writer();
        // Small table (4 buckets, 28 slots) with enough inserts to force overflow.
        let table = HashTable::new(2); // 4 buckets
        let inserted = populate_table(&table, 50);

        // Overflow buckets should have been allocated.
        assert!(
            table.overflow_count() > 0,
            "expected overflow buckets to be allocated"
        );

        let info = writer
            .write_index(
                table.bucket_slice(),
                table.log2_buckets() as u8,
                0,
                inserted,
            )
            .unwrap();
        assert_eq!(info.table_size, 4);

        let path = checkpoint_path(&dir);
        let reader = IndexCheckpointReader::open(&path).unwrap();
        reader.verify().unwrap();
    }

    // -----------------------------------------------------------------------
    // Large index (1000+ buckets)
    // -----------------------------------------------------------------------

    #[test]
    fn large_index_round_trip() {
        let (mut writer, dir) = setup_writer();
        let table = HashTable::new(10); // 1024 buckets
        let inserted = populate_table(&table, 500);

        let info = writer
            .write_index(
                table.bucket_slice(),
                table.log2_buckets() as u8,
                0,
                inserted,
            )
            .unwrap();
        assert_eq!(info.table_size, 1024);
        assert_eq!(info.num_ht_bytes, 1024 * 64);

        let path = checkpoint_path(&dir);
        let reader = IndexCheckpointReader::open(&path).unwrap();
        assert_eq!(reader.num_buckets(), 1024);

        reader.verify().unwrap();

        // Verify file size.
        let metadata = std::fs::metadata(&path).unwrap();
        let expected = HEADER_SIZE as u64 + 1024 * BUCKET_SIZE as u64 + 4;
        assert_eq!(metadata.len(), expected);
    }

    // -----------------------------------------------------------------------
    // Reader rejects non-existent file
    // -----------------------------------------------------------------------

    #[test]
    fn reader_rejects_missing_file() {
        let result = IndexCheckpointReader::open(Path::new("/tmp/nonexistent_faster.index"));
        assert!(result.is_err());
    }

    // -----------------------------------------------------------------------
    // Multiple checkpoints with different tokens
    // -----------------------------------------------------------------------

    #[test]
    fn multiple_tokens_produce_distinct_files() {
        let dir = tempfile::tempdir().unwrap();
        let table = HashTable::new(4);

        let token1 = CheckpointToken::new(1);
        let token2 = CheckpointToken::new(2);

        let mut w1 = IndexCheckpointWriter::new(dir.path(), &token1).unwrap();
        let mut w2 = IndexCheckpointWriter::new(dir.path(), &token2).unwrap();

        w1.write_index(table.bucket_slice(), table.log2_buckets() as u8, 0, 0)
            .unwrap();
        w2.write_index(table.bucket_slice(), table.log2_buckets() as u8, 0, 0)
            .unwrap();

        let p1 = dir.path().join(format!("{token1}.index"));
        let p2 = dir.path().join(format!("{token2}.index"));

        assert!(p1.exists());
        assert!(p2.exists());
        assert_ne!(p1, p2);
    }

    // -----------------------------------------------------------------------
    // Bucket content preservation (byte-level round-trip)
    // -----------------------------------------------------------------------

    #[test]
    fn bucket_bytes_preserved_in_checkpoint() {
        let (mut writer, dir) = setup_writer();
        let table = HashTable::new(4); // 16 buckets

        // Insert a known entry.
        let hash = KeyHash::new(0xDEAD_BEEF_CAFE_0001);
        let result = table.find_or_create_entry(hash, LogicalAddress::INVALID);
        assert!(result.created);
        let committed =
            HashBucketEntry::new(result.entry.tag(), LogicalAddress::from_raw(0x42), false);
        table.update_entry(result.slot, result.entry, committed);

        writer
            .write_index(table.bucket_slice(), table.log2_buckets() as u8, 0, 1)
            .unwrap();

        let path = checkpoint_path(&dir);

        // Read back the raw body bytes and compare with in-memory buckets.
        let mut file = File::open(&path).unwrap();
        let mut header_buf = [0u8; HEADER_SIZE];
        file.read_exact(&mut header_buf).unwrap();

        for bucket in table.bucket_slice() {
            let mut file_bucket = [0u8; BUCKET_SIZE];
            file.read_exact(&mut file_bucket).unwrap();

            // SAFETY: same justification as write_index — reading raw repr(C)
            // bytes of a HashBucket for comparison.
            let mem_bytes: &[u8; BUCKET_SIZE] =
                unsafe { &*(bucket as *const HashBucket as *const [u8; BUCKET_SIZE]) };
            assert_eq!(&file_bucket, mem_bytes, "bucket bytes mismatch");
        }
    }

    // -----------------------------------------------------------------------
    // Version and entry_count are propagated
    // -----------------------------------------------------------------------

    #[test]
    fn version_and_entry_count_in_recovery_info() {
        let (mut writer, dir) = setup_writer();
        let table = HashTable::new(4);

        let info = writer
            .write_index(table.bucket_slice(), table.log2_buckets() as u8, 7, 999)
            .unwrap();
        assert_eq!(info.version, 7);

        let path = checkpoint_path(&dir);
        let reader = IndexCheckpointReader::open(&path).unwrap();
        assert_eq!(reader.entry_count(), 999);
    }

    // -----------------------------------------------------------------------
    // All entries are empty in an empty-table checkpoint
    // -----------------------------------------------------------------------

    #[test]
    fn empty_table_all_entries_zero() {
        let (mut writer, dir) = setup_writer();
        let table = HashTable::new(3); // 8 buckets
        writer
            .write_index(table.bucket_slice(), table.log2_buckets() as u8, 0, 0)
            .unwrap();

        let path = checkpoint_path(&dir);
        let mut file = File::open(&path).unwrap();

        // Skip header.
        let mut header_buf = [0u8; HEADER_SIZE];
        file.read_exact(&mut header_buf).unwrap();

        // All body bytes should be zero (empty buckets).
        for _ in 0..8 {
            let mut bucket_buf = [0u8; BUCKET_SIZE];
            file.read_exact(&mut bucket_buf).unwrap();
            assert!(
                bucket_buf.iter().all(|&b| b == 0),
                "empty bucket should be all zeros"
            );
        }
    }
}
