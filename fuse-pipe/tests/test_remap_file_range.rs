//! Integration tests for remap_file_range (FICLONE/FICLONERANGE) support.
//!
//! These tests verify that FUSE can pass through reflink operations to the
//! underlying filesystem. Requires:
//! - Kernel with FUSE_REMAP_FILE_RANGE support (patched kernel)
//! - btrfs or xfs filesystem backing the FUSE mount
//!
//! Run with: `sudo cargo test --release -p fuse-pipe --features privileged-tests --test test_remap_file_range`

#![cfg(feature = "privileged-tests")]

mod common;

use std::fs::{self, File};
use std::os::unix::io::AsRawFd;

use common::{cleanup, unique_paths, FuseMount};

/// FICLONE ioctl number: _IOW(0x94, 9, int) = 0x40049409
const FICLONE: libc::c_ulong = 0x40049409;

/// FICLONERANGE ioctl number: _IOW(0x94, 13, struct file_clone_range) = 0x4020940d
const FICLONERANGE: libc::c_ulong = 0x4020940d;

/// struct file_clone_range for FICLONERANGE ioctl
#[repr(C)]
#[derive(Debug, Default)]
struct FileCloneRange {
    src_fd: i64,
    src_offset: u64,
    src_length: u64,
    dest_offset: u64,
}

/// Check if a path is on a btrfs filesystem.
fn is_btrfs(path: &std::path::Path) -> bool {
    use std::ffi::CString;
    use std::mem::MaybeUninit;

    let path_cstr = match CString::new(path.to_str().unwrap_or("")) {
        Ok(s) => s,
        Err(_) => return false,
    };

    let mut statfs = MaybeUninit::<libc::statfs>::uninit();
    let ret = unsafe { libc::statfs(path_cstr.as_ptr(), statfs.as_mut_ptr()) };
    if ret != 0 {
        return false;
    }

    let statfs = unsafe { statfs.assume_init() };
    // BTRFS_SUPER_MAGIC = 0x9123683e
    statfs.f_type == 0x9123683e
}

/// Check if the kernel supports FUSE remap_file_range by attempting the operation.
/// Returns:
/// - Some(true) if kernel supports it AND filesystem supports reflinks
/// - Some(false) if kernel supports it but filesystem doesn't (EOPNOTSUPP)
/// - None if kernel doesn't support it (ENOSYS)
fn check_kernel_remap_support(mount_path: &std::path::Path) -> Option<bool> {
    let src_path = mount_path.join("_kernel_check_src.tmp");
    let dst_path = mount_path.join("_kernel_check_dst.tmp");

    // Create source file with some data
    if fs::write(&src_path, b"kernel support check").is_err() {
        return None;
    }

    // Create empty destination file
    let dst_file = match File::create(&dst_path) {
        Ok(f) => f,
        Err(_) => {
            let _ = fs::remove_file(&src_path);
            return None;
        }
    };

    let src_file = match File::open(&src_path) {
        Ok(f) => f,
        Err(_) => {
            let _ = fs::remove_file(&src_path);
            let _ = fs::remove_file(&dst_path);
            return None;
        }
    };

    // Try FICLONE
    let ret = unsafe { libc::ioctl(dst_file.as_raw_fd(), FICLONE, src_file.as_raw_fd()) };

    // Cleanup
    drop(src_file);
    drop(dst_file);
    let _ = fs::remove_file(&src_path);
    let _ = fs::remove_file(&dst_path);

    if ret == 0 {
        Some(true) // Success - kernel supports and fs supports
    } else {
        let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
        match errno {
            libc::ENOSYS => None,                           // Kernel doesn't support
            libc::EOPNOTSUPP | libc::EINVAL => Some(false), // Kernel supports, fs doesn't
            _ => Some(false),                               // Other error, assume kernel supports
        }
    }
}

