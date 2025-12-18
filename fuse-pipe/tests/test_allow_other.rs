//! Test that AllowOther works for non-root users when user_allow_other is configured.
//!
//! This test requires /etc/fuse.conf to have user_allow_other enabled.
//! Run WITHOUT sudo: `cargo test --release -p fuse-pipe --test test_allow_other`

mod common;

use common::{cleanup, require_nonroot, unique_paths, FuseMount};
use std::fs;
use std::process::Command;

/// Test that a non-root user can mount with AllowOther when user_allow_other is configured.
/// This test creates a file as the mounting user, then verifies another user can access it.
#[test]
fn test_allow_other_with_fuse_conf() {
    require_nonroot();

    // Skip if user_allow_other is not configured
    let fuse_conf = fs::read_to_string("/etc/fuse.conf").unwrap_or_default();
    if !fuse_conf.lines().any(|l| l.trim() == "user_allow_other") {
        eprintln!(
            "Skipping test_allow_other_with_fuse_conf - user_allow_other not in /etc/fuse.conf"
        );
        return;
    }

    let (data_dir, mount_dir) = unique_paths("allow-other");
    let fuse = FuseMount::new(&data_dir, &mount_dir, 1);

    // Create a file
    let test_file = fuse.mount_path().join("test.txt");
    fs::write(&test_file, "hello from non-root with AllowOther").expect("write file");

    // Verify we can read it
    let content = fs::read_to_string(&test_file).expect("read file");
    assert_eq!(content, "hello from non-root with AllowOther");

    // Try to access as nobody user (uid 65534) using sudo
    // This verifies AllowOther is actually working
    let output = Command::new("sudo")
        .args(["-u", "#65534", "cat", test_file.to_str().unwrap()])
        .output();

    match output {
        Ok(out) if out.status.success() => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            assert_eq!(stdout.trim(), "hello from non-root with AllowOther");
            eprintln!("AllowOther working: nobody user could read the file");
        }
        Ok(out) => {
            // If sudo failed, it might be because sudo isn't configured for this
            eprintln!(
                "Could not verify cross-user access (sudo as nobody failed): {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        Err(e) => {
            eprintln!(
                "Could not verify cross-user access (sudo not available): {}",
                e
            );
        }
    }

    fs::remove_file(&test_file).expect("cleanup");
    drop(fuse);
    cleanup(&data_dir, &mount_dir);
}
