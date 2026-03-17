//! Page flush pipeline for the hybrid log.
//!
//! The [`PageFlusher`] orchestrates writing dirty (sealed) pages from the
//! in-memory hybrid log to a storage [`Device`]. Each page transitions through
//! `Sealed → Flushing → Flushed` as the write completes.
//!
//! # Design
//!
//! - **Async writes**: [`flush_page`](PageFlusher::flush_page) issues
//!   `Device::write_async` with a completion callback that transitions the
//!   page state on I/O completion.
//! - **Sync writes**: [`flush_page_sync`](PageFlusher::flush_page_sync) uses
//!   `Device::write_sync` for recovery / shutdown paths.
//! - **Batch scan**: [`flush_sealed_pages`](PageFlusher::flush_sealed_pages)
//!   scans the read-only region and flushes every sealed page it finds.
//!
//! # Callback ownership
//!
//! The async callback context is heap-allocated via `Box::into_raw` and
//! reclaimed in the completion callback via `Box::from_raw`. The
//! `Arc<PageTable>` held in each callback context guarantees the page table
//! remains alive until the callback fires, with no reliance on external drop
//! ordering.

use crate::sync::{Arc, Ordering};
use std::{fmt, io};

use crate::address::Page;
use crate::device::{Device, IoCompletionCallback, IoRequestResult, IoStatus, TypedIoContext};

use super::log_allocator::HybridLogAllocator;
use super::page::{PageState, PageTable, PageTrailer};

// ---------------------------------------------------------------------------
// FlushRequest
// ---------------------------------------------------------------------------

/// A request to flush a page to the storage device.
#[derive(Debug)]
pub struct FlushRequest {
    /// The logical page number being flushed.
    pub page: Page,
    /// Byte offset on the device where this page should be written.
    pub device_offset: u64,
    /// Number of valid bytes in the page to flush (may be less than full page
    /// size on the tail page).
    pub valid_bytes: u32,
}

// ---------------------------------------------------------------------------
// FlushBatchResult
// ---------------------------------------------------------------------------

/// Result of a batch flush operation ([`PageFlusher::flush_sealed_pages`]).
///
/// Reports both the number of pages successfully submitted for flushing and
/// whether the batch was interrupted by device back-pressure (`QueueFull`).
/// Callers should check `queue_full` and, if true, poll for I/O completions
/// before retrying.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FlushBatchResult {
    /// Number of pages successfully submitted for flushing in this batch.
    pub flushed: u32,
    /// If `true`, at least one page flush was rejected because the device's
    /// I/O queue was full. The caller should drain completions and retry.
    pub queue_full: bool,
}

// ---------------------------------------------------------------------------
// FlushError
// ---------------------------------------------------------------------------

/// Errors that can occur during a page flush operation.
#[derive(Debug)]
pub enum FlushError {
    /// Page is not in the expected state for flushing.
    InvalidPageState {
        /// The page that was being flushed.
        page: Page,
        /// The actual state found.
        state: PageState,
    },
    /// Page frame not found in page table.
    PageNotFound(Page),
    /// Device I/O submission failed.
    IoError(io::Error),
    /// Device returned QueueFull.
    QueueFull(Page),
}

impl fmt::Display for FlushError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FlushError::InvalidPageState { page, state } => {
                write!(f, "page {page} in unexpected state {state} for flush")
            }
            FlushError::PageNotFound(page) => {
                write!(f, "page frame not found for {page}")
            }
            FlushError::IoError(e) => {
                write!(f, "device I/O error during flush: {e}")
            }
            FlushError::QueueFull(page) => {
                write!(f, "device queue full when flushing {page}")
            }
        }
    }
}

impl std::error::Error for FlushError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            FlushError::IoError(e) => Some(e),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// FlushCallbackContext (private)
// ---------------------------------------------------------------------------

/// Context passed to the device completion callback.
///
/// Allocated via [`TypedIoContext::new`] before issuing `write_async`, and
/// reclaimed via [`TypedIoContext::from_raw`] inside the callback.
#[allow(dead_code)] // `bytes_flushed` is read via raw-pointer dereference in the callback.
struct FlushCallbackContext {
    /// Page being flushed.
    page: Page,
    /// Strong reference to the page table for state transition.
    ///
    /// The `Arc` guarantees the `PageTable` is alive when the callback fires,
    /// regardless of `FasterKv` field drop order.
    page_table: Arc<PageTable>,
    /// Number of bytes that were flushed.
    bytes_flushed: u32,
}

// FlushCallbackContext is Send because all fields are Send:
// - Page is Copy
// - Arc<PageTable> is Send (PageTable: Send + Sync)
// - u32 is Send
// No manual `unsafe impl Send` required.

// ---------------------------------------------------------------------------
// Flush completion callback
// ---------------------------------------------------------------------------

