//! Integration test for --rootfs-size flag
//!
//! Verifies that the root filesystem is expanded to provide
//! the requested minimum free space.

#![cfg(feature = "privileged-tests")]

mod common;

use anyhow::{Context, Result};

#[tokio::test]
async fn test_rootfs_size_50g() -> Result<()> {
    println!("\nTest --rootfs-size 50G");
    println!("======================");

    let (vm_name, _, _, _) = common::unique_names("rootfs-size");

    let (_, fcvm_pid) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &vm_name,
        "--rootfs-size",
        "50G",
        common::TEST_IMAGE,
    ])
    .await
    .context("spawning fcvm")?;

    println!("  fcvm PID: {}", fcvm_pid);
    println!("  Waiting for VM to become healthy...");

    common::poll_health_by_pid(fcvm_pid, 300).await?;
    println!("  VM healthy");

    // Check root filesystem size
    let df_output = common::exec_in_vm(fcvm_pid, &["df", "-B1", "/"]).await?;
    println!("  df output: {}", df_output.trim());

    // Parse available bytes from df output (4th column = Available)
    // Example: "Filesystem     1B-blocks      Used  Available Use% Mounted on"
    //          "/dev/vda       53151899648 3048108032 50103791616   6% /"
    let avail_bytes: u64 = df_output
        .lines()
        .last()
        .context("no df output")?
        .split_whitespace()
        .nth(3)
        .context("no Available column in df")?
        .parse()
        .context("parsing available bytes")?;

    let fifty_gb = 50u64 * 1024 * 1024 * 1024;
    // Allow 5% tolerance for ext4 overhead (reserved blocks, journal, etc.)
    let threshold = (fifty_gb as f64 * 0.95) as u64;

    println!(
        "  Available: {:.1} GB (need >= {:.1} GB)",
        avail_bytes as f64 / 1e9,
        threshold as f64 / 1e9
    );

    assert!(
        avail_bytes >= threshold,
        "Root filesystem should have at least ~50GB available, got {:.1} GB",
        avail_bytes as f64 / 1e9,
    );

    println!("  Stopping VM...");
    common::kill_process(fcvm_pid).await;

    println!("✅ ROOTFS SIZE TEST PASSED!");
    Ok(())
}
