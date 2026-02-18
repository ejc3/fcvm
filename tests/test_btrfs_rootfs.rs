//! Integration tests for btrfs root filesystem support.
//!
//! Tests two modes:
//! 1. **Native btrfs rootfs** (--rootfs-type btrfs): Root filesystem is btrfs,
//!    no loopback needed. fc-agent resizes btrfs at boot and uses it directly.
//! 2. **ext4 rootfs + btrfs loopback** (--rootfs-type ext4 or default): Root
//!    filesystem is ext4, fc-agent creates a btrfs loopback for container storage.
//!
//! Both modes use --kernel-profile btrfs (kernel with btrfs support).

#![cfg(feature = "integration-fast")]

mod common;

use anyhow::{Context, Result};

/// Native btrfs rootfs — root is btrfs, no loopback file.
///
/// Verifies:
/// - Root filesystem (/) is btrfs
/// - No loopback file at /var/lib/containers/btrfs.img
/// - Podman uses btrfs storage driver
/// - Container runs successfully
#[tokio::test]
async fn test_btrfs_rootfs_native() -> Result<()> {
    println!("\nNative Btrfs Rootfs Test");
    println!("========================");

    // Step 1: Start VM with btrfs kernel and btrfs rootfs
    println!("\n1. Starting VM (rootless, --kernel-profile btrfs --rootfs-type btrfs)...");
    let (vm_name, _, _, _) = common::unique_names("btrfs-rootfs");

    let (mut _child, fcvm_pid) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &vm_name,
        "--kernel-profile",
        "btrfs",
        "--rootfs-type",
        "btrfs",
        "nginx:alpine",
    ])
    .await
    .context("spawning fcvm")?;
    println!("  fcvm PID: {}", fcvm_pid);

    // Step 2: Wait for healthy
    println!("\n2. Waiting for VM to become healthy...");
    common::poll_health_by_pid(fcvm_pid, 300)
        .await
        .context("VM failed to become healthy")?;
    println!("  VM is healthy");

    // Step 3: Verify root filesystem is btrfs
    println!("\n3. Checking root filesystem type...");
    let root_fstype = common::exec_in_vm(fcvm_pid, &["findmnt", "-n", "-o", "FSTYPE", "/"])
        .await
        .context("checking root fstype")?;
    let root_fstype = root_fstype.trim();
    println!("  / fstype: {}", root_fstype);
    assert_eq!(
        root_fstype, "btrfs",
        "Root filesystem should be btrfs, got: {}",
        root_fstype
    );

    // Step 4: Verify no loopback file exists
    println!("\n4. Checking that no btrfs loopback file exists...");
    let loopback_check = common::exec_in_vm(
        fcvm_pid,
        &[
            "test",
            "-f",
            "/var/lib/containers/btrfs.img",
            "&&",
            "echo",
            "exists",
            "||",
            "echo",
            "absent",
        ],
    )
    .await
    .context("checking loopback file")?;
    let loopback_check = loopback_check.trim();
    println!("  btrfs.img: {}", loopback_check);
    assert_eq!(
        loopback_check, "absent",
        "Loopback file should not exist on native btrfs root, got: {}",
        loopback_check
    );

    // Step 5: Verify btrfs storage driver
    println!("\n5. Checking podman storage driver...");
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

    // Step 6: Verify container ran
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

    println!("\n  NATIVE BTRFS ROOTFS TEST PASSED");
    println!("  - Root filesystem is btrfs (no ext4)");
    println!("  - No loopback file created");
    println!("  - btrfs storage driver auto-configured");
    println!("  - Container ran successfully");

    Ok(())
}

/// Native btrfs rootfs with --user (keep-id).
///
/// Verifies that user namespace mapping works on native btrfs root.
#[tokio::test]
async fn test_btrfs_rootfs_keepid() -> Result<()> {
    println!("\nNative Btrfs Rootfs + Keep-ID Test");
    println!("===================================");

    // Step 1: Start VM with btrfs rootfs and user mapping
    println!("\n1. Starting VM (rootless, --kernel-profile btrfs --rootfs-type btrfs --user 1000:1000)...");
    let (vm_name, _, _, _) = common::unique_names("btrfs-rootfs-kid");

    let (mut _child, fcvm_pid) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &vm_name,
        "--kernel-profile",
        "btrfs",
        "--rootfs-type",
        "btrfs",
        "--user",
        "1000:1000",
        "alpine:latest",
        "sleep",
        "3600",
    ])
    .await
    .context("spawning fcvm")?;
    println!("  fcvm PID: {}", fcvm_pid);

    // Step 2: Wait for healthy
    println!("\n2. Waiting for VM to become healthy...");
    common::poll_health_by_pid(fcvm_pid, 300)
        .await
        .context("VM failed to become healthy")?;
    println!("  VM is healthy");

    // Step 3: Verify root is btrfs
    println!("\n3. Checking root filesystem type...");
    let root_fstype = common::exec_in_vm(fcvm_pid, &["findmnt", "-n", "-o", "FSTYPE", "/"])
        .await
        .context("checking root fstype")?;
    let root_fstype = root_fstype.trim();
    println!("  / fstype: {}", root_fstype);
    assert_eq!(root_fstype, "btrfs", "Root should be btrfs");

    // Step 4: Verify container runs as UID 1000
    println!("\n4. Checking container UID...");
    let uid = common::exec_in_container(fcvm_pid, &["id", "-u"])
        .await
        .context("checking uid in container")?;
    let uid = uid.trim();
    println!("  Container UID: {}", uid);
    assert_eq!(
        uid, "1000",
        "Container should run as UID 1000, got: {}",
        uid
    );

    // Cleanup
    println!("\n5. Cleaning up...");
    common::kill_process(fcvm_pid).await;

    println!("\n  BTRFS ROOTFS + KEEP-ID TEST PASSED");
    println!("  - Root filesystem is btrfs");
    println!("  - Container runs as UID 1000");

    Ok(())
}

