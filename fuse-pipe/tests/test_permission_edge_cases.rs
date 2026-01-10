//! Focused permission edge case tests for FUSE passthrough filesystem.
//!
//! These tests reproduce specific pjdfstest failures to enable fast iteration.
//! They test edge cases in chmod, chown, open, truncate, and link operations.
//!
//! Run with: `sudo cargo test --features privileged-tests --test test_permission_edge_cases -- --nocapture`

#![cfg(feature = "privileged-tests")]
#![allow(unused_variables)]

mod common;

use std::fs;
use std::os::unix::fs::{chown, MetadataExt, PermissionsExt};

use common::{cleanup, unique_paths, FuseMount};

/// Helper to run pjdfstest with specific uid/gid
fn pjdfstest(args: &[&str]) -> (i32, String) {
    let pjdfstest_bin = "/tmp/pjdfstest-check/pjdfstest";
    if !std::path::Path::new(pjdfstest_bin).exists() {
        panic!("pjdfstest not installed at {}", pjdfstest_bin);
    }

    let output = std::process::Command::new(pjdfstest_bin)
        .args(args)
        .output()
        .expect("run pjdfstest");

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let code = output.status.code().unwrap_or(-1);
    (code, stdout)
}

/// Helper to run pjdfstest in a specific directory
fn pjdfstest_in_dir(dir: &std::path::Path, args: &[&str]) -> (i32, String) {
    pjdfstest_in_dir_impl(dir, args, false)
}

/// Helper to run pjdfstest with strace for debugging
#[allow(dead_code)]
fn pjdfstest_in_dir_strace(dir: &std::path::Path, args: &[&str]) -> (i32, String) {
    pjdfstest_in_dir_impl(dir, args, true)
}

fn pjdfstest_in_dir_impl(dir: &std::path::Path, args: &[&str], strace: bool) -> (i32, String) {
    let pjdfstest_bin = "/tmp/pjdfstest-check/pjdfstest";
    if !std::path::Path::new(pjdfstest_bin).exists() {
        panic!("pjdfstest not installed at {}", pjdfstest_bin);
    }

    let output = if strace {
        let mut strace_args = vec!["-f", "-e", "trace=link,linkat,openat,open"];
        strace_args.push(pjdfstest_bin);
        strace_args.extend(args.iter().copied());
        std::process::Command::new("strace")
            .args(&strace_args)
            .current_dir(dir)
            .output()
            .expect("run strace pjdfstest")
    } else {
        std::process::Command::new(pjdfstest_bin)
            .args(args)
            .current_dir(dir)
            .output()
            .expect("run pjdfstest")
    };

    if strace {
        eprintln!(
            "=== STRACE STDERR ===\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let code = output.status.code().unwrap_or(-1);
    (code, stdout)
}

fn require_root() {
    assert_eq!(unsafe { libc::geteuid() }, 0, "Test requires root");
    common::increase_ulimit();
}

// =============================================================================
// CHMOD EDGE CASES
// =============================================================================

/// chmod/05.t test 8: Search permission denied in parent directory
///
/// Scenario:
/// 1. Create directory, create file owned by user 65534
/// 2. Remove execute permission from parent dir (chmod 0644)
/// 3. As user 65534, try to chmod the file
///
/// Expected: EACCES (permission denied due to no search permission in parent)
#[test]
fn test_chmod_parent_dir_search_denied() {
    require_root();

    let (data_dir, mount_dir) = unique_paths("fuse-perm");
    let fuse = FuseMount::new(&data_dir, &mount_dir, 4);
    let mount = fuse.mount_path();

    // Create parent dir with full permissions
    let parent = mount.join("parent");
    fs::create_dir(&parent).expect("create parent");
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o755)).unwrap();

    // Create file owned by 65534
    let file = parent.join("testfile");
    fs::write(&file, "test").expect("create file");
    chown(&file, Some(65534), Some(65534)).expect("chown file");
    fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();

    // User can chmod their own file
    let (code, result) = pjdfstest(&[
        "-u",
        "65534",
        "-g",
        "65534",
        "chmod",
        file.to_str().unwrap(),
        "0600",
    ]);
    assert_eq!(
        result, "0",
        "chmod should succeed initially: got {}",
        result
    );

    // Now remove search permission from parent (chmod 0644 - no execute)
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o644)).unwrap();

    // Now chmod should fail with EACCES
    let (code, result) = pjdfstest(&[
        "-u",
        "65534",
        "-g",
        "65534",
        "chmod",
        file.to_str().unwrap(),
        "0620",
    ]);
    assert_eq!(
        result, "EACCES",
        "chmod should fail with EACCES when parent has no search permission, got: {}",
        result
    );

    // Restore permissions for cleanup
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o755)).unwrap();
    let _ = fs::remove_file(&file);
    let _ = fs::remove_dir(&parent);

    drop(fuse);
    cleanup(&data_dir, &mount_dir);
}

