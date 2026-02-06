//! Egress connectivity tests - verifies VMs can reach the host network
//!
//! Tests both fresh VMs and cloned VMs for:
//! - HTTP connectivity from VM to a local test server on the host
//! - HTTP connectivity from container to the same test server
//!
//! Uses a pure Rust LocalTestServer bound on the host - no external network dependencies.
//! Both bridged and rootless networking modes are tested.

#![cfg(feature = "integration-slow")]

mod common;

use anyhow::{Context, Result};
use std::time::Duration;

/// Test egress connectivity for fresh VM with bridged networking
#[cfg(feature = "privileged-tests")]
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
#[cfg(feature = "privileged-tests")]
#[tokio::test]
async fn test_egress_clone_bridged() -> Result<()> {
    egress_clone_test_impl("bridged").await
}

/// Test egress connectivity for cloned VM with rootless networking
#[tokio::test]
async fn test_egress_clone_rootless() -> Result<()> {
    egress_clone_test_impl("rootless").await
}

/// Get the host's primary network interface IP (used for reaching external networks)
/// For bridged mode, VMs can reach this IP via NAT
async fn get_host_primary_ip() -> Result<String> {
    // Use "ip route get 8.8.8.8" to find which interface/IP is used for external traffic
    let output = tokio::process::Command::new("ip")
        .args(["route", "get", "8.8.8.8"])
        .output()
        .await
        .context("running ip route get")?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    // Output looks like: "8.8.8.8 via 172.31.0.1 dev enp3s0 src 172.31.15.123 uid 0"
    // We want the IP after "src"
    for part in stdout.split_whitespace().collect::<Vec<_>>().windows(2) {
        if part[0] == "src" {
            return Ok(part[1].to_string());
        }
    }

    anyhow::bail!("Could not determine host primary IP from: {}", stdout)
}

/// Calculate the URL a VM should use to reach a test server on the host
async fn get_egress_url(network: &str, port: u16) -> Result<String> {
    match network {
        "rootless" => {
            // For rootless, slirp4netns gateway is 10.0.2.2
            Ok(format!("http://10.0.2.2:{}/", port))
        }
        "bridged" => {
            // For bridged, use host's primary IP (reachable via NAT)
            let host_ip = get_host_primary_ip().await?;
            Ok(format!("http://{}:{}/", host_ip, port))
        }
        _ => anyhow::bail!("Unknown network type: {}", network),
    }
}