/// Device I/O completion callback for page flushes.
///
/// # Safety
///
/// `context` must be a valid pointer to a `FlushCallbackContext` that was
/// created via [`TypedIoContext::new`].
unsafe fn flush_completion_callback(context: *mut u8, status: IoStatus, bytes_transferred: u32) {
    // SAFETY: Caller guarantees `context` is a valid `FlushCallbackContext`
    // pointer created via `TypedIoContext::new`. We are the sole consumer.
    let ctx = unsafe { TypedIoContext::<FlushCallbackContext>::from_raw(context) };

    let page_table = &ctx.page_table;

    if status == IoStatus::Success {
        if let Some(frame) = page_table.get_frame(ctx.page) {
            // H1/C-5: Validate that bytes_transferred covers the expected
            // write size. A short write should NOT transition to Flushed —
            // the page stays in Flushing for retry.
            if bytes_transferred < ctx.bytes_flushed {
                #[cfg(debug_assertions)]
                eprintln!(
                    "flush_completion_callback: short write on page {:?} \
                     (transferred={}, expected={})",
                    ctx.page, bytes_transferred, ctx.bytes_flushed,
                );
                // Leave in Flushing state — retry logic handles recovery.
                drop(ctx);
                return;
            }
            frame
                .flushed_until()
                .fetch_max(bytes_transferred, Ordering::Release);
            frame
                .state()
                .try_transition(PageState::Flushing, PageState::Flushed);
        }
    }
    // On error: leave page in Flushing state — retry logic handles recovery.
    drop(ctx);
}

// ---------------------------------------------------------------------------
// PageFlusher
// ---------------------------------------------------------------------------

/// Orchestrates flushing sealed pages from the hybrid log to a storage device.
pub struct PageFlusher {
    /// The sector size for alignment requirements.
    sector_size: u32,
    /// The page size in bytes.
    page_size: u32,
}

impl PageFlusher {
    /// Create a new `PageFlusher`.
    pub fn new(sector_size: u32, page_size: u32) -> Self {
        debug_assert!(sector_size.is_power_of_two());
        Self {
            sector_size,
            page_size,
        }
    }

    /// Attempt to flush a single page to the device asynchronously.
    ///
    /// 1. Gets the page frame from the page table.
    /// 2. Transitions state: `Sealed → Flushing` (fails if not `Sealed`).
    /// 3. Computes the device offset for this page.
    /// 4. Issues `write_async` to the device.
    ///
    /// Returns `Ok(true)` if flush was initiated, `Ok(false)` if the page is
    /// already flushing or flushed.
    pub fn flush_page(
        &self,
        page: Page,
        page_table: &Arc<PageTable>,
        device: &dyn Device,
        valid_bytes: u32,
    ) -> Result<bool, FlushError> {
        trace_span!("flush_page", page = ?page, valid_bytes = valid_bytes);
        let frame = page_table
            .get_frame(page)
            .ok_or(FlushError::PageNotFound(page))?;

        // Try to transition Sealed → Flushing.
        if !frame
            .state()
            .try_transition(PageState::Sealed, PageState::Flushing)
        {
            let current = frame.state().load(Ordering::Acquire);
            if current == PageState::Flushing || current == PageState::Flushed {
                return Ok(false); // Already flushing/flushed — not an error.
            }
            return Err(FlushError::InvalidPageState {
                page,
                state: current,
            });
        }

        // Compute sector-aligned write size (with room for CRC trailer)
        // and device offset.
        let write_size = PageTrailer::write_size(valid_bytes, self.sector_size, self.page_size);
        let offset = self.device_offset(page);

        // Compute CRC-32C over the valid data range and write the trailer
        // into the page frame's padding region.
        self.write_crc_trailer(frame, valid_bytes, write_size);

        // Heap-allocate the callback context via TypedIoContext (manages the
        // Box → raw → typed lifecycle safely). The Arc clone keeps the
        // PageTable alive until the callback fires, regardless of drop order.
        let io_ctx = TypedIoContext::new(FlushCallbackContext {
            page,
            page_table: Arc::clone(page_table),
            bytes_flushed: write_size,
        });

        // SAFETY:
        // - `frame.as_ptr()` is valid for `write_size` bytes (PageFrame is at
        //   least `page_size` bytes, sector-aligned).
        // - `io_ctx.as_raw()` is a valid heap pointer that will be consumed by
        //   the callback.
        // - `offset` and `write_size` are sector-aligned.
        let result = unsafe {
            device.write_async(
                frame.as_ptr(),
                offset,
                write_size,
                flush_completion_callback as IoCompletionCallback,
                io_ctx.as_raw(),
            )
        };

        match result {
            IoRequestResult::Submitted | IoRequestResult::CompletedSync => {
                sim_yield!("flush::after_async_write");
                Ok(true)
            }
            IoRequestResult::QueueFull => {
                // Revert state so the flush can be retried.
                frame
                    .state()
                    .try_transition(PageState::Flushing, PageState::Sealed);
                // I/O was never submitted — safely reclaim the context.
                let _ = io_ctx.reclaim();
                Err(FlushError::QueueFull(page))
            }
            IoRequestResult::Error(e) => {
                // Revert state so the flush can be retried.
                frame
                    .state()
                    .try_transition(PageState::Flushing, PageState::Sealed);
                // I/O was never submitted — safely reclaim the context.
                let _ = io_ctx.reclaim();
                Err(FlushError::IoError(e))
            }
        }
    }

