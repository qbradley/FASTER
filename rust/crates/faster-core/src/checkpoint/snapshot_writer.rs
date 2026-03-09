//! Snapshot checkpoint writer for the hybrid log.
//!
//! Unlike fold-over checkpoints (which flush everything to the main log file),
//! snapshot checkpoints create a point-in-time copy of the in-memory portion
//! of the log to a **separate** snapshot file. The main log file is left
//! untouched, and the hybrid log can continue accepting writes after the
//! snapshot tail is captured.
//!
//! # Key types
//!
//! - [`SnapshotCheckpointContext`] — captures the address boundaries at
//!   snapshot time and tracks flush progress.
//! - [`SnapshotFileWriter`] — writes individual in-memory pages to the
//!   snapshot file using buffered I/O.
//!
//! # Snapshot protocol overview
//!
//! 1. **Begin** — capture the tail address; writes after this point are
//!    excluded from the checkpoint.
//! 2. **Page scanning** — pages between `head` and `snapshot_tail` that are
//!    still in memory are written to the snapshot file.
//! 3. **Complete** — once all pages are persisted, produce a
//!    [`LogRecoveryInfo`] with `use_snapshot_file = true`.

use crate::sync::Instant;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use crate::address::LogicalAddress;
use crate::hybrid_log::HybridLogAllocator;

use super::{
    CheckpointError, CheckpointToken, CheckpointType, FORMAT_VERSION_CURRENT, LogRecoveryInfo,
};

// ---------------------------------------------------------------------------
// SnapshotCheckpointContext
// ---------------------------------------------------------------------------

/// Captures the hybrid log's address boundaries at snapshot checkpoint start.
///
/// Created by [`LogCheckpointWriter::begin_snapshot_checkpoint`][super::log_writer::LogCheckpointWriter::begin_snapshot_checkpoint]
/// and threaded through the snapshot write phases until completion.
#[derive(Debug)]
pub struct SnapshotCheckpointContext {
    /// Unique token identifying this checkpoint.
    pub token: CheckpointToken,
    /// Tail address at snapshot time — records up to (but not including) this
    /// address are part of the checkpoint.
    pub snapshot_tail: LogicalAddress,
    /// Head address at snapshot time — boundary between on-disk and in-memory.
    pub snapshot_head: LogicalAddress,
    /// Earliest valid address in the log.
    pub begin_address: LogicalAddress,
    /// How far the snapshot flush has progressed.
    pub flushed_until: LogicalAddress,
    /// Timestamp when the snapshot began.
    pub started_at: Instant,
}

impl SnapshotCheckpointContext {
    /// Begin a snapshot checkpoint by capturing the current log boundaries.
    ///
    /// The tail at this instant becomes `snapshot_tail` — any records appended
    /// after this call are **not** part of the checkpoint.
    ///
    /// # Errors
    ///
    /// Returns [`CheckpointError::InvalidState`] if the log's tail is before
    /// the head (which would indicate a corrupted allocator state).
    pub fn begin(
        log: &HybridLogAllocator,
        token: &CheckpointToken,
    ) -> Result<Self, CheckpointError> {
        let snap = log.snapshot();

        if snap.tail_address < snap.head_address {
            return Err(CheckpointError::InvalidState(
                "tail address is before head address".into(),
            ));
        }

        Ok(Self {
            token: *token,
            snapshot_tail: snap.tail_address,
            snapshot_head: snap.head_address,
            begin_address: snap.begin_address,
            flushed_until: snap.head_address,
            started_at: Instant::now(),
        })
    }

    /// Returns the number of in-memory pages that need to be written to the
    /// snapshot file.
    ///
    /// Pages from `snapshot_head` (inclusive) to `snapshot_tail` (exclusive)
    /// are in memory and must be persisted.
    pub fn in_memory_page_count(&self) -> u32 {
        crate::hybrid_log::regions::pages_between(self.snapshot_head, self.snapshot_tail)
    }

    /// Generate the [`LogRecoveryInfo`] for this snapshot checkpoint.
    ///
    /// Should only be called after all in-memory pages have been written to
    /// the snapshot file.
    pub fn into_recovery_info(self) -> LogRecoveryInfo {
        LogRecoveryInfo {
            format_version: FORMAT_VERSION_CURRENT,
            version: 1,
            checkpoint_type: CheckpointType::Snapshot,
            begin_address: self.begin_address,
            flushed_until_address: self.snapshot_tail,
            final_address: self.snapshot_tail,
            head_address: self.snapshot_head,
            snapshot_start_address: self.snapshot_head,
            snapshot_final_address: self.snapshot_tail,
            use_snapshot_file: true,
            object_log_segment_count: 0,
        }
    }
}

