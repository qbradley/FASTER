//! Builder pattern for constructing [`FasterKv`] instances.
//!
//! The [`FasterKvBuilder`] provides a fluent API for configuring and creating
//! a FASTER store with validation. It complements the direct
//! [`FasterKvConfig`] approach for users who prefer discoverable method
//! chaining.
//!
//! # Example
//!
//! ```
//! use faster_core::store::{FasterKv, SimpleFunctions};
//! use faster_core::NullDevice;
//!
//! let store: FasterKv<SimpleFunctions<u64, u64>> =
//!     FasterKv::<SimpleFunctions<u64, u64>>::builder()
//!         .hash_index_size_log2(16)
//!         .buffer_size_pages(8)
//!         .mutable_fraction(0.9)
//!         .build(SimpleFunctions::default(), NullDevice::new())
//!         .expect("valid config");
//! ```

use crate::device::Device;
use crate::error::FasterError;
use crate::grow::GrowConfig;
use crate::hybrid_log::eviction::EvictionPolicy;
use crate::store::functions::Functions;
use crate::store::kv::{FasterKv, FasterKvConfig};

/// A builder for constructing [`FasterKv`] instances with validation.
///
/// Obtain a builder via [`FasterKv::builder()`], configure it with fluent
/// method chaining, then call [`build()`](Self::build) to create the store.
///
/// All fields have sensible defaults matching [`FasterKvConfig::default()`].
///
/// # Example
///
/// ```
/// use faster_core::store::{FasterKv, SimpleFunctions};
/// use faster_core::NullDevice;
///
/// let store: FasterKv<SimpleFunctions<u64, u64>> =
///     FasterKv::<SimpleFunctions<u64, u64>>::builder()
///         .hash_index_size_log2(16)
///         .mutable_fraction(0.9)
///         .build(SimpleFunctions::default(), NullDevice::new())
///         .expect("valid config");
/// ```
#[derive(Debug, Clone)]
pub struct FasterKvBuilder {
    hash_index_size_log2: usize,
    buffer_size_pages: usize,
    mutable_fraction: f64,
    sector_size: usize,
    eviction_policy: EvictionPolicy,
    grow_load_factor_threshold: f64,
    grow_chunks_per_operation: u32,
    grow_enabled: bool,
}

impl Default for FasterKvBuilder {
    fn default() -> Self {
        let defaults = FasterKvConfig::default();
        let grow_defaults = GrowConfig::default();
        Self {
            hash_index_size_log2: defaults.hash_index_size_log2,
            buffer_size_pages: defaults.buffer_size_pages,
            mutable_fraction: defaults.mutable_fraction,
            sector_size: defaults.sector_size,
            eviction_policy: defaults.eviction_policy,
            grow_load_factor_threshold: grow_defaults.load_factor_threshold,
            grow_chunks_per_operation: grow_defaults.chunks_per_operation,
            grow_enabled: grow_defaults.enabled,
        }
    }
}

impl FasterKvBuilder {
    /// Create a new builder with default configuration values.
    ///
    /// Equivalent to `FasterKvBuilder::default()`.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the log₂ of the number of hash index buckets.
    ///
    /// For example, 20 → 2²⁰ = 1M buckets. Must be in `4..=30`.
    ///
    /// Default: `20`.
    #[must_use]
    pub fn hash_index_size_log2(mut self, bits: usize) -> Self {
        self.hash_index_size_log2 = bits;
        self
    }

    /// Set the number of in-memory page frames.
    ///
    /// Must be a power of 2 and greater than zero.
    ///
    /// Default: `16`.
    #[must_use]
    pub fn buffer_size_pages(mut self, pages: usize) -> Self {
        self.buffer_size_pages = pages;
        self
    }

    /// Set the fraction of the buffer that is mutable.
    ///
    /// Must be in the range `(0.0, 1.0)` exclusive.
    ///
    /// Default: `0.9`.
    #[must_use]
    pub fn mutable_fraction(mut self, fraction: f64) -> Self {
        self.mutable_fraction = fraction;
        self
    }

