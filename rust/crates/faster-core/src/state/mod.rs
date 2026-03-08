//! Epoch-Protected Version Scheme (EPVS) — unified atomic state.
//!
//! Packs `(phase: u8, version: u56)` into a single `AtomicU64` CAS word,
//! replacing the separate `AtomicU8 phase` + `AtomicU32 version` fields.
//! This eliminates torn reads and race windows during checkpoint/grow
//! coordination, matching the C# `SystemState` / Tsavorite layout.
//!
//! # Key Types
//!
//! | Type | Purpose |
//! |------|---------|
//! | [`Phase`] | Unified phase enum (checkpoint + grow phases) |
//! | [`SystemState`] | Packed `(phase, version)` value type |
//! | [`AtomicSystemState`] | Thread-safe atomic container with CAS |

#[deny(unsafe_code)]
mod phase;
#[deny(unsafe_code)]
mod system_state;

pub use phase::Phase;
pub use system_state::{AtomicSystemState, SystemState};
