//! Vsock data integrity test for NV2 nested virtualization.
//!
//! Tests whether large vsock writes get corrupted under nested virtualization
//! due to cache coherency issues in double S2 translation.
//!
//! Test flow:
//! 1. Host starts L1 VM with --vsock-dir for predictable socket path
//! 2. Before L2 starts, echo server listens on vsock.sock_9999
//! 3. L2 connects via vsock port 9999, sends data, receives echo
//! 4. L2 verifies data integrity

#![cfg(feature = "privileged-tests")]

mod common;

use anyhow::{Context, Result};
use std::process::Stdio;

const VSOCK_TEST_PORT: u32 = 9999;

/// Test vsock data integrity under nested virtualization.
///
/// This test:
/// 1. Builds vsock-integrity binary
/// 2. Starts L1 VM with nested kernel + --vsock-dir
/// 3. Inside L1: starts echo server, then starts L2 with vsock client
/// 4. Verifies no corruption in the vsock data path
#[tokio::test]
async fn test_vsock_integrity_nested() -> Result<()> {
    println!("\nVsock Integrity Test (NV2 Nested)");
    println!("==================================\n");

    // 1. Build vsock-integrity binary
    println!("1. Building vsock-integrity binary...");
    let status = tokio::process::Command::new("cargo")
        .args([
            "build",
            "--release",
            "--manifest-path",
            "tests/vsock-integrity/Cargo.toml",
        ])
        .status()
        .await
        .context("building vsock-integrity")?;

    if !status.success() {
        anyhow::bail!("Failed to build vsock-integrity");
    }
    println!("   ✓ Built");

    // 2. Copy binary to shared storage
    println!("2. Copying binary to shared storage...");
    tokio::fs::create_dir_all("/mnt/fcvm-btrfs/bin")
        .await
        .ok();
    tokio::fs::copy(
        "tests/vsock-integrity/target/release/vsock-integrity",
        "/mnt/fcvm-btrfs/bin/vsock-integrity",
    )
    .await
    .context("copying vsock-integrity to shared storage")?;
    println!("   ✓ Copied to /mnt/fcvm-btrfs/bin/vsock-integrity");

    // 3. Start L1 VM
    let (vm_name, _, _, _) = common::unique_names("vsock-int");
    let vsock_dir = format!("/tmp/vsock-test-{}", std::process::id());

    println!("3. Starting L1 VM...");
    println!("   --vsock-dir {}", vsock_dir);

    let fcvm_path = common::find_fcvm_binary()?;

    // L1 script: start echo server, start L2, L2 runs client
    let l1_script = format!(
        r#"#!/bin/bash
set -ex

VSOCK_DIR="{vsock_dir}"
PORT={port}

echo "=== L1: Starting echo server ==="
mkdir -p "$VSOCK_DIR"
/mnt/fcvm-btrfs/bin/vsock-integrity server "$VSOCK_DIR" $PORT &
SERVER_PID=$!
sleep 1

echo "=== L1: Starting L2 VM ==="
# L2 runs the vsock client
fcvm podman run \
    --name l2-vsock-client \
    --network bridged \
    --vsock-dir "$VSOCK_DIR" \
    --map /mnt/fcvm-btrfs:/mnt/fcvm-btrfs \
    alpine:latest \
    --cmd "/mnt/fcvm-btrfs/bin/vsock-integrity client-vsock 2 $PORT"

kill $SERVER_PID 2>/dev/null || true
echo "VSOCK_TEST_COMPLETE"
"#,
        vsock_dir = vsock_dir,
        port = VSOCK_TEST_PORT,
    );

    // Write L1 script to shared storage
    let script_path = format!("/mnt/fcvm-btrfs/vsock-test-{}.sh", std::process::id());
    tokio::fs::write(&script_path, &l1_script)
        .await
        .context("writing L1 script")?;
    tokio::process::Command::new("chmod")
        .args(["+x", &script_path])
        .status()
        .await?;

    // Start L1 with nested kernel
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
        "--vsock-dir",
        &vsock_dir,
        "alpine:latest",
        "--cmd",
        &script_path,
    ])
    .await
    .context("spawning L1 VM")?;

    println!("   L1 started (PID: {})", fcvm_pid);

    // Wait for completion (with timeout)
    let output = tokio::time::timeout(std::time::Duration::from_secs(300), child.wait_with_output())
        .await
        .context("timeout waiting for test")?
        .context("waiting for test output")?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = format!("{}\n{}", stdout, stderr);

    // Cleanup
    tokio::fs::remove_file(&script_path).await.ok();

    // Check results
    println!("\n4. Checking results...");

    if combined.contains("VSOCK_INTEGRITY_OK") {
        println!("   ✓ No corruption detected");
        println!("\n✅ VSOCK INTEGRITY TEST PASSED");
        Ok(())
    } else if combined.contains("CORRUPTION") {
        println!("   ✗ Data corruption detected!");
        println!("\nOutput (last 50 lines):");
        for line in combined.lines().rev().take(50).collect::<Vec<_>>().into_iter().rev() {
            println!("   {}", line);
        }
        anyhow::bail!("Vsock data corruption detected under NV2 nested virtualization")
    } else {
        println!("   ? Test did not complete");
        println!("\nOutput (last 50 lines):");
        for line in combined.lines().rev().take(50).collect::<Vec<_>>().into_iter().rev() {
            println!("   {}", line);
        }
        anyhow::bail!("Vsock integrity test did not complete")
    }
}
