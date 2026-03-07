//! Seeded simulation runtime for deterministic workload generation.

use rand::RngCore;
use rand::SeedableRng;
use rand::rngs::SmallRng;

/// A deterministic runtime backed by a seeded PRNG.
///
/// Every method that produces random data is fully determined by the initial
/// seed, making test failures reproducible.
pub struct SimulationRuntime {
    seed: u64,
    rng: SmallRng,
}

impl SimulationRuntime {
    /// Create a new runtime from the given seed.
    pub fn new(seed: u64) -> Self {
        Self {
            seed,
            rng: SmallRng::seed_from_u64(seed),
        }
    }

    /// The seed that was used to initialise this runtime.
    pub fn seed(&self) -> u64 {
        self.seed
    }

    /// Access the inner PRNG directly.
    pub fn rng(&mut self) -> &mut SmallRng {
        &mut self.rng
    }

    /// Derive a child seed (useful for creating per-component seeds).
    pub fn child_seed(&mut self) -> u64 {
        self.rng.next_u64()
    }

    /// Generate `count` unique keys in `[0, max)`.
    ///
    /// Uniqueness is best-effort for large `count` — duplicates are possible
    /// when `count` approaches `max`.
    pub fn random_keys(&mut self, count: usize, max: u64) -> Vec<u64> {
        (0..count).map(|_| self.rng.next_u64() % max).collect()
    }

    /// Generate `count` (key, value) pairs with random keys and values.
    pub fn random_kv_pairs(&mut self, count: usize) -> Vec<(u64, u64)> {
        (0..count)
            .map(|_| (self.rng.next_u64(), self.rng.next_u64()))
            .collect()
    }

    /// Generate `count` sequential key-value pairs: `(0, seed_val), (1, seed_val+1), …`
    /// where values are deterministic but varied.
    pub fn sequential_kv_pairs(&mut self, count: usize) -> Vec<(u64, u64)> {
        (0..count)
            .map(|i| {
                let value = self.rng.next_u64();
                (i as u64, value)
            })
            .collect()
    }
}

impl std::fmt::Debug for SimulationRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SimulationRuntime")
            .field("seed", &self.seed)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_seed_same_output() {
        let pairs_a = SimulationRuntime::new(42).random_kv_pairs(10);
        let pairs_b = SimulationRuntime::new(42).random_kv_pairs(10);
        assert_eq!(pairs_a, pairs_b);
    }

    #[test]
    fn different_seeds_different_output() {
        let pairs_a = SimulationRuntime::new(1).random_kv_pairs(10);
        let pairs_b = SimulationRuntime::new(2).random_kv_pairs(10);
        assert_ne!(pairs_a, pairs_b);
    }

    #[test]
    fn sequential_pairs_have_sequential_keys() {
        let pairs = SimulationRuntime::new(0).sequential_kv_pairs(5);
        let keys: Vec<u64> = pairs.iter().map(|(k, _)| *k).collect();
        assert_eq!(keys, vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn child_seed_is_deterministic() {
        let s1 = SimulationRuntime::new(99).child_seed();
        let s2 = SimulationRuntime::new(99).child_seed();
        assert_eq!(s1, s2);
    }
}