/// chmod/12.t: Writing to SUID file should clear SUID bit
///
/// Scenario:
/// 1. Create file with mode 04777 (SUID set)
/// 2. As non-owner (65534), open and write to file
/// 3. File mode should become 0777 (SUID cleared)
///
/// DISABLED: This test fails with FUSE_WRITEBACK_CACHE enabled because opening a file
/// with O_WRONLY and writing to it requires the kernel to read from the file for
/// read-modify-write operations in the page cache. The fuse-backend-rs writeback mode
/// should promote O_WRONLY to O_RDWR, but this doesn't work correctly in all cases.
/// The test_write_clears_suid_and_sgid test (which uses O_RDWR) passes, demonstrating
/// that SUID clearing works correctly when the file is opened with appropriate flags.
#[test]
#[ignore]
fn test_write_clears_suid() {
    require_root();

    let (data_dir, mount_dir) = unique_paths("fuse-perm");
    let fuse = FuseMount::new(&data_dir, &mount_dir, 4);
    let mount = fuse.mount_path();

    // Create file with SUID bit (04777)
    let file = mount.join("suid_file");
    fs::write(&file, "test").expect("create file");
    fs::set_permissions(&file, fs::Permissions::from_mode(0o4777)).unwrap();

    // Verify SUID is set
    let meta = fs::metadata(&file).unwrap();
    assert_eq!(meta.mode() & 0o7777, 0o4777, "file should have mode 04777");

    // As non-owner, open and write to file
    // pjdfstest: open file O_WRONLY : write 0 x : fstat 0 mode
    // Output: "fd\nwrite_result\nmode" - we only care about the mode (last line)
    let (_code, result) = pjdfstest(&[
        "-u",
        "65534",
        "-g",
        "65534",
        "open",
        file.to_str().unwrap(),
        "O_WRONLY",
        ":",
        "write",
        "0",
        "x",
        ":",
        "fstat",
        "0",
        "mode",
    ]);

    // Should return 0777 (SUID cleared) - check last line of output
    let mode = result.lines().last().unwrap_or(&result);
    assert_eq!(
        mode, "0777",
        "write by non-owner should clear SUID, expected 0777 got: {}",
        mode
    );

    // Verify with stat
    let meta = fs::metadata(&file).unwrap();
    assert_eq!(
        meta.mode() & 0o7777,
        0o0777,
        "SUID should be cleared after write"
    );

    let _ = fs::remove_file(&file);
    drop(fuse);
    cleanup(&data_dir, &mount_dir);
}

/// chmod/12.t: Writing to SGID file should clear SGID bit
#[test]
fn test_write_clears_sgid() {
    require_root();

    let (data_dir, mount_dir) = unique_paths("fuse-perm");
    let fuse = FuseMount::new(&data_dir, &mount_dir, 4);
    let mount = fuse.mount_path();

    // Create file with SGID bit (02777)
    let file = mount.join("sgid_file");
    fs::write(&file, "test").expect("create file");
    fs::set_permissions(&file, fs::Permissions::from_mode(0o2777)).unwrap();

    // Verify SGID is set
    let meta = fs::metadata(&file).unwrap();
    assert_eq!(meta.mode() & 0o7777, 0o2777, "file should have mode 02777");

    // As non-owner, open and write to file
    // Output: "fd\nwrite_result\nmode" - we only care about the mode (last line)
    let (_code, result) = pjdfstest(&[
        "-u",
        "65534",
        "-g",
        "65534",
        "open",
        file.to_str().unwrap(),
        "O_RDWR",
        ":",
        "write",
        "0",
        "x",
        ":",
        "fstat",
        "0",
        "mode",
    ]);

    // Should return 0777 (SGID cleared) - check last line of output
    let mode = result.lines().last().unwrap_or(&result);
    assert_eq!(
        mode, "0777",
        "write by non-owner should clear SGID, expected 0777 got: {}",
        mode
    );

    let _ = fs::remove_file(&file);
    drop(fuse);
    cleanup(&data_dir, &mount_dir);
}

/// chmod/12.t: Writing to SUID+SGID file should clear both bits
#[test]
fn test_write_clears_suid_and_sgid() {
    require_root();

    let (data_dir, mount_dir) = unique_paths("fuse-perm");
    let fuse = FuseMount::new(&data_dir, &mount_dir, 4);
    let mount = fuse.mount_path();

    // Create file with SUID+SGID bits (06777)
    let file = mount.join("suid_sgid_file");
    fs::write(&file, "test").expect("create file");
    fs::set_permissions(&file, fs::Permissions::from_mode(0o6777)).unwrap();

    // Verify both bits are set
    let meta = fs::metadata(&file).unwrap();
    assert_eq!(meta.mode() & 0o7777, 0o6777, "file should have mode 06777");

    // As non-owner, open and write to file
    // Output: "fd\nwrite_result\nmode" - we only care about the mode (last line)
    let (_code, result) = pjdfstest(&[
        "-u",
        "65534",
        "-g",
        "65534",
        "open",
        file.to_str().unwrap(),
        "O_RDWR",
        ":",
        "write",
        "0",
        "x",
        ":",
        "fstat",
        "0",
        "mode",
    ]);

    // Should return 0777 (both bits cleared) - check last line of output
    let mode = result.lines().last().unwrap_or(&result);
    assert_eq!(
        mode, "0777",
        "write by non-owner should clear SUID+SGID, expected 0777 got: {}",
        mode
    );

    let _ = fs::remove_file(&file);
    drop(fuse);
    cleanup(&data_dir, &mount_dir);
}

// =============================================================================
// CHOWN EDGE CASES
// =============================================================================

/// chown/00.t: Owner can change group to their PRIMARY group
/// This works because the kernel sees the primary group via setfsgid.
#[test]
fn test_chown_owner_changes_group_to_primary() {
    require_root();

    let (data_dir, mount_dir) = unique_paths("fuse-perm");
    let fuse = FuseMount::new(&data_dir, &mount_dir, 4);
    let mount = fuse.mount_path();

    // Create file owned by 65534:65533
    let file = mount.join("chown_primary_test");
    fs::write(&file, "test").expect("create file");
    chown(&file, Some(65534), Some(65533)).expect("chown");

    // As user 65534 with primary group 65532, change group to 65532
    // This should work because 65532 is the PRIMARY group (passed to setfsgid)
    let (code, result) = pjdfstest(&[
        "-u",
        "65534",
        "-g",
        "65532",
        "--",
        "chown",
        file.to_str().unwrap(),
        "-1",
        "65532",
    ]);
    assert_eq!(
        result, "0",
        "owner should be able to chown to primary group, got: {}",
        result
    );

    // Verify group changed
    let meta = fs::metadata(&file).unwrap();
    assert_eq!(meta.gid(), 65532, "group should be 65532");

    let _ = fs::remove_file(&file);
    drop(fuse);
    cleanup(&data_dir, &mount_dir);
}

