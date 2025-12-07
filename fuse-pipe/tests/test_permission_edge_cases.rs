//! Focused permission edge case tests for FUSE passthrough filesystem.
//!
//! These tests reproduce specific pjdfstest failures to enable fast iteration.
//! They test edge cases in chmod, chown, open, truncate, and link operations.
//!
//! Run with: `sudo cargo test --test test_permission_edge_cases -- --nocapture`

// Allow unused variables - test code often has unused return values
#![allow(unused_variables)]

mod common;

use std::fs;
use std::os::unix::fs::{chown, MetadataExt, PermissionsExt};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Once;

use common::FuseMount;

static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);
static ULIMIT_INIT: Once = Once::new();

/// Increase file descriptor limit for tests with many readers
fn init_ulimit() {
    ULIMIT_INIT.call_once(|| {
        unsafe {
            let mut rlim = std::mem::MaybeUninit::<libc::rlimit>::uninit();
            if libc::getrlimit(libc::RLIMIT_NOFILE, rlim.as_mut_ptr()) == 0 {
                let mut rlim = rlim.assume_init();
                let target = 65536u64.min(rlim.rlim_max);
                if rlim.rlim_cur < target {
                    rlim.rlim_cur = target;
                    if libc::setrlimit(libc::RLIMIT_NOFILE, &rlim) == 0 {
                        eprintln!("[init] Raised fd limit to {}", target);
                    }
                }
            }
        }
    });
}

/// Create unique paths for each test.
fn unique_paths() -> (PathBuf, PathBuf) {
    let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
    let pid = std::process::id();
    let data_dir = PathBuf::from(format!("/tmp/fuse-perm-data-{}-{}", pid, id));
    let mount_dir = PathBuf::from(format!("/tmp/fuse-perm-mount-{}-{}", pid, id));

    let _ = fs::remove_dir_all(&data_dir);
    let _ = std::process::Command::new("fusermount3")
        .args(["-u", mount_dir.to_str().unwrap()])
        .status();
    let _ = fs::remove_dir_all(&mount_dir);

    (data_dir, mount_dir)
}

fn cleanup(data_dir: &PathBuf, mount_dir: &PathBuf) {
    let _ = fs::remove_dir_all(data_dir);
    let _ = fs::remove_dir_all(mount_dir);
}

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
        eprintln!("=== STRACE STDERR ===\n{}", String::from_utf8_lossy(&output.stderr));
    }

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let code = output.status.code().unwrap_or(-1);
    (code, stdout)
}

fn require_root() -> bool {
    if unsafe { libc::geteuid() } != 0 {
        eprintln!("[skip] Test requires root");
        return false;
    }
    init_ulimit();
    true
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
    if !require_root() { return; }

    let (data_dir, mount_dir) = unique_paths();
    let fuse = FuseMount::new(&data_dir, &mount_dir, 256);
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
    let (code, result) = pjdfstest(&["-u", "65534", "-g", "65534", "chmod", file.to_str().unwrap(), "0600"]);
    assert_eq!(result, "0", "chmod should succeed initially: got {}", result);

    // Now remove search permission from parent (chmod 0644 - no execute)
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o644)).unwrap();

    // Now chmod should fail with EACCES
    let (code, result) = pjdfstest(&["-u", "65534", "-g", "65534", "chmod", file.to_str().unwrap(), "0620"]);
    assert_eq!(result, "EACCES", "chmod should fail with EACCES when parent has no search permission, got: {}", result);

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
#[test]
fn test_write_clears_suid() {
    if !require_root() { return; }

    let (data_dir, mount_dir) = unique_paths();
    let fuse = FuseMount::new(&data_dir, &mount_dir, 256);
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
        "-u", "65534", "-g", "65534",
        "open", file.to_str().unwrap(), "O_WRONLY",
        ":", "write", "0", "x",
        ":", "fstat", "0", "mode"
    ]);

    // Should return 0777 (SUID cleared) - check last line of output
    let mode = result.lines().last().unwrap_or(&result);
    assert_eq!(mode, "0777", "write by non-owner should clear SUID, expected 0777 got: {}", mode);

    // Verify with stat
    let meta = fs::metadata(&file).unwrap();
    assert_eq!(meta.mode() & 0o7777, 0o0777, "SUID should be cleared after write");

    let _ = fs::remove_file(&file);
    drop(fuse);
    cleanup(&data_dir, &mount_dir);
}