    /// Set the sector size for page alignment (bytes).
    ///
    /// Must be a power of 2 and greater than zero.
    ///
    /// Default: `512`.
    #[must_use]
    pub fn sector_size(mut self, size: usize) -> Self {
        self.sector_size = size;
        self
    }

    /// Set the eviction policy for managing in-memory pages.
    ///
    /// Default: [`EvictionPolicy::default()`].
    #[must_use]
    pub fn eviction_policy(mut self, policy: EvictionPolicy) -> Self {
        self.eviction_policy = policy;
        self
    }

    /// Set the load-factor threshold that triggers an automatic hash grow.
    ///
    /// Must be in the range `(0.0, 1.0)` exclusive.
    ///
    /// Default: `0.75`.
    #[must_use]
    pub fn grow_threshold(mut self, threshold: f64) -> Self {
        self.grow_load_factor_threshold = threshold;
        self
    }

    /// Set how many chunks to split per cooperative grow call.
    ///
    /// Higher values finish the grow faster but increase per-operation
    /// latency. Default: `16`.
    #[must_use]
    pub fn grow_chunks_per_operation(mut self, chunks: u32) -> Self {
        self.grow_chunks_per_operation = chunks;
        self
    }

    /// Enable or disable automatic hash index grow.
    ///
    /// When disabled, grows can still be triggered manually.
    ///
    /// Default: `true`.
    #[must_use]
    pub fn grow_enabled(mut self, enabled: bool) -> Self {
        self.grow_enabled = enabled;
        self
    }

    /// Validate the configuration and build a [`FasterKv`] store.
    ///
    /// Returns [`FasterError::InvalidOperation`] if any configuration value
    /// is out of range.
    ///
    /// # Errors
    ///
    /// - `hash_index_size_log2` not in `4..=30`
    /// - `buffer_size_pages` is zero or not a power of 2
    /// - `mutable_fraction` not in `(0.0, 1.0)`
    /// - `sector_size` is zero or not a power of 2
    /// - `grow_threshold` not in `(0.0, 1.0)`
    ///
    /// # Example
    ///
    /// ```
    /// use faster_core::store::{FasterKv, SimpleFunctions};
    /// use faster_core::NullDevice;
    ///
    /// let result = FasterKv::<SimpleFunctions<u64, u64>>::builder()
    ///     .hash_index_size_log2(2) // too small
    ///     .build(
    ///         SimpleFunctions::<u64, u64>::default(),
    ///         NullDevice::new(),
    ///     );
    /// assert!(result.is_err());
    /// ```
    pub fn build<F: Functions>(
        self,
        functions: F,
        device: impl Device,
    ) -> Result<FasterKv<F>, FasterError> {
        self.validate()?;

        let config = FasterKvConfig {
            hash_index_size_log2: self.hash_index_size_log2,
            buffer_size_pages: self.buffer_size_pages,
            mutable_fraction: self.mutable_fraction,
            sector_size: self.sector_size,
            eviction_policy: self.eviction_policy,
            grow_config: GrowConfig {
                load_factor_threshold: self.grow_load_factor_threshold,
                chunks_per_operation: self.grow_chunks_per_operation,
                enabled: self.grow_enabled,
            },
            auto_compact: false,
        };

        Ok(FasterKv::new(config, functions, device))
    }