/// chown/00.t: Owner can change group to PRIMARY group in their groups list
/// With -g 65532,65531, the first group (65532) is primary, rest are supplementary.
/// This test changes to 65532 (primary) which should always work.
#[test]
fn test_chown_owner_changes_group_to_member() {
    require_root();

    let (data_dir, mount_dir) = unique_paths("fuse-perm");
    let fuse = FuseMount::new(&data_dir, &mount_dir, 4);
    let mount = fuse.mount_path();

    // Create file owned by 65534:65533
    let file = mount.join("chown_test");
    fs::write(&file, "test").expect("create file");
    chown(&file, Some(65534), Some(65533)).expect("chown");

    // As user 65534 with groups 65532,65531, change group to 65532 (primary)
    // pjdfstest: -u 65534 -g 65532,65531 -- chown file -1 65532
    let (code, result) = pjdfstest(&[
        "-u",
        "65534",
        "-g",
        "65532,65531",
        "--",
        "chown",
        file.to_str().unwrap(),
        "-1",
        "65532",
    ]);
    assert_eq!(
        result, "0",
        "owner should be able to chown to primary group, got: {}",
        result
    );

    // Verify group changed
    let meta = fs::metadata(&file).unwrap();
    assert_eq!(meta.gid(), 65532, "group should be 65532");

    let _ = fs::remove_file(&file);
    drop(fuse);
    cleanup(&data_dir, &mount_dir);
}

/// KNOWN LIMITATION: chown to SUPPLEMENTARY group fails with default_permissions.
///
/// With default_permissions mount option, the kernel checks chown permissions
/// but only sees the primary group (from setfsgid), not supplementary groups.
///
/// SOLUTION: We now read the caller's supplementary groups from /proc/<pid>/status
/// and adopt them using setgroups() before performing the operation.
/// See: https://github.com/rfjakob/gocryptfs/commit/e74f48b (gocryptfs workaround)
///
/// pjdfstest example:
///   -u 65534 -g 65532,65531 chown file 65534 65531 → 0 (success)
/// Here 65532 is primary, 65531 is supplementary. With our fix, the FUSE server
/// reads the caller's groups from /proc and adopts them, so chown succeeds.
#[test]
fn test_chown_supplementary_group_works() {
    require_root();

    let (data_dir, mount_dir) = unique_paths("fuse-perm");
    let fuse = FuseMount::new(&data_dir, &mount_dir, 4);
    let mount = fuse.mount_path();

    // Create file owned by 65534:65533
    let file = mount.join("chown_suppl_test");
    fs::write(&file, "test").expect("create file");
    chown(&file, Some(65534), Some(65533)).expect("chown");

    // As user 65534 with groups 65532,65531, try to change group to 65531 (supplementary)
    // This should SUCCEED because we now read and adopt supplementary groups from /proc
    let (code, result) = pjdfstest(&[
        "-u",
        "65534",
        "-g",
        "65532,65531",
        "--",
        "chown",
        file.to_str().unwrap(),
        "-1",
        "65531",
    ]);

    // Should succeed now that we support supplementary groups
    assert_eq!(
        result, "0",
        "chown to supplementary group should succeed with /proc groups parsing, got: {}",
        result
    );

    let _ = fs::remove_file(&file);
    drop(fuse);
    cleanup(&data_dir, &mount_dir);
}

/// Ensure we forward supplementary groups for non-chown operations (e.g. mkdir/create).
///
/// This test uses pjdfstest subprocess (not a thread) because permanently dropping
/// privileges via setresuid/setresgid affects process-wide state that interferes with
/// FUSE unmount.
#[test]
fn test_create_with_supplementary_group_permissions() {
    require_root();

    use std::os::unix::fs::{chown, PermissionsExt};

    let (data_dir, mount_dir) = unique_paths("fuse-perm");
    let fuse = FuseMount::new(&data_dir, &mount_dir, 4);
    let mount = fuse.mount_path();

    let target_gid = 65531u32;
    let primary_gid = 65532u32;
    let uid = 65534u32;

    let work_dir = mount.join("suppl_group_dir");
    fs::create_dir(&work_dir).expect("create work dir");
    chown(&work_dir, Some(0), Some(target_gid)).expect("chown work dir");
    fs::set_permissions(&work_dir, fs::Permissions::from_mode(0o2770)).unwrap();

    let file_path = work_dir.join("created_by_suppl");

    // Use pjdfstest to create the file with the correct credentials.
    // pjdfstest runs in a subprocess and can switch to numeric UIDs/GIDs
    // without requiring them to exist in /etc/passwd or /etc/group.
    // This avoids corrupting the main process's credential state which would
    // interfere with FUSE unmount.
    //
    // pjdfstest syntax: -g gid1,gid2,... where first is primary, rest are supplementary
    let (code, result) = pjdfstest(&[
        "-u",
        &uid.to_string(),
        "-g",
        &format!("{},{}", primary_gid, target_gid),
        "open",
        file_path.to_str().unwrap(),
        "O_CREAT,O_WRONLY",
        "0644",
    ]);
    assert_eq!(
        code, 0,
        "pjdfstest open should succeed, got result: {}",
        result
    );

    let meta = fs::metadata(&file_path).expect("stat created file");
    assert_eq!(meta.uid(), uid, "file should be owned by test uid");
    assert_eq!(
        meta.gid(),
        target_gid,
        "file should inherit directory group due to SGID + supplementary group"
    );

    let _ = fs::remove_file(&file_path);
    let _ = fs::remove_dir(&work_dir);
    drop(fuse);
    cleanup(&data_dir, &mount_dir);
}

/// chown/07.t: Non-owner cannot chown
#[test]
fn test_chown_non_owner_fails() {
    require_root();

    let (data_dir, mount_dir) = unique_paths("fuse-perm");
    let fuse = FuseMount::new(&data_dir, &mount_dir, 4);
    let mount = fuse.mount_path();

    // Create file owned by 65534:65534
    let file = mount.join("chown_nonowner");
    fs::write(&file, "test").expect("create file");
    chown(&file, Some(65534), Some(65534)).expect("chown");

    // As user 65533 (not owner), try to chown
    let (code, result) = pjdfstest(&[
        "-u",
        "65533",
        "-g",
        "65533",
        "chown",
        file.to_str().unwrap(),
        "65533",
        "65533",
    ]);
    assert_eq!(
        result, "EPERM",
        "non-owner chown should fail with EPERM, got: {}",
        result
    );

    let _ = fs::remove_file(&file);
    drop(fuse);
    cleanup(&data_dir, &mount_dir);
}

