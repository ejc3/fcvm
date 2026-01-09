//! Test that verifies the kernel patch for utimensat POSIX compliance
//!
//! This test uses a nested VM with our patched kernel to verify that
//! utimensat(UTIME_NOW) works correctly for non-owner users with write permission.
//!
//! The kernel patch (0002-fuse-fix-utimensat-with-default-permissions.patch) adds
//! ATTR_FORCE when ATTR_TOUCH is set and user has write permission.

#![cfg(feature = "privileged-tests")]

mod common;

use anyhow::{bail, Context, Result};
use std::process::Stdio;

/// Test POSIX utimensat behavior on FUSE with default_permissions
///
/// POSIX says: To set timestamps to current time (UTIME_NOW), caller needs
/// write permission OR be the owner. With our kernel patch, this should work.
///
/// Test steps:
/// 1. Start VM with nested kernel (includes our patch)
/// 2. Mount FUSE filesystem with default_permissions inside VM
/// 3. Create file as root, give write permission to others
/// 4. Switch to non-owner user
/// 5. Call utimensat(UTIME_NOW) - should succeed
#[tokio::test]
async fn test_utimensat_non_owner_with_write_permission() -> Result<()> {
    println!("\nUtimensat POSIX Compliance Test");
    println!("================================");
    println!("Testing kernel patch: 0002-fuse-fix-utimensat-with-default-permissions.patch\n");

    let fcvm_path = common::find_fcvm_binary()?;
    let (vm_name, _, _, _) = common::unique_names("utimensat");

    // Start VM with nested kernel (which has our patch)
    println!("1. Starting VM with nested kernel profile...");
    let (mut child, fcvm_pid) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &vm_name,
        "--network",
        "bridged",
        "--kernel-profile",
        "nested",
        "--privileged",
        "--map",
        "/mnt/fcvm-btrfs:/mnt/fcvm-btrfs",
        common::TEST_IMAGE,
    ])
    .await
    .context("spawning VM with nested kernel")?;

    println!("   VM PID: {}", fcvm_pid);

    // Wait for VM to become healthy
    println!("   Waiting for VM health...");
    if let Err(e) = common::poll_health(&mut child, 180).await {
        let _ = child.kill().await;
        return Err(e.context("VM failed to become healthy"));
    }
    println!("   ✓ VM is healthy\n");

    // Test script that verifies utimensat behavior
    // This runs in the VM's guest OS where our patched kernel is active
    let test_script = r#"
set -e

echo "=== Setting up test environment ==="

# Create a test directory on the FUSE mount
TEST_DIR="/mnt/fcvm-btrfs/utimensat-test-$$"
mkdir -p "$TEST_DIR"
cd "$TEST_DIR"

# Create test file as root
echo "test content" > testfile.txt

# Set permissions: owner=root, write permission for others
chmod 666 testfile.txt
ls -la testfile.txt

# Create a test user (non-root)
useradd -m testuser 2>/dev/null || true

echo ""
echo "=== Testing utimensat as non-owner with write permission ==="

# Run utimensat test as non-owner
# touch -m uses utimensat() to set mtime to current time
su -s /bin/sh testuser -c "
    echo 'Running as:' \$(id)
    echo 'File owner:' \$(stat -c '%U' testfile.txt)
    echo 'File permissions:' \$(stat -c '%a' testfile.txt)

    # Get original mtime
    ORIG_MTIME=\$(stat -c '%Y' testfile.txt)
    echo \"Original mtime: \$ORIG_MTIME\"

    # Sleep to ensure time difference
    sleep 1

    # This calls utimensat(fd, NULL, UTIME_NOW) internally
    # Should succeed if user has write permission (POSIX compliant)
    if touch -m testfile.txt 2>/dev/null; then
        NEW_MTIME=\$(stat -c '%Y' testfile.txt)
        echo \"New mtime: \$NEW_MTIME\"

        if [ \"\$NEW_MTIME\" -gt \"\$ORIG_MTIME\" ]; then
            echo 'UTIMENSAT_TEST_PASSED'
        else
            echo 'UTIMENSAT_MTIME_NOT_CHANGED'
        fi
    else
        echo 'UTIMENSAT_TEST_FAILED_EPERM'
    fi
"

# Cleanup
cd /
rm -rf "$TEST_DIR"
userdel testuser 2>/dev/null || true

echo ""
echo "=== Test complete ==="
"#;

    // Run the test script in the VM
    println!("2. Running utimensat test in VM...\n");
    let output = tokio::process::Command::new(&fcvm_path)
        .args([
            "exec",
            "--pid",
            &fcvm_pid.to_string(),
            "--vm",
            "--",
            "sh",
            "-c",
            test_script,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .context("running test script")?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // Print output
    for line in stdout.lines() {
        println!("   {}", line);
    }
    if !stderr.is_empty() {
        println!("\n   stderr:");
        for line in stderr.lines() {
            println!("   {}", line);
        }
    }

    // Cleanup VM
    println!("\n3. Cleaning up...");
    common::kill_process(fcvm_pid).await;

    // Check results
    let combined = format!("{}\n{}", stdout, stderr);

    if combined.contains("UTIMENSAT_TEST_PASSED") {
        println!("\n✅ UTIMENSAT TEST PASSED!");
        println!("   Non-owner with write permission can call utimensat(UTIME_NOW)");
        println!("   Kernel patch is working correctly.");
        Ok(())
    } else if combined.contains("UTIMENSAT_TEST_FAILED_EPERM") {
        bail!(
            "UTIMENSAT TEST FAILED: Got EPERM\n\n\
            The kernel patch may not be applied or loaded.\n\
            Expected: utimensat(UTIME_NOW) succeeds for non-owner with write permission\n\
            Got: EPERM (operation not permitted)\n\n\
            Make sure:\n\
            1. Kernel is built with patch 0002-fuse-fix-utimensat-with-default-permissions.patch\n\
            2. The nested kernel profile is using the patched kernel\n\
            3. Reboot if needed after kernel rebuild"
        )
    } else if combined.contains("UTIMENSAT_MTIME_NOT_CHANGED") {
        bail!(
            "UTIMENSAT TEST FAILED: mtime not changed\n\n\
            utimensat() may have silently failed or the sleep wasn't long enough."
        )
    } else {
        bail!(
            "UTIMENSAT TEST FAILED: Unexpected output\n\
            stdout: {}\n\
            stderr: {}",
            stdout, stderr
        )
    }
}
