//! Report formatting: human-readable tables and CSV output.

use std::io::Write;
use std::time::Duration;

/// Result of a single benchmark run.
#[derive(Debug, Clone)]
pub struct BenchmarkResult {
    pub workload: String,
    pub threads: usize,
    pub distribution: String,
    pub value_size: u64,
    pub total_ops: u64,
    pub reads: u64,
    pub writes: u64,
    pub rmws: u64,
    pub elapsed: Duration,
    pub ops_per_sec: f64,
    pub throughput_mb_s: f64,
    pub p50_ns: f64,
    pub p99_ns: f64,
    pub p999_ns: f64,
    pub safe_context: bool,
}

pub struct Reporter;

impl Reporter {
    /// Print a summary table of all results.
    pub fn print_summary_table(results: &[BenchmarkResult]) {
        if results.is_empty() {
            return;
        }

        println!();
        println!(
            "╔═══════════════════════════════════════════════════════════════════════════════════════════════════════╗"
        );
        println!(
            "║                                    BENCHMARK RESULTS SUMMARY                                         ║"
        );
        println!(
            "╠═══════════╤════════╤══════════╤════════╤══════════════╤═══════════╤══════════╤══════════╤═════════════╣"
        );
        println!(
            "║ Workload  │ Thds   │ ValSize  │ Dist   │ Ops/sec      │ MB/s      │ P50 (ns) │ P99 (ns) │ P99.9 (ns)  ║"
        );
        println!(
            "╠═══════════╪════════╪══════════╪════════╪══════════════╪═══════════╪══════════╪══════════╪═════════════╣"
        );

        for r in results {
            println!(
                "║ {:9} │ {:>6} │ {:>6}B  │ {:6} │ {:>12} │ {:>9.1} │ {:>8.0} │ {:>8.0} │ {:>11.0} ║",
                r.workload,
                r.threads,
                r.value_size,
                r.distribution,
                format_ops(r.ops_per_sec),
                r.throughput_mb_s,
                r.p50_ns,
                r.p99_ns,
                r.p999_ns,
            );
        }

        println!(
            "╚═══════════╧════════╧══════════╧════════╧══════════════╧═══════════╧══════════╧══════════╧═════════════╝"
        );
    }

    /// Print a thread-scaling analysis table grouped by workload.
    pub fn print_scaling_table(results: &[BenchmarkResult]) {
        // Group by (workload, value_size, distribution)
        let mut groups: Vec<(String, Vec<&BenchmarkResult>)> = Vec::new();

        for r in results {
            let key = format!("{}-{}B-{}", r.workload, r.value_size, r.distribution);
            if let Some(g) = groups.iter_mut().find(|(k, _)| k == &key) {
                g.1.push(r);
            } else {
                groups.push((key, vec![r]));
            }
        }

        if groups.iter().all(|(_, g)| g.len() <= 1) {
            return; // No scaling data
        }

        println!();
        println!("── Thread Scaling Analysis ─────────────────────────────────────");

        for (name, group) in &groups {
            if group.len() <= 1 {
                continue;
            }
            println!();
            println!("  {name}:");

            let base_ops = group
                .iter()
                .find(|r| r.threads == 1)
                .map(|r| r.ops_per_sec)
                .unwrap_or(group[0].ops_per_sec);

            println!(
                "  {:>6}  {:>14}  {:>10}  {:>10}",
                "Thds", "Ops/sec", "Speedup", "Efficiency"
            );
            println!(
                "  {:>6}  {:>14}  {:>10}  {:>10}",
                "----", "-------", "-------", "----------"
            );

            for r in group {
                let speedup = r.ops_per_sec / base_ops;
                let efficiency = speedup / r.threads as f64 * 100.0;
                println!(
                    "  {:>6}  {:>14}  {:>9.2}×  {:>9.1}%",
                    r.threads,
                    format_ops(r.ops_per_sec),
                    speedup,
                    efficiency,
                );
            }
        }
    }

    /// Write results to a CSV file.
    pub fn write_csv(results: &[BenchmarkResult], path: &str) -> std::io::Result<()> {
        let mut f = std::fs::File::create(path)?;
        writeln!(
            f,
            "workload,threads,value_size_bytes,distribution,context,total_ops,reads,writes,rmws,elapsed_secs,ops_per_sec,throughput_mb_s,p50_ns,p99_ns,p999_ns"
        )?;
        for r in results {
            let ctx = if r.safe_context { "safe" } else { "unsafe" };
            writeln!(
                f,
                "{},{},{},{},{},{},{},{},{},{:.3},{:.0},{:.1},{:.0},{:.0},{:.0}",
                r.workload,
                r.threads,
                r.value_size,
                r.distribution,
                ctx,
                r.total_ops,
                r.reads,
                r.writes,
                r.rmws,
                r.elapsed.as_secs_f64(),
                r.ops_per_sec,
                r.throughput_mb_s,
                r.p50_ns,
                r.p99_ns,
                r.p999_ns,
            )?;
        }
        Ok(())
    }
}

/// Format operations per second for human readability.
fn format_ops(ops: f64) -> String {
    if ops >= 1_000_000_000.0 {
        format!("{:.2}B", ops / 1_000_000_000.0)
    } else if ops >= 1_000_000.0 {
        format!("{:.2}M", ops / 1_000_000.0)
    } else if ops >= 1_000.0 {
        format!("{:.1}K", ops / 1_000.0)
    } else {
        format!("{:.0}", ops)
    }
}