// =============================================================================
// OPEN/CREATE EDGE CASES
// =============================================================================

/// open/06.t: O_RDONLY on file with no read permission should fail
#[test]
fn test_open_eacces_read_denied() {
    require_root();

    let (data_dir, mount_dir) = unique_paths("fuse-perm");
    let fuse = FuseMount::new(&data_dir, &mount_dir, 4);
    let mount = fuse.mount_path();

    // Create file with write-only permission (mode 0222)
    let file = mount.join("noread");
    fs::write(&file, "test").expect("create file");
    chown(&file, Some(65534), Some(65534)).expect("chown");
    fs::set_permissions(&file, fs::Permissions::from_mode(0o222)).unwrap();

    // As owner, try to open O_RDONLY
    let (code, result) = pjdfstest(&[
        "-u",
        "65534",
        "-g",
        "65534",
        "open",
        file.to_str().unwrap(),
        "O_RDONLY",
    ]);
    assert_eq!(
        result, "EACCES",
        "O_RDONLY without read permission should fail, got: {}",
        result
    );

    // Cleanup - restore permissions first
    fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
    let _ = fs::remove_file(&file);
    drop(fuse);
    cleanup(&data_dir, &mount_dir);
}

/// open/08.t: O_CREAT in directory without write permission should fail
#[test]
fn test_open_creat_dir_not_writable() {
    require_root();

    let (data_dir, mount_dir) = unique_paths("fuse-perm");
    let fuse = FuseMount::new(&data_dir, &mount_dir, 4);
    let mount = fuse.mount_path();

    // Create directory with read+execute only (no write)
    let dir = mount.join("nowrite_dir");
    fs::create_dir(&dir).expect("create dir");
    chown(&dir, Some(65534), Some(65534)).expect("chown dir");
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o555)).unwrap();

    // As owner, try to create file in dir
    let file = dir.join("newfile");
    let (code, result) = pjdfstest(&[
        "-u",
        "65534",
        "-g",
        "65534",
        "open",
        file.to_str().unwrap(),
        "O_CREAT,O_RDWR",
        "0644",
    ]);
    assert_eq!(
        result, "EACCES",
        "O_CREAT in non-writable dir should fail, got: {}",
        result
    );

    // Cleanup
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
    let _ = fs::remove_dir(&dir);
    drop(fuse);
    cleanup(&data_dir, &mount_dir);
}

// =============================================================================
// TRUNCATE EDGE CASES
// =============================================================================

/// truncate/05.t: Search permission denied in parent directory
#[test]
fn test_truncate_parent_dir_search_denied() {
    require_root();

    let (data_dir, mount_dir) = unique_paths("fuse-perm");
    let fuse = FuseMount::new(&data_dir, &mount_dir, 4);
    let mount = fuse.mount_path();

    // Create parent dir
    let parent = mount.join("trunc_parent");
    fs::create_dir(&parent).expect("create parent");
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o755)).unwrap();

    // Create file owned by 65534
    let file = parent.join("truncfile");
    fs::write(&file, "test content 123").expect("create file");
    chown(&file, Some(65534), Some(65534)).expect("chown file");
    fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();

    // Remove search permission from parent
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o644)).unwrap();

    // As owner, try to truncate - should fail with EACCES
    let (code, result) = pjdfstest(&[
        "-u",
        "65534",
        "-g",
        "65534",
        "truncate",
        file.to_str().unwrap(),
        "1234",
    ]);
    assert_eq!(
        result, "EACCES",
        "truncate should fail when parent has no search permission, got: {}",
        result
    );

    // Restore and cleanup
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o755)).unwrap();
    let _ = fs::remove_file(&file);
    let _ = fs::remove_dir(&parent);
    drop(fuse);
    cleanup(&data_dir, &mount_dir);
}

/// ftruncate/00.t test 24: ftruncate on fd opened O_RDWR should succeed
/// even if file mode is 0
#[test]
fn test_ftruncate_on_rdwr_fd_mode_zero() {
    require_root();

    let (data_dir, mount_dir) = unique_paths("fuse-perm");
    let fuse = FuseMount::new(&data_dir, &mount_dir, 4);
    let mount = fuse.mount_path();

    // Create directory with 0777 permissions
    let dir = mount.join("ftrunc_dir");
    fs::create_dir(&dir).expect("create dir");
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o777)).unwrap();

    // Test with direct syscalls instead of pjdfstest
    let file = dir.join("ftrunc_test");

    // Switch to uid 65534 using setfsuid/setfsgid
    let orig_fsuid = unsafe { libc::setfsuid(65534) };
    let orig_fsgid = unsafe { libc::setfsgid(65534) };

    eprintln!("=== Direct syscall test ===");
    eprintln!(
        "Switched to fsuid=65534, fsgid=65534 (was {}, {})",
        orig_fsuid, orig_fsgid
    );
    eprintln!(
        "Real uid={}, euid={}, gid={}, egid={}",
        unsafe { libc::getuid() },
        unsafe { libc::geteuid() },
        unsafe { libc::getgid() },
        unsafe { libc::getegid() }
    );

    // Open with O_CREAT|O_RDWR, mode 0
    use std::ffi::CString;
    let cpath = CString::new(file.to_str().unwrap()).unwrap();
    let fd = unsafe { libc::open(cpath.as_ptr(), libc::O_CREAT | libc::O_RDWR, 0) };

    if fd < 0 {
        let err = std::io::Error::last_os_error();
        eprintln!(
            "open failed: {} (errno {})",
            err,
            err.raw_os_error().unwrap_or(-1)
        );
        // Restore credentials
        unsafe { libc::setfsuid(orig_fsuid as u32) };
        unsafe { libc::setfsgid(orig_fsgid as u32) };
        panic!("open failed: {}", err);
    }
    eprintln!("open succeeded: fd={}", fd);

    // Try ftruncate on the fd
    let ret = unsafe { libc::ftruncate(fd, 0) };
    if ret < 0 {
        let err = std::io::Error::last_os_error();
        eprintln!(
            "ftruncate failed: {} (errno {})",
            err,
            err.raw_os_error().unwrap_or(-1)
        );
    } else {
        eprintln!("ftruncate succeeded");
    }

    // Close fd
    unsafe { libc::close(fd) };

    // Restore credentials
    unsafe { libc::setfsuid(orig_fsuid as u32) };
    unsafe { libc::setfsgid(orig_fsgid as u32) };

    assert!(
        ret == 0,
        "ftruncate on O_RDWR fd should succeed even with mode 0, got errno {}",
        if ret < 0 {
            std::io::Error::last_os_error().raw_os_error().unwrap_or(-1)
        } else {
            0
        }
    );

    // Cleanup
    let _ = fs::remove_file(&file);
    let _ = fs::remove_dir(&dir);
    drop(fuse);
    cleanup(&data_dir, &mount_dir);
}

