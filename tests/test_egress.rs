//! Egress connectivity tests - verifies VMs can reach the internet
//!
//! Tests both fresh VMs and cloned VMs for:
//! - HTTP connectivity from VM to container registry (ghcr.io)
//! - HTTP connectivity from container to container registry
//!
//! Uses ghcr.io (GitHub Container Registry) as a reliable external endpoint.
//! Any HTTP response (200, 401, etc.) proves egress connectivity works.
//!
//! Both bridged and rootless networking modes are tested.

mod common;

use anyhow::{Context, Result};
use std::time::Duration;

/// External URL to test egress connectivity - Docker Hub auth endpoint (returns 200)
const EGRESS_TEST_URL: &str = "https://auth.docker.io/token?service=registry.docker.io";

/// Test egress connectivity for fresh VM with bridged networking
#[tokio::test]
async fn test_egress_fresh_bridged() -> Result<()> {
    egress_fresh_test_impl("bridged").await
}

/// Test egress connectivity for fresh VM with rootless networking
#[tokio::test]
async fn test_egress_fresh_rootless() -> Result<()> {
    egress_fresh_test_impl("rootless").await
}

/// Test egress connectivity for cloned VM with bridged networking
#[tokio::test]
async fn test_egress_clone_bridged() -> Result<()> {
    egress_clone_test_impl("bridged").await
}

/// Test egress connectivity for cloned VM with rootless networking
#[tokio::test]
async fn test_egress_clone_rootless() -> Result<()> {
    egress_clone_test_impl("rootless").await
}

/// Implementation for testing egress on a fresh (non-cloned) VM
async fn egress_fresh_test_impl(network: &str) -> Result<()> {
    let vm_name = format!("egress-fresh-{}", network);

    println!("\n╔═══════════════════════════════════════════════════════════════╗");
    println!(
        "║     Fresh VM Egress Test ({:8})                          ║",
        network
    );
    println!("╚═══════════════════════════════════════════════════════════════╝\n");

    let fcvm_path = common::find_fcvm_binary()?;

    // Step 1: Start VM
    println!("Step 1: Starting fresh VM '{}'...", vm_name);
    let (_child, vm_pid) = common::spawn_fcvm_with_logs(
        &[
            "podman",
            "run",
            "--name",
            &vm_name,
            "--network",
            network,
            common::TEST_IMAGE,
        ],
        &vm_name,
    )
    .await
    .context("spawning VM")?;

    println!("  Waiting for VM to become healthy (PID: {})...", vm_pid);
    common::poll_health_by_pid(vm_pid, 60).await?;
    println!("  ✓ VM healthy");

    // Step 2: Test egress
    println!(
        "\nStep 2: Testing egress connectivity to {}...",
        EGRESS_TEST_URL
    );
    let egress_result = test_egress(&fcvm_path, vm_pid).await;

    // Cleanup
    println!("\nCleaning up...");
    common::kill_process(vm_pid).await;
    println!("  Killed VM");

    // Report result
    match egress_result {
        Ok(()) => {
            println!("\n✅ FRESH VM EGRESS TEST PASSED! (network: {})", network);
            Ok(())
        }
        Err(e) => {
            println!("\n❌ FRESH VM EGRESS TEST FAILED!");
            println!("  Error: {}", e);
            Err(e)
        }
    }
}

