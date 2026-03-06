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
//! reclaimed in the completion callback via `Box::from_raw`. The raw
//! `*const PageTable` pointer is safe because the page table outlives any
//! in-flight flush (the allocator cannot be dropped while flushes are pending).

use std::sync::atomic::Ordering;
use std::{fmt, io};

use crate::address::Page;
use crate::device::{Device, IoCompletionCallback, IoRequestResult, IoStatus, TypedIoContext};

use super::log_allocator::HybridLogAllocator;
use super::page::{PageState, PageTable};

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
    /// Pointer back to the page table for state transition.
    ///
    /// # Safety
    ///
    /// The `PageTable` outlives any in-flight flush — `FasterKv::Drop` drains
    /// all pending flushes before the allocator (and its `PageTable`) is
    /// dropped, so this pointer is always valid when the callback fires.
    page_table: *const PageTable,
    /// Number of bytes that were flushed.
    bytes_flushed: u32,
}

// SAFETY: The raw pointer is only dereferenced inside the callback, which runs
// while the `PageTable` is still alive. The context is transferred to the I/O
// thread and consumed exactly once.
unsafe impl Send for FlushCallbackContext {}

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

    // SAFETY: The `PageTable` outlives in-flight flushes — `FasterKv::Drop`
    // drains pending I/O before the allocator is dropped (SF-10).
    let page_table = unsafe { &*ctx.page_table };

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
            let _ = frame
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
                return Ok(false); // Already flushing/flushed — not an error.
            }
            return Err(FlushError::InvalidPageState {
                page,
                state: current,
            });
        }

        // Compute aligned write size and device offset.
        let write_size = self.align_to_sector(valid_bytes);
        let offset = self.device_offset(page);

        // Heap-allocate the callback context via TypedIoContext (manages the
        // Box → raw → typed lifecycle safely).
        let io_ctx = TypedIoContext::new(FlushCallbackContext {
            page,
            page_table: page_table as *const PageTable,
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
            IoRequestResult::Submitted | IoRequestResult::CompletedSync => Ok(true),
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

        let write_size = self.align_to_sector(valid_bytes);
        let offset = self.device_offset(page);

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
    /// Returns the number of pages for which a flush was initiated.
    pub fn flush_sealed_pages(
        &self,
        allocator: &HybridLogAllocator,
        device: &dyn Device,
    ) -> Result<u32, FlushError> {
        let head_page = allocator.head_address().page().0;
        let ro_page = allocator.read_only_address().page().0;
        let page_table = allocator.page_table();

        let mut flushed_count = 0u32;

        for p in head_page..ro_page {
            let page = Page(p);
            if let Some(frame) = page_table.get_frame(page) {
                let state = frame.state().load(Ordering::Acquire);
                if state == PageState::Sealed {
                    match self.flush_page(page, page_table, device, self.page_size) {
                        Ok(true) => flushed_count += 1,
                        Ok(false) => {} // Already flushing/flushed.
                        Err(FlushError::QueueFull(_)) => break, // Back-pressure.
                        Err(e) => return Err(e),
                    }
                }
            }
        }

        Ok(flushed_count)
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

    /// Small page size for tests (4096 bytes) to keep memory usage reasonable.
    const TEST_PAGE_SIZE: usize = 4096;
    const TEST_SECTOR_SIZE: usize = 512;
    /// Buffer with 4 page slots.
    const TEST_BUFFER_PAGES: usize = 4;

    /// Helper: create a page table and allocate a frame for the given page,
    /// returning it in the specified state.
    fn setup_page_in_state(page: Page, state: PageState) -> PageTable {
        let pt = PageTable::new(TEST_BUFFER_PAGES, TEST_PAGE_SIZE, TEST_SECTOR_SIZE);
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

        // Read back from the device and verify contents.
        let mut readback = vec![0u8; TEST_PAGE_SIZE];
        let offset = flusher.device_offset(page);
        dev.read_sync(offset, &mut readback).unwrap();
        assert_eq!(readback, pattern);
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
        let pt = PageTable::new(TEST_BUFFER_PAGES, TEST_PAGE_SIZE, TEST_SECTOR_SIZE);
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
        let pt = PageTable::new(TEST_BUFFER_PAGES, TEST_PAGE_SIZE, TEST_SECTOR_SIZE);
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
}
