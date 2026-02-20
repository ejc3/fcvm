//! Verifies that podman HEALTHCHECK works in rootless mode (no systemd).
//!
//! Root cause of the original bug: rootless podman without a systemd user session
//! doesn't run the periodic healthcheck timer. The container health stays "starting"
//! forever. fcvm's health monitor sees "starting" and reports Unhealthy.
//!
//! Fix: when fcvm's health monitor sees podman health "starting", it manually
//! triggers `podman healthcheck run` to kick the check.
//!
//! This test builds a container with a HEALTHCHECK that passes immediately,
//! runs it in rootless mode, and verifies fcvm reports Healthy.

#![cfg(feature = "integration-fast")]

mod common;

use anyhow::{Context, Result};

#[tokio::test]
async fn test_rootless_podman_healthcheck_becomes_healthy() -> Result<()> {
    println!("\nTest: rootless podman healthcheck trigger");
    println!("==========================================");

    // Build a container image with a HEALTHCHECK defined.
    // The healthcheck passes immediately (exit 0).
    // Use --format=docker to preserve HEALTHCHECK instruction.
    let image_name = format!(
        "localhost/test-rootless-hc-{}",
        std::process::id() % 10000
    );

    println!("  Building image with HEALTHCHECK: {}", image_name);
    let tmpdir = tempfile::tempdir()?;
    let containerfile = tmpdir.path().join("Containerfile");
    std::fs::write(
        &containerfile,
        format!(
            "FROM {}\nHEALTHCHECK --interval=2s --timeout=2s --retries=1 CMD exit 0\nCMD [\"sleep\", \"120\"]\n",
            common::ALPINE_IMAGE
        ),
    )?;

    let build = tokio::process::Command::new("podman")
        .args([
            "build",
            "--format=docker",
            "-t",
            &image_name,
            "-f",
            &containerfile.to_string_lossy(),
            &tmpdir.path().to_string_lossy(),
        ])
        .output()
        .await
        .context("building test image")?;

    if !build.status.success() {
        let stderr = String::from_utf8_lossy(&build.stderr);
        anyhow::bail!("Image build failed: {}", stderr);
    }
    println!("  Image built");

    // Run the container in rootless mode (no --network bridged, no sudo).
    // The VM doesn't have a systemd user session, so podman's healthcheck
    // timer won't run automatically. fcvm's health monitor must trigger it.
    let (vm_name, _, _, _) = common::unique_names("rootless-hc");
    println!("  Starting rootless VM: {}", vm_name);

    let (mut child, fcvm_pid) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &vm_name,
        &image_name,
    ])
    .await
    .context("spawning fcvm")?;

    println!("  fcvm PID: {}", fcvm_pid);
    println!("  Waiting for Healthy (fcvm must trigger podman healthcheck)...");

    // Without the fix, this would time out because podman health stays "starting".
    // With the fix, fcvm triggers `podman healthcheck run` when it sees "starting",
    // podman runs the check (exit 0), marks container healthy, fcvm sees "healthy".
    let health_result = common::poll_health_by_pid(fcvm_pid, 180).await;

    // Cleanup
    println!("  Stopping VM...");
    common::kill_process(fcvm_pid).await;
    let _ = child.wait().await;

    match health_result {
        Ok(()) => {
            println!("  PASSED: rootless container became Healthy");
            println!("  (fcvm triggered podman healthcheck in VM without systemd)");
            Ok(())
        }
        Err(e) => {
            anyhow::bail!(
                "Container never became Healthy — fcvm failed to trigger \
                 podman healthcheck in rootless mode: {}",
                e
            );
        }
    }
}
