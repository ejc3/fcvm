//! spawn_blocking latency microbenchmark
//!
//! Run with: cargo run --release --example spawn_blocking_bench

use std::time::{Duration, Instant};

fn main() {
    // Build tokio runtime with same settings as server
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .max_blocking_threads(1024)
        .build()
        .unwrap();

    rt.block_on(async {
        println!("=== spawn_blocking Latency Benchmark ===\n");

        let iterations = 1000;

        // Warmup
        for _ in 0..100 {
            tokio::task::spawn_blocking(|| std::fs::metadata("/tmp"))
                .await
                .unwrap()
                .unwrap();
        }

        // 1. Direct stat (no spawn_blocking)
        let mut direct_latencies = Vec::with_capacity(iterations);
        for _ in 0..iterations {
            let start = Instant::now();
            let _ = std::fs::metadata("/tmp");
            direct_latencies.push(start.elapsed());
        }
        direct_latencies.sort();
        let avg = direct_latencies.iter().sum::<Duration>() / iterations as u32;
        println!("1. Direct stat() (no async):");
        println!(
            "   Avg: {:>8.2} µs",
            avg.as_nanos() as f64 / 1000.0
        );
        println!(
            "   P50: {:>8.2} µs",
            direct_latencies[iterations / 2].as_nanos() as f64 / 1000.0
        );
        println!();

        // 2. spawn_blocking with stat
        let mut spawn_latencies = Vec::with_capacity(iterations);
        for _ in 0..iterations {
            let start = Instant::now();
            let _ = tokio::task::spawn_blocking(|| std::fs::metadata("/tmp")).await;
            spawn_latencies.push(start.elapsed());
        }
        spawn_latencies.sort();
        let avg = spawn_latencies.iter().sum::<Duration>() / iterations as u32;
        println!("2. spawn_blocking + stat():");
        println!(
            "   Avg: {:>8.2} µs",
            avg.as_nanos() as f64 / 1000.0
        );
        println!(
            "   P50: {:>8.2} µs",
            spawn_latencies[iterations / 2].as_nanos() as f64 / 1000.0
        );
        println!(
            "   P99: {:>8.2} µs",
            spawn_latencies[iterations * 99 / 100].as_nanos() as f64 / 1000.0
        );
        println!(
            "   Max: {:>8.2} µs",
            spawn_latencies[iterations - 1].as_nanos() as f64 / 1000.0
        );
        println!();

        // 3. Just spawn_blocking, no work
        let mut empty_latencies = Vec::with_capacity(iterations);
        for _ in 0..iterations {
            let start = Instant::now();
            let _ = tokio::task::spawn_blocking(|| {}).await;
            empty_latencies.push(start.elapsed());
        }
        empty_latencies.sort();
        let avg = empty_latencies.iter().sum::<Duration>() / iterations as u32;
        println!("3. spawn_blocking only (no work):");
        println!(
            "   Avg: {:>8.2} µs",
            avg.as_nanos() as f64 / 1000.0
        );
        println!(
            "   P50: {:>8.2} µs",
            empty_latencies[iterations / 2].as_nanos() as f64 / 1000.0
        );
        println!();

        // 4. tokio::spawn (async task, not blocking)
        let mut async_latencies = Vec::with_capacity(iterations);
        for _ in 0..iterations {
            let start = Instant::now();
            let _ = tokio::spawn(async {}).await;
            async_latencies.push(start.elapsed());
        }
        async_latencies.sort();
        let avg = async_latencies.iter().sum::<Duration>() / iterations as u32;
        println!("4. tokio::spawn (async task, no work):");
        println!(
            "   Avg: {:>8.2} µs",
            avg.as_nanos() as f64 / 1000.0
        );
        println!(
            "   P50: {:>8.2} µs",
            async_latencies[iterations / 2].as_nanos() as f64 / 1000.0
        );
        println!();

        let spawn_overhead = spawn_latencies[iterations / 2].as_nanos() as f64
            - direct_latencies[iterations / 2].as_nanos() as f64;
        println!(
            "Overhead from spawn_blocking scheduling: ~{:.2} µs",
            spawn_overhead / 1000.0
        );
        println!();
        println!("NOTE: If spawn_blocking overhead is high (>10µs), the problem is");
        println!("thread pool wake-up latency, not the actual work.");
    });
}