/// chmod/12.t: Writing to SGID file should clear SGID bit
#[test]
fn test_write_clears_sgid() {
    if !require_root() { return; }

    let (data_dir, mount_dir) = unique_paths();
    let fuse = FuseMount::new(&data_dir, &mount_dir, 256);
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
        "-u", "65534", "-g", "65534",
        "open", file.to_str().unwrap(), "O_RDWR",
        ":", "write", "0", "x",
        ":", "fstat", "0", "mode"
    ]);

    // Should return 0777 (SGID cleared) - check last line of output
    let mode = result.lines().last().unwrap_or(&result);
    assert_eq!(mode, "0777", "write by non-owner should clear SGID, expected 0777 got: {}", mode);

    let _ = fs::remove_file(&file);
    drop(fuse);
    cleanup(&data_dir, &mount_dir);
}

/// chmod/12.t: Writing to SUID+SGID file should clear both bits
#[test]
fn test_write_clears_suid_and_sgid() {
    if !require_root() { return; }

    let (data_dir, mount_dir) = unique_paths();
    let fuse = FuseMount::new(&data_dir, &mount_dir, 256);
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
        "-u", "65534", "-g", "65534",
        "open", file.to_str().unwrap(), "O_RDWR",
        ":", "write", "0", "x",
        ":", "fstat", "0", "mode"
    ]);

    // Should return 0777 (both bits cleared) - check last line of output
    let mode = result.lines().last().unwrap_or(&result);
    assert_eq!(mode, "0777", "write by non-owner should clear SUID+SGID, expected 0777 got: {}", mode);

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
    if !require_root() { return; }

    let (data_dir, mount_dir) = unique_paths();
    let fuse = FuseMount::new(&data_dir, &mount_dir, 256);
    let mount = fuse.mount_path();

    // Create file owned by 65534:65533
    let file = mount.join("chown_primary_test");
    fs::write(&file, "test").expect("create file");
    chown(&file, Some(65534), Some(65533)).expect("chown");

    // As user 65534 with primary group 65532, change group to 65532
    // This should work because 65532 is the PRIMARY group (passed to setfsgid)
    let (code, result) = pjdfstest(&[
        "-u", "65534", "-g", "65532", "--",
        "chown", file.to_str().unwrap(), "-1", "65532"
    ]);
    assert_eq!(result, "0", "owner should be able to chown to primary group, got: {}", result);

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
    if !require_root() { return; }

    let (data_dir, mount_dir) = unique_paths();
    let fuse = FuseMount::new(&data_dir, &mount_dir, 256);
    let mount = fuse.mount_path();

    // Create file owned by 65534:65533
    let file = mount.join("chown_test");
    fs::write(&file, "test").expect("create file");
    chown(&file, Some(65534), Some(65533)).expect("chown");

    // As user 65534 with groups 65532,65531, change group to 65532 (primary)
    // pjdfstest: -u 65534 -g 65532,65531 -- chown file -1 65532
    let (code, result) = pjdfstest(&[
        "-u", "65534", "-g", "65532,65531", "--",
        "chown", file.to_str().unwrap(), "-1", "65532"
    ]);
    assert_eq!(result, "0", "owner should be able to chown to primary group, got: {}", result);

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
    if !require_root() { return; }

    let (data_dir, mount_dir) = unique_paths();
    let fuse = FuseMount::new(&data_dir, &mount_dir, 256);
    let mount = fuse.mount_path();

    // Create file owned by 65534:65533
    let file = mount.join("chown_suppl_test");
    fs::write(&file, "test").expect("create file");
    chown(&file, Some(65534), Some(65533)).expect("chown");

    // As user 65534 with groups 65532,65531, try to change group to 65531 (supplementary)
    // This should SUCCEED because we now read and adopt supplementary groups from /proc
    let (code, result) = pjdfstest(&[
        "-u", "65534", "-g", "65532,65531", "--",
        "chown", file.to_str().unwrap(), "-1", "65531"
    ]);

    // Should succeed now that we support supplementary groups
    assert_eq!(result, "0",
        "chown to supplementary group should succeed with /proc groups parsing, got: {}",
        result);

    let _ = fs::remove_file(&file);
    drop(fuse);
    cleanup(&data_dir, &mount_dir);
}