/// Explicit ext4 rootfs with btrfs kernel — uses loopback (contrast test).
///
/// Verifies the existing behavior: even with btrfs kernel, when rootfs_type is
/// explicitly ext4, fc-agent creates a btrfs loopback for container storage.
#[tokio::test]
async fn test_btrfs_rootfs_ext4_loopback() -> Result<()> {
    println!("\nExt4 Rootfs + Btrfs Loopback Test (contrast)");
    println!("=============================================");

    // Step 1: Start VM with btrfs kernel but ext4 rootfs
    println!("\n1. Starting VM (rootless, --kernel-profile btrfs --rootfs-type ext4)...");
    let (vm_name, _, _, _) = common::unique_names("btrfs-rootfs-ext4");

    let (mut _child, fcvm_pid) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &vm_name,
        "--kernel-profile",
        "btrfs",
        "--rootfs-type",
        "ext4",
        "nginx:alpine",
    ])
    .await
    .context("spawning fcvm")?;
    println!("  fcvm PID: {}", fcvm_pid);

    // Step 2: Wait for healthy
    println!("\n2. Waiting for VM to become healthy...");
    common::poll_health_by_pid(fcvm_pid, 300)
        .await
        .context("VM failed to become healthy")?;
    println!("  VM is healthy");

    // Step 3: Verify root filesystem is ext4
    println!("\n3. Checking root filesystem type...");
    let root_fstype = common::exec_in_vm(fcvm_pid, &["findmnt", "-n", "-o", "FSTYPE", "/"])
        .await
        .context("checking root fstype")?;
    let root_fstype = root_fstype.trim();
    println!("  / fstype: {}", root_fstype);
    assert_eq!(
        root_fstype, "ext4",
        "Root filesystem should be ext4, got: {}",
        root_fstype
    );

    // Step 4: Verify btrfs loopback exists
    println!("\n4. Checking btrfs loopback file...");
    let loopback_check = common::exec_in_vm(
        fcvm_pid,
        &[
            "test",
            "-f",
            "/var/lib/containers/btrfs.img",
            "&&",
            "echo",
            "exists",
            "||",
            "echo",
            "absent",
        ],
    )
    .await
    .context("checking loopback file")?;
    let loopback_check = loopback_check.trim();
    println!("  btrfs.img: {}", loopback_check);
    assert_eq!(
        loopback_check, "exists",
        "Loopback file should exist on ext4 root, got: {}",
        loopback_check
    );

    // Step 5: Verify btrfs storage at storage dir (via loopback mount)
    println!("\n5. Checking btrfs mount at storage dir...");
    let storage_fstype = common::exec_in_vm(
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
    .context("checking storage fstype")?;
    let storage_fstype = storage_fstype.trim();
    println!("  /var/lib/containers/storage fstype: {}", storage_fstype);
    assert_eq!(
        storage_fstype, "btrfs",
        "Storage should be btrfs via loopback, got: {}",
        storage_fstype
    );

    // Step 6: Verify btrfs storage driver
    println!("\n6. Checking podman storage driver...");
    let driver = common::exec_in_vm(
        fcvm_pid,
        &["podman", "info", "--format", "{{.Store.GraphDriverName}}"],
    )
    .await
    .context("checking storage driver")?;
    let driver = driver.trim();
    println!("  GraphDriverName: {}", driver);
    assert_eq!(driver, "btrfs", "Expected btrfs driver, got: {}", driver);

    // Step 7: Verify container ran
    println!("\n7. Checking container execution...");
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
    println!("\n8. Cleaning up...");
    common::kill_process(fcvm_pid).await;

    println!("\n  EXT4 + BTRFS LOOPBACK TEST PASSED");
    println!("  - Root filesystem is ext4");
    println!("  - btrfs loopback created at /var/lib/containers/btrfs.img");
    println!("  - Storage mounted as btrfs via loopback");
    println!("  - Container ran successfully");

    Ok(())
}