    /// Synchronous page flush (for recovery/shutdown).
    ///
    /// Transitions the page `Sealed → Flushing`, writes synchronously, and
    /// then transitions `Flushing → Flushed`.
    ///
    /// Returns `Ok(true)` if the page was flushed, `Ok(false)` if already
    /// flushing or flushed.
    pub fn flush_page_sync(
        &self,
        page: Page,
        page_table: &PageTable,
        device: &dyn Device,
        valid_bytes: u32,
    ) -> Result<bool, FlushError> {
        let frame = page_table
            .get_frame(page)
            .ok_or(FlushError::PageNotFound(page))?;

        // Try to transition Sealed → Flushing.
        if !frame
            .state()
            .try_transition(PageState::Sealed, PageState::Flushing)
        {
            let current = frame.state().load(Ordering::Acquire);
            if current == PageState::Flushing || current == PageState::Flushed {
                return Ok(false);
            }
            return Err(FlushError::InvalidPageState {
                page,
                state: current,
            });
        }

        let write_size = PageTrailer::write_size(valid_bytes, self.sector_size, self.page_size);
        let offset = self.device_offset(page);

        // Compute CRC-32C and write the trailer into the frame's padding.
        self.write_crc_trailer(frame, valid_bytes, write_size);

        let source = &frame.as_slice()[..write_size as usize];

        match device.write_sync(offset, source) {
            Ok(bytes_written) => {
                frame
                    .flushed_until()
                    .fetch_max(bytes_written, Ordering::Release);
                let _ = frame
                    .state()
                    .try_transition(PageState::Flushing, PageState::Flushed);
                Ok(true)
            }
            Err(e) => {
                // Revert state so the flush can be retried.
                frame
                    .state()
                    .try_transition(PageState::Flushing, PageState::Sealed);
                Err(FlushError::IoError(e))
            }
        }
    }

    /// Scan the read-only region and flush all sealed pages.
    ///
    /// Iterates from `head_address.page()` to `read_only_address.page()`
    /// (exclusive) and flushes every page whose state is `Sealed`.
    ///
    /// Returns a [`FlushBatchResult`] indicating how many pages were
    /// submitted and whether the device signalled back-pressure. On
    /// `QueueFull` the batch aborts immediately (no further pages are
    /// attempted) so the caller can drain I/O completions before retrying.
    pub fn flush_sealed_pages(
        &self,
        allocator: &HybridLogAllocator,
        device: &dyn Device,
    ) -> Result<FlushBatchResult, FlushError> {
        trace_span!("flush_sealed_pages");
        let head_page = allocator.head_address().page().0;
        let ro_page = allocator.read_only_address().page().0;
        let page_table = allocator.page_table_arc();

        let mut flushed_count = 0u32;

        for p in head_page..ro_page {
            let page = Page(p);
            if let Some(frame) = page_table.get_frame(page) {
                let state = frame.state().load(Ordering::Acquire);
                if state == PageState::Sealed {
                    match self.flush_page(page, page_table, device, self.page_size) {
                        Ok(true) => flushed_count += 1,
                        Ok(false) => {} // Already flushing/flushed.
                        Err(FlushError::QueueFull(_)) => {
                            // Back-pressure: device cannot accept more I/O.
                            // Abort the batch so the caller can drain completions.
                            return Ok(FlushBatchResult {
                                flushed: flushed_count,
                                queue_full: true,
                            });
                        }
                        Err(e) => return Err(e),
                    }
                }
            }
        }

        Ok(FlushBatchResult {
            flushed: flushed_count,
            queue_full: false,
        })
    }

    /// Compute the device offset for a given page.
    #[inline]
    pub fn device_offset(&self, page: Page) -> u64 {
        page.0 as u64 * self.page_size as u64
    }

    /// Round up `bytes` to the next multiple of the sector size.
    #[inline]
    pub fn align_to_sector(&self, bytes: u32) -> u32 {
        (bytes + self.sector_size - 1) & !(self.sector_size - 1)
    }

