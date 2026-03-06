//! Hybrid log page structures and page table.
//!
//! The hybrid log is the central data structure in FASTER — a circular buffer of
//! pages that spans in-memory and on-disk regions. This module provides the
//! physical memory management layer:
//!
//! - [`PageState`] / [`AtomicPageState`] — page lifecycle state machine
//! - [`PageFrame`] — sector-aligned memory for a single page
//! - [`PageTable`] — circular buffer mapping logical pages to physical frames

pub mod eviction;
pub mod flush;
pub mod log_allocator;
pub mod page;
pub mod record_ops;
pub mod regions;
pub mod scan;

pub use eviction::{EvictionPolicy, PageEvictor};
pub use flush::{FlushError, FlushRequest, PageFlusher};
pub use log_allocator::HybridLogAllocator;
pub use page::{AtomicPageState, PageFrame, PageState, PageTable};
pub use record_ops::{
    LogRecordReader, LogRecordWriter, MutableRecordAccessor, RecordAccessor, VersionChainIterator,
};
pub use regions::{AddressInfo, AddressRegion};
pub use scan::{LogScanIterator, ScanOptions, ScanRecord};
