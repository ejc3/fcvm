//! Metrics collection and reporting for fuse-test.
//!
//! Uses the `metrics` crate with `metrics-util` for in-memory collection.

use metrics_util::debugging::{DebugValue, DebuggingRecorder, Snapshotter};
use std::sync::OnceLock;

static SNAPSHOTTER: OnceLock<Snapshotter> = OnceLock::new();

/// Initialize the metrics recorder. Call once at program start.
pub fn init() {
    let recorder = DebuggingRecorder::new();
    let snapshotter = recorder.snapshotter();
    SNAPSHOTTER.set(snapshotter).ok();
    metrics::set_global_recorder(recorder).ok();
}

/// Get a snapshot of all metrics and print them.
pub fn print_snapshot() {
    let Some(snapshotter) = SNAPSHOTTER.get() else {
        eprintln!("[metrics] not initialized");
        return;
    };

    let snapshot = snapshotter.snapshot();

    eprintln!("\n=== Metrics Snapshot ===\n");

    // Collect and sort by key name for consistent output
    let mut entries: Vec<_> = snapshot.into_hashmap().into_iter().collect();
    entries.sort_by(|a, b| a.0.key().name().cmp(b.0.key().name()));

    for (key, (_, _, value)) in entries {
        let name = key.key().name();
        let labels: Vec<_> = key.key().labels().map(|l| format!("{}={}", l.key(), l.value())).collect();
        let label_str = if labels.is_empty() {
            String::new()
        } else {
            format!("{{{}}}", labels.join(","))
        };

        match value {
            DebugValue::Counter(v) => {
                eprintln!("  {}{}: {}", name, label_str, v);
            }
            DebugValue::Gauge(v) => {
                eprintln!("  {}{}: {:.2}", name, label_str, v.into_inner());
            }
            DebugValue::Histogram(samples) => {
                if samples.is_empty() {
                    eprintln!("  {}{}: (no samples)", name, label_str);
                } else {
                    let count = samples.len();
                    // Convert OrderedFloat to f64
                    let values: Vec<f64> = samples.iter().map(|v| v.into_inner()).collect();
                    let sum: f64 = values.iter().sum();
                    let min = values.iter().cloned().fold(f64::INFINITY, f64::min);
                    let max = values.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
                    let avg = sum / count as f64;

                    // Calculate p50, p95, p99
                    let mut sorted = values.clone();
                    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
                    let p50 = sorted[count / 2];
                    let p95 = sorted[(count as f64 * 0.95) as usize];
                    let p99_idx = ((count as f64 * 0.99) as usize).min(count - 1);
                    let p99 = sorted[p99_idx];

                    eprintln!(
                        "  {}{}: count={} min={:.0} avg={:.0} p50={:.0} p95={:.0} p99={:.0} max={:.0}",
                        name, label_str, count, min, avg, p50, p95, p99, max
                    );
                }
            }
        }
    }

    eprintln!();
}

/// Get metrics as a structured summary for programmatic use.
#[allow(dead_code)]
pub struct MetricsSummary {
    pub total_requests: u64,
    pub total_responses: u64,
    pub total_errors: u64,
    pub latency_avg_us: f64,
    pub latency_p99_us: f64,
    pub ops_per_sec: f64,
}

/// Calculate summary from snapshot, given elapsed time.
#[allow(dead_code)]
pub fn get_summary(elapsed_secs: f64) -> Option<MetricsSummary> {
    let snapshotter = SNAPSHOTTER.get()?;
    let snapshot = snapshotter.snapshot();
    let map = snapshot.into_hashmap();

    let mut total_requests = 0u64;
    let mut total_responses = 0u64;
    let mut total_errors = 0u64;
    let mut all_latencies: Vec<f64> = Vec::new();

    for (key, (_, _, value)) in map {
        let name = key.key().name();
        match (name, value) {
            ("server.requests.total", DebugValue::Counter(v)) => total_requests = v,
            ("server.responses.total", DebugValue::Counter(v)) => total_responses = v,
            ("server.errors", DebugValue::Counter(v)) => total_errors += v,
            (n, DebugValue::Histogram(samples)) if n.contains("latency") => {
                all_latencies.extend(samples.iter().map(|v| v.into_inner()));
            }
            _ => {}
        }
    }

    let (latency_avg_us, latency_p99_us) = if all_latencies.is_empty() {
        (0.0, 0.0)
    } else {
        let sum: f64 = all_latencies.iter().sum();
        let avg = sum / all_latencies.len() as f64;
        all_latencies.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let p99_idx = (all_latencies.len() as f64 * 0.99) as usize;
        let p99 = all_latencies[p99_idx.min(all_latencies.len() - 1)];
        (avg, p99)
    };

    let ops_per_sec = if elapsed_secs > 0.0 {
        total_requests as f64 / elapsed_secs
    } else {
        0.0
    };

    Some(MetricsSummary {
        total_requests,
        total_responses,
        total_errors,
        latency_avg_us,
        latency_p99_us,
        ops_per_sec,
    })
}