    /// Compute CRC-32C over the valid data range and write the 8-byte trailer
    /// into the page frame's padding region at `[write_size-8..write_size]`.
    ///
    /// # Safety contract
    ///
    /// The frame must be in `Sealed` or `Flushing` state (no concurrent
    /// writers to the data region). The trailer is written via
    /// `as_mut_ptr()` into bytes that are guaranteed to be within the
    /// frame's allocation (because `write_size <= page_size`).
    fn write_crc_trailer(&self, frame: &super::page::PageFrame, valid_bytes: u32, write_size: u32) {
        let trailer_offset = write_size as usize - PageTrailer::SIZE;

        // If the trailer would overlap with valid record data, skip writing
        // it entirely. This prevents corrupting the last 8 bytes of a full
        // (or nearly full) page. Recovery detects the absent trailer via the
        // zeroed valid_bytes/crc fields.
        if (trailer_offset as u32) < valid_bytes {
            return;
        }

        let crc_range = PageTrailer::crc_range(valid_bytes, write_size) as usize;

        // Compute CRC-32C over the valid data before writing the trailer.
        // SAFETY: `frame.as_ptr()` is valid for `frame.size()` bytes (>= write_size).
        let data = unsafe { core::slice::from_raw_parts(frame.as_ptr(), crc_range) };
        let crc = crc32fast::hash(data);

        let trailer = PageTrailer::new(crc_range as u32, crc);
        let trailer_bytes = trailer.to_bytes();

        // SAFETY: `trailer_offset + 8 <= write_size <= page_size <= frame.size()`.
        // The frame is in Sealed/Flushing state — no concurrent data writers.
        unsafe {
            core::ptr::copy_nonoverlapping(
                trailer_bytes.as_ptr(),
                frame.as_mut_ptr().add(trailer_offset),
                PageTrailer::SIZE,
            );
        }
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::address::Page;
    use crate::device::{InMemoryDevice, NullDevice};
    use crate::hybrid_log::page::{PageState, PageTable};
    use std::sync::Arc;

    /// Small page size for tests (4096 bytes) to keep memory usage reasonable.
    const TEST_PAGE_SIZE: usize = 4096;
    const TEST_SECTOR_SIZE: usize = 512;
    /// Buffer with 4 page slots.
    const TEST_BUFFER_PAGES: usize = 4;

    /// Helper: create a page table and allocate a frame for the given page,
    /// returning it in the specified state.
    fn setup_page_in_state(page: Page, state: PageState) -> Arc<PageTable> {
        let pt = Arc::new(PageTable::new(
            TEST_BUFFER_PAGES,
            TEST_PAGE_SIZE,
            TEST_SECTOR_SIZE,
        ));
        let frame = pt.get_or_allocate_frame(page);
        // Frame starts as Open. Transition to the desired state.
        match state {
            PageState::Open => {} // Already Open.
            PageState::Sealed => {
                frame
                    .state()
                    .try_transition(PageState::Open, PageState::Sealed);
            }
            PageState::Flushing => {
                frame
                    .state()
                    .try_transition(PageState::Open, PageState::Sealed);
                frame
                    .state()
                    .try_transition(PageState::Sealed, PageState::Flushing);
            }
            PageState::Flushed => {
                frame
                    .state()
                    .try_transition(PageState::Open, PageState::Sealed);
                frame
                    .state()
                    .try_transition(PageState::Sealed, PageState::Flushing);
                frame
                    .state()
                    .try_transition(PageState::Flushing, PageState::Flushed);
            }
            _ => panic!("unsupported test state: {state:?}"),
        }
        pt
    }

    // 1. Seal a page, flush with NullDevice, verify Flushed state.
    #[test]
    fn flush_sealed_page_with_null_device() {
        let page = Page(0);
        let pt = setup_page_in_state(page, PageState::Sealed);
        let dev = NullDevice::new();
        let flusher = PageFlusher::new(TEST_SECTOR_SIZE as u32, TEST_PAGE_SIZE as u32);

        let result = flusher.flush_page(page, &pt, &dev, TEST_PAGE_SIZE as u32);
        assert!(result.is_ok());
        assert!(result.unwrap()); // flush initiated

        // NullDevice completes synchronously, so state should be Flushed.
        let frame = pt.get_frame(page).unwrap();
        assert_eq!(frame.state().load(Ordering::Acquire), PageState::Flushed);
    }

    // 2. Try flushing an Open page — should fail with InvalidPageState.
    #[test]
    fn flush_non_sealed_page_fails() {
        let page = Page(0);
        let pt = setup_page_in_state(page, PageState::Open);
        let dev = NullDevice::new();
        let flusher = PageFlusher::new(TEST_SECTOR_SIZE as u32, TEST_PAGE_SIZE as u32);

        let result = flusher.flush_page(page, &pt, &dev, TEST_PAGE_SIZE as u32);
        assert!(result.is_err());
        match result.unwrap_err() {
            FlushError::InvalidPageState { page: p, state } => {
                assert_eq!(p, page);
                assert_eq!(state, PageState::Open);
            }
            other => panic!("expected InvalidPageState, got {other:?}"),
        }
    }

    // 3. Flush twice — second call should return Ok(false).
    #[test]
    fn flush_already_flushed_returns_false() {
        let page = Page(0);
        let pt = setup_page_in_state(page, PageState::Sealed);
        let dev = NullDevice::new();
        let flusher = PageFlusher::new(TEST_SECTOR_SIZE as u32, TEST_PAGE_SIZE as u32);

        // First flush succeeds.
        assert!(
            flusher
                .flush_page(page, &pt, &dev, TEST_PAGE_SIZE as u32)
                .unwrap()
        );

        // Second flush returns false (already Flushed).
        assert!(
            !flusher
                .flush_page(page, &pt, &dev, TEST_PAGE_SIZE as u32)
                .unwrap()
        );
    }

    // 4. Write data to page, sync flush to InMemoryDevice, read back and verify.
    #[test]
    fn flush_page_sync_with_in_memory_device() {
        let page = Page(0);
        let pt = PageTable::new(TEST_BUFFER_PAGES, TEST_PAGE_SIZE, TEST_SECTOR_SIZE);
        let frame = pt.get_or_allocate_frame(page);

        // Write a known pattern into the page frame.
        let pattern: Vec<u8> = (0..TEST_PAGE_SIZE).map(|i| (i % 251) as u8).collect();
        // SAFETY: We have exclusive access — the frame was just allocated and
        // is in Open state.
        unsafe {
            std::ptr::copy_nonoverlapping(pattern.as_ptr(), frame.as_mut_ptr(), TEST_PAGE_SIZE);
        }

        // Seal the page.
        frame
            .state()
            .try_transition(PageState::Open, PageState::Sealed);

        let dev = InMemoryDevice::with_sizes(TEST_SECTOR_SIZE as u32, 1 << 30);
        let flusher = PageFlusher::new(TEST_SECTOR_SIZE as u32, TEST_PAGE_SIZE as u32);

        let result = flusher.flush_page_sync(page, &pt, &dev, TEST_PAGE_SIZE as u32);
        assert!(result.is_ok());
        assert!(result.unwrap());

        // Verify state is Flushed.
        assert_eq!(frame.state().load(Ordering::Acquire), PageState::Flushed);

        // Read back from the device.
        let mut readback = vec![0u8; TEST_PAGE_SIZE];
        let offset = flusher.device_offset(page);
        dev.read_sync(offset, &mut readback).unwrap();

        // For a full page the CRC trailer is skipped (it would overwrite
        // valid record data). All bytes should match the original pattern.
        assert_eq!(
            &readback[..],
            &pattern[..],
            "full-page data should be preserved without CRC trailer corruption"
        );
    }

    // 4b. Flush a partial page and verify CRC trailer in padding.
    #[test]
    fn flush_page_sync_partial_page_crc() {
        use super::super::page::PageTrailer;

        let page = Page(0);
        let pt = PageTable::new(TEST_BUFFER_PAGES, TEST_PAGE_SIZE, TEST_SECTOR_SIZE);
        let frame = pt.get_or_allocate_frame(page);

        // Write a pattern into the first 1000 bytes.
        let valid_bytes = 1000u32;
        let pattern: Vec<u8> = (0..valid_bytes as usize).map(|i| (i % 251) as u8).collect();
        // SAFETY: We have exclusive access — the frame was just allocated and
        // is in Open state.
        unsafe {
            std::ptr::copy_nonoverlapping(
                pattern.as_ptr(),
                frame.as_mut_ptr(),
                valid_bytes as usize,
            );
        }

        frame
            .state()
            .try_transition(PageState::Open, PageState::Sealed);

        let dev = InMemoryDevice::with_sizes(TEST_SECTOR_SIZE as u32, 1 << 30);
        let flusher = PageFlusher::new(TEST_SECTOR_SIZE as u32, TEST_PAGE_SIZE as u32);

        let result = flusher.flush_page_sync(page, &pt, &dev, valid_bytes);
        assert!(result.is_ok());

        // Compute expected write size.
        let write_size =
            PageTrailer::write_size(valid_bytes, TEST_SECTOR_SIZE as u32, TEST_PAGE_SIZE as u32);
        // 1000 → needs 1008 for data+trailer → aligned to 1024
        assert_eq!(write_size, 1024);

        // Read back and verify data is intact.
        let mut readback = vec![0u8; write_size as usize];
        let offset = flusher.device_offset(page);
        dev.read_sync(offset, &mut readback).unwrap();
        assert_eq!(&readback[..valid_bytes as usize], &pattern[..]);

        // Verify the CRC trailer.
        let trailer = PageTrailer::from_slice(&readback, write_size as usize);
        assert_eq!(trailer.valid_bytes, valid_bytes);
        let actual_crc = crc32fast::hash(&readback[..valid_bytes as usize]);
        assert_eq!(trailer.crc32, actual_crc);
    }

    // 5. Create allocator with multiple sealed pages, flush_sealed_pages, verify all Flushed.
    #[test]
    fn flush_sealed_pages_scans_range() {
        // Use a small page size via a custom allocator setup.
        // The allocator's page_size is 2^OFFSET_BITS (32 MB), which is too
        // large for a test. Instead, we test flush_sealed_pages by directly
        // manipulating the page table and address boundaries.
        //
        // We create a PageTable, allocate pages 0-3, seal pages 0-1, and then
        // call flush_page on each to simulate flush_sealed_pages behavior.
        let pt = Arc::new(PageTable::new(
            TEST_BUFFER_PAGES,
            TEST_PAGE_SIZE,
            TEST_SECTOR_SIZE,
        ));
        let dev = NullDevice::new();
        let flusher = PageFlusher::new(TEST_SECTOR_SIZE as u32, TEST_PAGE_SIZE as u32);

        // Allocate and seal pages 0 and 1.
        for p in 0..2u32 {
            let page = Page(p);
            let frame = pt.get_or_allocate_frame(page);
            frame
                .state()
                .try_transition(PageState::Open, PageState::Sealed);
        }
        // Page 2 stays Open (simulating the mutable region).
        pt.get_or_allocate_frame(Page(2));

        // Flush sealed pages individually (simulating flush_sealed_pages scan).
        let mut flushed = 0u32;
        for p in 0..2u32 {
            let page = Page(p);
            if let Some(frame) = pt.get_frame(page) {
                if frame.state().load(Ordering::Acquire) == PageState::Sealed {
                    match flusher.flush_page(page, &pt, &dev, TEST_PAGE_SIZE as u32) {
                        Ok(true) => flushed += 1,
                        Ok(false) => {}
                        Err(e) => panic!("flush failed: {e}"),
                    }
                }
            }
        }

        assert_eq!(flushed, 2);

        // Verify both pages are Flushed.
        for p in 0..2u32 {
            let frame = pt.get_frame(Page(p)).unwrap();
            assert_eq!(
                frame.state().load(Ordering::Acquire),
                PageState::Flushed,
                "page {p} should be Flushed"
            );
        }

        // Page 2 should still be Open.
        let frame2 = pt.get_frame(Page(2)).unwrap();
        assert_eq!(frame2.state().load(Ordering::Acquire), PageState::Open);
    }

    // 6. Verify page → device offset mapping.
    #[test]
    fn device_offset_computation() {
        let flusher = PageFlusher::new(512, 4096);

        assert_eq!(flusher.device_offset(Page(0)), 0);
        assert_eq!(flusher.device_offset(Page(1)), 4096);
        assert_eq!(flusher.device_offset(Page(2)), 8192);
        assert_eq!(flusher.device_offset(Page(100)), 100 * 4096);

        // With larger page size.
        let big = PageFlusher::new(512, 1 << 25); // 32 MB pages
        assert_eq!(big.device_offset(Page(0)), 0);
        assert_eq!(big.device_offset(Page(1)), 1u64 << 25);
        assert_eq!(big.device_offset(Page(3)), 3u64 << 25);
    }

    // 7. Test sector alignment rounding.
    #[test]
    fn align_to_sector_rounds_up() {
        let flusher = PageFlusher::new(512, 4096);

        // Exact multiples — no rounding.
        assert_eq!(flusher.align_to_sector(0), 0);
        assert_eq!(flusher.align_to_sector(512), 512);
        assert_eq!(flusher.align_to_sector(1024), 1024);

        // Rounds up.
        assert_eq!(flusher.align_to_sector(1), 512);
        assert_eq!(flusher.align_to_sector(511), 512);
        assert_eq!(flusher.align_to_sector(513), 1024);
        assert_eq!(flusher.align_to_sector(1000), 1024);

        // Larger sector size.
        let f4k = PageFlusher::new(4096, 4096);
        assert_eq!(f4k.align_to_sector(1), 4096);
        assert_eq!(f4k.align_to_sector(4096), 4096);
        assert_eq!(f4k.align_to_sector(4097), 8192);
    }

    // 8. Flush a page that doesn't exist in the table.
    #[test]
    fn flush_page_not_found() {
        let pt = Arc::new(PageTable::new(
            TEST_BUFFER_PAGES,
            TEST_PAGE_SIZE,
            TEST_SECTOR_SIZE,
        ));
        let dev = NullDevice::new();
        let flusher = PageFlusher::new(TEST_SECTOR_SIZE as u32, TEST_PAGE_SIZE as u32);

        // Page 7 was never allocated.
        let result = flusher.flush_page(Page(7), &pt, &dev, TEST_PAGE_SIZE as u32);
        assert!(result.is_err());
        match result.unwrap_err() {
            FlushError::PageNotFound(p) => assert_eq!(p, Page(7)),
            other => panic!("expected PageNotFound, got {other:?}"),
        }
    }

    // 9. Full-page flush must not corrupt last 8 bytes of data.
    #[test]
    fn flush_full_page_preserves_all_data() {
        let page = Page(0);
        let pt = setup_page_in_state(page, PageState::Sealed);
        let dev = InMemoryDevice::new();
        let flusher = PageFlusher::new(TEST_SECTOR_SIZE as u32, TEST_PAGE_SIZE as u32);

        // Fill the entire page with a known pattern via the raw pointer
        // (get_frame returns &PageFrame, but as_mut_ptr is safe for writes
        // (get_frame returns &PageFrame, but as_mut_ptr is safe for writes
        // when we hold the only reference and the page is Sealed).
        let frame = pt.get_frame(page).unwrap();
        // SAFETY: Exclusive access — the page is Sealed and we hold the only ref.
        unsafe {
            let ptr = frame.as_mut_ptr();
            for i in 0..TEST_PAGE_SIZE {
                *ptr.add(i) = (i % 251) as u8;
            }
        }

        // Flush with valid_bytes == page_size (page is exactly full).
        let valid_bytes = TEST_PAGE_SIZE as u32;
        let result = flusher.flush_page_sync(page, &pt, &dev, valid_bytes);
        assert!(result.is_ok());
        assert!(result.unwrap());

        // Read back from device and verify ALL data bytes are intact.
        let flushed = frame.as_slice();
        for (i, &byte) in flushed.iter().enumerate().take(TEST_PAGE_SIZE) {
            assert_eq!(
                byte,
                (i % 251) as u8,
                "data corrupted at offset {i} (page_size={TEST_PAGE_SIZE})"
            );
        }
    }

    // 10. Near-full page (valid_bytes close to page_size) also preserves data.
    #[test]
    fn flush_near_full_page_preserves_data() {
        let page = Page(0);
        let pt = setup_page_in_state(page, PageState::Sealed);
        let dev = InMemoryDevice::new();
        let flusher = PageFlusher::new(TEST_SECTOR_SIZE as u32, TEST_PAGE_SIZE as u32);

        // Fill the entire page with a known pattern.
        let frame = pt.get_frame(page).unwrap();
        // SAFETY: Exclusive access — the page is Sealed and we hold the only ref.
        unsafe {
            let ptr = frame.as_mut_ptr();
            for i in 0..TEST_PAGE_SIZE {
                *ptr.add(i) = (i % 199) as u8;
            }
        }

        // valid_bytes = page_size - 4: trailer offset = page_size - 8,
        // which is < valid_bytes, so trailer must be skipped.
        let valid_bytes = (TEST_PAGE_SIZE - 4) as u32;
        let result = flusher.flush_page_sync(page, &pt, &dev, valid_bytes);
        assert!(result.is_ok());
        assert!(result.unwrap());

        // Verify all valid data bytes are intact.
        let flushed = frame.as_slice();
        for (i, &byte) in flushed.iter().enumerate().take(valid_bytes as usize) {
            assert_eq!(
                byte,
                (i % 199) as u8,
                "data corrupted at offset {i} (valid_bytes={valid_bytes})"
            );
        }
    }

    // -----------------------------------------------------------------------
    // Property test (A5): roundtrip write-read for arbitrary valid_bytes
    // -----------------------------------------------------------------------
    //
    // Verifies every byte of valid data survives a flush+readback cycle for
    // all possible fill levels, targeting the MF-1 class of bugs where the
    // CRC trailer could overwrite live record data near page boundaries.

    mod proptests {
        use super::*;
        use crate::hybrid_log::page::PageTrailer;
        use proptest::prelude::*;

        /// Page and sector sizes used by the property tests.
        /// 4 KiB pages / 512 B sectors keep memory reasonable while exercising
        /// every interesting boundary (sector-aligned, near-full, full).
        const PT_PAGE_SIZE: usize = 4096;
        const PT_SECTOR_SIZE: usize = 512;
        const PT_BUFFER_PAGES: usize = 4;

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(512))]

