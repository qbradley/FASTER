//! Conditional tracing instrumentation macros.
//!
//! When the `tracing` feature is enabled, these macros emit [`tracing`] spans
//! and events. When disabled, they compile to nothing — zero overhead.
//!
//! When the `metrics` feature is enabled, `metrics_inc!` increments the
//! given counter. When disabled, it compiles to nothing.

/// Enters a [`tracing::info_span`] for the duration of the enclosing scope.
///
/// Compiles to nothing when the `tracing` feature is disabled.
///
/// # Examples
///
/// ```ignore
/// trace_span!("my_operation");
/// trace_span!("my_operation", key = %key_hash);
/// ```
macro_rules! trace_span {
    ($name:expr) => {
        #[cfg(feature = "tracing")]
        let _span = tracing::info_span!($name).entered();
    };
    ($name:expr, $($field:tt)*) => {
        #[cfg(feature = "tracing")]
        let _span = tracing::info_span!($name, $($field)*).entered();
    };
}

/// Emits a [`tracing::debug`] event.
///
/// Compiles to nothing when the `tracing` feature is disabled.
#[allow(unused_macros)]
macro_rules! trace_event {
    ($($arg:tt)*) => {
        #[cfg(feature = "tracing")]
        tracing::debug!($($arg)*);
    };
}

/// Increments a `Metrics` counter by 1 when the `metrics` feature is enabled.
///
/// Compiles to nothing when the `metrics` feature is disabled — zero overhead.
///
/// # Examples
///
/// ```ignore
/// metrics_inc!(self.metrics, total_operations);
/// metrics_inc!(metrics, flush_count);
/// ```
macro_rules! metrics_inc {
    ($metrics:expr, $field:ident) => {
        #[cfg(feature = "metrics")]
        {
            $metrics
                .$field
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    };
}
