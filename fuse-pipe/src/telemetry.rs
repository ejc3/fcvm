//! Telemetry collection for FUSE operation tracing.
//!
//! This module provides span collection and aggregation for analyzing
//! FUSE operation latencies. Spans are collected during operation and
//! can be summarized at the end of a test/benchmark run.
//!
//! # Example
//!
//! ```rust,ignore
//! use fuse_pipe::telemetry::SpanCollector;
//!
//! let collector = SpanCollector::new();
//! // ... run FUSE operations with tracing enabled ...
//! collector.print_summary();
//! ```

use crate::protocol::Span;
use std::sync::{Arc, Mutex};

/// Collects and aggregates trace spans for latency analysis.
#[derive(Debug, Clone)]
pub struct SpanCollector {
    spans: Arc<Mutex<Vec<CollectedSpan>>>,
}

/// A collected span with operation metadata.
#[derive(Debug, Clone)]
pub struct CollectedSpan {
    pub unique: u64,
    pub op_name: String,
    pub span: Span,
}

impl Default for SpanCollector {
    fn default() -> Self {
        Self::new()
    }
}

impl SpanCollector {
    /// Create a new span collector.
    pub fn new() -> Self {
        Self {
            spans: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Record a span with its request ID and operation name.
    pub fn record(&self, unique: u64, op_name: &str, span: Span) {
        if let Ok(mut spans) = self.spans.lock() {
            spans.push(CollectedSpan {
                unique,
                op_name: op_name.to_string(),
                span,
            });
        }
    }

    /// Get the number of collected spans.
    pub fn len(&self) -> usize {
        self.spans.lock().map(|s| s.len()).unwrap_or(0)
    }

    /// Check if no spans have been collected.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Clear all collected spans.
    pub fn clear(&self) {
        if let Ok(mut spans) = self.spans.lock() {
            spans.clear();
        }
    }

    /// Compute latency summary statistics.
    pub fn summary(&self) -> Option<SpanSummary> {
        use std::collections::HashMap;

        let spans = self.spans.lock().ok()?;
        if spans.is_empty() {
            return None;
        }

        let mut total_latencies: Vec<u64> = Vec::with_capacity(spans.len());
        let mut to_server: Vec<u64> = Vec::with_capacity(spans.len());
        let mut server_deser: Vec<u64> = Vec::with_capacity(spans.len());
        let mut server_spawn: Vec<u64> = Vec::with_capacity(spans.len());
        let mut server_fs: Vec<u64> = Vec::with_capacity(spans.len());
        let mut server_chan: Vec<u64> = Vec::with_capacity(spans.len());
        let mut to_client: Vec<u64> = Vec::with_capacity(spans.len());
        let mut client_done: Vec<u64> = Vec::with_capacity(spans.len());

        // Per-operation tracking: op_name -> (total_latencies, fs_latencies)
        let mut by_op: HashMap<String, (Vec<u64>, Vec<u64>)> = HashMap::new();

        for cs in spans.iter() {
            let s = &cs.span;

            // Only include complete spans
            if s.t0 == 0 || s.client_done == 0 {
                continue;
            }

            let total = s.client_done.saturating_sub(s.t0);
            total_latencies.push(total);

            // Track per-operation stats
            let op_entry = by_op
                .entry(cs.op_name.clone())
                .or_insert_with(|| (Vec::new(), Vec::new()));
            op_entry.0.push(total);
            if s.server_fs_done > s.server_spawn {
                op_entry.1.push(s.server_fs_done - s.server_spawn);
            }

            // Individual phases (may be zero if not recorded)
            if s.server_recv > s.t0 {
                to_server.push(s.server_recv - s.t0);
            }
            if s.server_deser > s.server_recv {
                server_deser.push(s.server_deser - s.server_recv);
            }
            if s.server_spawn > s.server_deser {
                server_spawn.push(s.server_spawn - s.server_deser);
            }
            if s.server_fs_done > s.server_spawn {
                server_fs.push(s.server_fs_done - s.server_spawn);
            }
            if s.server_resp_chan > s.server_fs_done {
                server_chan.push(s.server_resp_chan - s.server_fs_done);
            }
            if s.client_recv > s.server_resp_chan {
                to_client.push(s.client_recv - s.server_resp_chan);
            }
            if s.client_done > s.client_recv {
                client_done.push(s.client_done - s.client_recv);
            }
        }

        if total_latencies.is_empty() {
            return None;
        }

        // Build per-operation stats, sorted by count descending
        let mut by_operation: Vec<OperationStats> = by_op
            .into_iter()
            .map(|(op_name, (mut totals, mut fs_times))| {
                let count = totals.len();
                OperationStats {
                    op_name,
                    count,
                    total: compute_stats(&mut totals),
                    server_fs: compute_stats(&mut fs_times),
                }
            })
            .collect();
        by_operation.sort_by(|a, b| b.count.cmp(&a.count));

        Some(SpanSummary {
            count: total_latencies.len(),
            total: compute_stats(&mut total_latencies),
            to_server: compute_stats(&mut to_server),
            server_deser: compute_stats(&mut server_deser),
            server_spawn: compute_stats(&mut server_spawn),
            server_fs: compute_stats(&mut server_fs),
            server_chan: compute_stats(&mut server_chan),
            to_client: compute_stats(&mut to_client),
            client_done: compute_stats(&mut client_done),
            by_operation,
        })
    }

    /// Print a summary of collected spans to stderr.
    pub fn print_summary(&self) {
        match self.summary() {
            Some(summary) => summary.print(),
            None => eprintln!("[telemetry] No spans collected"),
        }
    }

    /// Get JSON representation of the summary.
    pub fn summary_json(&self) -> Option<String> {
        let summary = self.summary()?;
        serde_json::to_string_pretty(&summary).ok()
    }
}

/// Statistics for a latency metric.
#[derive(Debug, Clone, serde::Serialize)]
pub struct LatencyStats {
    /// Number of samples
    pub count: usize,
    /// Minimum latency in nanoseconds
    pub min_ns: u64,
    /// Maximum latency in nanoseconds
    pub max_ns: u64,
    /// Mean latency in nanoseconds
    pub mean_ns: u64,
    /// Median (p50) latency in nanoseconds
    pub p50_ns: u64,
    /// 90th percentile latency in nanoseconds
    pub p90_ns: u64,
    /// 99th percentile latency in nanoseconds
    pub p99_ns: u64,
}

impl LatencyStats {
    /// Format as human-readable string with microseconds.
    pub fn format_us(&self) -> String {
        format!(
            "min={:.1}µs p50={:.1}µs p90={:.1}µs p99={:.1}µs max={:.1}µs (n={})",
            self.min_ns as f64 / 1000.0,
            self.p50_ns as f64 / 1000.0,
            self.p90_ns as f64 / 1000.0,
            self.p99_ns as f64 / 1000.0,
            self.max_ns as f64 / 1000.0,
            self.count,
        )
    }
}

/// Summary of span latencies broken down by phase.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SpanSummary {
    /// Number of complete spans
    pub count: usize,
    /// Total end-to-end latency (t0 → client_done)
    pub total: LatencyStats,
    /// Client → Server (network + serialization overhead)
    pub to_server: LatencyStats,
    /// Server deserialization time
    pub server_deser: LatencyStats,
    /// Server spawn_blocking scheduling delay
    pub server_spawn: LatencyStats,
    /// Server filesystem operation time
    pub server_fs: LatencyStats,
    /// Server response channel wait time
    pub server_chan: LatencyStats,
    /// Server → Client (network + serialization)
    pub to_client: LatencyStats,
    /// Client final processing
    pub client_done: LatencyStats,
    /// Per-operation breakdown (lookup, getattr, read, write, etc.)
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub by_operation: Vec<OperationStats>,
}

/// Statistics for a specific FUSE operation type.
#[derive(Debug, Clone, serde::Serialize)]
pub struct OperationStats {
    /// Operation name (lookup, getattr, read, write, etc.)
    pub op_name: String,
    /// Number of spans for this operation
    pub count: usize,
    /// Total end-to-end latency for this operation
    pub total: LatencyStats,
    /// Filesystem operation time (the main work)
    pub server_fs: LatencyStats,
}

impl SpanSummary {
    /// Print summary to stderr in a formatted table.
    pub fn print(&self) {
        eprintln!();
        eprintln!("╔══════════════════════════════════════════════════════════════════════════════════════════════════╗");
        eprintln!("║                               FUSE Operation Latency Summary                                     ║");
        eprintln!("╠══════════════════════════════════════════════════════════════════════════════════════════════════╣");
        eprintln!("║  Spans collected: {:>6}                                                                         ║", self.count);
        eprintln!("╠══════════════════════════════════════════════════════════════════════════════════════════════════╣");
        eprintln!("║  Phase            │     min │     p50 │     p90 │     p99 │     max │    mean │  count           ║");
        eprintln!("╠═══════════════════╪═════════╪═════════╪═════════╪═════════╪═════════╪═════════╪══════════════════╣");

        self.print_row("Total (e2e)", &self.total);
        eprintln!("╠───────────────────┼─────────┼─────────┼─────────┼─────────┼─────────┼─────────┼──────────────────╣");
        self.print_row("→ to_server", &self.to_server);
        self.print_row("  deser", &self.server_deser);
        self.print_row("  spawn", &self.server_spawn);
        self.print_row("  fs_op", &self.server_fs);
        self.print_row("  chan", &self.server_chan);
        self.print_row("← to_client", &self.to_client);
        self.print_row("  done", &self.client_done);

        eprintln!("╚═══════════════════╧═════════╧═════════╧═════════╧═════════╧═════════╧═════════╧══════════════════╝");

        // Print breakdown percentages
        if self.total.p50_ns > 0 {
            eprintln!();
            eprintln!("Latency Breakdown (based on p50):");
            let total = self.total.p50_ns as f64;
            eprintln!(
                "  → to_server:  {:5.1}%  (client serialize + network)",
                self.to_server.p50_ns as f64 / total * 100.0
            );
            eprintln!(
                "    deser:      {:5.1}%  (server deserialize)",
                self.server_deser.p50_ns as f64 / total * 100.0
            );
            eprintln!(
                "    spawn:      {:5.1}%  (spawn_blocking scheduling)",
                self.server_spawn.p50_ns as f64 / total * 100.0
            );
            eprintln!(
                "    fs_op:      {:5.1}%  (filesystem operation)",
                self.server_fs.p50_ns as f64 / total * 100.0
            );
            eprintln!(
                "    chan:       {:5.1}%  (response channel wait)",
                self.server_chan.p50_ns as f64 / total * 100.0
            );
            eprintln!(
                "  ← to_client:  {:5.1}%  (server serialize + network)",
                self.to_client.p50_ns as f64 / total * 100.0
            );
            eprintln!(
                "    done:       {:5.1}%  (client finalize)",
                self.client_done.p50_ns as f64 / total * 100.0
            );
        }

        // Print per-operation breakdown
        if !self.by_operation.is_empty() {
            eprintln!();
            eprintln!("Per-Operation Latency (sorted by count):");
            eprintln!(
                "  {:12} {:>8} {:>10} {:>10} {:>10} {:>10}",
                "Operation", "Count", "p50 Total", "p99 Total", "p50 fs_op", "p99 fs_op"
            );
            eprintln!(
                "  {:─<12} {:─>8} {:─>10} {:─>10} {:─>10} {:─>10}",
                "", "", "", "", "", ""
            );

            let fmt_us = |ns: u64| -> String {
                let us = ns as f64 / 1000.0;
                if us >= 1000.0 {
                    format!("{:.1}ms", us / 1000.0)
                } else {
                    format!("{:.1}µs", us)
                }
            };

            for op in &self.by_operation {
                eprintln!(
                    "  {:12} {:>8} {:>10} {:>10} {:>10} {:>10}",
                    op.op_name,
                    op.count,
                    fmt_us(op.total.p50_ns),
                    fmt_us(op.total.p99_ns),
                    fmt_us(op.server_fs.p50_ns),
                    fmt_us(op.server_fs.p99_ns)
                );
            }
        }
    }