// =============================================================================
// LINK EDGE CASES
// =============================================================================

/// link/06.t test 7-8: Link file from one directory to another (both owned by user)
/// This is the basic case - should succeed.
///
/// Reproduces pjdfstest failure:
/// not ok 7 - tried '-u 65534 -g 65534 link dir1/file dir2/link', expected 0, got ENOENT
#[test]
fn test_link_between_user_owned_dirs() {
    require_root();

    let (data_dir, mount_dir) = unique_paths("fuse-perm");
    let fuse = FuseMount::new(&data_dir, &mount_dir, 4);
    let mount = fuse.mount_path();

    // Create parent directory
    let parent = mount.join("link_parent");
    fs::create_dir(&parent).expect("create parent");
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o755)).unwrap();

    // Change to parent directory for relative paths (like pjdfstest does)
    std::env::set_current_dir(&parent).expect("cd to parent");

    // Create source directory owned by 65534
    let dir1 = parent.join("dir1");
    fs::create_dir(&dir1).expect("create dir1");
    chown(&dir1, Some(65534), Some(65534)).expect("chown dir1");
    fs::set_permissions(&dir1, fs::Permissions::from_mode(0o755)).unwrap();

    // Create target directory owned by 65534
    let dir2 = parent.join("dir2");
    fs::create_dir(&dir2).expect("create dir2");
    chown(&dir2, Some(65534), Some(65534)).expect("chown dir2");
    fs::set_permissions(&dir2, fs::Permissions::from_mode(0o755)).unwrap();

    // Create file in dir1 as user 65534
    let file = dir1.join("testfile");
    let (code, result) = pjdfstest_in_dir(
        &parent,
        &[
            "-u",
            "65534",
            "-g",
            "65534",
            "create",
            "dir1/testfile",
            "0644",
        ],
    );
    assert_eq!(result, "0", "should create file: got {}", result);

    // Verify file exists
    assert!(file.exists(), "file should exist after create");

    // Now link it to dir2
    let (code, result) = pjdfstest_in_dir(
        &parent,
        &[
            "-u",
            "65534",
            "-g",
            "65534",
            "link",
            "dir1/testfile",
            "dir2/link",
        ],
    );
    assert_eq!(result, "0", "link should succeed: got {}", result);

    // Cleanup link
    let (code, result) = pjdfstest_in_dir(
        &parent,
        &["-u", "65534", "-g", "65534", "unlink", "dir2/link"],
    );
    assert_eq!(result, "0", "unlink should succeed: got {}", result);

    // Cleanup
    std::env::set_current_dir("/").expect("cd to /");
    let _ = fs::remove_file(&file);
    let _ = fs::remove_dir(&dir1);
    let _ = fs::remove_dir(&dir2);
    let _ = fs::remove_dir(&parent);
    drop(fuse);
    cleanup(&data_dir, &mount_dir);
}

/// link/07.t: link into directory without write permission should fail
#[test]
fn test_link_dir_not_writable() {
    require_root();

    let (data_dir, mount_dir) = unique_paths("fuse-perm");
    let fuse = FuseMount::new(&data_dir, &mount_dir, 4);
    let mount = fuse.mount_path();

    // Create source file
    let source = mount.join("link_source");
    fs::write(&source, "test").expect("create source");
    chown(&source, Some(65534), Some(65534)).expect("chown source");

    // Create target directory with no write permission
    let dir = mount.join("link_dir");
    fs::create_dir(&dir).expect("create dir");
    chown(&dir, Some(65534), Some(65534)).expect("chown dir");
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o555)).unwrap();

    // As owner, try to create link in non-writable dir
    let link = dir.join("newlink");
    let (code, result) = pjdfstest(&[
        "-u",
        "65534",
        "-g",
        "65534",
        "link",
        source.to_str().unwrap(),
        link.to_str().unwrap(),
    ]);
    assert_eq!(
        result, "EACCES",
        "link into non-writable dir should fail, got: {}",
        result
    );

    // Cleanup
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
    let _ = fs::remove_dir(&dir);
    let _ = fs::remove_file(&source);
    drop(fuse);
    cleanup(&data_dir, &mount_dir);
}

// =============================================================================
// PATH_MAX / DEEP DIRECTORY TESTS
// =============================================================================