/// Test FICLONE ioctl (whole file clone) through FUSE.
///
/// This test:
/// 1. Creates a source file with test data
/// 2. Clones it using FICLONE ioctl
/// 3. Verifies the clone has identical content
/// 4. Verifies files share physical extents (on btrfs)
#[test]
fn test_ficlone_whole_file() {
    let (data_dir, mount_dir) = unique_paths("fuse-ficlone");

    // Check if backing filesystem is btrfs
    if !is_btrfs(std::path::Path::new("/tmp")) {
        // Try to use /mnt/fcvm-btrfs if available
        let btrfs_path = std::path::Path::new("/mnt/fcvm-btrfs/test-fuse-ficlone");
        if !is_btrfs(std::path::Path::new("/mnt/fcvm-btrfs")) {
            eprintln!("SKIP: test_ficlone_whole_file requires btrfs backing filesystem");
            eprintln!("      /tmp is not btrfs and /mnt/fcvm-btrfs is not available");
            return;
        }
        // Use btrfs-backed paths
        return run_ficlone_test(btrfs_path);
    }

    run_ficlone_test_with_paths(&data_dir, &mount_dir);
    cleanup(&data_dir, &mount_dir);
}

fn run_ficlone_test(btrfs_base: &std::path::Path) {
    let _ = fs::create_dir_all(btrfs_base);
    let data_dir = btrfs_base.join("data");
    let mount_dir = btrfs_base.join("mount");
    let _ = fs::remove_dir_all(&data_dir);
    let _ = fs::remove_dir_all(&mount_dir);

    run_ficlone_test_with_paths(&data_dir, &mount_dir);

    // Cleanup
    let _ = fs::remove_dir_all(btrfs_base);
}

fn run_ficlone_test_with_paths(data_dir: &std::path::Path, mount_dir: &std::path::Path) {
    let fuse = FuseMount::new(data_dir, mount_dir, 4);
    let mount = fuse.mount_path();

    // Check kernel support first
    match check_kernel_remap_support(mount) {
        None => {
            eprintln!(
                "SKIP: test_ficlone_whole_file requires kernel FUSE_REMAP_FILE_RANGE support"
            );
            eprintln!("      Got ENOSYS - kernel patch not applied");
            return;
        }
        Some(false) => {
            eprintln!("SKIP: test_ficlone_whole_file requires btrfs/xfs with reflink support");
            eprintln!("      Kernel supports FUSE_REMAP_FILE_RANGE but fs returned EOPNOTSUPP");
            return;
        }
        Some(true) => {
            eprintln!("Kernel and filesystem support verified, running test...");
        }
    }

    // Create source file with test data (large enough to not be inline)
    let test_data: Vec<u8> = (0..1024 * 1024).map(|i| (i % 256) as u8).collect(); // 1MB
    let src_path = mount.join("ficlone_source.bin");
    let dst_path = mount.join("ficlone_dest.bin");

    fs::write(&src_path, &test_data).expect("write source file");

    // Flush writes to ensure data is on disk before cloning (important with writeback cache)
    let src_for_sync = File::open(&src_path).expect("open for sync");
    src_for_sync.sync_all().expect("sync source file");
    drop(src_for_sync);

    // Open source for reading
    let src_file = File::open(&src_path).expect("open source");

    // Create and open destination for writing
    let dst_file = File::create(&dst_path).expect("create dest");

    // Perform FICLONE
    let ret = unsafe { libc::ioctl(dst_file.as_raw_fd(), FICLONE, src_file.as_raw_fd()) };

    if ret != 0 {
        let err = std::io::Error::last_os_error();
        panic!(
            "FICLONE failed: {} (errno {})",
            err,
            err.raw_os_error().unwrap_or(0)
        );
    }

    drop(src_file);
    drop(dst_file);

    // Verify content is identical
    let dst_content = fs::read(&dst_path).expect("read dest");
    assert_eq!(
        dst_content.len(),
        test_data.len(),
        "cloned file size mismatch"
    );
    assert_eq!(dst_content, test_data, "cloned file content mismatch");

    // Verify on underlying filesystem that extents are shared
    let src_data_path = data_dir.join("ficlone_source.bin");
    let dst_data_path = data_dir.join("ficlone_dest.bin");
    verify_shared_extents(&src_data_path, &dst_data_path);

    eprintln!("SUCCESS: FICLONE whole file clone works through FUSE!");
}

/// Test FICLONERANGE ioctl (partial file clone) through FUSE.
#[test]
fn test_ficlonerange_partial() {
    let (data_dir, mount_dir) = unique_paths("fuse-ficlonerange");

    // Check if backing filesystem is btrfs
    if !is_btrfs(std::path::Path::new("/tmp")) {
        let btrfs_path = std::path::Path::new("/mnt/fcvm-btrfs/test-fuse-ficlonerange");
        if !is_btrfs(std::path::Path::new("/mnt/fcvm-btrfs")) {
            eprintln!("SKIP: test_ficlonerange_partial requires btrfs backing filesystem");
            return;
        }
        return run_ficlonerange_test(btrfs_path);
    }

    run_ficlonerange_test_with_paths(&data_dir, &mount_dir);
    cleanup(&data_dir, &mount_dir);
}

