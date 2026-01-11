//! Test that verifies the kernel patch for utimensat POSIX compliance
//!
//! This test uses a nested VM with our patched kernel to verify that
//! utimensat(UTIME_NOW) works correctly for non-owner users with write permission.
//!
//! The kernel patch (0002-fuse-fix-utimensat-with-default-permissions.patch) fixes
//! two issues:
//! 1. Adds ATTR_FORCE when ATTR_TOUCH is set and user has write permission
//! 2. Sends FATTR_MTIME_NOW even with writeback cache for touch operations

#![cfg(feature = "privileged-tests")]

mod common;

use anyhow::{bail, Context, Result};

/// Test POSIX utimensat behavior using pjdfstest on nested kernel with our patch
///
/// Runs the full pjdfstest utimensat category (122 tests) inside a VM
/// with the nested kernel profile that includes our FUSE patch.
#[tokio::test]
async fn test_utimensat_pjdfstest_nested_kernel() -> Result<()> {
    println!("\nUtimensat POSIX Compliance Test (pjdfstest)");
    println!("============================================");
    println!("Testing kernel patch: 0002-fuse-fix-utimensat-with-default-permissions.patch\n");

    let _fcvm_path = common::find_fcvm_binary()?;
    let (vm_name, _, _, _) = common::unique_names("utimensat");

    // Ensure pjdfstest container exists
    println!("1. Checking pjdfstest container...");
    let check = tokio::process::Command::new("podman")
        .args(["image", "exists", "localhost/pjdfstest"])
        .output()
        .await?;

    if !check.status.success() {
        println!("   Building pjdfstest container...");
        let build = tokio::process::Command::new("podman")
            .args([
                "build",
                "-t",
                "pjdfstest",
                "-f",
                "Containerfile.pjdfstest",
                ".",
            ])
            .output()
            .await
            .context("building pjdfstest container")?;

        if !build.status.success() {
            bail!(
                "Failed to build pjdfstest: {}",
                String::from_utf8_lossy(&build.stderr)
            );
        }
    }
    println!("   pjdfstest container ready\n");

    // Create temp directory for FUSE mount
    let data_dir = format!("/tmp/utimensat-test-{}", std::process::id());
    tokio::fs::create_dir_all(&data_dir).await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(&data_dir, std::fs::Permissions::from_mode(0o777)).await?;
    }
    let map_arg = format!("{}:/testdir", data_dir);

    // Start VM with nested kernel (which has our patch)
    println!("2. Starting VM with nested kernel profile...");
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
        &map_arg,
        "--cmd",
        "prove -v -j 8 /opt/pjdfstest/tests/utimensat/",
        "localhost/pjdfstest",
    ])
    .await
    .context("spawning VM with nested kernel")?;

    println!("   VM PID: {}", fcvm_pid);

    // Wait for completion (5 min timeout)
    println!("   Running pjdfstest utimensat category...\n");
    let timeout = std::time::Duration::from_secs(300);
    let result = tokio::time::timeout(timeout, child.wait()).await;

    // Cleanup
    let _ = tokio::fs::remove_dir_all(&data_dir).await;

    let exit_status = match result {
        Ok(Ok(status)) => status,
        Ok(Err(e)) => {
            common::kill_process(fcvm_pid).await;
            bail!("Error waiting for VM: {}", e);
        }
        Err(_) => {
            common::kill_process(fcvm_pid).await;
            bail!("VM timeout after {} seconds", timeout.as_secs());
        }
    };

    if exit_status.success() {
        println!("\n✅ PJDFSTEST UTIMENSAT PASSED!");
        println!("   All 122 utimensat tests passed with patched kernel.");
        println!("   Kernel patch is working correctly.");
        Ok(())
    } else {
        bail!(
            "pjdfstest utimensat failed: exit={}\n\n\
            The kernel patch may not be applied correctly.\n\
            Check: fcvm setup --kernel-profile nested --build-kernels",
            exit_status.code().unwrap_or(-1)
        )
    }
}