// ---------------------------------------------------------------------------
// SnapshotFileWriter
// ---------------------------------------------------------------------------

/// Writes in-memory pages to a separate snapshot file.
///
/// The snapshot file stores a copy of every in-memory page between `head` and
/// `snapshot_tail`. Each page is written at its logical offset
/// (`page_index × page_size`) within the file so that recovery can memory-map
/// or seek directly to any page.
///
/// Uses [`BufWriter`] for performance — call [`finalize`](Self::finalize) to
/// flush the buffer and close the file.
pub struct SnapshotFileWriter {
    /// Buffered writer wrapping the snapshot file.
    writer: BufWriter<std::fs::File>,
    /// Total bytes written so far.
    bytes_written: u64,
    /// Path to the snapshot file (retained for diagnostics).
    path: PathBuf,
}

impl SnapshotFileWriter {
    /// Create a new snapshot file writer at the given path.
    ///
    /// Creates (or truncates) the file and wraps it in a [`BufWriter`].
    ///
    /// # Errors
    ///
    /// Returns [`CheckpointError::IoError`] if the file cannot be created.
    pub fn new(path: &Path) -> Result<Self, CheckpointError> {
        let file = std::fs::File::create(path)?;
        Ok(Self {
            writer: BufWriter::new(file),
            bytes_written: 0,
            path: path.to_path_buf(),
        })
    }

    /// Write a single page to the snapshot file.
    ///
    /// `page_index` is the logical page number and `data` is the raw page
    /// content. The data is written sequentially (pages are expected to be
    /// written in order).
    ///
    /// # Errors
    ///
    /// Returns [`CheckpointError::IoError`] on write failure.
    pub fn write_page(&mut self, page_index: u64, data: &[u8]) -> Result<(), CheckpointError> {
        // Write a small page header: the 8-byte page index (little-endian).
        let header = page_index.to_le_bytes();
        self.writer.write_all(&header)?;
        self.bytes_written += header.len() as u64;

        // Write the page data.
        self.writer.write_all(data)?;
        self.bytes_written += data.len() as u64;

        Ok(())
    }

    /// Flush the buffer and finalize the snapshot file.
    ///
    /// Returns the total number of bytes written.
    ///
    /// # Errors
    ///
    /// Returns [`CheckpointError::IoError`] if the final flush fails.
    pub fn finalize(mut self) -> Result<u64, CheckpointError> {
        self.writer.flush()?;
        Ok(self.bytes_written)
    }

    /// Returns the path to the snapshot file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Returns the number of bytes written so far (before finalization).
    pub fn bytes_written(&self) -> u64 {
        self.bytes_written
    }
}

// ---------------------------------------------------------------------------
// SnapshotFileReader (for tests / recovery)
// ---------------------------------------------------------------------------

/// Reads pages back from a snapshot file written by [`SnapshotFileWriter`].
///
/// Used during recovery (and in tests) to verify snapshot contents.
pub struct SnapshotFileReader {
    /// Raw file data.
    data: Vec<u8>,
    /// Size of each page in bytes.
    page_size: usize,
}

impl SnapshotFileReader {
    /// Open and read the entire snapshot file into memory.
    ///
    /// # Errors
    ///
    /// Returns [`CheckpointError::IoError`] if the file cannot be read.
    pub fn open(path: &Path) -> Result<Self, CheckpointError> {
        let data = std::fs::read(path)?;
        Ok(Self { data, page_size: 0 })
    }

    /// Set the page size used to parse the snapshot file.
    ///
    /// Must match the page size used when writing the snapshot.
    pub fn with_page_size(mut self, page_size: usize) -> Self {
        self.page_size = page_size;
        self
    }

    /// Iterate over pages in the snapshot file.
    ///
    /// Yields `(page_index, page_data)` tuples in the order they were written.
    ///
    /// # Panics
    ///
    /// Panics if `page_size` has not been set (is zero).
    pub fn pages(&self) -> SnapshotPageIterator<'_> {
        assert!(self.page_size > 0, "page_size must be set before iterating");
        SnapshotPageIterator {
            data: &self.data,
            page_size: self.page_size,
            offset: 0,
        }
    }

    /// Returns the total file size in bytes.
    pub fn file_size(&self) -> u64 {
        self.data.len() as u64
    }
}