/// chown/07.t: Non-owner cannot chown
#[test]
fn test_chown_non_owner_fails() {
    if !require_root() { return; }

    let (data_dir, mount_dir) = unique_paths();
    let fuse = FuseMount::new(&data_dir, &mount_dir, 256);
    let mount = fuse.mount_path();

    // Create file owned by 65534:65534
    let file = mount.join("chown_nonowner");
    fs::write(&file, "test").expect("create file");
    chown(&file, Some(65534), Some(65534)).expect("chown");

    // As user 65533 (not owner), try to chown
    let (code, result) = pjdfstest(&[
        "-u", "65533", "-g", "65533",
        "chown", file.to_str().unwrap(), "65533", "65533"
    ]);
    assert_eq!(result, "EPERM", "non-owner chown should fail with EPERM, got: {}", result);

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
    if !require_root() { return; }

    let (data_dir, mount_dir) = unique_paths();
    let fuse = FuseMount::new(&data_dir, &mount_dir, 256);
    let mount = fuse.mount_path();

    // Create file with write-only permission (mode 0222)
    let file = mount.join("noread");
    fs::write(&file, "test").expect("create file");
    chown(&file, Some(65534), Some(65534)).expect("chown");
    fs::set_permissions(&file, fs::Permissions::from_mode(0o222)).unwrap();

    // As owner, try to open O_RDONLY
    let (code, result) = pjdfstest(&[
        "-u", "65534", "-g", "65534",
        "open", file.to_str().unwrap(), "O_RDONLY"
    ]);
    assert_eq!(result, "EACCES", "O_RDONLY without read permission should fail, got: {}", result);

    // Cleanup - restore permissions first
    fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
    let _ = fs::remove_file(&file);
    drop(fuse);
    cleanup(&data_dir, &mount_dir);
}

