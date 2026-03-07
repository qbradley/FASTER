//! Key distribution generators: uniform and Zipfian.

/// Key access distribution.
#[derive(Debug, Clone, Copy)]
pub enum Distribution {
    Uniform,
    Zipfian(f64), // theta parameter
}

impl Distribution {
    pub fn name(&self) -> &'static str {
        match self {
            Distribution::Uniform => "uniform",
            Distribution::Zipfian(_) => "zipfian",
        }
    }
}

/// Thread-local key generator with configurable distribution.
pub struct KeyGenerator {
    dist: Distribution,
    num_keys: u64,
    // xorshift64 state
    rng_state: u64,
    // Zipfian precomputed state
    zipf_zetan: f64,
    #[allow(dead_code)]
    zipf_zeta2: f64,
    zipf_alpha: f64,
    zipf_eta: f64,
}

impl KeyGenerator {
    pub fn new(dist: Distribution, num_keys: u64, thread_id: u64) -> Self {
        let seed = 0x1234_5678_u64
            .wrapping_mul(thread_id.wrapping_add(1))
            .wrapping_add(0xABCD_EF01);
        let (zipf_zetan, zipf_zeta2, zipf_alpha, zipf_eta) = match dist {
            Distribution::Zipfian(theta) => {
                let zetan = zeta(num_keys, theta);
                let zeta2 = zeta(2, theta);
                let alpha = 1.0 / (1.0 - theta);
                let eta = (1.0 - (2.0 / num_keys as f64).powf(1.0 - theta)) / (1.0 - zeta2 / zetan);
                (zetan, zeta2, alpha, eta)
            }
            Distribution::Uniform => (0.0, 0.0, 0.0, 0.0),
        };

        KeyGenerator {
            dist,
            num_keys,
            rng_state: seed,
            zipf_zetan,
            zipf_zeta2,
            zipf_alpha,
            zipf_eta,
        }
    }

    /// Generate the next key according to the configured distribution.
    #[inline]
    pub fn next_key(&mut self) -> u64 {
        match self.dist {
            Distribution::Uniform => self.next_u64() % self.num_keys,
            Distribution::Zipfian(_) => self.next_zipfian(),
        }
    }

    #[inline]
    fn next_u64(&mut self) -> u64 {
        let mut x = self.rng_state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.rng_state = x;
        x
    }

    /// Generate next f64 in [0, 1).
    #[inline]
    fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Scrambled Zipfian distribution (YCSB-style).
    ///
    /// Based on "Quickly Generating Billion-Record Synthetic Databases"
    /// (Gray et al., SIGMOD 1994) and the YCSB ScrambledZipfianGenerator.
    #[inline]
    fn next_zipfian(&mut self) -> u64 {
        let u = self.next_f64();
        let uz = u * self.zipf_zetan;

        let raw = if uz < 1.0 {
            0
        } else if uz < 1.0 + 0.5_f64.powf(self.zipf_alpha - 1.0) {
            1
        } else {
            let spread = self.num_keys as f64
                * ((self.zipf_eta * u - self.zipf_eta + 1.0).powf(self.zipf_alpha));
            spread as u64
        };

        // Scramble with FNV hash to avoid hot-spotting on low keys
        fnv_hash(raw) % self.num_keys
    }
}

/// Compute the generalized Harmonic number H_{n,theta} = sum_{i=1}^{n} 1/i^theta.
fn zeta(n: u64, theta: f64) -> f64 {
    let mut sum = 0.0;
    for i in 1..=n {
        sum += 1.0 / (i as f64).powf(theta);
    }
    sum
}

/// FNV-1a hash for key scrambling.
#[inline]
fn fnv_hash(mut val: u64) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for _ in 0..8 {
        hash ^= val & 0xFF;
        hash = hash.wrapping_mul(0x100000001b3);
        val >>= 8;
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uniform_stays_in_range() {
        let mut keygen = KeyGenerator::new(Distribution::Uniform, 1000, 0);
        for _ in 0..10_000 {
            assert!(keygen.next_key() < 1000);
        }
    }

    #[test]
    fn zipfian_stays_in_range() {
        let mut keygen = KeyGenerator::new(Distribution::Zipfian(0.99), 1000, 0);
        for _ in 0..10_000 {
            assert!(keygen.next_key() < 1000);
        }
    }

    #[test]
    fn zipfian_is_skewed() {
        let n = 1000u64;
        let mut keygen = KeyGenerator::new(Distribution::Zipfian(0.99), n, 42);
        let mut counts = vec![0u64; n as usize];
        let total = 100_000;
        for _ in 0..total {
            counts[keygen.next_key() as usize] += 1;
        }
        // Top 1% of keys should have significantly more than 1% of accesses
        let mut sorted = counts.clone();
        sorted.sort_unstable_by(|a, b| b.cmp(a));
        let top_1_pct: u64 = sorted[..10].iter().sum();
        // With theta=0.99, top 1% typically gets >10% of accesses
        assert!(
            top_1_pct > (total / 10) as u64,
            "Zipfian not skewed enough: top 1% got {top_1_pct} out of {total}"
        );
    }
}