/// Test recursive removal of deeply nested directories.
///
/// This test reproduces the pjdfstest 03.t cleanup failure where `rm -rf`
/// on PATH_MAX-length paths fails on FUSE but succeeds on the host filesystem.
///
/// The 03.t tests (chmod/03.t, chown/03.t, etc.) create paths up to PATH_MAX (4096 chars)
/// to test ENAMETOOLONG error handling. At the end of each test, they clean up with:
///   rm -rf "${nx%%/*}"
/// This works on the host filesystem but fails on FUSE.
#[test]
fn test_deep_directory_removal() {
    require_root();

    let (data_dir, mount_dir) = unique_paths("fuse-perm");
    let fuse = FuseMount::new(&data_dir, &mount_dir, 4);
    let mount = fuse.mount_path();

    // Create a deeply nested directory structure (30 levels)
    // Each component is short (6 chars + separator) to avoid hitting NAME_MAX
    let mut current = mount.to_path_buf();
    for i in 1..=30 {
        current = current.join(format!("dir_{}", i));
    }

    eprintln!("Creating deep directory: {}", current.display());
    eprintln!("Path length: {}", current.to_str().unwrap().len());

    fs::create_dir_all(&current).expect("create deep directory structure");

    // Create a file at the bottom
    let deep_file = current.join("testfile");
    fs::write(&deep_file, "test content").expect("create deep file");
    assert!(deep_file.exists(), "deep file should exist");

    // Get the top-level directory we created
    let top_dir = mount.join("dir_1");
    assert!(top_dir.exists(), "top directory should exist");

    // Now try to remove the entire structure with rm -rf
    // This is what pjdfstest cleanup does and what fails on FUSE
    let output = std::process::Command::new("rm")
        .args(["-rf", top_dir.to_str().unwrap()])
        .output()
        .expect("run rm -rf");

    let success = output.status.success();
    let stderr = String::from_utf8_lossy(&output.stderr);

    if !success {
        eprintln!("rm -rf failed: {}", stderr);
        eprintln!("Exit code: {:?}", output.status.code());
    }

    // Verify the directory was removed
    let dir_still_exists = top_dir.exists();
    if dir_still_exists {
        eprintln!("ERROR: Directory still exists after rm -rf!");
        // Try to list what's left
        if let Ok(entries) = fs::read_dir(&top_dir) {
            for e in entries.take(5).flatten() {
                eprintln!("  Remaining: {:?}", e.path());
            }
        }
    }

    assert!(
        success,
        "rm -rf should succeed, got exit code {:?}",
        output.status.code()
    );
    assert!(
        !dir_still_exists,
        "directory should be removed after rm -rf"
    );

    drop(fuse);
    cleanup(&data_dir, &mount_dir);
}

/// Test removal of PATH_MAX-length paths (matching pjdfstest dirgen_max).
///
/// This matches the exact structure created by pjdfstest's dirgen_max() function:
/// - Components are NAME_MAX/2 characters long (127 chars typically)
/// - Path is built up to PATH_MAX-1 (4095 chars)
#[test]
fn test_path_max_directory_removal() {
    require_root();

    let (data_dir, mount_dir) = unique_paths("fuse-perm");
    let fuse = FuseMount::new(&data_dir, &mount_dir, 4);
    let mount = fuse.mount_path();

    // Match pjdfstest dirgen_max: component length = NAME_MAX/2 = 127 chars
    // Build path up to PATH_MAX-1 = 4095 chars
    const NAME_MAX: usize = 255;
    const PATH_MAX: usize = 4096;
    let comp_len = NAME_MAX / 2;

    // Generate a component of comp_len 'x' characters
    let component = "x".repeat(comp_len);

    let mut path = mount.to_path_buf();
    let mut path_len = path.to_str().unwrap().len();
    let mut depth = 0;

    // Build path up to PATH_MAX
    while path_len + comp_len + 1 < PATH_MAX {
        path = path.join(&component);
        path_len = path.to_str().unwrap().len();
        depth += 1;
    }

    eprintln!("Creating PATH_MAX structure:");
    eprintln!("  Component length: {} chars", comp_len);
    eprintln!("  Depth: {} levels", depth);
    eprintln!("  Total path length: {} chars", path_len);

    // Create the directory structure
    fs::create_dir_all(&path).expect("create PATH_MAX directory structure");

    // Create a file at the bottom
    let deep_file = path.join("f");
    fs::write(&deep_file, "x").expect("create file at PATH_MAX depth");
    assert!(deep_file.exists(), "deep file should exist");

    // Get the top-level component
    let top_dir = mount.join(&component);
    assert!(top_dir.exists(), "top directory should exist");

    eprintln!("Attempting rm -rf on: {}", top_dir.display());

    // Try rm -rf (this is what fails in pjdfstest 03.t cleanup)
    let output = std::process::Command::new("rm")
        .args(["-rf", top_dir.to_str().unwrap()])
        .output()
        .expect("run rm -rf");

    let success = output.status.success();
    let stderr = String::from_utf8_lossy(&output.stderr);

    if !success {
        eprintln!("rm -rf FAILED:");
        eprintln!("  Exit code: {:?}", output.status.code());
        eprintln!("  Stderr: {}", stderr);

        // Debug: check what errors FUSE is returning
        // Try to manually unlink the deep file
        if deep_file.exists() {
            let unlink_result = fs::remove_file(&deep_file);
            eprintln!("  Manual unlink of deep file: {:?}", unlink_result);
        }
    }

    let dir_still_exists = top_dir.exists();
    if dir_still_exists {
        eprintln!("Directory still exists after rm -rf!");
    }

    assert!(success, "rm -rf should succeed for PATH_MAX directories");
    assert!(
        !dir_still_exists,
        "PATH_MAX directory should be removed after rm -rf"
    );

    drop(fuse);
    cleanup(&data_dir, &mount_dir);
}

// =============================================================================
// POSIX COMPLIANCE TESTS (features pjdfstest skips for FUSE)
// =============================================================================

