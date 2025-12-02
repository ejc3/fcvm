//! Single-operation latency benchmarks for FUSE passthrough.
//!
//! Tests individual FUSE operations to identify bottlenecks.
//!
//! See `fuse-pipe/TESTING.md` for complete testing documentation.

use criterion::{criterion_group, criterion_main, Criterion};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

// Include the shared fixture module
#[path = "../tests/common/mod.rs"]
mod common;

use common::FuseMount;

const FILE_SIZE: usize = 4096; // 4KB test file

/// Setup test files in the data directory
fn setup_test_files(dir: &PathBuf) {
    fs::create_dir_all(dir).unwrap();

    // Create test file
    let test_file = dir.join("test.dat");
    let mut f = File::create(&test_file).unwrap();
    f.write_all(&vec![0x42u8; FILE_SIZE]).unwrap();
    f.sync_all().unwrap();

    // Create subdirectory with files for readdir
    let subdir = dir.join("subdir");
    fs::create_dir_all(&subdir).unwrap();
    for i in 0..10 {
        let path = subdir.join(format!("file_{}.txt", i));
        fs::write(&path, format!("content {}", i)).unwrap();
    }
}

fn cleanup(data_dir: &PathBuf, mount_dir: &PathBuf) {
    let _ = Command::new("fusermount")
        .args(["-u", mount_dir.to_str().unwrap()])
        .status();
    let _ = Command::new("fusermount3")
        .args(["-u", mount_dir.to_str().unwrap()])
        .status();
    let _ = fs::remove_dir_all(data_dir);
    let _ = fs::remove_dir_all(mount_dir);
}

fn bench_getattr(c: &mut Criterion) {
    let data_dir = PathBuf::from("/tmp/fuse-ops-data-getattr");
    let mount_dir = PathBuf::from("/tmp/fuse-ops-mount-getattr");

    cleanup(&data_dir, &mount_dir);
    setup_test_files(&data_dir);

    let mut group = c.benchmark_group("single_op/getattr");
    group.sample_size(100);

    // Host filesystem baseline
    let test_file = data_dir.join("test.dat");
    group.bench_function("host_fs", |b| {
        b.iter(|| {
            let _ = fs::metadata(&test_file).unwrap();
        })
    });

    // FUSE with 256 readers (our recommended default)
    let fuse = FuseMount::new(&data_dir, &mount_dir, 256);
    let fuse_file = fuse.mount_path().join("test.dat");
    group.bench_function("fuse_256_readers", |b| {
        b.iter(|| {
            let _ = fs::metadata(&fuse_file).unwrap();
        })
    });

    drop(fuse);
    group.finish();
    cleanup(&data_dir, &mount_dir);
}

fn bench_lookup(c: &mut Criterion) {
    let data_dir = PathBuf::from("/tmp/fuse-ops-data-lookup");
    let mount_dir = PathBuf::from("/tmp/fuse-ops-mount-lookup");

    cleanup(&data_dir, &mount_dir);
    setup_test_files(&data_dir);

    let mut group = c.benchmark_group("single_op/lookup");
    group.sample_size(100);

    // Host filesystem baseline - lookup via exists()
    let test_file = data_dir.join("test.dat");
    group.bench_function("host_fs", |b| {
        b.iter(|| {
            let _ = test_file.exists();
        })
    });

    // FUSE
    let fuse = FuseMount::new(&data_dir, &mount_dir, 256);
    let fuse_file = fuse.mount_path().join("test.dat");
    group.bench_function("fuse_256_readers", |b| {
        b.iter(|| {
            let _ = fuse_file.exists();
        })
    });

    drop(fuse);
    group.finish();
    cleanup(&data_dir, &mount_dir);
}

fn bench_open_close(c: &mut Criterion) {
    let data_dir = PathBuf::from("/tmp/fuse-ops-data-open");
    let mount_dir = PathBuf::from("/tmp/fuse-ops-mount-open");

    cleanup(&data_dir, &mount_dir);
    setup_test_files(&data_dir);

    let mut group = c.benchmark_group("single_op/open_close");
    group.sample_size(100);

    // Host filesystem baseline
    let test_file = data_dir.join("test.dat");
    group.bench_function("host_fs", |b| {
        b.iter(|| {
            let f = File::open(&test_file).unwrap();
            drop(f);
        })
    });

    // FUSE
    let fuse = FuseMount::new(&data_dir, &mount_dir, 256);
    let fuse_file = fuse.mount_path().join("test.dat");
    group.bench_function("fuse_256_readers", |b| {
        b.iter(|| {
            let f = File::open(&fuse_file).unwrap();
            drop(f);
        })
    });

    drop(fuse);
    group.finish();
    cleanup(&data_dir, &mount_dir);
}

