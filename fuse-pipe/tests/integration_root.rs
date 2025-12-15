//! Integration tests requiring root privileges.
//!
//! These tests verify FUSE operations that require elevated privileges:
//! - chown() to arbitrary users
//! - setfsuid()/setfsgid() credential switching
//! - mkdir as non-root user via credential switching
//!
//! Run with: `sudo cargo test --release -p fuse-pipe --test integration_root`

mod common;

use std::fs;
use std::os::unix::fs::MetadataExt;

use common::{cleanup, unique_paths, FuseMount};

/// Helper to run a closure with switched credentials (uid/gid 65534).
/// Returns the result of the closure.
fn with_credentials<F, R>(uid: u32, gid: u32, f: F) -> R
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    std::thread::spawn(move || {
        // Switch effective gid first (must be done before dropping root uid)
        let ret = unsafe { libc::syscall(libc::SYS_setresgid, -1i32, gid, -1i32) };
        assert_eq!(ret, 0, "setresgid to {} failed", gid);

        // Switch effective uid
        let ret = unsafe { libc::syscall(libc::SYS_setresuid, -1i32, uid, -1i32) };
        assert_eq!(ret, 0, "setresuid to {} failed", uid);

        // Run the closure
        let result = f();

        // Switch back to root
        let ret = unsafe { libc::syscall(libc::SYS_setresuid, -1i32, 0u32, -1i32) };
        assert_eq!(ret, 0, "switch back to root uid failed");
        let ret = unsafe { libc::syscall(libc::SYS_setresgid, -1i32, 0u32, -1i32) };
        assert_eq!(ret, 0, "switch back to root gid failed");

        result
    })
    .join()
    .expect("credential thread should not panic")
}

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

    // Now try to create a subdirectory as uid 65534 using credential switching
    let subdir = test_dir.join("subdir_by_65534");
    let subdir_clone = subdir.clone();

    let result = with_credentials(65534, 65534, move || fs::create_dir(&subdir_clone));

    assert!(
        result.is_ok(),
        "mkdir as uid 65534 should succeed, got {:?}",
        result
    );

    // Verify the directory was created with correct ownership
    let subdir_meta = fs::metadata(&subdir).expect("stat subdir");
    assert_eq!(subdir_meta.uid(), 65534, "subdir uid should be 65534");
    assert_eq!(subdir_meta.gid(), 65534, "subdir gid should be 65534");

    // Cleanup
    fs::remove_dir(&subdir).expect("remove subdir");
    fs::remove_dir(&test_dir).expect("remove test_dir");
    drop(fuse);
    cleanup(&data_dir, &mount_dir);
}

/// Test that non-root users can create directories in a world-writable directory.
/// This matches the exact scenario in pjdfstest mkdir/00.t test 18.
///
/// The test:
/// 1. Creates work directory as root with mode 0777
/// 2. Switches credentials to uid/gid 65534
/// 3. Creates a subdirectory
/// 4. Expects success
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
    let subdir_clone = subdir.clone();

    let result = with_credentials(65534, 65534, move || fs::create_dir(&subdir_clone));

    eprintln!(
        "[test {} readers] mkdir as uid 65534 result: {:?}",
        num_readers, result
    );

    // This is the critical test - mkdir should succeed
    // but the bug causes it to return EACCES with multiple readers
    assert!(
        result.is_ok(),
        "mkdir as uid 65534 with {} readers should succeed, got {:?}",
        num_readers,
        result
    );

    // Cleanup
    let _ = fs::remove_dir(&subdir);
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
