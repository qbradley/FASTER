//! Standard scenario template library.
//!
//! Contains base scenario templates (5 original + 8 new patterns) and the
//! [`expansion`] module which generates 100+ parameterized variants for
//! comprehensive campaign-based seed exploration.

// ── Original templates ──────────────────────────────────────────────────
pub mod checkpoint_crash;
pub mod compaction_crash;
pub mod crud_stress;
pub mod recovery_stress;
pub mod torn_write;

// ── New templates ───────────────────────────────────────────────────────
pub mod concurrent_crash;
pub mod dual_subsystem_crash;
pub mod high_density_crash;
pub mod large_record_recovery;
pub mod mixed_crash_timing;
pub mod overwrite_recovery;
pub mod torn_write_varied;
pub mod write_error_recovery;

// ── Parameterized expansion engine ──────────────────────────────────────
pub mod expansion;
