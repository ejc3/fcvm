//! End-to-end throughput benchmarks comparing host filesystem vs FUSE passthrough.
//!
//! Tests actual file I/O performance with varying concurrency levels.
//!
//! See `fuse-pipe/TESTING.md` for complete testing documentation.

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;
use std::thread;
use std::time::Duration;

// Include the shared fixture module
#[path = "../tests/common/mod.rs"]
mod common;

use common::{cleanup, increase_ulimit, setup_test_data, unique_paths, FuseMount};

const FILE_SIZE: usize = 4096; // 4KB files
const NUM_FILES: usize = 1024; // More files for higher concurrency
const NUM_WORKERS: usize = 256; // Fixed worker count (saturates at this level)

/// Run parallel write benchmark on a directory
fn parallel_write_bench(dir: &Path, num_workers: usize, ops_per_worker: usize) -> Duration {
    let dir = dir.to_path_buf();
    let start = std::time::Instant::now();

    let handles: Vec<_> = (0..num_workers)
        .map(|worker_id| {
            let dir = dir.clone();
            thread::spawn(move || {
                let data = vec![0x42u8; FILE_SIZE];
                for i in 0..ops_per_worker {
                    let file_idx = (worker_id * ops_per_worker + i) % NUM_FILES;
                    let path = dir.join(format!("file_{}.dat", file_idx));
                    let mut f = OpenOptions::new().write(true).open(&path).unwrap();
                    f.write_all(&data).unwrap();
                    f.sync_all().unwrap();
                }
            })
        })
        .collect();

    for h in handles {
        h.join().unwrap();
    }

    start.elapsed()
}

/// Run parallel read benchmark on a directory
fn parallel_read_bench(dir: &Path, num_workers: usize, ops_per_worker: usize) -> Duration {
    let dir = dir.to_path_buf();
    let start = std::time::Instant::now();

    let handles: Vec<_> = (0..num_workers)
        .map(|worker_id| {
            let dir = dir.clone();
            thread::spawn(move || {
                let mut buf = vec![0u8; FILE_SIZE];
                for i in 0..ops_per_worker {
                    let file_idx = (worker_id * ops_per_worker + i) % NUM_FILES;
                    let path = dir.join(format!("file_{}.dat", file_idx));
                    let mut f = File::open(&path).unwrap();
                    f.read_exact(&mut buf).unwrap();
                }
            })
        })
        .collect();

    for h in handles {
        h.join().unwrap();
    }

    start.elapsed()
}

fn bench_parallel_reads(c: &mut Criterion) {
    increase_ulimit();

    let (data_dir, mount_dir) = unique_paths("fuse-bench-read");
    setup_test_data(&data_dir, NUM_FILES, FILE_SIZE);

    let mut group = c.benchmark_group("parallel_reads");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(5));

    // Host filesystem baseline
    group.bench_function("host_fs", |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                total += parallel_read_bench(&data_dir, NUM_WORKERS, 10);
            }
            total
        })
    });

    // Test different FUSE reader counts
    for num_readers in [1, 2, 4, 8, 16, 32, 64, 128, 256] {
        let fuse = FuseMount::new(&data_dir, &mount_dir, num_readers);

        group.bench_with_input(
            BenchmarkId::new("fuse_readers", num_readers),
            &num_readers,
            |b, _| {
                b.iter_custom(|iters| {
                    let mut total = Duration::ZERO;
                    for _ in 0..iters {
                        total += parallel_read_bench(fuse.mount_path(), NUM_WORKERS, 10);
                    }
                    total
                })
            },
        );

        drop(fuse);
    }

    group.finish();
    cleanup(&data_dir, &mount_dir);
}

fn bench_parallel_writes(c: &mut Criterion) {
    increase_ulimit();

    let (data_dir, mount_dir) = unique_paths("fuse-bench-write");
    setup_test_data(&data_dir, NUM_FILES, FILE_SIZE);

    let mut group = c.benchmark_group("parallel_writes");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(5));

    // Host filesystem baseline
    group.bench_function("host_fs", |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                total += parallel_write_bench(&data_dir, NUM_WORKERS, 10);
            }
            total
        })
    });

    // Test different FUSE reader counts
    for num_readers in [1, 4, 16, 64, 256] {
        let fuse = FuseMount::new(&data_dir, &mount_dir, num_readers);

        group.bench_with_input(
            BenchmarkId::new("fuse_readers", num_readers),
            &num_readers,
            |b, _| {
                b.iter_custom(|iters| {
                    let mut total = Duration::ZERO;
                    for _ in 0..iters {
                        total += parallel_write_bench(fuse.mount_path(), NUM_WORKERS, 10);
                    }
                    total
                })
            },
        );

        drop(fuse);
    }

    group.finish();
    cleanup(&data_dir, &mount_dir);
}

criterion_group!(benches, bench_parallel_reads, bench_parallel_writes);

criterion_main!(benches);
