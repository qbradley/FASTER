//! Hybrid log page structures and page table.
//!
//! The hybrid log is the central data structure in FASTER — a circular buffer of
//! pages that spans in-memory and on-disk regions. This module provides the
//! physical memory management layer:
//!
//! - [`PageState`] / [`AtomicPageState`] — page lifecycle state machine
//! - [`PageFrame`] — sector-aligned memory for a single page
//! - [`PageTable`] — circular buffer mapping logical pages to physical frames

pub mod log_allocator;
pub mod page;
pub mod regions;

pub use log_allocator::HybridLogAllocator;
pub use page::{AtomicPageState, PageFrame, PageState, PageTable};
pub use regions::{AddressInfo, AddressRegion};
