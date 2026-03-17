use rand::Rng;
use rand::SeedableRng;
use rand_chacha::ChaCha20Rng;

/// Seeds from regressions — each caught a real bug. **Never remove entries.**
pub mod regression {
    /// Seeds that triggered real bugs. Append-only.
    pub const REGRESSION_SEEDS: &[u64] = &[];
}

/// Diverse canary seeds that exercise different code paths.
pub mod canary {
    pub const CANARY_SEEDS: &[u64] = &[
        0x0000_0000_0000_0001, // minimal — near-zero seed
        0xDEAD_BEEF_CAFE_BABE, // high bits set — classic magic value
        0x0123_4567_89AB_CDEF, // sequential bit pattern
        0xFFFF_FFFF_FFFF_FFFF, // all ones — edge case for masking
        0x5555_5555_5555_5555, // alternating bits — stresses XOR paths
    ];
}

/// Generate `count` deterministic exploration seeds from a root seed.
///
/// Uses ChaCha20 for high-quality, reproducible randomness.
/// Same `(root, count)` always produces the same output.
pub fn exploration_seeds(root: u64, count: usize) -> Vec<u64> {
    let mut rng = ChaCha20Rng::seed_from_u64(root);
    (0..count).map(|_| rng.random::<u64>()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn exploration_seeds_deterministic() {
        let a = exploration_seeds(42, 100);
        let b = exploration_seeds(42, 100);
        assert_eq!(a, b, "same root + count must produce identical seeds");
    }

    #[test]
    fn exploration_seeds_unique() {
        let seeds = exploration_seeds(42, 100);
        let unique: HashSet<u64> = seeds.iter().copied().collect();
        assert_eq!(unique.len(), 100, "all 100 seeds should be unique");
    }

    #[test]
    fn exploration_seeds_different_roots() {
        let a = exploration_seeds(1, 10);
        let b = exploration_seeds(2, 10);
        assert_ne!(a, b, "different roots must produce different seeds");
    }

    #[test]
    fn canary_seeds_count() {
        assert!(
            canary::CANARY_SEEDS.len() >= 5,
            "canary set must have at least 5 diverse seeds"
        );
    }

    #[test]
    fn canary_seeds_unique() {
        let unique: HashSet<u64> = canary::CANARY_SEEDS.iter().copied().collect();
        assert_eq!(
            unique.len(),
            canary::CANARY_SEEDS.len(),
            "canary seeds must all be unique"
        );
    }

    #[test]
    fn regression_seeds_exists() {
        // Initially empty — just verify the constant compiles and is accessible
        let _ = regression::REGRESSION_SEEDS;
    }
}