    /// Validate configuration without building.
    ///
    /// Returns `Ok(())` if all values are within acceptable ranges, or
    /// an error describing the first invalid value found.
    pub fn validate(&self) -> Result<(), FasterError> {
        if !(4..=30).contains(&self.hash_index_size_log2) {
            return Err(FasterError::InvalidOperation(format!(
                "hash_index_size_log2 must be in 4..=30, got {}",
                self.hash_index_size_log2
            )));
        }

        if self.buffer_size_pages == 0 || !self.buffer_size_pages.is_power_of_two() {
            return Err(FasterError::InvalidOperation(format!(
                "buffer_size_pages must be a positive power of 2, got {}",
                self.buffer_size_pages
            )));
        }

        if self.mutable_fraction <= 0.0 || self.mutable_fraction >= 1.0 {
            return Err(FasterError::InvalidOperation(format!(
                "mutable_fraction must be in (0.0, 1.0), got {}",
                self.mutable_fraction
            )));
        }

        if self.sector_size == 0 || !self.sector_size.is_power_of_two() {
            return Err(FasterError::InvalidOperation(format!(
                "sector_size must be a positive power of 2, got {}",
                self.sector_size
            )));
        }

        if self.grow_load_factor_threshold <= 0.0 || self.grow_load_factor_threshold >= 1.0 {
            return Err(FasterError::InvalidOperation(format!(
                "grow_threshold must be in (0.0, 1.0), got {}",
                self.grow_load_factor_threshold
            )));
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NullDevice;
    use crate::store::functions::SimpleFunctions;

    type TestFunctions = SimpleFunctions<u64, u64>;

    // ── Defaults ────────────────────────────────────────────────────

    #[test]
    fn builder_defaults_create_valid_store() {
        let store: FasterKv<TestFunctions> = FasterKv::<TestFunctions>::builder()
            .build(SimpleFunctions::default(), NullDevice::new())
            .expect("default config should be valid");
        // Smoke-test: we can create a session.
        let session = store.new_session();
        store.dispose_session(session);
    }

    #[test]
    fn builder_default_matches_config_default() {
        let builder = FasterKvBuilder::default();
        let config = FasterKvConfig::default();
        let grow = GrowConfig::default();

        assert_eq!(builder.hash_index_size_log2, config.hash_index_size_log2);
        assert_eq!(builder.buffer_size_pages, config.buffer_size_pages);
        assert!((builder.mutable_fraction - config.mutable_fraction).abs() < f64::EPSILON);
        assert_eq!(builder.sector_size, config.sector_size);
        assert!(
            (builder.grow_load_factor_threshold - grow.load_factor_threshold).abs() < f64::EPSILON
        );
        assert_eq!(builder.grow_chunks_per_operation, grow.chunks_per_operation);
        assert_eq!(builder.grow_enabled, grow.enabled);
    }

    // ── Custom values ───────────────────────────────────────────────

    #[test]
    fn builder_with_custom_values() {
        let store: FasterKv<TestFunctions> = FasterKv::<TestFunctions>::builder()
            .hash_index_size_log2(16)
            .buffer_size_pages(8)
            .mutable_fraction(0.5)
            .sector_size(1024)
            .grow_threshold(0.8)
            .grow_enabled(false)
            .grow_chunks_per_operation(32)
            .build(SimpleFunctions::default(), NullDevice::new())
            .expect("custom config should be valid");

        let session = store.new_session();
        store.dispose_session(session);
    }

    // ── Validation errors ───────────────────────────────────────────

    #[test]
    fn rejects_hash_index_size_too_small() {
        let err = FasterKv::<TestFunctions>::builder()
            .hash_index_size_log2(3)
            .build::<TestFunctions>(SimpleFunctions::default(), NullDevice::new())
            .map(drop)
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("hash_index_size_log2"), "got: {msg}");
        assert!(msg.contains("4..=30"), "got: {msg}");
    }

    #[test]
    fn rejects_hash_index_size_too_large() {
        let err = FasterKv::<TestFunctions>::builder()
            .hash_index_size_log2(31)
            .build::<TestFunctions>(SimpleFunctions::default(), NullDevice::new())
            .map(drop)
            .unwrap_err();
        assert!(err.to_string().contains("hash_index_size_log2"));
    }

    #[test]
    fn rejects_zero_buffer_size_pages() {
        let err = FasterKv::<TestFunctions>::builder()
            .buffer_size_pages(0)
            .build::<TestFunctions>(SimpleFunctions::default(), NullDevice::new())
            .map(drop)
            .unwrap_err();
        assert!(err.to_string().contains("buffer_size_pages"));
    }

    #[test]
    fn rejects_non_power_of_two_buffer_size() {
        let err = FasterKv::<TestFunctions>::builder()
            .buffer_size_pages(3)
            .build::<TestFunctions>(SimpleFunctions::default(), NullDevice::new())
            .map(drop)
            .unwrap_err();
        assert!(err.to_string().contains("buffer_size_pages"));
    }

    #[test]
    fn rejects_mutable_fraction_zero() {
        let err = FasterKv::<TestFunctions>::builder()
            .mutable_fraction(0.0)
            .build::<TestFunctions>(SimpleFunctions::default(), NullDevice::new())
            .map(drop)
            .unwrap_err();
        assert!(err.to_string().contains("mutable_fraction"));
    }

    #[test]
    fn rejects_mutable_fraction_one() {
        let err = FasterKv::<TestFunctions>::builder()
            .mutable_fraction(1.0)
            .build::<TestFunctions>(SimpleFunctions::default(), NullDevice::new())
            .map(drop)
            .unwrap_err();
        assert!(err.to_string().contains("mutable_fraction"));
    }

    #[test]
    fn rejects_mutable_fraction_negative() {
        let err = FasterKv::<TestFunctions>::builder()
            .mutable_fraction(-0.1)
            .build::<TestFunctions>(SimpleFunctions::default(), NullDevice::new())
            .map(drop)
            .unwrap_err();
        assert!(err.to_string().contains("mutable_fraction"));
    }

    #[test]
    fn rejects_zero_sector_size() {
        let err = FasterKv::<TestFunctions>::builder()
            .sector_size(0)
            .build::<TestFunctions>(SimpleFunctions::default(), NullDevice::new())
            .map(drop)
            .unwrap_err();
        assert!(err.to_string().contains("sector_size"));
    }

    #[test]
    fn rejects_non_power_of_two_sector_size() {
        let err = FasterKv::<TestFunctions>::builder()
            .sector_size(100)
            .build::<TestFunctions>(SimpleFunctions::default(), NullDevice::new())
            .map(drop)
            .unwrap_err();
        assert!(err.to_string().contains("sector_size"));
    }

    #[test]
    fn rejects_grow_threshold_zero() {
        let err = FasterKv::<TestFunctions>::builder()
            .grow_threshold(0.0)
            .build::<TestFunctions>(SimpleFunctions::default(), NullDevice::new())
            .map(drop)
            .unwrap_err();
        assert!(err.to_string().contains("grow_threshold"));
    }

    #[test]
    fn rejects_grow_threshold_one() {
        let err = FasterKv::<TestFunctions>::builder()
            .grow_threshold(1.0)
            .build::<TestFunctions>(SimpleFunctions::default(), NullDevice::new())
            .map(drop)
            .unwrap_err();
        assert!(err.to_string().contains("grow_threshold"));
    }

    // ── validate() standalone ───────────────────────────────────────

    #[test]
    fn validate_returns_ok_for_defaults() {
        assert!(FasterKvBuilder::default().validate().is_ok());
    }

    #[test]
    fn validate_catches_errors_without_building() {
        let builder = FasterKvBuilder::default().hash_index_size_log2(0);
        assert!(builder.validate().is_err());
    }

    // ── Boundary values ─────────────────────────────────────────────

    #[test]
    fn accepts_boundary_hash_index_sizes() {
        // Minimum valid
        assert!(
            FasterKvBuilder::default()
                .hash_index_size_log2(4)
                .validate()
                .is_ok()
        );
        // Maximum valid
        assert!(
            FasterKvBuilder::default()
                .hash_index_size_log2(30)
                .validate()
                .is_ok()
        );
    }

    #[test]
    fn accepts_small_valid_mutable_fraction() {
        assert!(
            FasterKvBuilder::default()
                .mutable_fraction(0.01)
                .validate()
                .is_ok()
        );
    }

    #[test]
    fn accepts_large_valid_mutable_fraction() {
        assert!(
            FasterKvBuilder::default()
                .mutable_fraction(0.99)
                .validate()
                .is_ok()
        );
    }
}