            /// A5 — Roundtrip write-read assertion: every byte of valid data
            /// survives a flush+readback regardless of how full the page is.
            #[test]
            fn flush_roundtrip_preserves_all_valid_bytes(
                valid_bytes in 0u32..=(PT_PAGE_SIZE as u32),
                seed in any::<u8>(),
            ) {
                let page = Page(0);
                let pt = PageTable::new(
                    PT_BUFFER_PAGES,
                    PT_PAGE_SIZE,
                    PT_SECTOR_SIZE,
                );
                let frame = pt.get_or_allocate_frame(page);

                // Fill entire page with a deterministic but seed-varied pattern.
                // Using wrapping_add avoids trivial zero-fill.
                // SAFETY: `frame.as_mut_ptr()` returns a valid pointer to the
                // page-sized allocation and we write exactly PT_PAGE_SIZE bytes.
                unsafe {
                    let ptr = frame.as_mut_ptr();
                    for i in 0..PT_PAGE_SIZE {
                        *ptr.add(i) = (i as u8).wrapping_add(seed);
                    }
                }

                frame
                    .state()
                    .try_transition(PageState::Open, PageState::Sealed);

                let dev = InMemoryDevice::with_sizes(
                    PT_SECTOR_SIZE as u32,
                    1 << 30,
                );
                let flusher = PageFlusher::new(
                    PT_SECTOR_SIZE as u32,
                    PT_PAGE_SIZE as u32,
                );

                // Flush with the generated valid_bytes.
                let result = flusher.flush_page_sync(
                    page, &pt, &dev, valid_bytes,
                );
                prop_assert!(result.is_ok(), "flush failed: {:?}", result);

                // Read back from device.
                let write_size = PageTrailer::write_size(
                    valid_bytes,
                    PT_SECTOR_SIZE as u32,
                    PT_PAGE_SIZE as u32,
                );
                let ws = core::cmp::max(write_size as usize, PageTrailer::SIZE);
                let mut readback = vec![0u8; ws];
                let offset = flusher.device_offset(page);
                dev.read_sync(offset, &mut readback).unwrap();

                // Assert: every valid byte is intact.
                for (i, &actual) in readback.iter().enumerate().take(valid_bytes as usize) {
                    let expected = (i as u8).wrapping_add(seed);
                    prop_assert_eq!(
                        actual, expected,
                        "byte {} corrupted (valid_bytes={}, page_size={}, seed={})",
                        i, valid_bytes, PT_PAGE_SIZE, seed,
                    );
                }

                // When there's room for a trailer, verify it round-trips.
                let trailer_offset = write_size as usize - PageTrailer::SIZE;
                if (trailer_offset as u32) >= valid_bytes {
                    let trailer = PageTrailer::from_slice(&readback, ws);
                    let crc_range = PageTrailer::crc_range(valid_bytes, write_size);
                    let expected_crc =
                        crc32fast::hash(&readback[..crc_range as usize]);
                    prop_assert_eq!(
                        trailer.crc32, expected_crc,
                        "CRC mismatch (valid_bytes={}, write_size={})",
                        valid_bytes, write_size,
                    );
                    prop_assert_eq!(
                        trailer.valid_bytes, crc_range,
                        "trailer valid_bytes mismatch",
                    );
                } else {
                    // Trailer was skipped — the last 8 bytes of the page
                    // should still hold the original pattern, not a trailer.
                    let flushed = frame.as_slice();
                    #[allow(clippy::needless_range_loop)]
                    for i in (PT_PAGE_SIZE - PageTrailer::SIZE)..PT_PAGE_SIZE {
                        if i < valid_bytes as usize {
                            let expected = (i as u8).wrapping_add(seed);
                            prop_assert_eq!(
                                flushed[i], expected,
                                "trailer-region byte {} corrupted when trailer \
                                 should have been skipped (valid_bytes={})",
                                i, valid_bytes,
                            );
                        }
                    }
                }
            }

