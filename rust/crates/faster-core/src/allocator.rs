//! Arena allocator, aligned buffers, and overflow bucket pool.
//!
//! Provides memory allocation primitives used by the hash index and hybrid
//! log. All allocations are cache-line aligned to avoid false sharing.