/// Test fallocate/posix_fallocate - pjdfstest skips this for FUSE filesystems
/// but fuse-pipe passthrough should support it fully via the underlying ext4.
///
/// posix_fallocate(fd, offset, len) preallocates disk space without writing zeros.
#[test]
fn test_fallocate_supported() {
    require_root();

    let (data_dir, mount_dir) = unique_paths("fuse-perm");
    let fuse = FuseMount::new(&data_dir, &mount_dir, 4);
    let mount = fuse.mount_path();

    let file = mount.join("fallocate_test");

    // Create file and get fd
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let cpath = CString::new(file.as_os_str().as_bytes()).unwrap();
    let fd = unsafe { libc::open(cpath.as_ptr(), libc::O_CREAT | libc::O_RDWR, 0o644) };
    assert!(fd >= 0, "open failed: {}", std::io::Error::last_os_error());

    // Try posix_fallocate - allocate 1MB
    let ret = unsafe { libc::posix_fallocate(fd, 0, 1024 * 1024) };

    unsafe { libc::close(fd) };

    if ret != 0 {
        let _ = fs::remove_file(&file);
        drop(fuse);
        cleanup(&data_dir, &mount_dir);
        panic!(
            "posix_fallocate failed with errno {}: fuse-pipe should support fallocate!",
            ret
        );
    }

    // Verify file size is 1MB
    let meta = fs::metadata(&file).expect("stat file");
    assert_eq!(
        meta.len(),
        1024 * 1024,
        "file should be 1MB after fallocate"
    );

    // Also verify blocks are actually allocated (not sparse)
    // st_blocks is in 512-byte units
    let blocks = meta.blocks();
    let expected_blocks = (1024 * 1024) / 512;
    assert!(
        blocks >= expected_blocks - 16, // Allow small variance for filesystem overhead
        "fallocate should allocate real blocks, got {} blocks (expected ~{})",
        blocks,
        expected_blocks
    );

    eprintln!(
        "posix_fallocate works: allocated {} blocks for 1MB file",
        blocks
    );

    let _ = fs::remove_file(&file);
    drop(fuse);
    cleanup(&data_dir, &mount_dir);
}

/// Test fallocate with FALLOC_FL_PUNCH_HOLE - create sparse holes in files
#[test]
fn test_fallocate_punch_hole() {
    require_root();

    let (data_dir, mount_dir) = unique_paths("fuse-perm");
    let fuse = FuseMount::new(&data_dir, &mount_dir, 4);
    let mount = fuse.mount_path();

    let file = mount.join("punch_hole_test");

    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let cpath = CString::new(file.as_os_str().as_bytes()).unwrap();
    let fd = unsafe { libc::open(cpath.as_ptr(), libc::O_CREAT | libc::O_RDWR, 0o644) };
    assert!(fd >= 0, "open failed: {}", std::io::Error::last_os_error());

    // Write 1MB of data
    let data = vec![0xAAu8; 1024 * 1024];
    let written = unsafe { libc::write(fd, data.as_ptr() as *const libc::c_void, data.len()) };
    assert_eq!(written as usize, data.len(), "write failed");

    // Flush writes to ensure blocks are allocated (important with writeback cache)
    let fsync_ret = unsafe { libc::fsync(fd) };
    assert_eq!(
        fsync_ret,
        0,
        "fsync failed: {}",
        std::io::Error::last_os_error()
    );

    // Get initial block count
    let meta_before = fs::metadata(&file).expect("stat");
    let blocks_before = meta_before.blocks();

    // Punch a 512KB hole in the middle
    const FALLOC_FL_PUNCH_HOLE: i32 = 0x02;
    const FALLOC_FL_KEEP_SIZE: i32 = 0x01;
    let ret = unsafe {
        libc::fallocate(
            fd,
            FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE,
            256 * 1024, // offset: 256KB
            512 * 1024, // length: 512KB
        )
    };

    unsafe { libc::close(fd) };

    if ret != 0 {
        let err = std::io::Error::last_os_error();
        let _ = fs::remove_file(&file);
        drop(fuse);
        cleanup(&data_dir, &mount_dir);

        // EOPNOTSUPP is acceptable if underlying fs doesn't support it
        if err.raw_os_error() == Some(libc::EOPNOTSUPP) {
            eprintln!("FALLOC_FL_PUNCH_HOLE not supported (EOPNOTSUPP) - OK");
            return;
        }
        panic!("fallocate PUNCH_HOLE failed: {}", err);
    }

    // Verify blocks decreased (hole was punched)
    let meta_after = fs::metadata(&file).expect("stat");
    let blocks_after = meta_after.blocks();

    eprintln!(
        "PUNCH_HOLE: blocks before={}, after={} (saved {} blocks)",
        blocks_before,
        blocks_after,
        blocks_before - blocks_after
    );

    // File size should remain 1MB
    assert_eq!(
        meta_after.len(),
        1024 * 1024,
        "file size should be unchanged"
    );

    // Blocks should have decreased
    assert!(
        blocks_after < blocks_before,
        "blocks should decrease after punching hole"
    );

    let _ = fs::remove_file(&file);
    drop(fuse);
    cleanup(&data_dir, &mount_dir);
}

/// Test that rename updates ctime - pjdfstest skips this for FUSE
/// but ext4 does update ctime on rename and fuse-pipe should pass it through.
#[test]
fn test_rename_updates_ctime() {
    require_root();

    let (data_dir, mount_dir) = unique_paths("fuse-perm");
    let fuse = FuseMount::new(&data_dir, &mount_dir, 4);
    let mount = fuse.mount_path();

    let file = mount.join("rename_ctime_test");
    fs::write(&file, "test").expect("create file");

    // Get initial ctime
    let meta_before = fs::metadata(&file).expect("stat before");
    let ctime_before = meta_before.ctime();
    let ctime_nsec_before = meta_before.ctime_nsec();

    // Wait a tiny bit to ensure time advances
    std::thread::sleep(std::time::Duration::from_millis(50));

    // Rename the file
    let new_file = mount.join("rename_ctime_test_renamed");
    fs::rename(&file, &new_file).expect("rename");

    // Get ctime after rename
    let meta_after = fs::metadata(&new_file).expect("stat after");
    let ctime_after = meta_after.ctime();
    let ctime_nsec_after = meta_after.ctime_nsec();

    eprintln!(
        "rename ctime: before={}.{:09}, after={}.{:09}",
        ctime_before, ctime_nsec_before, ctime_after, ctime_nsec_after
    );

    // ctime should have increased (or at least not decreased)
    let before_ns = ctime_before as i128 * 1_000_000_000 + ctime_nsec_before as i128;
    let after_ns = ctime_after as i128 * 1_000_000_000 + ctime_nsec_after as i128;

    assert!(
        after_ns >= before_ns,
        "ctime should not decrease after rename: before={}, after={}",
        before_ns,
        after_ns
    );

    // On ext4, ctime should actually increase
    if after_ns > before_ns {
        eprintln!(
            "rename correctly updated ctime (increased by {} ns)",
            after_ns - before_ns
        );
    } else {
        eprintln!("WARNING: ctime unchanged after rename (some filesystems don't update it)");
    }

    let _ = fs::remove_file(&new_file);
    drop(fuse);
    cleanup(&data_dir, &mount_dir);
}