/// Implementation for testing egress on a cloned VM
async fn egress_clone_test_impl(network: &str) -> Result<()> {
    let snapshot_name = format!("egress-snapshot-{}", network);
    let baseline_name = format!("egress-baseline-{}", network);
    let clone_name = format!("egress-clone-{}", network);

    println!("\n╔═══════════════════════════════════════════════════════════════╗");
    println!(
        "║     Cloned VM Egress Test ({:8})                         ║",
        network
    );
    println!("╚═══════════════════════════════════════════════════════════════╝\n");

    let fcvm_path = common::find_fcvm_binary()?;

    // Step 1: Start baseline VM
    println!("Step 1: Starting baseline VM '{}'...", baseline_name);
    let (_baseline_child, baseline_pid) = common::spawn_fcvm_with_logs(
        &[
            "podman",
            "run",
            "--name",
            &baseline_name,
            "--network",
            network,
            common::TEST_IMAGE,
        ],
        &baseline_name,
    )
    .await
    .context("spawning baseline VM")?;

    println!(
        "  Waiting for baseline VM to become healthy (PID: {})...",
        baseline_pid
    );
    common::poll_health_by_pid(baseline_pid, 60).await?;
    println!("  ✓ Baseline VM healthy");

    // Test egress on baseline first
    println!("\n  Testing baseline egress to {}...", EGRESS_TEST_URL);
    if let Err(e) = test_egress(&fcvm_path, baseline_pid).await {
        common::kill_process(baseline_pid).await;
        return Err(anyhow::anyhow!("Baseline egress failed: {}", e));
    }
    println!("  ✓ Baseline VM egress works");

    // Step 2: Create snapshot
    println!("\nStep 2: Creating snapshot '{}'...", snapshot_name);
    let output = tokio::process::Command::new(&fcvm_path)
        .args([
            "snapshot",
            "create",
            "--pid",
            &baseline_pid.to_string(),
            "--tag",
            &snapshot_name,
        ])
        .output()
        .await
        .context("running snapshot create")?;

    if !output.status.success() {
        common::kill_process(baseline_pid).await;
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Snapshot creation failed: {}", stderr);
    }
    println!("  ✓ Snapshot created");

    // Kill baseline - we only need the snapshot
    common::kill_process(baseline_pid).await;
    println!("  Killed baseline VM");
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Step 3: Start memory server
    println!("\nStep 3: Starting memory server...");
    let (_serve_child, serve_pid) =
        common::spawn_fcvm_with_logs(&["snapshot", "serve", &snapshot_name], "uffd-server")
            .await
            .context("spawning memory server")?;

    // Wait for serve process to save its state file
    common::poll_serve_state_by_pid(serve_pid, 10).await?;
    println!("  ✓ Memory server ready (PID: {})", serve_pid);

    // Step 4: Spawn clone
    println!("\nStep 4: Spawning clone '{}'...", clone_name);
    let serve_pid_str = serve_pid.to_string();
    let (_clone_child, clone_pid) = common::spawn_fcvm_with_logs(
        &[
            "snapshot",
            "run",
            "--pid",
            &serve_pid_str,
            "--name",
            &clone_name,
            "--network",
            network,
        ],
        &clone_name,
    )
    .await
    .context("spawning clone")?;

    println!(
        "  Waiting for clone to become healthy (PID: {})...",
        clone_pid
    );
    common::poll_health_by_pid(clone_pid, 60).await?;
    println!("  ✓ Clone is healthy");

    // Step 5: Test egress on clone
    println!(
        "\nStep 5: Testing clone egress connectivity to {}...",
        EGRESS_TEST_URL
    );
    let clone_egress = test_egress(&fcvm_path, clone_pid).await;

    // Cleanup
    println!("\nCleaning up...");
    common::kill_process(clone_pid).await;
    println!("  Killed clone");
    common::kill_process(serve_pid).await;
    println!("  Killed memory server");

    // Report result
    match clone_egress {
        Ok(()) => {
            println!("\n✅ CLONED VM EGRESS TEST PASSED! (network: {})", network);
            Ok(())
        }
        Err(e) => {
            println!("\n❌ CLONED VM EGRESS TEST FAILED!");
            println!("  Error: {}", e);
            Err(e)
        }
    }
}

/// Test egress connectivity from both VM and container level
async fn test_egress(fcvm_path: &std::path::Path, pid: u32) -> Result<()> {
    // Test 1: VM-level egress using curl (available in Ubuntu guest)
    // ghcr.io returns 401 for unauthenticated requests - any HTTP response proves egress works
    println!("  Testing VM-level egress (curl to {})...", EGRESS_TEST_URL);
    let vm_output = tokio::process::Command::new(fcvm_path)
        .args([
            "exec",
            "--pid",
            &pid.to_string(),
            "--vm",
            "--",
            "curl",
            "-s",
            "--max-time",
            "15",
            "-o",
            "/dev/null",
            "-w",
            "%{http_code}",
            EGRESS_TEST_URL,
        ])
        .output()
        .await
        .context("running curl in VM")?;

    if !vm_output.status.success() {
        let stderr = String::from_utf8_lossy(&vm_output.stderr);
        anyhow::bail!(
            "VM egress failed: exit={}, stderr='{}'",
            vm_output.status,
            stderr.trim()
        );
    }

    let status_code = String::from_utf8_lossy(&vm_output.stdout);
    let code = status_code.trim();
    if code != "200" {
        anyhow::bail!("VM egress got HTTP {}, expected 200", code);
    }
    println!("    ✓ VM egress succeeded (HTTP 200)");

    // Test 2: Container-level egress using wget (available in alpine nginx)
    println!(
        "  Testing container-level egress (wget to {})...",
        EGRESS_TEST_URL
    );
    let container_output = tokio::process::Command::new(fcvm_path)
        .args([
            "exec",
            "--pid",
            &pid.to_string(),
            "--",
            "wget",
            "-q",
            "-O",
            "/dev/null",
            "--timeout=15",
            EGRESS_TEST_URL,
        ])
        .output()
        .await
        .context("running wget in container")?;

    if !container_output.status.success() {
        let stderr = String::from_utf8_lossy(&container_output.stderr);
        anyhow::bail!(
            "Container egress failed: exit={}, stderr='{}'",
            container_output.status,
            stderr.trim()
        );
    }
    println!("    ✓ Container egress succeeded");

    Ok(())
}
