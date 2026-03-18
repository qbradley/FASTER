//! Hybrid log — the central data structure in FASTER.
//!
//! The hybrid log is a circular buffer of pages that seamlessly spans
//! in-memory and on-disk regions. It provides three address regions:
//!
//! ```text
//!  ┌──────────┐  ┌───────────────┐  ┌───────────────┐
//!  │  On-disk  │  │   Read-only   │  │    Mutable    │ ← new records
//!  │  (device) │  │  (in-memory)  │  │  (in-memory)  │   appended here
//!  └──────────┘  └───────────────┘  └───────────────┘
//!  ← head_addr    ← ro_addr          ← tail_addr →
//! ```
//!
//! - **Mutable region** — the hot tail where new records are allocated and
//!   in-place updates are allowed.
//! - **Read-only region** — recently sealed pages still in memory but no
//!   longer modifiable; updates create a copy at the tail.
//! - **On-disk region** — pages that have been flushed to the [`Device`](crate::Device)
//!   and evicted from memory; accessing them returns
//!   [`OperationStatus::Pending`](crate::status::OperationStatus::Pending).
//!
//! # Key Types
//!
//! - [`HybridLogAllocator`] — manages page allocation, region boundaries,
//!   and the circular page table.
//! - [`PageTable`] / [`PageFrame`] — circular buffer mapping logical pages
//!   to physical memory frames.
//! - [`PageState`] / [`AtomicPageState`] — page lifecycle state machine
//!   (Free → Allocated → Sealed → Flushing → Flushed → Evicted).
//! - [`PageFlusher`] — writes sealed pages to the storage device.
//! - [`PageEvictor`] — reclaims in-memory pages when the buffer is full.
//! - [`LogRecordReader`] / [`LogRecordWriter`] — read/write records within
//!   page frames.
//! - [`LogScanIterator`] — sequential scan over a range of log addresses.

#[deny(unsafe_code)]
pub mod eviction;
pub mod flush; // contains unsafe: I/O completion callbacks
pub mod log_allocator; // contains unsafe: raw pointer arithmetic
pub mod page; // contains unsafe: PageFrame raw memory management
pub mod record_ops; // contains unsafe: record accessor raw pointers
#[deny(unsafe_code)]
pub mod regions;
pub mod scan; // contains unsafe: raw pointer slice in scan iterator

pub use eviction::{EvictionPolicy, PageEvictor};
pub use flush::{FlushBatchResult, FlushError, FlushRequest, PageFlusher};
pub use log_allocator::{AllocError, HybridLogAllocator};
pub use page::{AtomicPageState, PageFrame, PageState, PageTable, PinnedPage};
pub use record_ops::{
    LogRecordReader, LogRecordWriter, MutableRecordAccessor, RecordAccessor, VersionChainIterator,
};
pub use regions::{AddressInfo, AddressRegion};
pub use scan::{LogScanIterator, ScanOptions, ScanRecord};