fn bench_read_4kb(c: &mut Criterion) {
    let data_dir = PathBuf::from("/tmp/fuse-ops-data-read");
    let mount_dir = PathBuf::from("/tmp/fuse-ops-mount-read");

    cleanup(&data_dir, &mount_dir);
    setup_test_files(&data_dir);

    let mut group = c.benchmark_group("single_op/read_4kb");
    group.sample_size(100);

    // Host filesystem baseline
    let test_file = data_dir.join("test.dat");
    group.bench_function("host_fs", |b| {
        let mut f = File::open(&test_file).unwrap();
        let mut buf = vec![0u8; FILE_SIZE];
        b.iter(|| {
            f.seek(SeekFrom::Start(0)).unwrap();
            f.read_exact(&mut buf).unwrap();
        })
    });

    // FUSE
    let fuse = FuseMount::new(&data_dir, &mount_dir, 256);
    let fuse_file = fuse.mount_path().join("test.dat");
    group.bench_function("fuse_256_readers", |b| {
        let mut f = File::open(&fuse_file).unwrap();
        let mut buf = vec![0u8; FILE_SIZE];
        b.iter(|| {
            f.seek(SeekFrom::Start(0)).unwrap();
            f.read_exact(&mut buf).unwrap();
        })
    });

    drop(fuse);
    group.finish();
    cleanup(&data_dir, &mount_dir);
}

fn bench_write_4kb(c: &mut Criterion) {
    let data_dir = PathBuf::from("/tmp/fuse-ops-data-write");
    let mount_dir = PathBuf::from("/tmp/fuse-ops-mount-write");

    cleanup(&data_dir, &mount_dir);
    setup_test_files(&data_dir);

    let mut group = c.benchmark_group("single_op/write_4kb");
    group.sample_size(100);

    let data = vec![0x42u8; FILE_SIZE];

    // Host filesystem baseline (no sync)
    let test_file = data_dir.join("test.dat");
    group.bench_function("host_fs", |b| {
        let mut f = OpenOptions::new().write(true).open(&test_file).unwrap();
        b.iter(|| {
            f.seek(SeekFrom::Start(0)).unwrap();
            f.write_all(&data).unwrap();
        })
    });

    // FUSE (no sync)
    let fuse = FuseMount::new(&data_dir, &mount_dir, 256);
    let fuse_file = fuse.mount_path().join("test.dat");
    group.bench_function("fuse_256_readers", |b| {
        let mut f = OpenOptions::new().write(true).open(&fuse_file).unwrap();
        b.iter(|| {
            f.seek(SeekFrom::Start(0)).unwrap();
            f.write_all(&data).unwrap();
        })
    });

    drop(fuse);
    group.finish();
    cleanup(&data_dir, &mount_dir);
}

fn bench_readdir(c: &mut Criterion) {
    let data_dir = PathBuf::from("/tmp/fuse-ops-data-readdir");
    let mount_dir = PathBuf::from("/tmp/fuse-ops-mount-readdir");

    cleanup(&data_dir, &mount_dir);
    setup_test_files(&data_dir);

    let mut group = c.benchmark_group("single_op/readdir");
    group.sample_size(100);

    // Host filesystem baseline
    let subdir = data_dir.join("subdir");
    group.bench_function("host_fs", |b| {
        b.iter(|| {
            let entries: Vec<_> = fs::read_dir(&subdir)
                .unwrap()
                .filter_map(|e| e.ok())
                .collect();
            assert_eq!(entries.len(), 10);
        })
    });

    // FUSE
    let fuse = FuseMount::new(&data_dir, &mount_dir, 256);
    let fuse_subdir = fuse.mount_path().join("subdir");
    group.bench_function("fuse_256_readers", |b| {
        b.iter(|| {
            let entries: Vec<_> = fs::read_dir(&fuse_subdir)
                .unwrap()
                .filter_map(|e| e.ok())
                .collect();
            assert_eq!(entries.len(), 10);
        })
    });

    drop(fuse);
    group.finish();
    cleanup(&data_dir, &mount_dir);
}

fn bench_create_unlink(c: &mut Criterion) {
    let data_dir = PathBuf::from("/tmp/fuse-ops-data-create");
    let mount_dir = PathBuf::from("/tmp/fuse-ops-mount-create");

    cleanup(&data_dir, &mount_dir);
    fs::create_dir_all(&data_dir).unwrap();

    let mut group = c.benchmark_group("single_op/create_unlink");
    group.sample_size(100);

    let counter = AtomicU64::new(0);

    // Host filesystem baseline
    group.bench_function("host_fs", |b| {
        b.iter(|| {
            let n = counter.fetch_add(1, Ordering::SeqCst);
            let path = data_dir.join(format!("tmp_{}.txt", n));
            File::create(&path).unwrap();
            fs::remove_file(&path).unwrap();
        })
    });

    // FUSE
    let fuse = FuseMount::new(&data_dir, &mount_dir, 256);
    group.bench_function("fuse_256_readers", |b| {
        b.iter(|| {
            let n = counter.fetch_add(1, Ordering::SeqCst);
            let path = fuse.mount_path().join(format!("tmp_{}.txt", n));
            File::create(&path).unwrap();
            fs::remove_file(&path).unwrap();
        })
    });

    drop(fuse);
    group.finish();
    cleanup(&data_dir, &mount_dir);
}

criterion_group!(
    benches,
    bench_getattr,
    bench_lookup,
    bench_open_close,
    bench_read_4kb,
    bench_write_4kb,
    bench_readdir,
    bench_create_unlink,
);

criterion_main!(benches);