    fn print_row(&self, label: &str, stats: &LatencyStats) {
        // Format values in microseconds with appropriate precision
        let fmt_us = |ns: u64| -> String {
            let us = ns as f64 / 1000.0;
            if us >= 1000.0 {
                format!("{:.0}ms", us / 1000.0)
            } else if us >= 100.0 {
                format!("{:.0}µs", us)
            } else if us >= 10.0 {
                format!("{:.1}µs", us)
            } else {
                format!("{:.2}µs", us)
            }
        };

        eprintln!(
            "║  {:16} │ {:>7} │ {:>7} │ {:>7} │ {:>7} │ {:>7} │ {:>7} │ {:>6}           ║",
            label,
            fmt_us(stats.min_ns),
            fmt_us(stats.p50_ns),
            fmt_us(stats.p90_ns),
            fmt_us(stats.p99_ns),
            fmt_us(stats.max_ns),
            fmt_us(stats.mean_ns),
            stats.count,
        );
    }
}

/// Compute statistics from a vector of latencies (in nanoseconds).
fn compute_stats(values: &mut [u64]) -> LatencyStats {
    if values.is_empty() {
        return LatencyStats {
            count: 0,
            min_ns: 0,
            max_ns: 0,
            mean_ns: 0,
            p50_ns: 0,
            p90_ns: 0,
            p99_ns: 0,
        };
    }

    values.sort_unstable();

    let count = values.len();
    let min_ns = values[0];
    let max_ns = values[count - 1];
    let sum: u64 = values.iter().sum();
    let mean_ns = sum / count as u64;

    let percentile = |p: f64| -> u64 {
        let idx = ((count as f64 * p) as usize)
            .saturating_sub(1)
            .min(count - 1);
        values[idx]
    };

    LatencyStats {
        count,
        min_ns,
        max_ns,
        mean_ns,
        p50_ns: percentile(0.50),
        p90_ns: percentile(0.90),
        p99_ns: percentile(0.99),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_collector() {
        let collector = SpanCollector::new();
        assert!(collector.is_empty());
        assert!(collector.summary().is_none());
    }

    #[test]
    fn test_collect_spans() {
        let collector = SpanCollector::new();

        let span = Span {
            t0: 1000,
            server_recv: 1100,
            server_deser: 1150,
            server_spawn: 1200,
            server_fs_done: 1500,
            server_resp_chan: 1550,
            client_recv: 1650,
            client_done: 1700,
        };

        collector.record(1, "getattr", span.clone());
        collector.record(2, "read", span.clone());

        assert_eq!(collector.len(), 2);

        let summary = collector.summary().unwrap();
        assert_eq!(summary.count, 2);
        assert_eq!(summary.total.p50_ns, 700); // 1700 - 1000

        // Check per-operation breakdown
        assert_eq!(summary.by_operation.len(), 2);
        let getattr_op = summary
            .by_operation
            .iter()
            .find(|op| op.op_name == "getattr")
            .unwrap();
        assert_eq!(getattr_op.count, 1);
    }

    #[test]
    fn test_percentiles() {
        let mut values: Vec<u64> = (1..=100).collect();
        let stats = compute_stats(&mut values);

        assert_eq!(stats.count, 100);
        assert_eq!(stats.min_ns, 1);
        assert_eq!(stats.max_ns, 100);
        assert_eq!(stats.p50_ns, 50);
        assert_eq!(stats.p90_ns, 90);
        assert_eq!(stats.p99_ns, 99);
    }
}