/// Iterator over pages in a snapshot file.
pub struct SnapshotPageIterator<'a> {
    data: &'a [u8],
    page_size: usize,
    offset: usize,
}

impl<'a> Iterator for SnapshotPageIterator<'a> {
    type Item = (u64, &'a [u8]);

    fn next(&mut self) -> Option<Self::Item> {
        // Each entry: 8 bytes page_index header + page_size bytes of data.
        let entry_size = 8 + self.page_size;
        if self.offset + entry_size > self.data.len() {
            return None;
        }

        // Read page_index header.
        let header_bytes: [u8; 8] = self.data[self.offset..self.offset + 8]
            .try_into()
            .expect("header is exactly 8 bytes");
        let page_index = u64::from_le_bytes(header_bytes);

        // Read page data.
        let page_start = self.offset + 8;
        let page_end = page_start + self.page_size;
        let page_data = &self.data[page_start..page_end];

        self.offset += entry_size;
        Some((page_index, page_data))
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::address::{LogicalAddress, Offset, Page};

    const SECTOR_SIZE: usize = 512;
    const BUFFER_PAGES: usize = 4;
    const MUTABLE_FRAC: f64 = 0.5;

    fn make_allocator() -> HybridLogAllocator {
        HybridLogAllocator::new(BUFFER_PAGES, MUTABLE_FRAC, SECTOR_SIZE)
    }

    fn make_token(value: u128) -> CheckpointToken {
        CheckpointToken::new(value)
    }

    // -----------------------------------------------------------------------
    // SnapshotCheckpointContext — creation
    // -----------------------------------------------------------------------

    #[test]
    fn snapshot_context_empty_log() {
        let log = make_allocator();
        let token = make_token(1);
        let ctx = SnapshotCheckpointContext::begin(&log, &token).unwrap();

        assert_eq!(ctx.token, token);
        let zero = LogicalAddress::ZERO;
        assert_eq!(ctx.snapshot_tail, zero);
        assert_eq!(ctx.snapshot_head, zero);
        assert_eq!(ctx.begin_address, zero);
        assert_eq!(ctx.flushed_until, zero);
        assert_eq!(ctx.in_memory_page_count(), 0);
    }

    #[test]
    fn snapshot_context_with_data() {
        let log = make_allocator();
        log.try_allocate(64).unwrap();
        log.try_allocate(128).unwrap();

        let token = make_token(42);
        let ctx = SnapshotCheckpointContext::begin(&log, &token).unwrap();

        let expected_tail = LogicalAddress::new(Page(0), Offset(192));
        assert_eq!(ctx.snapshot_tail, expected_tail);
        assert_eq!(ctx.snapshot_head, LogicalAddress::ZERO);
        assert_eq!(ctx.begin_address, LogicalAddress::ZERO);
        // flushed_until starts at head for snapshot mode.
        assert_eq!(ctx.flushed_until, ctx.snapshot_head);
    }

    #[test]
    fn snapshot_context_captures_head() {
        let log = make_allocator();
        log.try_allocate(64).unwrap();
        let new_head = LogicalAddress::new(Page(0), Offset(32));
        log.try_advance_head(new_head);

        let token = make_token(7);
        let ctx = SnapshotCheckpointContext::begin(&log, &token).unwrap();

        assert_eq!(ctx.snapshot_head, new_head);
        assert_eq!(ctx.flushed_until, new_head);
    }

    #[test]
    fn snapshot_context_started_at_is_recent() {
        let log = make_allocator();
        let before = Instant::now();
        let ctx = SnapshotCheckpointContext::begin(&log, &make_token(1)).unwrap();
        let after = Instant::now();

        assert!(ctx.started_at >= before);
        assert!(ctx.started_at <= after);
    }

    #[test]
    fn snapshot_context_debug_format() {
        let log = make_allocator();
        let ctx = SnapshotCheckpointContext::begin(&log, &make_token(1)).unwrap();
        let dbg = format!("{ctx:?}");
        assert!(dbg.contains("SnapshotCheckpointContext"));
    }

    // -----------------------------------------------------------------------
    // SnapshotCheckpointContext — in_memory_page_count
    // -----------------------------------------------------------------------

    #[test]
    fn in_memory_page_count_empty() {
        let log = make_allocator();
        let ctx = SnapshotCheckpointContext::begin(&log, &make_token(1)).unwrap();
        assert_eq!(ctx.in_memory_page_count(), 0);
    }

    #[test]
    fn in_memory_page_count_single_page() {
        let log = make_allocator();
        log.try_allocate(64).unwrap();

        let ctx = SnapshotCheckpointContext::begin(&log, &make_token(1)).unwrap();
        assert_eq!(ctx.in_memory_page_count(), 1);
    }

    #[test]
    fn in_memory_page_count_multiple_pages() {
        let log = make_allocator();
        let page_size = log.page_size();

        // Fill first page and advance to second.
        log.try_allocate(page_size - 64).unwrap();
        log.advance_to_next_page().unwrap();
        // Allocate on second page.
        log.try_allocate(128).unwrap();

        let ctx = SnapshotCheckpointContext::begin(&log, &make_token(1)).unwrap();
        assert_eq!(ctx.in_memory_page_count(), 2);
    }

    // -----------------------------------------------------------------------
    // SnapshotCheckpointContext — recovery info generation
    // -----------------------------------------------------------------------

    #[test]
    fn recovery_info_for_empty_snapshot() {
        let log = make_allocator();
        let ctx = SnapshotCheckpointContext::begin(&log, &make_token(99)).unwrap();
        let info = ctx.into_recovery_info();

        assert_eq!(info.version, 1);
        assert_eq!(info.checkpoint_type, CheckpointType::Snapshot);
        assert!(info.use_snapshot_file);
        assert_eq!(info.begin_address, LogicalAddress::ZERO);
        assert_eq!(info.head_address, LogicalAddress::ZERO);
        assert_eq!(info.final_address, LogicalAddress::ZERO);
        assert_eq!(info.flushed_until_address, LogicalAddress::ZERO);
        assert_eq!(info.snapshot_start_address, LogicalAddress::ZERO);
        assert_eq!(info.snapshot_final_address, LogicalAddress::ZERO);
    }

    #[test]
    fn recovery_info_for_snapshot_with_data() {
        let log = make_allocator();
        log.try_allocate(64).unwrap();
        log.try_allocate(128).unwrap();

        let ctx = SnapshotCheckpointContext::begin(&log, &make_token(42)).unwrap();
        let info = ctx.into_recovery_info();

        let expected_tail = LogicalAddress::new(Page(0), Offset(192));
        assert_eq!(info.checkpoint_type, CheckpointType::Snapshot);
        assert!(info.use_snapshot_file);
        assert_eq!(info.final_address, expected_tail);
        assert_eq!(info.flushed_until_address, expected_tail);
        assert_eq!(info.snapshot_start_address, LogicalAddress::ZERO);
        assert_eq!(info.snapshot_final_address, expected_tail);
    }

    #[test]
    fn recovery_info_with_head_advanced() {
        let log = make_allocator();
        let page_size = log.page_size();

        // Fill several pages.
        log.try_allocate(page_size - 64).unwrap();
        log.advance_to_next_page().unwrap();
        log.try_allocate(128).unwrap();

        // Advance head to simulate eviction.
        let new_head = LogicalAddress::new(Page(1), Offset(0));
        log.try_advance_head(new_head);
        let new_begin = LogicalAddress::new(Page(0), Offset(256));
        log.try_advance_begin(new_begin);

        let ctx = SnapshotCheckpointContext::begin(&log, &make_token(7)).unwrap();
        let info = ctx.into_recovery_info();

        assert_eq!(info.head_address, new_head);
        assert_eq!(info.begin_address, new_begin);
        assert_eq!(info.snapshot_start_address, new_head);
    }

    #[test]
    fn recovery_info_serializable() {
        let log = make_allocator();
        log.try_allocate(64).unwrap();

        let ctx = SnapshotCheckpointContext::begin(&log, &make_token(1)).unwrap();
        let info = ctx.into_recovery_info();

        let json = serde_json::to_string(&info).unwrap();
        let back: LogRecoveryInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(info, back);
    }

    // -----------------------------------------------------------------------
    // SnapshotFileWriter — basic operations
    // -----------------------------------------------------------------------

    #[test]
    fn snapshot_file_writer_create() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.snapshot.log");
        let writer = SnapshotFileWriter::new(&path).unwrap();

        assert_eq!(writer.bytes_written(), 0);
        assert_eq!(writer.path(), path);
    }

    #[test]
    fn snapshot_file_writer_write_single_page() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.snapshot.log");
        let mut writer = SnapshotFileWriter::new(&path).unwrap();

        let page_data = vec![0xABu8; 4096];
        writer.write_page(0, &page_data).unwrap();

        let bytes = writer.finalize().unwrap();
        // 8 bytes header + 4096 bytes data.
        assert_eq!(bytes, 8 + 4096);

        // Verify file on disk.
        let file_size = std::fs::metadata(&path).unwrap().len();
        assert_eq!(file_size, 8 + 4096);
    }

