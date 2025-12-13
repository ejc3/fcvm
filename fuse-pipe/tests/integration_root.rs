//! Integration tests requiring root privileges.
//!
//! These tests verify FUSE operations that require elevated privileges:
//! - chown() to arbitrary users
//! - setfsuid()/setfsgid() credential switching
//! - pjdfstest with -u/-g flags
//!
//! Run with: `sudo cargo test --release -p fuse-pipe --test integration_root`

mod common;

use std::fs;
use std::os::unix::fs::MetadataExt;

use common::{cleanup, unique_paths, FuseMount};

/// Test that non-root users can create directories in directories they own.
/// This is a regression test for the pjdfstest uid 65534 EACCES failures.
///
/// The test:
/// 1. Creates a directory as root
/// 2. Changes ownership to uid 65534 (nobody)
/// 3. Attempts to create a subdirectory as uid 65534
/// 4. Should succeed, but was failing with EACCES
#[test]
fn test_nonroot_user_mkdir_in_owned_directory() {
    use std::os::unix::fs::{chown, PermissionsExt};

    let (data_dir, mount_dir) = unique_paths("fuse-integ");
    let fuse = FuseMount::new(&data_dir, &mount_dir, 1);
    let mount = fuse.mount_path();

    // Require root for chown and credential tests
    assert_eq!(
        unsafe { libc::geteuid() },
        0,
        "test_nonroot_user_mkdir_in_owned_directory requires root"
    );

    // Create a test directory as root
    let test_dir = mount.join("cred_test");
    fs::create_dir(&test_dir).expect("create test dir as root");

    // Set permissions to 0755 (rwxr-xr-x)
    fs::set_permissions(&test_dir, fs::Permissions::from_mode(0o755)).expect("set permissions");

    // Change ownership to uid/gid 65534 (nobody/nogroup)
    chown(&test_dir, Some(65534), Some(65534)).expect("chown to 65534");

    // Verify ownership changed
    let meta = fs::metadata(&test_dir).expect("stat test_dir");
    assert_eq!(meta.uid(), 65534, "uid should be 65534");
    assert_eq!(meta.gid(), 65534, "gid should be 65534");

    // Now try to create a subdirectory as uid 65534
    // Use pjdfstest binary if available, otherwise use direct syscall
    let subdir = test_dir.join("subdir_by_65534");
    let pjdfstest_bin = std::path::Path::new("/tmp/pjdfstest-check/pjdfstest");

    if pjdfstest_bin.exists() {
        // Use pjdfstest to create directory as uid 65534
        let output = std::process::Command::new(pjdfstest_bin)
            .args([
                "-u",
                "65534",
                "-g",
                "65534",
                "mkdir",
                subdir.to_str().unwrap(),
                "0755",
            ])
            .output()
            .expect("run pjdfstest");

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        eprintln!("[pjdfstest mkdir] stdout: {}", stdout);
        eprintln!("[pjdfstest mkdir] stderr: {}", stderr);
        eprintln!("[pjdfstest mkdir] exit code: {:?}", output.status.code());

        // pjdfstest returns 0 on success, outputs "0" for success
        assert!(
            output.status.success() && stdout.trim() == "0",
            "mkdir as uid 65534 should succeed, got stdout='{}' stderr='{}' code={:?}",
            stdout.trim(),
            stderr.trim(),
            output.status.code()
        );

        // Verify the directory was created with correct ownership
        let subdir_meta = fs::metadata(&subdir).expect("stat subdir");
        assert_eq!(subdir_meta.uid(), 65534, "subdir uid should be 65534");
        assert_eq!(subdir_meta.gid(), 65534, "subdir gid should be 65534");

        // Cleanup
        fs::remove_dir(&subdir).expect("remove subdir");
    } else {
        panic!("pjdfstest not found at {:?}", pjdfstest_bin);
    }

    fs::remove_dir(&test_dir).expect("remove test_dir");
    drop(fuse);
    cleanup(&data_dir, &mount_dir);
}

/// Test that non-root users can create directories in a world-writable directory.
/// This matches the exact scenario in pjdfstest mkdir/00.t test 18.
///
/// The pjdfstest test:
/// 1. Creates work directory as root with mode 0777
/// 2. cd into work directory
/// 3. Runs: pjdfstest -u 65534 -g 65534 mkdir <name> 0755
/// 4. Expects success (0), but gets EACCES through FUSE
///
/// CRITICAL FINDING: This test PASSES with 1 reader but FAILS with 256 readers!
/// The bug is related to the multi-reader/multi-threaded architecture.
#[test]
fn test_nonroot_mkdir_in_world_writable_directory() {
    test_nonroot_mkdir_with_readers(256);
}

/// Test with 1 reader - should pass (baseline sanity check)
#[test]
fn test_nonroot_mkdir_with_1_reader() {
    test_nonroot_mkdir_with_readers(1);
}

