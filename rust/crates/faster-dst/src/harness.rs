//! Convenient test harness that ties together simulated storage, a runtime,
//! and a temporary checkpoint directory.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use faster_core::SyncFileDevice;
use faster_core::grow::GrowConfig;
use faster_core::hybrid_log::EvictionPolicy;
use faster_core::store::{FasterKv, FasterKvConfig, SimpleFunctions};

use crate::device::{SimulatedDevice, SimulatedStorage};
use crate::fault::FaultConfig;

/// All-in-one harness for DST scenarios.
///
/// Holds a shared [`SimulatedStorage`] (for in-memory device tests), a temp
/// directory for checkpoint files and log segments, and a seed-derived counter
/// for creating deterministic device instances.
pub struct SimulationHarness {
    seed: u64,
    storage: Arc<SimulatedStorage>,
    checkpoint_dir: tempfile::TempDir,
    device_seed_counter: AtomicU64,
}

impl SimulationHarness {
    /// Create a new harness from the given master seed.
    pub fn new(seed: u64) -> Self {
        Self {
            seed,
            storage: SimulatedStorage::new(),
            checkpoint_dir: tempfile::TempDir::new().expect("failed to create temp dir"),
            device_seed_counter: AtomicU64::new(0),
        }
    }

    /// Path to the temporary checkpoint directory (stable for the harness lifetime).
    pub fn checkpoint_dir(&self) -> &Path {
        self.checkpoint_dir.path()
    }

    /// Access the shared storage backing all in-memory devices created by this
    /// harness.
    pub fn storage(&self) -> &Arc<SimulatedStorage> {
        &self.storage
    }

    /// The master seed.
    pub fn seed(&self) -> u64 {
        self.seed
    }

    // ── device & store factories ───────────────────────────────────────

    fn next_device_seed(&self) -> u64 {
        let counter = self.device_seed_counter.fetch_add(1, Ordering::Relaxed);
        self.seed
            .wrapping_add(counter.wrapping_mul(6_364_136_223_846_793_005))
    }

    /// Create a fault-free [`SimulatedDevice`] backed by the shared storage.
    pub fn create_device(&self) -> SimulatedDevice {
        SimulatedDevice::new(Arc::clone(&self.storage), self.next_device_seed())
    }

    /// Create a [`SimulatedDevice`] with the given fault configuration.
    pub fn create_device_with_faults(&self, config: FaultConfig) -> SimulatedDevice {
        SimulatedDevice::new(Arc::clone(&self.storage), self.next_device_seed()).with_faults(config)
    }

    /// Default store configuration used by the `create_store*` helpers.
    pub fn default_config() -> FasterKvConfig {
        FasterKvConfig {
            hash_index_size_log2: 10, // 1 024 buckets
            buffer_size_pages: 8,
            mutable_fraction: 0.9,
            sector_size: 512,
            eviction_policy: EvictionPolicy::default(),
            grow_config: GrowConfig::default(),
            auto_compact: false,
            lossy: false,
        }
    }

    // ── in-memory stores (SimulatedDevice) ─────────────────────────────

    /// Convenience: create a `FasterKv` with an in-memory [`SimulatedDevice`].
    ///
    /// Use for tests that do NOT involve checkpoint/recovery (the recovery
    /// system validates on-disk log segment files).
    pub fn create_store(&self) -> FasterKv<SimpleFunctions<u64, u64>> {
        FasterKv::new(
            Self::default_config(),
            SimpleFunctions::default(),
            self.create_device(),
        )
    }

    /// Create an in-memory store whose device has the given fault configuration.
    pub fn create_store_with_faults(
        &self,
        config: FaultConfig,
    ) -> FasterKv<SimpleFunctions<u64, u64>> {
        FasterKv::new(
            Self::default_config(),
            SimpleFunctions::default(),
            self.create_device_with_faults(config),
        )
    }

    // ── file-backed stores (SyncFileDevice) ────────────────────────────

    /// Create a `FasterKv` backed by [`SyncFileDevice`] writing to the temp
    /// directory.
    ///
    /// Use for checkpoint/recovery tests — log segment files persist on disk
    /// so recovery validation succeeds.
    pub fn create_file_store(&self) -> FasterKv<SimpleFunctions<u64, u64>> {
        self.create_file_store_with_config(Self::default_config())
    }

    /// Create a file-backed store with a custom config.
    pub fn create_file_store_with_config(
        &self,
        config: FasterKvConfig,
    ) -> FasterKv<SimpleFunctions<u64, u64>> {
        let device = SyncFileDevice::new(
            self.checkpoint_dir(),
            "log.", // matches recovery expectation: log.0, log.1, …
            config.sector_size as u32,
            1 << 30, // 1 GiB segments
            2,       // I/O threads
        )
        .expect("failed to create SyncFileDevice");
        FasterKv::new(config, SimpleFunctions::default(), device)
    }
}

impl std::fmt::Debug for SimulationHarness {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SimulationHarness")
            .field("seed", &self.seed)
            .field("checkpoint_dir", &self.checkpoint_dir.path())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn harness_creates_store() {
        let harness = SimulationHarness::new(1);
        let store = harness.create_store();
        let mut s = store.new_session();
        let _ = store.upsert(&mut s, &1u64, &100u64, ());
        let val: Option<u64> = store.read_simple(&mut s, &1u64);
        assert_eq!(val, Some(100));
        store.dispose_session(s);
    }

    #[test]
    fn harness_creates_file_store() {
        let harness = SimulationHarness::new(1);
        let store = harness.create_file_store();
        let mut s = store.new_session();
        let _ = store.upsert(&mut s, &1u64, &200u64, ());
        let val: Option<u64> = store.read_simple(&mut s, &1u64);
        assert_eq!(val, Some(200));
        store.dispose_session(s);
    }

    #[test]
    fn checkpoint_dir_exists() {
        let harness = SimulationHarness::new(1);
        assert!(harness.checkpoint_dir().exists());
    }
}
