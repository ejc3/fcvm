//! End-to-end throughput benchmarks.
//!
//! These benchmarks require the server module to be implemented.
//! Currently a placeholder.

use criterion::{criterion_group, criterion_main, Criterion};

fn bench_throughput_placeholder(c: &mut Criterion) {
    c.bench_function("throughput_placeholder", |b| {
        b.iter(|| {
            // Placeholder - will be implemented after server module
            std::hint::black_box(42)
        })
    });
}

criterion_group!(benches, bench_throughput_placeholder);
criterion_main!(benches);
