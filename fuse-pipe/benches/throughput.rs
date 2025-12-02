//! End-to-end throughput benchmarks comparing host filesystem vs FUSE passthrough.
//!
//! Tests actual file I/O performance with varying concurrency levels.
//!
//! See `fuse-pipe/TESTING.md` for complete testing documentation.

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::Duration;

// Include the shared fixture module
#[path = "../tests/common/mod.rs"]
mod common;

use common::{increase_ulimit, setup_test_data, FuseMount};

const FILE_SIZE: usize = 4096; // 4KB files
const NUM_FILES: usize = 1024; // More files for higher concurrency

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
                    let mut f = OpenOptions::new()
                        .write(true)
                        .open(&path)
                        .unwrap();
                    f.write_all(&data).unwrap();
                    f.sync_all().unwrap(); // Ensure data hits disk
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

    let data_dir = PathBuf::from("/tmp/fuse-bench-data");
    let mount_dir = PathBuf::from("/tmp/fuse-bench-mount");

    // Cleanup any previous runs
    let _ = fs::remove_dir_all(&data_dir);
    let _ = Command::new("fusermount")
        .args(["-u", mount_dir.to_str().unwrap()])
        .status();
    let _ = Command::new("fusermount3")
        .args(["-u", mount_dir.to_str().unwrap()])
        .status();
    let _ = fs::remove_dir_all(&mount_dir);

    // Setup test data
    setup_test_data(&data_dir, NUM_FILES, FILE_SIZE);

    let mut group = c.benchmark_group("parallel_reads");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(5));

    const NUM_WORKERS: usize = 256; // Fixed worker count (saturates at this level)

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
        // Cleanup previous mount
        let _ = Command::new("fusermount")
            .args(["-u", mount_dir.to_str().unwrap()])
            .status();
        let _ = Command::new("fusermount3")
            .args(["-u", mount_dir.to_str().unwrap()])
            .status();
        thread::sleep(Duration::from_millis(100));

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
    let _ = fs::remove_dir_all(&data_dir);
    let _ = fs::remove_dir_all(&mount_dir);
}

fn bench_parallel_writes(c: &mut Criterion) {
    increase_ulimit();

    let data_dir = PathBuf::from("/tmp/fuse-bench-write-data");
    let mount_dir = PathBuf::from("/tmp/fuse-bench-write-mount");

    // Cleanup any previous runs
    let _ = fs::remove_dir_all(&data_dir);
    let _ = Command::new("fusermount")
        .args(["-u", mount_dir.to_str().unwrap()])
        .status();
    let _ = Command::new("fusermount3")
        .args(["-u", mount_dir.to_str().unwrap()])
        .status();
    let _ = fs::remove_dir_all(&mount_dir);

    // Setup test data
    setup_test_data(&data_dir, NUM_FILES, FILE_SIZE);

    let mut group = c.benchmark_group("parallel_writes");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(5));

    const NUM_WORKERS: usize = 256;

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
        let _ = Command::new("fusermount")
            .args(["-u", mount_dir.to_str().unwrap()])
            .status();
        let _ = Command::new("fusermount3")
            .args(["-u", mount_dir.to_str().unwrap()])
            .status();
        thread::sleep(Duration::from_millis(100));

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
    let _ = fs::remove_dir_all(&data_dir);
    let _ = fs::remove_dir_all(&mount_dir);
}

criterion_group!(
    benches,
    bench_parallel_reads,
    bench_parallel_writes,
);

criterion_main!(benches);