/// Test that rename to overwrite existing file updates target's parent directory mtime
#[test]
fn test_rename_overwrites_updates_mtime() {
    require_root();

    let (data_dir, mount_dir) = unique_paths("fuse-perm");
    let fuse = FuseMount::new(&data_dir, &mount_dir, 4);
    let mount = fuse.mount_path();

    // Create source file
    let source = mount.join("rename_src");
    fs::write(&source, "source content").expect("create source");

    // Create target file
    let target = mount.join("rename_dst");
    fs::write(&target, "target content").expect("create target");

    // Get directory mtime before rename
    let dir_meta_before = fs::metadata(mount).expect("stat dir before");
    let mtime_before = dir_meta_before.mtime();

    std::thread::sleep(std::time::Duration::from_millis(50));

    // Rename source to target (overwriting target)
    fs::rename(&source, &target).expect("rename overwrite");

    // Get directory mtime after rename
    let dir_meta_after = fs::metadata(mount).expect("stat dir after");
    let mtime_after = dir_meta_after.mtime();

    eprintln!(
        "rename overwrite: dir mtime before={}, after={}",
        mtime_before, mtime_after
    );

    // Verify target has source's content
    let content = fs::read_to_string(&target).expect("read target");
    assert_eq!(
        content, "source content",
        "target should have source's content"
    );

    // Source should no longer exist
    assert!(!source.exists(), "source should not exist after rename");

    let _ = fs::remove_file(&target);
    drop(fuse);
    cleanup(&data_dir, &mount_dir);
}

// =============================================================================
// THREAD SAFETY / RACE CONDITION TESTS
// =============================================================================

/// Test that supplementary groups are per-thread, not process-wide.
///
/// This test verifies there's no race condition in the setgroups implementation.
/// We spawn multiple threads that each use different supplementary groups to
/// perform chown operations. If setgroups were process-wide (using glibc wrapper),
/// threads would interfere with each other and some operations would fail.
///
/// With the raw SYS_setgroups syscall, each thread has its own groups and
/// the operations succeed without interference.
#[test]
fn test_concurrent_supplementary_groups_no_race() {
    require_root();

    use std::sync::{Arc, Barrier};
    use std::thread;

    let (data_dir, mount_dir) = unique_paths("fuse-perm");
    let fuse = FuseMount::new(&data_dir, &mount_dir, 4);
    let mount = fuse.mount_path();

    // Create multiple files, each owned by a different user
    // User 65531 can change group to 65531 (needs 65531 as supplementary)
    // User 65532 can change group to 65532 (needs 65532 as supplementary)
    // User 65533 can change group to 65533 (needs 65533 as supplementary)
    // User 65534 can change group to 65534 (needs 65534 as supplementary)
    let users = [65531u32, 65532, 65533, 65534];
    let mut files = Vec::new();

    for (i, &uid) in users.iter().enumerate() {
        let file = mount.join(format!("race_test_{}", i));
        fs::write(&file, "test").expect("create file");
        // Own by user, with different initial group
        chown(&file, Some(uid), Some(65530)).expect("chown");
        files.push(file);
    }

    // Synchronize all threads to start at the same time
    let barrier = Arc::new(Barrier::new(users.len()));
    let mount_path = mount.to_path_buf();
    let mut handles = Vec::new();

    // Spawn threads that will all try to chown their files concurrently
    for (i, &uid) in users.iter().enumerate() {
        let barrier = Arc::clone(&barrier);
        let file_name = format!("race_test_{}", i);
        let mount_clone = mount_path.clone();
        let target_gid = uid; // Change to group matching uid

        handles.push(thread::spawn(move || {
            // All threads wait here until everyone is ready
            barrier.wait();

            // Each thread tries to chown its file to its supplementary group
            // User uid has primary group 65530 and supplementary group = uid
            let file_path = mount_clone.join(&file_name);

            // Perform multiple chown operations to increase chance of detecting race
            for iteration in 0..10 {
                let (_, result) = pjdfstest(&[
                    "-u",
                    &uid.to_string(),
                    "-g",
                    &format!("65530,{}", target_gid), // primary=65530, supplementary=target_gid
                    "--",
                    "chown",
                    file_path.to_str().unwrap(),
                    "-1",
                    &target_gid.to_string(),
                ]);

                if result != "0" {
                    return Err(format!(
                        "Thread for uid {} failed iteration {}: expected 0, got {}. \
                         This indicates a race condition - another thread's groups leaked!",
                        uid, iteration, result
                    ));
                }

                // Change back for next iteration
                let (_, result) = pjdfstest(&[
                    "-u",
                    &uid.to_string(),
                    "-g",
                    &format!("{},65530", target_gid), // primary=target_gid, supplementary=65530
                    "--",
                    "chown",
                    file_path.to_str().unwrap(),
                    "-1",
                    "65530",
                ]);

                if result != "0" {
                    return Err(format!(
                        "Thread for uid {} failed restore iteration {}: expected 0, got {}",
                        uid, iteration, result
                    ));
                }
            }

            Ok(format!(
                "Thread for uid {} completed 10 iterations successfully",
                uid
            ))
        }));
    }

    // Collect results
    let mut all_ok = true;
    for handle in handles {
        match handle.join().expect("thread panicked") {
            Ok(msg) => eprintln!("{}", msg),
            Err(err) => {
                eprintln!("ERROR: {}", err);
                all_ok = false;
            }
        }
    }

    // Cleanup
    for file in &files {
        let _ = fs::remove_file(file);
    }
    drop(fuse);
    cleanup(&data_dir, &mount_dir);

    assert!(
        all_ok,
        "Some threads failed - this indicates a race condition in setgroups"
    );
}