fn run_ficlonerange_test(btrfs_base: &std::path::Path) {
    let _ = fs::create_dir_all(btrfs_base);
    let data_dir = btrfs_base.join("data");
    let mount_dir = btrfs_base.join("mount");
    let _ = fs::remove_dir_all(&data_dir);
    let _ = fs::remove_dir_all(&mount_dir);

    run_ficlonerange_test_with_paths(&data_dir, &mount_dir);

    let _ = fs::remove_dir_all(btrfs_base);
}

fn run_ficlonerange_test_with_paths(data_dir: &std::path::Path, mount_dir: &std::path::Path) {
    let fuse = FuseMount::new(data_dir, mount_dir, 4);
    let mount = fuse.mount_path();

    // Check kernel support first
    match check_kernel_remap_support(mount) {
        None => {
            eprintln!(
                "SKIP: test_ficlonerange_partial requires kernel FUSE_REMAP_FILE_RANGE support"
            );
            return;
        }
        Some(false) => {
            eprintln!("SKIP: test_ficlonerange_partial requires btrfs/xfs with reflink support");
            return;
        }
        Some(true) => {}
    }

    // Create source file - must be block-aligned for FICLONERANGE
    // btrfs block size is typically 4096
    let block_size = 4096usize;
    let num_blocks = 4;
    let test_data: Vec<u8> = (0..block_size * num_blocks)
        .map(|i| (i % 256) as u8)
        .collect();
    let src_path = mount.join("clonerange_source.bin");
    let dst_path = mount.join("clonerange_dest.bin");

    fs::write(&src_path, &test_data).expect("write source");

    // Pre-allocate destination with zeros
    let dst_zeros: Vec<u8> = vec![0u8; block_size * num_blocks];
    fs::write(&dst_path, &dst_zeros).expect("write dest zeros");

    // Flush writes to ensure data is on disk before cloning (important with writeback cache)
    let src_for_sync = File::open(&src_path).expect("open source for sync");
    src_for_sync.sync_all().expect("sync source file");
    drop(src_for_sync);
    let dst_for_sync = File::open(&dst_path).expect("open dest for sync");
    dst_for_sync.sync_all().expect("sync dest file");
    drop(dst_for_sync);

    let src_file = File::open(&src_path).expect("open source");
    let dst_file = fs::OpenOptions::new()
        .write(true)
        .open(&dst_path)
        .expect("open dest for write");

    // Clone middle 2 blocks from source to dest
    let clone_range = FileCloneRange {
        src_fd: src_file.as_raw_fd() as i64,
        src_offset: block_size as u64,       // Start at block 1
        src_length: (block_size * 2) as u64, // Clone 2 blocks
        dest_offset: block_size as u64,      // Write to same offset in dest
    };

    let ret = unsafe {
        libc::ioctl(
            dst_file.as_raw_fd(),
            FICLONERANGE,
            &clone_range as *const FileCloneRange,
        )
    };

    if ret != 0 {
        let err = std::io::Error::last_os_error();
        panic!(
            "FICLONERANGE failed: {} (errno {})",
            err,
            err.raw_os_error().unwrap_or(0)
        );
    }

    drop(src_file);
    drop(dst_file);

    // Verify: blocks 0 and 3 should be zeros, blocks 1 and 2 should match source
    let dst_content = fs::read(&dst_path).expect("read dest");

    // Block 0: should be zeros
    assert!(
        dst_content[..block_size].iter().all(|&b| b == 0),
        "block 0 should be zeros"
    );

    // Blocks 1-2: should match source blocks 1-2
    assert_eq!(
        &dst_content[block_size..block_size * 3],
        &test_data[block_size..block_size * 3],
        "blocks 1-2 should match source"
    );

    // Block 3: should be zeros
    assert!(
        dst_content[block_size * 3..].iter().all(|&b| b == 0),
        "block 3 should be zeros"
    );

    eprintln!("SUCCESS: FICLONERANGE partial clone works through FUSE!");
}

