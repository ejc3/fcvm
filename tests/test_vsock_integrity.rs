//! Vsock data integrity test for NV2 nested virtualization.
//!
//! Tests whether large vsock writes get corrupted under nested virtualization
//! due to cache coherency issues in double S2 translation.
//!
//! Test flow:
//! 1. Host starts L1 VM with localhost/vsock-integrity
//! 2. Inside L1: echo server listens on vsock.sock_9999
//! 3. L1 starts L2, which connects via vsock port 9999
//! 4. L2 sends data, receives echo, verifies integrity

// The corruption class this hunts (vsock data visibility under NV2's double
// stage-2 translation) is ARM-specific, and the L2 leg needs a nested-KVM
// L1 kernel that only the arm64 profile provides — on x86 runners the L2
// InstanceStart times out before the test can do anything meaningful.
#![cfg(all(feature = "privileged-tests", target_arch = "aarch64"))]

mod common;

use anyhow::{Context, Result};

/// Test vsock data integrity under nested virtualization.
///
/// This test:
/// 1. Builds localhost/vsock-integrity container (extends localhost/nested-test)
/// 2. Starts L1 VM which runs the built-in test script
/// 3. L1 starts echo server, then L2 with vsock client
/// 4. Verifies no corruption in the vsock data path
#[tokio::test]
async fn test_vsock_integrity_nested() -> Result<()> {
    println!("\nVsock Integrity Test (NV2 Nested)");
    println!("==================================\n");

    // 1. Build localhost/nested-test first (base image)
    println!("1. Ensuring localhost/nested-test exists...");
    common::ensure_nested_image().await?;

    // 2. Build localhost/vsock-integrity (extends nested-test)
    println!("2. Building localhost/vsock-integrity container...");
    common::ensure_nested_container("localhost/vsock-integrity", "Containerfile.vsock-integrity")
        .await?;

    // 3. Start L1 VM with vsock-integrity container
    // The container's ENTRYPOINT runs the test automatically
    let (vm_name, _, _, _) = common::unique_names("vsock-int");

    // Remove any stale result file from a previous (crashed/killed) run BEFORE
    // starting the VM — otherwise a leftover "PASS" would be read as this run's
    // verdict even if L1 dies before writing one (false pass).
    std::fs::remove_file("/mnt/fcvm-btrfs/vsock-test-result.txt").ok();

    println!("3. Starting L1 VM with localhost/vsock-integrity...");

    // Home dir for config mount (so L1 can find rootfs-config.toml)
    let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_string());
    let config_mount = format!("{0}/.config/fcvm:/root/.config/fcvm:ro", home);

    // Start L1 - the container's entrypoint (run-test.sh) handles everything
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
        "--mem",
        "2048", // Need enough for L2
        "--map",
        "/mnt/fcvm-btrfs:/mnt/fcvm-btrfs",
        "--map",
        &config_mount,
        "localhost/vsock-integrity",
    ])
    .await
    .context("spawning L1 VM")?;

    println!("   L1 started (PID: {})", fcvm_pid);

    // Wait for completion. Budget matches the other nested L1+L2 chains (840s
    // in-test, under the nested group's 1200s nextest kill): the inner
    // pipeline expands a 10GB rootfs through FUSE and cold-boots an L2 whose
    // container pulls through two NAT layers — a healthy run takes minutes.
    // On timeout the L1 MUST die here: an orphaned L1 keeps running into the
    // retry and races it for the shared result file (a late "PASS" from the
    // orphan would be read as the retry's verdict).
    let wait = tokio::time::timeout(std::time::Duration::from_secs(840), child.wait()).await;
    let _status = match wait {
        Ok(status) => status.context("waiting for test")?,
        Err(_) => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            anyhow::bail!("timeout waiting for vsock integrity test (L1 killed)");
        }
    };

    // Check results via marker file on shared storage
    // run-test.sh writes PASS/CORRUPTION/INCOMPLETE to this file
    let result_file = "/mnt/fcvm-btrfs/vsock-test-result.txt";
    println!("\n4. Checking results from {}...", result_file);

    let result = std::fs::read_to_string(result_file)
        .context("reading result file")?
        .trim()
        .to_string();

    // Clean up result file
    std::fs::remove_file(result_file).ok();

    match result.as_str() {
        "PASS" => {
            println!("   ✓ No corruption detected");
            println!("\n✅ VSOCK INTEGRITY TEST PASSED");
            Ok(())
        }
        "CORRUPTION" => {
            println!("   ✗ Data corruption detected!");
            anyhow::bail!("Vsock data corruption detected under NV2 nested virtualization")
        }
        _ => {
            println!("   ? Test result: {}", result);
            anyhow::bail!("Vsock integrity test did not complete (result: {})", result)
        }
    }
}