/// open/08.t: O_CREAT in directory without write permission should fail
#[test]
fn test_open_creat_dir_not_writable() {
    if !require_root() { return; }

    let (data_dir, mount_dir) = unique_paths();
    let fuse = FuseMount::new(&data_dir, &mount_dir, 256);
    let mount = fuse.mount_path();

    // Create directory with read+execute only (no write)
    let dir = mount.join("nowrite_dir");
    fs::create_dir(&dir).expect("create dir");
    chown(&dir, Some(65534), Some(65534)).expect("chown dir");
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o555)).unwrap();

    // As owner, try to create file in dir
    let file = dir.join("newfile");
    let (code, result) = pjdfstest(&[
        "-u", "65534", "-g", "65534",
        "open", file.to_str().unwrap(), "O_CREAT,O_RDWR", "0644"
    ]);
    assert_eq!(result, "EACCES", "O_CREAT in non-writable dir should fail, got: {}", result);

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
    if !require_root() { return; }

    let (data_dir, mount_dir) = unique_paths();
    let fuse = FuseMount::new(&data_dir, &mount_dir, 256);
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
        "-u", "65534", "-g", "65534",
        "truncate", file.to_str().unwrap(), "1234"
    ]);
    assert_eq!(result, "EACCES", "truncate should fail when parent has no search permission, got: {}", result);

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
    if !require_root() { return; }

    let (data_dir, mount_dir) = unique_paths();
    let fuse = FuseMount::new(&data_dir, &mount_dir, 256);
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
    eprintln!("Switched to fsuid=65534, fsgid=65534 (was {}, {})", orig_fsuid, orig_fsgid);
    eprintln!("Real uid={}, euid={}, gid={}, egid={}",
              unsafe { libc::getuid() }, unsafe { libc::geteuid() },
              unsafe { libc::getgid() }, unsafe { libc::getegid() });

    // Open with O_CREAT|O_RDWR, mode 0
    use std::ffi::CString;
    let cpath = CString::new(file.to_str().unwrap()).unwrap();
    let fd = unsafe {
        libc::open(cpath.as_ptr(), libc::O_CREAT | libc::O_RDWR, 0)
    };

    if fd < 0 {
        let err = std::io::Error::last_os_error();
        eprintln!("open failed: {} (errno {})", err, err.raw_os_error().unwrap_or(-1));
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
        eprintln!("ftruncate failed: {} (errno {})", err, err.raw_os_error().unwrap_or(-1));
    } else {
        eprintln!("ftruncate succeeded");
    }

    // Close fd
    unsafe { libc::close(fd) };

    // Restore credentials
    unsafe { libc::setfsuid(orig_fsuid as u32) };
    unsafe { libc::setfsgid(orig_fsgid as u32) };

    assert!(ret == 0, "ftruncate on O_RDWR fd should succeed even with mode 0, got errno {}",
            if ret < 0 { std::io::Error::last_os_error().raw_os_error().unwrap_or(-1) } else { 0 });

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
    if !require_root() { return; }

    let (data_dir, mount_dir) = unique_paths();
    let fuse = FuseMount::new(&data_dir, &mount_dir, 256);
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
    let (code, result) = pjdfstest_in_dir(&parent, &[
        "-u", "65534", "-g", "65534",
        "create", "dir1/testfile", "0644"
    ]);
    assert_eq!(result, "0", "should create file: got {}", result);

    // Verify file exists
    assert!(file.exists(), "file should exist after create");

    // Now link it to dir2
    let (code, result) = pjdfstest_in_dir(&parent, &[
        "-u", "65534", "-g", "65534",
        "link", "dir1/testfile", "dir2/link"
    ]);
    assert_eq!(result, "0", "link should succeed: got {}", result);

    // Cleanup link
    let (code, result) = pjdfstest_in_dir(&parent, &[
        "-u", "65534", "-g", "65534",
        "unlink", "dir2/link"
    ]);
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
    if !require_root() { return; }

    let (data_dir, mount_dir) = unique_paths();
    let fuse = FuseMount::new(&data_dir, &mount_dir, 256);
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
        "-u", "65534", "-g", "65534",
        "link", source.to_str().unwrap(), link.to_str().unwrap()
    ]);
    assert_eq!(result, "EACCES", "link into non-writable dir should fail, got: {}", result);

    // Cleanup
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
    let _ = fs::remove_dir(&dir);
    let _ = fs::remove_file(&source);
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
    if !require_root() { return; }

    use std::sync::{Arc, Barrier};
    use std::thread;

    let (data_dir, mount_dir) = unique_paths();
    let fuse = FuseMount::new(&data_dir, &mount_dir, 256);
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
                    "-u", &uid.to_string(),
                    "-g", &format!("65530,{}", target_gid), // primary=65530, supplementary=target_gid
                    "--",
                    "chown", file_path.to_str().unwrap(), "-1", &target_gid.to_string()
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
                    "-u", &uid.to_string(),
                    "-g", &format!("{},65530", target_gid), // primary=target_gid, supplementary=65530
                    "--",
                    "chown", file_path.to_str().unwrap(), "-1", "65530"
                ]);

                if result != "0" {
                    return Err(format!(
                        "Thread for uid {} failed restore iteration {}: expected 0, got {}",
                        uid, iteration, result
                    ));
                }
            }

            Ok(format!("Thread for uid {} completed 10 iterations successfully", uid))
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

    assert!(all_ok, "Some threads failed - this indicates a race condition in setgroups");
}