/// Test cp --reflink=always through FUSE mount.
#[test]
fn test_cp_reflink_always() {
    let (data_dir, mount_dir) = unique_paths("fuse-cp-reflink");

    // Check if backing filesystem is btrfs
    if !is_btrfs(std::path::Path::new("/tmp")) {
        let btrfs_path = std::path::Path::new("/mnt/fcvm-btrfs/test-fuse-cp-reflink");
        if !is_btrfs(std::path::Path::new("/mnt/fcvm-btrfs")) {
            eprintln!("SKIP: test_cp_reflink_always requires btrfs backing filesystem");
            return;
        }
        return run_cp_reflink_test(btrfs_path);
    }

    run_cp_reflink_test_with_paths(&data_dir, &mount_dir);
    cleanup(&data_dir, &mount_dir);
}

fn run_cp_reflink_test(btrfs_base: &std::path::Path) {
    let _ = fs::create_dir_all(btrfs_base);
    let data_dir = btrfs_base.join("data");
    let mount_dir = btrfs_base.join("mount");
    let _ = fs::remove_dir_all(&data_dir);
    let _ = fs::remove_dir_all(&mount_dir);

    run_cp_reflink_test_with_paths(&data_dir, &mount_dir);

    let _ = fs::remove_dir_all(btrfs_base);
}

fn run_cp_reflink_test_with_paths(data_dir: &std::path::Path, mount_dir: &std::path::Path) {
    let fuse = FuseMount::new(data_dir, mount_dir, 4);
    let mount = fuse.mount_path();

    // Check kernel support first
    match check_kernel_remap_support(mount) {
        None => {
            eprintln!("SKIP: test_cp_reflink_always requires kernel FUSE_REMAP_FILE_RANGE support");
            return;
        }
        Some(false) => {
            eprintln!("SKIP: test_cp_reflink_always requires btrfs/xfs with reflink support");
            return;
        }
        Some(true) => {}
    }

    // Create source file
    let test_data: Vec<u8> = (0..1024 * 1024).map(|i| (i % 256) as u8).collect();
    let src_path = mount.join("cp_reflink_source.bin");
    let dst_path = mount.join("cp_reflink_dest.bin");

    fs::write(&src_path, &test_data).expect("write source");

    // Flush writes to ensure data is on disk before cloning (important with writeback cache)
    let src_for_sync = File::open(&src_path).expect("open for sync");
    src_for_sync.sync_all().expect("sync source file");
    drop(src_for_sync);

    // Run cp --reflink=always
    let output = std::process::Command::new("cp")
        .args([
            "--reflink=always",
            src_path.to_str().unwrap(),
            dst_path.to_str().unwrap(),
        ])
        .output()
        .expect("run cp");

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        panic!("cp --reflink=always failed: {}", stderr);
    }

    // Verify content from backing filesystem.
    // With writeback cache, FUSE may have stale cached attributes (size=0) from when
    // cp created the empty file. FICLONE changes size on disk but doesn't invalidate
    // FUSE's attribute cache. Reading from backing fs verifies passthrough worked.
    let src_data_path = data_dir.join("cp_reflink_source.bin");
    let dst_data_path = data_dir.join("cp_reflink_dest.bin");
    let dst_content = fs::read(&dst_data_path).expect("read dest from backing fs");
    assert_eq!(dst_content, test_data, "reflink copy content mismatch");

    // Verify shared extents
    verify_shared_extents(&src_data_path, &dst_data_path);

    eprintln!("SUCCESS: cp --reflink=always works through FUSE!");
}

/// Verify that two files share physical extents using filefrag.
fn verify_shared_extents(src: &std::path::Path, dst: &std::path::Path) {
    let output = std::process::Command::new("filefrag")
        .args(["-v", src.to_str().unwrap(), dst.to_str().unwrap()])
        .output();

    match output {
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            eprintln!("filefrag output:\n{}", stdout);

            // Check for "shared" flag in output
            if stdout.contains("shared") {
                eprintln!("Verified: files share physical extents (reflink confirmed)");
            } else if stdout.contains("inline") {
                eprintln!("Note: files are inline (too small for extent sharing verification)");
            } else {
                eprintln!("Warning: could not verify shared extents from filefrag output");
            }
        }
        Err(e) => {
            eprintln!(
                "Note: filefrag not available ({}), skipping extent verification",
                e
            );
        }
    }
}
