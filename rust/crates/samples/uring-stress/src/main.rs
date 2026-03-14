//! io_uring stress test and comparison benchmark for FASTER.
//!
//! Exercises [`UringDevice`](faster_uring::UringDevice) under high-concurrency,
//! variable-size workloads and optionally compares it against
//! [`SyncFileDevice`](faster_core::SyncFileDevice).
//!
//! Requires the `io_uring` feature (Linux only).

#[cfg(feature = "io_uring")]
mod stress;

fn main() {
    #[cfg(feature = "io_uring")]
    stress::run();

    #[cfg(not(feature = "io_uring"))]
    {
        eprintln!("uring-stress requires the io_uring feature (Linux only).");
        eprintln!("Run with: cargo run -p uring-stress --features io_uring --release");
        std::process::exit(1);
    }
}
