//! Stress test for parallel FUSE mounts.
//!
//! This test verifies that multiple FUSE mounts can be created and destroyed
//! in parallel without hanging or resource leaks.

mod common;

use common::{cleanup, unique_paths, FuseMount};
use std::fs;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

/// Test that we can create and destroy many FUSE mounts in parallel.
/// This catches resource leaks, cleanup issues, and deadlocks.
#[test]
fn test_parallel_mount_stress() {
    const NUM_THREADS: usize = 8;
    const ITERATIONS_PER_THREAD: usize = 5;

    let success_count = Arc::new(AtomicUsize::new(0));
    let failure_count = Arc::new(AtomicUsize::new(0));

    let start = Instant::now();

    let handles: Vec<_> = (0..NUM_THREADS)
        .map(|thread_id| {
            let success = Arc::clone(&success_count);
            let failure = Arc::clone(&failure_count);

            thread::spawn(move || {
                for iter in 0..ITERATIONS_PER_THREAD {
                    let prefix = format!("stress-t{}-i{}", thread_id, iter);
                    let (data_dir, mount_dir) = unique_paths(&prefix);

                    // Create mount
                    let fuse = FuseMount::new(&data_dir, &mount_dir, 1);

                    // Do some operations
                    let test_file = fuse.mount_path().join("test.txt");
                    if fs::write(&test_file, format!("thread {} iter {}", thread_id, iter)).is_ok()
                    {
                        if fs::read_to_string(&test_file).is_ok() {
                            let _ = fs::remove_file(&test_file);
                            success.fetch_add(1, Ordering::SeqCst);
                        } else {
                            failure.fetch_add(1, Ordering::SeqCst);
                        }
                    } else {
                        failure.fetch_add(1, Ordering::SeqCst);
                    }

                    // Drop mount (cleanup)
                    drop(fuse);
                    cleanup(&data_dir, &mount_dir);
                }
            })
        })
        .collect();

    // Wait for all threads with timeout
    let timeout = Duration::from_secs(60);
    for handle in handles {
        let remaining = timeout.saturating_sub(start.elapsed());
        if remaining.is_zero() {
            panic!("Stress test timed out after {:?}", timeout);
        }

        // Can't use join_timeout in std, so just join and rely on test timeout
        handle.join().expect("thread panicked");
    }

    let elapsed = start.elapsed();
    let total_ops = NUM_THREADS * ITERATIONS_PER_THREAD;
    let successes = success_count.load(Ordering::SeqCst);
    let failures = failure_count.load(Ordering::SeqCst);

    eprintln!(
        "Stress test completed: {} successes, {} failures, {:?} elapsed ({:.1} mounts/sec)",
        successes,
        failures,
        elapsed,
        total_ops as f64 / elapsed.as_secs_f64()
    );

    assert_eq!(failures, 0, "Some operations failed");
    assert_eq!(
        successes, total_ops,
        "Not all operations completed successfully"
    );
}

/// Test rapid mount/unmount cycles on a single thread.
/// This catches cleanup issues that only manifest under rapid cycling.
#[test]
fn test_rapid_mount_unmount_cycles() {
    const CYCLES: usize = 20;

    let start = Instant::now();

    for i in 0..CYCLES {
        let prefix = format!("rapid-{}", i);
        let (data_dir, mount_dir) = unique_paths(&prefix);

        let fuse = FuseMount::new(&data_dir, &mount_dir, 1);

        // Quick operation
        let test_file = fuse.mount_path().join("test.txt");
        fs::write(&test_file, "rapid test").expect("write");
        let content = fs::read_to_string(&test_file).expect("read");
        assert_eq!(content, "rapid test");
        fs::remove_file(&test_file).expect("remove");

        drop(fuse);
        cleanup(&data_dir, &mount_dir);
    }

    let elapsed = start.elapsed();
    eprintln!(
        "Rapid cycle test: {} cycles in {:?} ({:.1} cycles/sec)",
        CYCLES,
        elapsed,
        CYCLES as f64 / elapsed.as_secs_f64()
    );
}

/// Test concurrent operations on multiple mounts simultaneously.
/// All mounts are created first, then operations run in parallel.
#[test]
fn test_concurrent_operations_on_multiple_mounts() {
    const NUM_MOUNTS: usize = 4;
    const OPS_PER_MOUNT: usize = 10;

    // Create all mounts first
    let mut mounts = Vec::new();
    let mut dirs = Vec::new();

    for i in 0..NUM_MOUNTS {
        let prefix = format!("concurrent-{}", i);
        let (data_dir, mount_dir) = unique_paths(&prefix);
        let fuse = FuseMount::new(&data_dir, &mount_dir, 1);
        mounts.push(fuse);
        dirs.push((data_dir, mount_dir));
    }

    let start = Instant::now();

    // Run operations in parallel across all mounts
    let handles: Vec<_> = mounts
        .iter()
        .enumerate()
        .map(|(mount_id, fuse)| {
            let mount_path = fuse.mount_path().to_path_buf();
            thread::spawn(move || {
                for op in 0..OPS_PER_MOUNT {
                    let file = mount_path.join(format!("file-{}.txt", op));
                    fs::write(&file, format!("mount {} op {}", mount_id, op)).expect("write");
                    let content = fs::read_to_string(&file).expect("read");
                    assert!(content.contains(&format!("mount {} op {}", mount_id, op)));
                    fs::remove_file(&file).expect("remove");
                }
            })
        })
        .collect();

    for handle in handles {
        handle.join().expect("operation thread panicked");
    }

    let elapsed = start.elapsed();
    let total_ops = NUM_MOUNTS * OPS_PER_MOUNT;
    eprintln!(
        "Concurrent ops test: {} ops across {} mounts in {:?} ({:.1} ops/sec)",
        total_ops,
        NUM_MOUNTS,
        elapsed,
        total_ops as f64 / elapsed.as_secs_f64()
    );

    // Clean up mounts
    drop(mounts);
    for (data_dir, mount_dir) in dirs {
        cleanup(&data_dir, &mount_dir);
    }
}
