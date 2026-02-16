//! Integration test for rootless localhost container with btrfs storage
//!
//! Validates that:
//! 1. A locally-built container image can run in rootless mode
//! 2. When the kernel supports btrfs, fc-agent auto-configures btrfs storage
//! 3. Podman uses the btrfs driver (not overlay with chown-copy fallback)
//!
//! This catches the containers/storage issue where checkAndRecordIDMappedSupport()
//! hard-disables idmapped overlay detection for rootless, forcing --userns=keep-id
//! through the expensive storage-chown-by-maps path.

#![cfg(feature = "integration-fast")]

mod common;

use anyhow::{Context, Result};

/// Build a simple test container image using podman
async fn build_test_image() -> Result<()> {
    use std::io::Write;
    use tempfile::TempDir;

    let temp_dir = TempDir::new().context("creating temp dir")?;
    let containerfile_path = temp_dir.path().join("Containerfile");

    let mut file = std::fs::File::create(&containerfile_path)?;
    writeln!(
        file,
        r#"FROM public.ecr.aws/nginx/nginx:alpine
CMD ["sh", "-c", "echo 'btrfs-rootless-test-ok' && sleep 600"]"#
    )?;

    let output = tokio::process::Command::new("podman")
        .args([
            "build",
            "-t",
            "localhost/test-btrfs-rootless",
            "-f",
            &containerfile_path.to_string_lossy(),
            temp_dir.path().to_str().unwrap(),
        ])
        .output()
        .await
        .context("running podman build")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Failed to build test image: {}", stderr);
    }

    println!("  Built localhost/test-btrfs-rootless image");
    Ok(())
}

/// Test rootless localhost container with btrfs kernel profile
///
/// Uses --kernel-profile btrfs to get a kernel with CONFIG_BTRFS_FS=y.
/// fc-agent detects btrfs support at runtime and auto-configures podman
/// to use the btrfs storage driver via a loopback filesystem.
#[tokio::test]
async fn test_localhost_rootless_btrfs_storage() -> Result<()> {
    println!("\nRootless Localhost + Btrfs Storage Test");
    println!("=======================================");

    // Step 1: Build test image
    println!("\n1. Building test container image...");
    build_test_image().await?;

    // Step 2: Start VM in rootless mode with btrfs kernel
    println!("\n2. Starting VM (rootless, --kernel-profile btrfs)...");
    let (vm_name, _, _, _) = common::unique_names("btrfs-rootless");

    let (mut _child, fcvm_pid) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &vm_name,
        "--kernel-profile",
        "btrfs",
        "localhost/test-btrfs-rootless",
    ])
    .await
    .context("spawning fcvm")?;
    println!("  fcvm PID: {}", fcvm_pid);

    // Step 3: Wait for healthy
    println!("\n3. Waiting for VM to become healthy...");
    common::poll_health_by_pid(fcvm_pid, 180)
        .await
        .context("VM failed to become healthy")?;
    println!("  VM is healthy");

    // Step 4: Verify btrfs storage driver
    println!("\n4. Checking podman storage driver...");
    let driver = common::exec_in_vm(
        fcvm_pid,
        &["podman", "info", "--format", "{{.Store.GraphDriverName}}"],
    )
    .await
    .context("checking storage driver")?;
    let driver = driver.trim();
    println!("  GraphDriverName: {}", driver);
    assert_eq!(
        driver, "btrfs",
        "Expected btrfs storage driver, got: {}",
        driver
    );

    // Step 5: Verify btrfs mount
    println!("\n5. Checking btrfs mount...");
    let fstype = common::exec_in_vm(
        fcvm_pid,
        &[
            "findmnt",
            "-n",
            "-o",
            "FSTYPE",
            "/var/lib/containers/storage",
        ],
    )
    .await
    .context("checking btrfs mount")?;
    let fstype = fstype.trim();
    println!("  /var/lib/containers/storage fstype: {}", fstype);
    assert_eq!(
        fstype, "btrfs",
        "Expected btrfs filesystem at /var/lib/containers/storage, got: {}",
        fstype
    );

    // Step 6: Verify container ran (check podman ps -a for our container)
    println!("\n6. Checking container execution...");
    let ps_output = common::exec_in_vm(fcvm_pid, &["podman", "ps", "-a", "--format", "{{.Names}}"])
        .await
        .context("checking container status")?;
    println!("  Container names: {}", ps_output.trim());
    assert!(
        ps_output.contains("fcvm-container"),
        "Container should have run: {}",
        ps_output
    );

    // Cleanup
    println!("\n7. Cleaning up...");
    common::kill_process(fcvm_pid).await;

    println!("\n  ROOTLESS + BTRFS STORAGE TEST PASSED");
    println!("  - localhost image built and loaded in rootless mode");
    println!("  - btrfs storage driver auto-detected and configured");
    println!("  - Container ran successfully on btrfs storage");

    Ok(())
}

