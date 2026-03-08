//! Disk I/O benchmark workload specifications.
//!
//! These workloads are designed to exercise the disk I/O path, unlike
//! the in-memory YCSB workloads in cross-impl-bench.

use crate::distribution::Distribution;

/// Operation type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Read,
    Upsert,
    #[expect(dead_code)]
    Rmw,
}

/// Disk I/O benchmark workload specifications.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkloadSpec {
    /// 100% random reads with 10x overcommit — nearly all reads hit disk.
    Overcommit,
    /// 100% upserts with sequential keys — forces continuous page flushing.
    WriteHeavy,
    /// 50/50 read/write with Zipfian distribution — hot keys in memory, cold on disk.
    Mixed,
    /// Sequential key scan — compaction-like access pattern.
    Scan,
}

impl WorkloadSpec {
    pub fn name(&self) -> &'static str {
        match self {
            WorkloadSpec::Overcommit => "overcommit",
            WorkloadSpec::WriteHeavy => "write-heavy",
            WorkloadSpec::Mixed => "mixed",
            WorkloadSpec::Scan => "scan",
        }
    }

    pub fn description(&self) -> &'static str {
        match self {
            WorkloadSpec::Overcommit => "100% random reads, data >> memory (cold read storm)",
            WorkloadSpec::WriteHeavy => "100% sequential upserts, continuous flush pressure",
            WorkloadSpec::Mixed => "50/50 read/write, Zipfian hot/cold distribution",
            WorkloadSpec::Scan => "Sequential key scan (compaction-like)",
        }
    }

    /// Select the next operation using a fast inline PRNG.
    #[inline]
    pub fn next_op(&self, rng_seed: &mut u64) -> Op {
        let mut x = *rng_seed;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *rng_seed = x;

        match self {
            WorkloadSpec::Overcommit => Op::Read,
            WorkloadSpec::WriteHeavy => Op::Upsert,
            WorkloadSpec::Mixed => {
                if x % 100 < 50 {
                    Op::Read
                } else {
                    Op::Upsert
                }
            }
            WorkloadSpec::Scan => Op::Read,
        }
    }

    /// Return the key distribution for this workload.
    pub fn distribution(&self) -> Distribution {
        match self {
            WorkloadSpec::Overcommit => Distribution::Uniform,
            WorkloadSpec::WriteHeavy => Distribution::Sequential,
            WorkloadSpec::Mixed => Distribution::Zipfian(0.99),
            WorkloadSpec::Scan => Distribution::Sequential,
        }
    }

    /// Suggested buffer_size_pages — controls memory pressure.
    ///
    /// # Memory Budget Relationship
    ///
    /// Each buffer page is 32 KiB. The in-memory working set is:
    ///   `buffer_pages * 32 KiB`
    ///
    /// For true disk I/O, the total dataset (`num_keys * (8 + value_size)`)
    /// must exceed the buffer memory AND the OS page cache. On a 32 GB VM
    /// with 10M x 1 KiB records (about 10 GB dataset), you need
    /// buffer_pages <= 16 to force reads off-disk once the OS cache is cold.
    ///
    /// Rule of thumb: `buffer_pages * 32 KiB < dataset_size / 100` for
    /// meaningful pending rates.
    pub fn default_buffer_pages(&self) -> usize {
        match self {
            WorkloadSpec::Overcommit => 16, // ~512KB — force reads to disk
            WorkloadSpec::WriteHeavy => 16, // ~512KB — force rapid flushing
            WorkloadSpec::Mixed => 32,      // ~1MB — moderate memory pressure
            WorkloadSpec::Scan => 16,       // ~512KB — test read-ahead from disk
        }
    }
}