/// Test with 2 readers - find the threshold
#[test]
fn test_nonroot_mkdir_with_2_readers() {
    test_nonroot_mkdir_with_readers(2);
}

fn test_nonroot_mkdir_with_readers(num_readers: usize) {
    use std::os::unix::fs::PermissionsExt;

    let (data_dir, mount_dir) = unique_paths("fuse-integ");
    let fuse = FuseMount::new(&data_dir, &mount_dir, num_readers);
    let mount = fuse.mount_path();

    // Require root
    assert_eq!(
        unsafe { libc::geteuid() },
        0,
        "test with {} readers requires root",
        num_readers
    );

    // Create work directory as root with mode 0777 (exactly like pjdfstest)
    let work_dir = mount.join("pjdfs_work");
    fs::create_dir(&work_dir).expect("create work dir");
    fs::set_permissions(&work_dir, fs::Permissions::from_mode(0o777))
        .expect("set permissions to 0777");

    // Verify permissions
    let meta = fs::metadata(&work_dir).expect("stat work_dir");
    eprintln!(
        "[test] {} readers - work_dir owner: uid={} gid={} mode={:o}",
        num_readers,
        meta.uid(),
        meta.gid(),
        meta.mode() & 0o7777
    );

    // Now try to create a subdirectory as uid 65534 in the world-writable directory
    // This is the exact scenario that fails in pjdfstest mkdir/00.t test 18
    let subdir = work_dir.join("test_subdir_by_65534");
    let pjdfstest_bin = std::path::Path::new("/tmp/pjdfstest-check/pjdfstest");

    if pjdfstest_bin.exists() {
        // Run from work_dir, just like pjdfstest does
        let output = std::process::Command::new(pjdfstest_bin)
            .args([
                "-u",
                "65534",
                "-g",
                "65534",
                "mkdir",
                "test_subdir_by_65534",
                "0755",
            ])
            .current_dir(&work_dir)
            .output()
            .expect("run pjdfstest");

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        eprintln!("[pjdfstest {} readers] stdout: {}", num_readers, stdout);
        eprintln!("[pjdfstest {} readers] stderr: {}", num_readers, stderr);
        eprintln!(
            "[pjdfstest {} readers] exit code: {:?}",
            num_readers,
            output.status.code()
        );

        // This is the critical test - pjdfstest should return 0 (success)
        // but the bug causes it to return EACCES with multiple readers
        assert!(
            output.status.success() && stdout.trim() == "0",
            "mkdir as uid 65534 with {} readers should succeed, got stdout='{}' stderr='{}' code={:?}",
            num_readers,
            stdout.trim(),
            stderr.trim(),
            output.status.code()
        );

        // Cleanup if it succeeded
        let _ = fs::remove_dir(&subdir);
    } else {
        panic!("pjdfstest not found at {:?}", pjdfstest_bin);
    }

    let _ = fs::remove_dir(&work_dir);
    drop(fuse);
    cleanup(&data_dir, &mount_dir);
}

/// Test that credential switching works correctly in spawn_blocking threads.
/// This directly tests the setresuid/setresgid syscalls used by fuse-backend-rs.
#[test]
fn test_credential_switching_in_thread() {
    // Require root
    assert_eq!(
        unsafe { libc::geteuid() },
        0,
        "test_credential_switching_in_thread requires root"
    );

    // Test that we can switch credentials in a spawned thread
    let result = std::thread::spawn(|| {
        // Get current effective uid (should be 0)
        let original_euid = unsafe { libc::geteuid() };
        assert_eq!(original_euid, 0, "should start as root");

        // Switch effective uid to 65534 using setresuid
        let ret = unsafe { libc::syscall(libc::SYS_setresuid, -1i32, 65534u32, -1i32) };
        if ret != 0 {
            let err = std::io::Error::last_os_error();
            panic!("setresuid to 65534 failed: {}", err);
        }

        // Verify we switched
        let new_euid = unsafe { libc::geteuid() };
        assert_eq!(new_euid, 65534, "euid should be 65534 after setresuid");

        // Try to create a file in /tmp as uid 65534
        let test_path = format!("/tmp/cred_test_{}", std::process::id());
        let result = std::fs::write(&test_path, "test");

        // Switch back to root
        let ret = unsafe { libc::syscall(libc::SYS_setresuid, -1i32, 0u32, -1i32) };
        assert_eq!(ret, 0, "switch back to root should succeed");

        // Cleanup
        let _ = std::fs::remove_file(&test_path);

        result
    })
    .join()
    .expect("thread should not panic");

    assert!(
        result.is_ok(),
        "file write as uid 65534 should succeed: {:?}",
        result
    );
    eprintln!("[pass] credential switching in thread works correctly");
}
