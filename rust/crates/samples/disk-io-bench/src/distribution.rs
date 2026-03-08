//! Key distribution generators: uniform and Zipfian.
//!
//! Adapted from cross-impl-bench for disk I/O workloads.

/// Key access distribution.
#[derive(Debug, Clone, Copy)]
pub enum Distribution {
    Uniform,
    Zipfian(f64),
    /// Sequential keys: 0, 1, 2, ... wrapping around num_keys.
    Sequential,
}

impl Distribution {
    #[expect(dead_code)]
    pub fn name(&self) -> &'static str {
        match self {
            Distribution::Uniform => "uniform",
            Distribution::Zipfian(_) => "zipfian",
            Distribution::Sequential => "sequential",
        }
    }
}

/// Thread-local key generator with configurable distribution.
pub struct KeyGenerator {
    dist: Distribution,
    num_keys: u64,
    rng_state: u64,
    seq_cursor: u64,
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
            _ => (0.0, 0.0, 0.0, 0.0),
        };

        // For sequential: each thread starts at a different offset
        let seq_start = thread_id.wrapping_mul(num_keys / 16);

        KeyGenerator {
            dist,
            num_keys,
            rng_state: seed,
            seq_cursor: seq_start % num_keys,
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
            Distribution::Sequential => {
                let k = self.seq_cursor;
                self.seq_cursor = (self.seq_cursor + 1) % self.num_keys;
                k
            }
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

    #[inline]
    fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

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

        fnv_hash(raw) % self.num_keys
    }
}

fn zeta(n: u64, theta: f64) -> f64 {
    let mut sum = 0.0;
    // For very large n, approximate after 10K to avoid slow startup
    let direct = n.min(10_000);
    for i in 1..=direct {
        sum += 1.0 / (i as f64).powf(theta);
    }
    if n > direct {
        // Euler-Maclaurin approximation for the tail
        let tail =
            ((n as f64).powf(1.0 - theta) - (direct as f64).powf(1.0 - theta)) / (1.0 - theta);
        sum += tail;
    }
    sum
}

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
