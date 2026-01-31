//! Tests for podman HEALTHCHECK integration
//!
//! Verifies that containers with HEALTHCHECK are properly monitored
//! and that unhealthy containers are detected.

#![cfg(feature = "integration-fast")]

mod common;

use anyhow::{Context, Result};

/// Test that a container with a passing HEALTHCHECK becomes healthy
#[cfg(feature = "privileged-tests")]
#[tokio::test]
async fn test_podman_healthcheck_healthy() -> Result<()> {
    println!("\nTest: podman healthcheck (healthy container)");
    println!("=============================================");

    // Build the test-healthy image with docker format (preserves HEALTHCHECK)
    println!("Building localhost/test-healthy image...");
    let build_output = tokio::process::Command::new("sudo")
        .args([
            "podman",
            "build",
            "--format=docker",
            "-t",
            "localhost/test-healthy",
            "-f",
            "Containerfile.test",
            ".",
        ])
        .output()
        .await
        .context("building test-healthy image")?;

    if !build_output.status.success() {
        let stderr = String::from_utf8_lossy(&build_output.stderr);
        anyhow::bail!("Failed to build test-healthy image: {}", stderr);
    }
    println!("  Image built successfully");

    // Start the VM with the healthy container
    let (vm_name, _, _, _) = common::unique_names("healthcheck-ok");
    println!("Starting VM with healthy container: {}", vm_name);

    let (mut child, fcvm_pid) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &vm_name,
        "--network",
        "bridged",
        "localhost/test-healthy",
    ])
    .await
    .context("spawning fcvm podman run")?;

    println!("  fcvm process started (PID: {})", fcvm_pid);
    println!("  Waiting for VM to become healthy (includes podman healthcheck)...");

    // Wait for health with 120s timeout (container needs to start + healthcheck must pass)
    let health_result = common::poll_health_by_pid(fcvm_pid, 120).await;

    // Kill the VM
    println!("  Stopping VM...");
    common::kill_process(fcvm_pid).await;
    let _ = child.wait().await;

    match health_result {
        Ok(()) => {
            println!("  Container became healthy (HTTP + podman healthcheck both passed)");
            Ok(())
        }
        Err(e) => {
            anyhow::bail!("Health check failed: {}", e);
        }
    }
}

/// Test that a container with a failing HEALTHCHECK stays unhealthy
#[cfg(feature = "privileged-tests")]
#[tokio::test]
async fn test_podman_healthcheck_unhealthy() -> Result<()> {
    println!("\nTest: podman healthcheck (unhealthy container)");
    println!("===============================================");

    // Build the test-unhealthy image with docker format
    println!("Building localhost/test-unhealthy image...");
    let build_output = tokio::process::Command::new("sudo")
        .args([
            "podman",
            "build",
            "--format=docker",
            "-t",
            "localhost/test-unhealthy",
            "-f",
            "Containerfile.unhealthy",
            ".",
        ])
        .output()
        .await
        .context("building test-unhealthy image")?;

    if !build_output.status.success() {
        let stderr = String::from_utf8_lossy(&build_output.stderr);
        anyhow::bail!("Failed to build test-unhealthy image: {}", stderr);
    }
    println!("  Image built successfully");

    // Start the VM with the unhealthy container
    let (vm_name, _, _, _) = common::unique_names("healthcheck-fail");
    println!("Starting VM with unhealthy container: {}", vm_name);

    let (mut child, fcvm_pid) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &vm_name,
        "--network",
        "bridged",
        "localhost/test-unhealthy",
    ])
    .await
    .context("spawning fcvm podman run")?;

    println!("  fcvm process started (PID: {})", fcvm_pid);

    // Poll health status until we see unhealthy or timeout
    // The unhealthy container has: --interval=1s --retries=1
    // So it should fail within a few seconds of the healthcheck running
    println!("  Polling health status (looking for Unhealthy)...");
    let result = common::poll_health_status_by_pid(
        fcvm_pid,
        fcvm::state::HealthStatus::Unhealthy,
        60,
    )
    .await;

    // Kill the VM
    println!("  Stopping VM...");
    common::kill_process(fcvm_pid).await;
    let _ = child.wait().await;

    match result {
        Ok(()) => {
            println!("  Container correctly detected as unhealthy");
            Ok(())
        }
        Err(e) => Err(e),
    }
}

/// Test that a container WITHOUT HEALTHCHECK still works (backwards compat)
#[cfg(feature = "privileged-tests")]
#[tokio::test]
async fn test_podman_healthcheck_none() -> Result<()> {
    println!("\nTest: podman healthcheck (no HEALTHCHECK defined)");
    println!("==================================================");

    // Use standard nginx:alpine which has no HEALTHCHECK
    let (vm_name, _, _, _) = common::unique_names("healthcheck-none");
    println!("Starting VM with container (no HEALTHCHECK): {}", vm_name);

    let (mut child, fcvm_pid) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &vm_name,
        "--network",
        "bridged",
        common::TEST_IMAGE,
    ])
    .await
    .context("spawning fcvm podman run")?;

    println!("  fcvm process started (PID: {})", fcvm_pid);
    println!("  Waiting for VM to become healthy (HTTP check only)...");

    // Should become healthy based on HTTP check alone
    let health_result = common::poll_health_by_pid(fcvm_pid, 120).await;

    // Kill the VM
    println!("  Stopping VM...");
    common::kill_process(fcvm_pid).await;
    let _ = child.wait().await;

    match health_result {
        Ok(()) => {
            println!("  Container without HEALTHCHECK became healthy (backwards compat OK)");
            Ok(())
        }
        Err(e) => {
            anyhow::bail!("Health check failed for container without HEALTHCHECK: {}", e);
        }
    }
}