/// Test rootless localhost container with --user (keep-id) and btrfs storage
///
/// This is the critical path that triggered the original issue:
/// - --user 1000:1000 causes fc-agent to run podman with --userns=keep-id
/// - With overlay, checkAndRecordIDMappedSupport() returns false for rootless
/// - This forces storage-chown-by-maps (expensive chown-copy of every layer)
/// - With btrfs, user namespaces work natively — no chown fallback needed
///
/// Note: --user causes fc-agent to run podman as fcvm-user (rootless).
/// Root's `podman inspect` can't see rootless containers, so the standard
/// health monitor can't detect container health. We poll btrfs readiness
/// and use `runuser -u fcvm-user` for podman queries.
#[tokio::test]
async fn test_localhost_rootless_btrfs_keepid() -> Result<()> {
    println!("\nRootless Localhost + Btrfs + keep-id Test");
    println!("==========================================");

    // Step 1: Build test image (same image, snapshot key includes --user)
    println!("\n1. Building test container image...");
    build_test_image().await?;

    // Step 2: Start VM with --user 1000:1000 (triggers keep-id in fc-agent)
    println!("\n2. Starting VM (rootless, --kernel-profile btrfs, --user 1000:1000)...");
    let (vm_name, _, _, _) = common::unique_names("btrfs-keepid");

    let (mut _child, fcvm_pid) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &vm_name,
        "--kernel-profile",
        "btrfs",
        "--user",
        "1000:1000",
        "localhost/test-btrfs-rootless",
    ])
    .await
    .context("spawning fcvm")?;
    println!("  fcvm PID: {}", fcvm_pid);

    // Step 3: Wait for container to start by polling btrfs mount and user container.
    // Standard health monitor uses root's `podman inspect` which can't see rootless
    // containers, so we poll directly using runuser.
    println!("\n3. Waiting for container to start...");
    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(180);
    loop {
        if tokio::time::Instant::now() > deadline {
            anyhow::bail!("Timed out waiting for container to start");
        }
        // Check if the container is visible via rootless podman
        if let Ok(ps) = common::exec_in_vm(
            fcvm_pid,
            &[
                "runuser",
                "-u",
                "fcvm-user",
                "--",
                "podman",
                "ps",
                "-a",
                "--format",
                "{{.Names}}",
            ],
        )
        .await
        {
            if ps.contains("fcvm-container") {
                println!("  Container is running");
                break;
            }
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
    }

    // Step 4: Verify btrfs storage driver (root's podman info shows system-wide config)
    println!("\n4. Checking podman storage driver...");
    let driver = common::exec_in_vm(
        fcvm_pid,
        &[
            "runuser",
            "-u",
            "fcvm-user",
            "--",
            "podman",
            "info",
            "--format",
            "{{.Store.GraphDriverName}}",
        ],
    )
    .await
    .context("checking storage driver")?;
    let driver = driver.trim();
    println!("  GraphDriverName: {}", driver);
    assert_eq!(
        driver, "btrfs",
        "Expected btrfs storage driver, got: {}",
        driver
    );

    // Step 5: Verify btrfs mount
    println!("\n5. Checking btrfs mount...");
    let fstype = common::exec_in_vm(
        fcvm_pid,
        &[
            "findmnt",
            "-n",
            "-o",
            "FSTYPE",
            "/var/lib/containers/storage",
        ],
    )
    .await
    .context("checking btrfs mount")?;
    let fstype = fstype.trim();
    println!("  /var/lib/containers/storage fstype: {}", fstype);
    assert_eq!(
        fstype, "btrfs",
        "Expected btrfs filesystem at /var/lib/containers/storage, got: {}",
        fstype
    );

    // Step 6: Verify container ran with correct user
    println!("\n6. Checking container execution...");
    let ps_output = common::exec_in_vm(
        fcvm_pid,
        &[
            "runuser",
            "-u",
            "fcvm-user",
            "--",
            "podman",
            "ps",
            "-a",
            "--format",
            "{{.Names}}",
        ],
    )
    .await
    .context("checking container status")?;
    println!("  Container names: {}", ps_output.trim());
    assert!(
        ps_output.contains("fcvm-container"),
        "Container should have run: {}",
        ps_output
    );

    // Step 7: Verify userns=keep-id was used (container process runs as uid 1000)
    // Note: podman's HostConfig.UsernsMode reports "private" for keep-id (Docker compat
    // doesn't distinguish the two). Instead, verify the actual behavior: the container
    // process should run as uid 1000, proving keep-id mapped host UID into the container.
    println!("\n7. Checking container runs as expected user...");
    let container_uid = common::exec_in_vm(
        fcvm_pid,
        &[
            "runuser",
            "-u",
            "fcvm-user",
            "--",
            "podman",
            "exec",
            "fcvm-container",
            "id",
            "-u",
        ],
    )
    .await
    .context("checking container uid")?;
    let container_uid = container_uid.trim();
    println!("  Container UID: {}", container_uid);
    assert_eq!(
        container_uid, "1000",
        "Expected container to run as uid 1000 (keep-id), got: {}",
        container_uid
    );

    // Cleanup
    println!("\n8. Cleaning up...");
    common::kill_process(fcvm_pid).await;

    println!("\n  ROOTLESS + BTRFS + KEEP-ID TEST PASSED");
    println!("  - localhost image with --user 1000:1000 (keep-id mode)");
    println!("  - btrfs storage driver avoids storage-chown-by-maps fallback");
    println!("  - Container ran successfully with user namespace mapping");

    Ok(())
}
