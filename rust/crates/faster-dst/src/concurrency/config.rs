use crate::SimIoConfig;

/// Distribution of key access patterns for writer threads.
#[derive(Clone, Debug)]
pub enum KeyDist {
    /// Keys chosen uniformly at random in `[0, range)`.
    Uniform { range: u64 },
    /// Zipfian (skewed) distribution over `[0, range)`.
    Zipfian { range: u64, exponent: f64 },
    /// Keys assigned sequentially (writer-local counter).
    Sequential,
}

/// Full scenario configuration for a concurrency DST run.
#[derive(Clone, Debug)]
pub struct ScenarioConfig {
    /// Number of concurrent writer threads.
    pub num_writers: usize,
    /// Operations each writer will execute.
    pub ops_per_writer: u64,
    /// Buffer pool size in pages.
    pub buffer_pool_pages: usize,
    /// Key access pattern.
    pub key_dist: KeyDist,
    /// Enable lossy (approximate) mode.
    pub lossy: bool,
    /// Simulated I/O device configuration (from P2).
    pub io: SimIoConfig,
    /// Call `maintenance()` every N operations.
    pub maintenance_interval: u64,
    /// Master seed — same seed = identical execution.
    pub seed: u64,
}

impl Default for ScenarioConfig {
    fn default() -> Self {
        Self {
            num_writers: 4,
            ops_per_writer: 10_000,
            buffer_pool_pages: 16,
            key_dist: KeyDist::Uniform { range: 100_000 },
            lossy: false,
            io: SimIoConfig::default(),
            maintenance_interval: 100,
            seed: 0,
        }
    }
}