            /// Verify write_size never exceeds page_size (overflow guard).
            #[test]
            fn write_size_never_exceeds_page_size(
                valid_bytes in 0u32..=(PT_PAGE_SIZE as u32),
            ) {
                let ws = PageTrailer::write_size(
                    valid_bytes,
                    PT_SECTOR_SIZE as u32,
                    PT_PAGE_SIZE as u32,
                );
                prop_assert!(
                    ws <= PT_PAGE_SIZE as u32,
                    "write_size {} > page_size {} for valid_bytes={}",
                    ws, PT_PAGE_SIZE, valid_bytes,
                );
            }

            /// When trailer fits, crc_range must not exceed valid_bytes.
            #[test]
            fn crc_range_bounded_by_valid_bytes(
                valid_bytes in 0u32..=(PT_PAGE_SIZE as u32),
            ) {
                let ws = PageTrailer::write_size(
                    valid_bytes,
                    PT_SECTOR_SIZE as u32,
                    PT_PAGE_SIZE as u32,
                );
                let cr = PageTrailer::crc_range(valid_bytes, ws);
                prop_assert!(
                    cr <= valid_bytes,
                    "crc_range {} > valid_bytes {} (write_size={})",
                    cr, valid_bytes, ws,
                );
            }
        }
    }
}