/// Implementation for testing egress on a fresh (non-cloned) VM
async fn egress_fresh_test_impl(network: &str) -> Result<()> {
    let (vm_name, _, _, _) = common::unique_names(&format!("egress-fresh-{}", network));

    println!("\n╔═══════════════════════════════════════════════════════════════╗");
    println!(
        "║     Fresh VM Egress Test ({:8})                          ║",
        network
    );
    println!("╚═══════════════════════════════════════════════════════════════╝\n");

    // Start local test server on host
    let bind_addr = if network == "rootless" {
        "127.0.0.1"
    } else {
        "0.0.0.0" // Bridged needs to bind to all interfaces
    };

    let test_server = common::LocalTestServer::start_on_available_port(bind_addr)
        .await
        .context("starting local test server")?;

    let egress_url = get_egress_url(network, test_server.port).await?;
    println!(
        "  Local test server: {} (VM will connect to {})",
        test_server.url, egress_url
    );

    let fcvm_path = common::find_fcvm_binary()?;

    // Step 1: Start VM
    println!("\nStep 1: Starting fresh VM '{}'...", vm_name);
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
    if let Err(e) = common::poll_health_by_pid(vm_pid, 180).await {
        test_server.stop().await;
        common::kill_process(vm_pid).await;
        return Err(e.context("VM failed to become healthy"));
    }
    println!("  ✓ VM healthy");

    // Step 2: Test egress
    println!("\nStep 2: Testing egress connectivity to local server...");
    let egress_result = test_egress(&fcvm_path, vm_pid, &egress_url).await;

    // Cleanup
    println!("\nCleaning up...");
    test_server.stop().await;
    common::kill_process(vm_pid).await;
    println!("  Killed VM and test server");

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
    let (baseline_name, clone_name, snapshot_name, _) =
        common::unique_names(&format!("egress-{}", network));

    println!("\n╔═══════════════════════════════════════════════════════════════╗");
    println!(
        "║     Cloned VM Egress Test ({:8})                         ║",
        network
    );
    println!("╚═══════════════════════════════════════════════════════════════╝\n");

    // Start local test server on host
    let bind_addr = if network == "rootless" {
        "127.0.0.1"
    } else {
        "0.0.0.0" // Bridged needs to bind to all interfaces
    };

    let test_server = common::LocalTestServer::start_on_available_port(bind_addr)
        .await
        .context("starting local test server")?;

    let egress_url = get_egress_url(network, test_server.port).await?;
    println!(
        "  Local test server: {} (VM will connect to {})",
        test_server.url, egress_url
    );

    let fcvm_path = common::find_fcvm_binary()?;

    // Step 1: Start baseline VM
    println!("\nStep 1: Starting baseline VM '{}'...", baseline_name);
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
    // Use 300 second timeout to account for rootfs creation on first run
    if let Err(e) = common::poll_health_by_pid(baseline_pid, 300).await {
        test_server.stop().await;
        common::kill_process(baseline_pid).await;
        return Err(e.context("baseline VM failed to become healthy"));
    }
    println!("  ✓ Baseline VM healthy");

    // Test egress on baseline first
    println!("\n  Testing baseline egress to local server...");
    if let Err(e) = test_egress(&fcvm_path, baseline_pid, &egress_url).await {
        test_server.stop().await;
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
        test_server.stop().await;
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
    common::poll_serve_state_by_pid(serve_pid, 30).await?;
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
    common::poll_health_by_pid(clone_pid, 120).await?;
    println!("  ✓ Clone is healthy");

    // Step 5: Test egress on clone
    println!("\nStep 5: Testing clone egress connectivity to local server...");
    let clone_egress = test_egress(&fcvm_path, clone_pid, &egress_url).await;

    // Cleanup
    println!("\nCleaning up...");
    common::kill_process(clone_pid).await;
    println!("  Killed clone");
    common::kill_process(serve_pid).await;
    println!("  Killed memory server");
    test_server.stop().await;
    println!("  Stopped test server");

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
async fn test_egress(fcvm_path: &std::path::Path, pid: u32, egress_url: &str) -> Result<()> {
    // Test 1: VM-level egress using curl (available in Ubuntu guest)
    println!("  Testing VM-level egress (curl to {})...", egress_url);
    let vm_output = tokio::process::Command::new(fcvm_path)
        .args([
            "exec",
            "--pid",
            &pid.to_string(),
            "--vm",
            "--",
            "curl",
            "-s",
            "--noproxy",
            "*",
            "--max-time",
            "5",
            egress_url,
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

    let response = String::from_utf8_lossy(&vm_output.stdout);
    if !response.contains("TEST_SUCCESS") {
        anyhow::bail!("VM egress got unexpected response: {}", response.trim());
    }
    println!("    ✓ VM egress succeeded (got TEST_SUCCESS)");

    // Test 2: Container-level egress using wget (available in nginx:alpine)
    println!(
        "  Testing container-level egress (wget to {})...",
        egress_url
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
            "-",
            "-Y",
            "off",
            "--timeout=10",
            egress_url,
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

    let response = String::from_utf8_lossy(&container_output.stdout);
    if !response.contains("TEST_SUCCESS") {
        anyhow::bail!(
            "Container egress got unexpected response: {}",
            response.trim()
        );
    }
    println!("    ✓ Container egress succeeded (got TEST_SUCCESS)");

    Ok(())
}
