//! Test to reproduce the unmount race condition with multi-reader FUSE.
//!
//! This test does heavy parallel I/O and then immediately unmounts to trigger
//! the race where reader threads get ECONNABORTED before destroy() is called.
//!
//! Run WITHOUT sudo: `cargo test --release -p fuse-pipe --test test_unmount_race`

mod common;

use std::fs::{self, File};
use std::io::{Read, Write};
use std::thread;

use common::{cleanup, unique_paths, FuseMount};

/// Reproduce the unmount race with heavy I/O.
///
/// The issue: with multiple readers, when we unmount after heavy I/O,
/// reader threads may get ECONNABORTED before any FuseClient::destroy()
/// is called, causing ERROR logs.
#[test]
fn test_unmount_after_heavy_io() {
    // Use many readers to increase chance of race
    const NUM_READERS: usize = 16;
    const NUM_FILES: usize = 100;
    const NUM_WORKERS: usize = 32;
    const FILE_SIZE: usize = 4096;

    let (data_dir, mount_dir) = unique_paths("fuse-unmount-race");

    // Setup test files
    fs::create_dir_all(&data_dir).expect("create data dir");
    for i in 0..NUM_FILES {
        let path = data_dir.join(format!("file_{}.dat", i));
        let mut f = File::create(&path).expect("create file");
        f.write_all(&vec![0x42u8; FILE_SIZE]).expect("write file");
    }

    // Mount with many readers
    let fuse = FuseMount::new(&data_dir, &mount_dir, NUM_READERS);

    // Do heavy parallel I/O
    let mount = fuse.mount_path().to_path_buf();
    let handles: Vec<_> = (0..NUM_WORKERS)
        .map(|worker_id| {
            let m = mount.clone();
            thread::spawn(move || {
                let mut buf = vec![0u8; FILE_SIZE];
                // Each worker reads all files multiple times
                for _round in 0..10 {
                    for i in 0..NUM_FILES {
                        let path = m.join(format!("file_{}.dat", i % NUM_FILES));
                        if let Ok(mut f) = File::open(&path) {
                            let _ = f.read(&mut buf);
                        }
                    }
                }
                eprintln!("[worker {}] done", worker_id);
            })
        })
        .collect();

    // Wait for all I/O to complete
    for h in handles {
        h.join().unwrap();
    }

    eprintln!("[test] I/O complete, dropping fuse mount...");

    // Drop triggers unmount - this is where the race happens
    drop(fuse);

    eprintln!("[test] unmount complete");

    cleanup(&data_dir, &mount_dir);
}

/// Run the test multiple times to increase chance of hitting the race.
#[test]
fn test_unmount_race_repeated() {
    for i in 0..5 {
        eprintln!("\n=== Iteration {} ===", i);
        test_unmount_after_heavy_io_inner(i);
    }
}

fn test_unmount_after_heavy_io_inner(iteration: usize) {
    const NUM_READERS: usize = 16;
    const NUM_FILES: usize = 50;
    const NUM_WORKERS: usize = 16;
    const FILE_SIZE: usize = 4096;

    let (data_dir, mount_dir) = unique_paths(&format!("fuse-race-{}", iteration));

    fs::create_dir_all(&data_dir).expect("create data dir");
    for i in 0..NUM_FILES {
        let path = data_dir.join(format!("file_{}.dat", i));
        let mut f = File::create(&path).expect("create file");
        f.write_all(&vec![0x42u8; FILE_SIZE]).expect("write file");
    }

    let fuse = FuseMount::new(&data_dir, &mount_dir, NUM_READERS);
    let mount = fuse.mount_path().to_path_buf();

    let handles: Vec<_> = (0..NUM_WORKERS)
        .map(|_worker_id| {
            let m = mount.clone();
            thread::spawn(move || {
                let mut buf = vec![0u8; FILE_SIZE];
                for _round in 0..5 {
                    for i in 0..NUM_FILES {
                        let path = m.join(format!("file_{}.dat", i));
                        if let Ok(mut f) = File::open(&path) {
                            let _ = f.read(&mut buf);
                        }
                    }
                }
            })
        })
        .collect();

    for h in handles {
        h.join().unwrap();
    }

    drop(fuse);
    cleanup(&data_dir, &mount_dir);
}
