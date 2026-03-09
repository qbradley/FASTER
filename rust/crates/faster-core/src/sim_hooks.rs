//! Simulation yield-point markers.
//!
//! The [`sim_yield!`] macro marks scheduling-relevant points inside
//! `faster-core`. Under the `simulation` feature these are trace markers
//! (Phase 3); in Phase 4 they will become crash-point injection hooks.
//! Without `simulation` the macro compiles to nothing.

/// Simulation yield point marker.
///
/// Under `simulation` feature: evaluates and discards the label expression
/// (preserved for documentation / future tracing integration).
/// Without `simulation` feature: compiles to nothing.
///
/// # Example
///
/// ```ignore
/// sim_yield!("read::after_epoch_protect");
/// ```
#[allow(unused_macros)]
macro_rules! sim_yield {
    ($label:expr) => {
        #[cfg(feature = "simulation")]
        {
            // Phase 3: no-op marker.  Phase 4 will add crash_point
            // checking and optional scheduler interaction here.
            let _ = $label;
        }
    };
}

#[allow(unused_imports)]
pub(crate) use sim_yield;
