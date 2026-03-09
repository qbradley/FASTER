//! Standard scenario template library.
//!
//! Contains base scenario templates (5 original + 15 patterns) and the
//! [`expansion`] module which generates 1000+ parameterized variants for
//! comprehensive campaign-based seed exploration.

// ── Original templates ──────────────────────────────────────────────────
pub mod checkpoint_crash;
pub mod compaction_crash;
pub mod crud_stress;
pub mod recovery_stress;
pub mod torn_write;

// ── Wave 2 templates ────────────────────────────────────────────────────
pub mod concurrent_crash;
pub mod dual_subsystem_crash;
pub mod high_density_crash;
pub mod large_record_recovery;
pub mod mixed_crash_timing;
pub mod overwrite_recovery;
pub mod torn_write_varied;
pub mod write_error_recovery;

// ── Wave 4 templates (extended campaign) ────────────────────────────────
pub mod boundary_record_crash;
pub mod graduated_fault_crash;
pub mod overwrite_compaction_crash;
pub mod overwrite_torn_write;
pub mod sparse_key_crash;
pub mod triple_crash;
pub mod write_error_crash;

// ── Parameterized expansion engine ──────────────────────────────────────
pub mod expansion;
