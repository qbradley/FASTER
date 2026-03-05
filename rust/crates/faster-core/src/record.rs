//! Record format and inline variable-length key-value storage.
//!
//! Defines the on-disk and in-memory record layout for FASTER's hybrid log.
//! Records store keys and values contiguously for optimal cache locality.
