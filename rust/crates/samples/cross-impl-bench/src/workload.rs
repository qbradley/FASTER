//! YCSB workload specifications.

/// Operation type for YCSB workloads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Read,
    Upsert,
    Rmw,
}

/// Standard YCSB workload specifications.
#[derive(Debug, Clone, Copy)]
pub enum WorkloadSpec {
    /// Workload A: 50% read, 50% update (update-heavy)
    A,
    /// Workload B: 95% read, 5% update (read-heavy)
    B,
    /// Workload C: 100% read (read-only)
    C,
    /// Workload F: 50% read, 50% RMW (read-modify-write)
    F,
}

impl WorkloadSpec {
    pub fn name(&self) -> &'static str {
        match self {
            WorkloadSpec::A => "A",
            WorkloadSpec::B => "B",
            WorkloadSpec::C => "C",
            WorkloadSpec::F => "F",
        }
    }

    #[allow(dead_code)]
    pub fn description(&self) -> &'static str {
        match self {
            WorkloadSpec::A => "50/50 read/update (update-heavy)",
            WorkloadSpec::B => "95/5 read/update (read-heavy)",
            WorkloadSpec::C => "100% read (read-only)",
            WorkloadSpec::F => "50/50 read/RMW",
        }
    }

    /// Select the next operation using a fast inline PRNG.
    ///
    /// Uses the mutable `rng_seed` as xorshift state to avoid
    /// function-call overhead in the hot loop.
    #[inline]
    pub fn next_op(&self, rng_seed: &mut u64) -> Op {
        // Inline xorshift64 step
        let mut x = *rng_seed;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *rng_seed = x;

        match self {
            WorkloadSpec::A => {
                if x % 100 < 50 {
                    Op::Read
                } else {
                    Op::Upsert
                }
            }
            WorkloadSpec::B => {
                if x % 100 < 95 {
                    Op::Read
                } else {
                    Op::Upsert
                }
            }
            WorkloadSpec::C => Op::Read,
            WorkloadSpec::F => {
                if x % 100 < 50 {
                    Op::Read
                } else {
                    Op::Rmw
                }
            }
        }
    }
}