    #[test]
    fn snapshot_file_writer_write_multiple_pages() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.snapshot.log");
        let mut writer = SnapshotFileWriter::new(&path).unwrap();

        let page_size = 512;
        for i in 0..4u64 {
            let page_data = vec![(i & 0xFF) as u8; page_size];
            writer.write_page(i, &page_data).unwrap();
        }

        let bytes = writer.finalize().unwrap();
        assert_eq!(bytes, 4 * (8 + page_size as u64));
    }

    #[test]
    fn snapshot_file_writer_finalize_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.snapshot.log");
        let writer = SnapshotFileWriter::new(&path).unwrap();

        let bytes = writer.finalize().unwrap();
        assert_eq!(bytes, 0);

        let file_size = std::fs::metadata(&path).unwrap().len();
        assert_eq!(file_size, 0);
    }

    // -----------------------------------------------------------------------
    // SnapshotFileWriter + SnapshotFileReader — round-trip
    // -----------------------------------------------------------------------

    #[test]
    fn snapshot_write_read_round_trip_single_page() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rt.snapshot.log");

        // Write.
        let page_size = 1024usize;
        let page_data: Vec<u8> = (0..page_size).map(|i| (i % 256) as u8).collect();
        {
            let mut writer = SnapshotFileWriter::new(&path).unwrap();
            writer.write_page(42, &page_data).unwrap();
            writer.finalize().unwrap();
        }

        // Read back.
        let reader = SnapshotFileReader::open(&path)
            .unwrap()
            .with_page_size(page_size);
        let pages: Vec<_> = reader.pages().collect();

        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0].0, 42);
        assert_eq!(pages[0].1, page_data.as_slice());
    }

    #[test]
    fn snapshot_write_read_round_trip_multiple_pages() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rt_multi.snapshot.log");

        let page_size = 512usize;
        let num_pages = 5u64;
        let page_contents: Vec<Vec<u8>> = (0..num_pages)
            .map(|i| vec![(i & 0xFF) as u8; page_size])
            .collect();

        // Write.
        {
            let mut writer = SnapshotFileWriter::new(&path).unwrap();
            for (i, data) in page_contents.iter().enumerate() {
                writer.write_page(i as u64, data).unwrap();
            }
            writer.finalize().unwrap();
        }

        // Read back.
        let reader = SnapshotFileReader::open(&path)
            .unwrap()
            .with_page_size(page_size);
        let pages: Vec<_> = reader.pages().collect();

        assert_eq!(pages.len(), num_pages as usize);
        for (i, (page_idx, data)) in pages.iter().enumerate() {
            assert_eq!(*page_idx, i as u64);
            assert_eq!(*data, page_contents[i].as_slice());
        }
    }

    #[test]
    fn snapshot_reader_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.snapshot.log");

        // Write an empty file.
        {
            let writer = SnapshotFileWriter::new(&path).unwrap();
            writer.finalize().unwrap();
        }

        let reader = SnapshotFileReader::open(&path).unwrap().with_page_size(512);
        let pages: Vec<_> = reader.pages().collect();
        assert!(pages.is_empty());
    }

    #[test]
    fn snapshot_reader_file_size() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("size.snapshot.log");

        let page_size = 256usize;
        {
            let mut writer = SnapshotFileWriter::new(&path).unwrap();
            writer.write_page(0, &vec![0u8; page_size]).unwrap();
            writer.write_page(1, &vec![0u8; page_size]).unwrap();
            writer.finalize().unwrap();
        }

        let reader = SnapshotFileReader::open(&path)
            .unwrap()
            .with_page_size(page_size);
        assert_eq!(reader.file_size(), 2 * (8 + page_size as u64));
    }

    // -----------------------------------------------------------------------
    // Snapshot tail semantics — no records after snapshot_tail
    // -----------------------------------------------------------------------

    #[test]
    fn snapshot_tail_excludes_later_records() {
        let log = make_allocator();
        log.try_allocate(64).unwrap();

        // Take snapshot — tail is at offset 64.
        let ctx = SnapshotCheckpointContext::begin(&log, &make_token(1)).unwrap();
        let snapshot_tail = ctx.snapshot_tail;
        assert_eq!(snapshot_tail, LogicalAddress::new(Page(0), Offset(64)));

        // Allocate more data after snapshot.
        log.try_allocate(128).unwrap();
        let current_tail = log.tail_address();

        // Current tail has advanced but snapshot_tail is frozen.
        assert!(current_tail > snapshot_tail);
        assert_eq!(ctx.snapshot_tail, snapshot_tail);
    }

    #[test]
    fn snapshot_tail_with_page_boundary() {
        let log = make_allocator();
        let page_size = log.page_size();

        // Fill first page exactly.
        log.try_allocate(page_size - 64).unwrap();
        log.advance_to_next_page().unwrap();

        // Take snapshot — includes first full page and start of second.
        let ctx = SnapshotCheckpointContext::begin(&log, &make_token(1)).unwrap();
        let snapshot_tail = ctx.snapshot_tail;

        // Add more data on page 1.
        log.try_allocate(256).unwrap();

        // Snapshot tail unchanged.
        assert_eq!(ctx.snapshot_tail, snapshot_tail);
        assert!(log.tail_address() > snapshot_tail);
    }

    // -----------------------------------------------------------------------
    // SnapshotFileWriter — error paths
    // -----------------------------------------------------------------------

    #[test]
    fn snapshot_file_writer_bad_path() {
        let result = SnapshotFileWriter::new(Path::new("/nonexistent/dir/test.snapshot.log"));
        assert!(result.is_err());
    }

    // -----------------------------------------------------------------------
    // Full snapshot lifecycle
    // -----------------------------------------------------------------------

    #[test]
    fn full_snapshot_lifecycle() {
        let log = make_allocator();
        let page_size = log.page_size() as usize;

        // Write data spanning two pages.
        log.try_allocate(page_size as u32 - 64).unwrap();
        log.advance_to_next_page().unwrap();
        log.try_allocate(128).unwrap();

        let token = make_token(0xDEAD_BEEF);

        // 1. Begin snapshot.
        let ctx = SnapshotCheckpointContext::begin(&log, &token).unwrap();
        assert_eq!(ctx.in_memory_page_count(), 2);

        // 2. Write snapshot file.
        let dir = tempfile::tempdir().unwrap();
        let snapshot_path = dir.path().join(format!("{token}.snapshot.log"));
        let mut file_writer = SnapshotFileWriter::new(&snapshot_path).unwrap();

        // Write pages from head to snapshot_tail.
        let head_page = ctx.snapshot_head.page().0;
        let tail_page = ctx.snapshot_tail.page().0;
        for page_num in head_page..=tail_page {
            let page = crate::address::Page(page_num);
            if let Some(frame) = log.page_table().get_frame(page) {
                file_writer
                    .write_page(page_num as u64, frame.as_slice())
                    .unwrap();
            }
        }

        let bytes = file_writer.finalize().unwrap();
        assert!(bytes > 0);

        // 3. Generate recovery info.
        let info = ctx.into_recovery_info();
        assert_eq!(info.checkpoint_type, CheckpointType::Snapshot);
        assert!(info.use_snapshot_file);

        // 4. Verify snapshot file.
        let reader = SnapshotFileReader::open(&snapshot_path)
            .unwrap()
            .with_page_size(page_size);
        let pages: Vec<_> = reader.pages().collect();
        assert_eq!(pages.len(), 2);
    }

    #[test]
    fn snapshot_lifecycle_empty_log() {
        let log = make_allocator();
        let token = make_token(1);

        let ctx = SnapshotCheckpointContext::begin(&log, &token).unwrap();
        assert_eq!(ctx.in_memory_page_count(), 0);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.snapshot.log");
        let file_writer = SnapshotFileWriter::new(&path).unwrap();
        let bytes = file_writer.finalize().unwrap();
        assert_eq!(bytes, 0);

        let info = ctx.into_recovery_info();
        assert_eq!(info.checkpoint_type, CheckpointType::Snapshot);
        assert!(info.use_snapshot_file);
        assert_eq!(info.final_address, LogicalAddress::ZERO);
    }

    #[test]
    fn snapshot_recovery_info_preserves_begin_address() {
        let log = make_allocator();
        log.try_allocate(64).unwrap();

        let new_begin = LogicalAddress::new(Page(0), Offset(32));
        log.try_advance_begin(new_begin);

        let ctx = SnapshotCheckpointContext::begin(&log, &make_token(1)).unwrap();
        let info = ctx.into_recovery_info();

        assert_eq!(info.begin_address, new_begin);
    }
}
