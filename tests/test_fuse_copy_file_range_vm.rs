//! Test copy_file_range through FUSE inside a VM with patched kernel.
//!
//! This test requires the nested kernel profile which includes the FUSE remap_file_range patch.
//! The patch adds kernel-side support for copy_file_range through FUSE filesystems.
//!
//! Run with: cargo nextest run --test test_fuse_copy_file_range_vm --features privileged-tests

#![cfg(feature = "privileged-tests")]

mod common;

use anyhow::{Context, Result};
use std::process::Stdio;

/// Test copy_file_range through FUSE inside a VM with the patched kernel.
///
/// This test:
/// 1. Uses the nested kernel profile (includes FUSE remap patch)
/// 2. Boots a VM with that kernel
/// 3. Runs a copy_file_range test inside the VM's FUSE mount
#[tokio::test]
async fn test_copy_file_range_in_vm() -> Result<()> {
    println!("\nFUSE copy_file_range test (in VM with patched kernel)");
    println!("=====================================================");

    // Create logger for file output
    let logger = common::TestLogger::new("fuse-cfr");

    let fcvm_path = common::find_fcvm_binary()?;
    let (vm_name, _, _, _) = common::unique_names("fuse-cfr");

    // Create temp directory for FUSE mount
    let data_dir = format!("/tmp/fuse-cfr-{}", std::process::id());
    tokio::fs::create_dir_all(&data_dir).await?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(&data_dir, std::fs::Permissions::from_mode(0o777)).await?;
    }

    let map_arg = format!("{}:/testdir", data_dir);

    // Test script that runs inside the VM
    // It creates files on the FUSE mount and tests copy_file_range
    // Note: --cmd is passed directly to container, so we need sh -c wrapper
    // Alpine needs build-base for gcc
    let test_script = r#"sh -c '
set -e
cd /testdir

# Install gcc for compiling the test program
apk add --no-cache build-base >/dev/null 2>&1

# Create source file
echo "Hello, copy_file_range through FUSE!" > source.txt

# Create empty destination
touch dest.txt

# Compile and run test program for copy_file_range
cat > /tmp/test_cfr.c << "CEOF"
#define _GNU_SOURCE
#include <fcntl.h>
#include <stdio.h>
#include <unistd.h>
#include <errno.h>
#include <string.h>

int main() {
    int fd_in = open("/testdir/source.txt", O_RDONLY);
    if (fd_in < 0) { perror("open source"); return 1; }

    int fd_out = open("/testdir/dest.txt", O_WRONLY | O_TRUNC);
    if (fd_out < 0) { perror("open dest"); return 1; }

    off_t off_in = 0, off_out = 0;
    ssize_t result = copy_file_range(fd_in, &off_in, fd_out, &off_out, 100, 0);

    if (result < 0) {
        fprintf(stderr, "copy_file_range failed: %s (errno=%d)\n", strerror(errno), errno);
        return 1;
    }

    printf("copy_file_range: copied %zd bytes\n", result);
    close(fd_in);
    close(fd_out);

    // Verify content
    fd_out = open("/testdir/dest.txt", O_RDONLY);
    char buf[256];
    ssize_t n = read(fd_out, buf, sizeof(buf)-1);
    buf[n] = 0;
    printf("Content: %s", buf);
    close(fd_out);

    return 0;
}
CEOF

gcc /tmp/test_cfr.c -o /tmp/test_cfr
/tmp/test_cfr

echo "SUCCESS: copy_file_range works through FUSE!"
'"#;

    // Start VM with nested kernel profile
    println!("\nStarting VM with nested kernel profile...");

    let mut cmd = tokio::process::Command::new(&fcvm_path);
    cmd.args([
        "podman",
        "run",
        "--name",
        &vm_name,
        "--network",
        "bridged",
        "--kernel-profile",
        "nested",
        "--map",
        &map_arg,
        "--cmd",
        test_script,
        common::TEST_IMAGE, // Use ECR to avoid Docker Hub rate limits
    ])
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());

    // Preserve SUDO_USER if set
    if let Ok(sudo_user) = std::env::var("SUDO_USER") {
        cmd.env("SUDO_USER", sudo_user);
    }

    let mut child = cmd.spawn().context("spawning VM")?;
    let vm_pid = child.id().ok_or_else(|| anyhow::anyhow!("no VM PID"))?;
    println!("  VM started (PID: {})", vm_pid);
    logger.info(&format!("Spawned VM PID={}", vm_pid));

    // Consume output with file logging
    common::spawn_log_consumer_with_logger(child.stdout.take(), "fuse-cfr", logger.clone());
    common::spawn_log_consumer_stderr_with_logger(child.stderr.take(), "fuse-cfr", logger.clone());

    // Wait for completion (5 min timeout)
    let timeout = std::time::Duration::from_secs(300);
    let result = tokio::time::timeout(timeout, child.wait()).await;

    // Cleanup
    let _ = tokio::fs::remove_dir_all(&data_dir).await;

    let exit_status = match result {
        Ok(Ok(status)) => status,
        Ok(Err(e)) => anyhow::bail!("Error waiting for VM: {}", e),
        Err(_) => {
            common::kill_process(vm_pid).await;
            anyhow::bail!("VM timeout after {} seconds", timeout.as_secs());
        }
    };

    if !exit_status.success() {
        anyhow::bail!(
            "copy_file_range test failed: exit={}",
            exit_status.code().unwrap_or(-1)
        );
    }

    println!("\n✅ FUSE copy_file_range test PASSED!");
    Ok(())
}
