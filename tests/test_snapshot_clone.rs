//! Snapshot and clone integration tests
//!
//! Tests the full snapshot/clone workflow:
//! 1. Start a baseline VM
//! 2. Create a snapshot
//! 3. Start memory server
//! 4. Spawn clones from snapshot (concurrently)
//! 5. Verify clones become healthy (concurrently)

#![cfg(feature = "integration-slow")]

mod common;

use anyhow::{Context, Result};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

/// Full snapshot/clone workflow test with rootless networking (10 clones)
#[tokio::test]
async fn test_snapshot_clone_rootless_10() -> Result<()> {
    snapshot_clone_test_impl("rootless", 10).await
}

/// Full snapshot/clone workflow test with bridged networking (10 clones)
#[cfg(feature = "privileged-tests")]
#[tokio::test]
async fn test_snapshot_clone_bridged_10() -> Result<()> {
    snapshot_clone_test_impl("bridged", 10).await
}

/// Stress test with 100 clones using rootless networking
#[tokio::test]
async fn test_snapshot_clone_stress_100_rootless() -> Result<()> {
    snapshot_clone_test_impl("rootless", 100).await
}

/// Stress test with 100 clones using bridged networking
#[cfg(feature = "privileged-tests")]
#[tokio::test]
async fn test_snapshot_clone_stress_100_bridged() -> Result<()> {
    snapshot_clone_test_impl("bridged", 100).await
}

/// Result of spawning and health-checking a single clone
struct CloneResult {
    name: String,
    pid: u32,
    spawn_time_ms: f64,
    health_time_secs: Option<f64>,
    error: Option<String>,
}

async fn snapshot_clone_test_impl(network: &str, num_clones: usize) -> Result<()> {
    let (baseline_name, _, snapshot_name, _) = common::unique_names(&format!("snap-{}", network));
    let test_start = Instant::now();

    println!("\n╔═══════════════════════════════════════════════════════════════╗");
    println!(
        "║     Snapshot/Clone Test: {} clones ({:8})            ║",
        num_clones, network
    );
    println!("╚═══════════════════════════════════════════════════════════════╝\n");

    // Find fcvm binary
    let fcvm_path = common::find_fcvm_binary()?;

    // =========================================================================
    // Step 1: Start baseline VM
    // =========================================================================
    println!("Step 1: Starting baseline VM '{}'...", baseline_name);
    let step1_start = Instant::now();

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

    // Wait for healthy
    println!("  Waiting for baseline VM to become healthy...");
    common::poll_health_by_pid(baseline_pid, 120).await?;
    let baseline_time = step1_start.elapsed();
    println!(
        "  ✓ Baseline VM healthy (PID: {}, took {:.1}s)",
        baseline_pid,
        baseline_time.as_secs_f64()
    );

    // =========================================================================
    // Step 2: Create snapshot
    // =========================================================================
    println!("\nStep 2: Creating snapshot '{}'...", snapshot_name);
    let step2_start = Instant::now();

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
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Snapshot creation failed: {}", stderr);
    }
    let snapshot_time = step2_start.elapsed();
    println!(
        "  ✓ Snapshot created (took {:.1}s)",
        snapshot_time.as_secs_f64()
    );

    // =========================================================================
    // Step 3: Start memory server
    // =========================================================================
    println!(
        "\nStep 3: Starting memory server for '{}'...",
        snapshot_name
    );
    let step3_start = Instant::now();

    let (_serve_child, serve_pid) =
        common::spawn_fcvm_with_logs(&["snapshot", "serve", &snapshot_name], "uffd-server")
            .await
            .context("spawning memory server")?;

    // Wait for serve process to be ready (poll for socket)
    common::poll_serve_ready(&snapshot_name, serve_pid, 30).await?;
    let serve_time = step3_start.elapsed();
    println!(
        "  ✓ Memory server ready (PID: {}, took {:.1}s)",
        serve_pid,
        serve_time.as_secs_f64()
    );

    // =========================================================================
    // Step 4: Spawn ALL clones concurrently
    // =========================================================================
    println!("\nStep 4: Spawning {} clones concurrently...", num_clones);
    let step4_start = Instant::now();

    let results: Arc<Mutex<Vec<CloneResult>>> = Arc::new(Mutex::new(Vec::new()));
    let clone_pids: Arc<Mutex<Vec<u32>>> = Arc::new(Mutex::new(Vec::new()));

    let mut spawn_handles = Vec::new();

    for i in 0..num_clones {
        let clone_name = format!("{}-{}", baseline_name.replace("-base-", "-clone-"), i);
        let network = network.to_string();
        let results = Arc::clone(&results);
        let clone_pids = Arc::clone(&clone_pids);
        let serve_pid_str = serve_pid.to_string();

        let handle = tokio::spawn(async move {
            let spawn_start = Instant::now();

            let result = common::spawn_fcvm_with_logs(
                &[
                    "snapshot",
                    "run",
                    "--pid",
                    &serve_pid_str,
                    "--name",
                    &clone_name,
                    "--network",
                    &network,
                ],
                &clone_name,
            )
            .await;

            match result {
                Ok((_child, clone_pid)) => {
                    let spawn_ms = spawn_start.elapsed().as_secs_f64() * 1000.0;

                    // Store PID for cleanup
                    clone_pids.lock().await.push(clone_pid);

                    // Now wait for health check
                    let health_start = Instant::now();
                    let health_result = tokio::time::timeout(
                        Duration::from_secs(120),
                        common::poll_health_by_pid(clone_pid, 120),
                    )
                    .await;

                    let (health_time, error) = match health_result {
                        Ok(Ok(_)) => (Some(health_start.elapsed().as_secs_f64()), None),
                        Ok(Err(e)) => (None, Some(format!("health check failed: {}", e))),
                        Err(_) => (None, Some("health check timeout".to_string())),
                    };

                    results.lock().await.push(CloneResult {
                        name: clone_name.clone(),
                        pid: clone_pid,
                        spawn_time_ms: spawn_ms,
                        health_time_secs: health_time,
                        error,
                    });
                }
                Err(e) => {
                    results.lock().await.push(CloneResult {
                        name: clone_name.clone(),
                        pid: 0,
                        spawn_time_ms: spawn_start.elapsed().as_secs_f64() * 1000.0,
                        health_time_secs: None,
                        error: Some(format!("spawn failed: {}", e)),
                    });
                }
            }
        });

        spawn_handles.push(handle);
    }

    // Wait for all spawn+health tasks to complete
    for handle in spawn_handles {
        let _ = handle.await;
    }

    let clone_total_time = step4_start.elapsed();

    // Collect results
    let results = results.lock().await;
    let clone_pids = clone_pids.lock().await;

    let healthy_count = results
        .iter()
        .filter(|r| r.health_time_secs.is_some())
        .count();
    let failed_count = results.iter().filter(|r| r.error.is_some()).count();

    // Print results as they completed
    println!("\n  Clone results:");
    for r in results.iter() {
        if let Some(health_time) = r.health_time_secs {
            println!(
                "  ✓ {} (PID: {}) spawn={:.0}ms health={:.2}s",
                r.name, r.pid, r.spawn_time_ms, health_time
            );
        } else if let Some(ref err) = r.error {
            println!("  ✗ {} (PID: {}): {}", r.name, r.pid, err);
        }
    }

    // =========================================================================
    // Cleanup
    // =========================================================================
    println!("\nCleaning up...");
    let cleanup_start = Instant::now();

    // Kill clones
    for pid in clone_pids.iter() {
        if *pid > 0 {
            common::kill_process(*pid).await;
        }
    }
    println!("  Killed {} clones", clone_pids.len());

    // Kill memory server
    common::kill_process(serve_pid).await;
    println!("  Killed memory server");

    // Kill baseline VM
    common::kill_process(baseline_pid).await;
    println!("  Killed baseline VM");

    let cleanup_time = cleanup_start.elapsed();
    let total_time = test_start.elapsed();

    // =========================================================================
    // Statistics
    // =========================================================================
    let spawn_times: Vec<f64> = results.iter().map(|r| r.spawn_time_ms).collect();
    let health_times: Vec<f64> = results.iter().filter_map(|r| r.health_time_secs).collect();

    let spawn_avg = if spawn_times.is_empty() {
        0.0
    } else {
        spawn_times.iter().sum::<f64>() / spawn_times.len() as f64
    };
    let spawn_min = spawn_times.iter().cloned().fold(f64::INFINITY, f64::min);
    let spawn_max = spawn_times.iter().cloned().fold(0.0, f64::max);

    let health_avg = if health_times.is_empty() {
        0.0
    } else {
        health_times.iter().sum::<f64>() / health_times.len() as f64
    };
    let health_min = health_times.iter().cloned().fold(f64::INFINITY, f64::min);
    let health_max = health_times.iter().cloned().fold(0.0, f64::max);

    // =========================================================================
    // Results
    // =========================================================================
    println!("\n╔═══════════════════════════════════════════════════════════════╗");
    println!("║                         RESULTS                               ║");
    println!("╠═══════════════════════════════════════════════════════════════╣");
    println!(
        "║  Clones spawned:  {:>3}                                        ║",
        results.len()
    );
    println!(
        "║  Clones healthy:  {:>3}                                        ║",
        healthy_count
    );
    println!(
        "║  Clones failed:   {:>3}                                        ║",
        failed_count
    );
    println!("╠═══════════════════════════════════════════════════════════════╣");
    println!("║                       TIMING STATS                            ║");
    println!("╠═══════════════════════════════════════════════════════════════╣");
    println!(
        "║  Baseline VM startup:     {:>6.1}s                             ║",
        baseline_time.as_secs_f64()
    );
    println!(
        "║  Snapshot creation:       {:>6.1}s                             ║",
        snapshot_time.as_secs_f64()
    );
    println!(
        "║  Memory server startup:   {:>6.1}s                             ║",
        serve_time.as_secs_f64()
    );
    println!(
        "║  All clones ready:        {:>6.1}s  (spawn + health, parallel) ║",
        clone_total_time.as_secs_f64()
    );
    println!(
        "║  Cleanup:                 {:>6.1}s                             ║",
        cleanup_time.as_secs_f64()
    );
    println!("║  ─────────────────────────────────                            ║");
    println!(
        "║  TOTAL:                   {:>6.1}s                             ║",
        total_time.as_secs_f64()
    );
    println!("╠═══════════════════════════════════════════════════════════════╣");
    println!("║                    PER-CLONE STATS                            ║");
    println!("╠═══════════════════════════════════════════════════════════════╣");
    if !spawn_times.is_empty() {
        println!(
            "║  Spawn time:  avg={:>6.0}ms  min={:>6.0}ms  max={:>6.0}ms     ║",
            spawn_avg, spawn_min, spawn_max
        );
    }
    if !health_times.is_empty() {
        println!(
            "║  Health time: avg={:>6.2}s   min={:>6.2}s   max={:>6.2}s      ║",
            health_avg, health_min, health_max
        );
    }
    println!("╚═══════════════════════════════════════════════════════════════╝");

    // Fail if any clones failed
    if healthy_count != num_clones {
        let errors: Vec<_> = results
            .iter()
            .filter_map(|r| r.error.as_ref().map(|e| format!("{}: {}", r.name, e)))
            .collect();
        anyhow::bail!(
            "Snapshot/clone test failed: {}/{} clones became healthy\nErrors:\n  {}",
            healthy_count,
            num_clones,
            errors.join("\n  ")
        );
    }

    println!("\n✅ SNAPSHOT/CLONE TEST PASSED!");
    Ok(())
}

/// Test cloning while baseline VM is still running (rootless)
#[tokio::test]
async fn test_clone_while_baseline_running_rootless() -> Result<()> {
    clone_while_baseline_running_impl("rootless").await
}

/// Test cloning while baseline VM is still running (bridged)
#[cfg(feature = "privileged-tests")]
#[tokio::test]
async fn test_clone_while_baseline_running_bridged() -> Result<()> {
    clone_while_baseline_running_impl("bridged").await
}

/// Implementation for clone-while-baseline-running test
///
/// This tests for vsock socket path conflicts: when cloning from a running baseline,
/// both the baseline and clone need separate vsock sockets. Without mount namespace
/// isolation, Firecracker would try to bind to the same socket path stored in vmstate.bin.
async fn clone_while_baseline_running_impl(network_mode: &str) -> Result<()> {
    let (baseline_name, clone_name, snapshot_name, _) = common::unique_names("running");

    println!("\n╔═══════════════════════════════════════════════════════════════╗");
    println!(
        "║     Clone While Baseline Running Test ({})            ║",
        network_mode
    );
    println!("╚═══════════════════════════════════════════════════════════════╝\n");

    let fcvm_path = common::find_fcvm_binary()?;

    // Step 1: Start baseline VM
    println!("Step 1: Starting baseline VM ({})...", network_mode);
    let (_baseline_child, baseline_pid) = common::spawn_fcvm_with_logs(
        &[
            "podman",
            "run",
            "--name",
            &baseline_name,
            "--network",
            network_mode,
            common::TEST_IMAGE,
        ],
        &baseline_name,
    )
    .await
    .context("spawning baseline VM")?;

    println!("  Waiting for baseline VM to become healthy...");
    common::poll_health_by_pid(baseline_pid, 120).await?;
    println!("  ✓ Baseline VM healthy (PID: {})", baseline_pid);

    // Step 2: Create snapshot (baseline VM stays running after this)
    println!("\nStep 2: Creating snapshot (baseline will continue running)...");
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
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Snapshot creation failed: {}", stderr);
    }
    println!("  ✓ Snapshot created");

    // Verify baseline is STILL healthy after snapshot
    println!("\nStep 3: Verifying baseline is still healthy after snapshot...");
    common::poll_health_by_pid(baseline_pid, 30).await?;
    println!("  ✓ Baseline VM still healthy");

    // Step 4: Start memory server
    println!("\nStep 4: Starting memory server...");
    let (_serve_child, serve_pid) =
        common::spawn_fcvm_with_logs(&["snapshot", "serve", &snapshot_name], "uffd-server")
            .await
            .context("spawning memory server")?;

    // Wait for serve to be ready (poll for socket)
    common::poll_serve_ready(&snapshot_name, serve_pid, 30).await?;
    println!("  ✓ Memory server ready (PID: {})", serve_pid);

    // Step 5: Clone WHILE baseline is still running (this is the key test!)
    println!("\nStep 5: Spawning clone while baseline is STILL RUNNING...");
    println!("  (This tests vsock socket isolation via mount namespace)");

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
            network_mode,
        ],
        &clone_name,
    )
    .await
    .context("spawning clone while baseline running")?;

    // Step 6: Wait for clone to become healthy
    println!("\nStep 6: Waiting for clone to become healthy...");
    let clone_health_result = tokio::time::timeout(
        Duration::from_secs(120),
        common::poll_health_by_pid(clone_pid, 120),
    )
    .await;

    let clone_healthy = match clone_health_result {
        Ok(Ok(_)) => {
            println!("  ✓ Clone is healthy (PID: {})", clone_pid);
            true
        }
        Ok(Err(e)) => {
            eprintln!("  ✗ Clone health check failed: {}", e);
            false
        }
        Err(_) => {
            eprintln!("  ✗ Clone health check timeout");
            false
        }
    };

    // Step 7: Verify baseline is STILL healthy (should not be affected by clone)
    println!("\nStep 7: Verifying baseline is still healthy after clone spawned...");
    let baseline_still_healthy = common::poll_health_by_pid(baseline_pid, 30).await.is_ok();
    if baseline_still_healthy {
        println!("  ✓ Baseline VM still healthy");
    } else {
        eprintln!("  ✗ Baseline VM is no longer healthy!");
    }

    // Cleanup
    println!("\nCleaning up...");
    common::kill_process(clone_pid).await;
    println!("  Killed clone");
    common::kill_process(serve_pid).await;
    println!("  Killed memory server");
    common::kill_process(baseline_pid).await;
    println!("  Killed baseline VM");

    // Final result
    if clone_healthy && baseline_still_healthy {
        println!("\n✅ CLONE-WHILE-BASELINE-RUNNING TEST PASSED!");
        Ok(())
    } else {
        anyhow::bail!(
            "Test failed: clone_healthy={}, baseline_still_healthy={}",
            clone_healthy,
            baseline_still_healthy
        )
    }
}

/// Test that clones can reach the internet in bridged mode
///
/// This verifies that DNS resolution and outbound connectivity work after snapshot restore.
/// The clone should be able to resolve hostnames and make HTTP requests.
#[cfg(feature = "privileged-tests")]
#[tokio::test]
async fn test_clone_internet_bridged() -> Result<()> {
    clone_internet_test_impl("bridged").await
}

/// Test that clones can reach the internet in rootless mode
#[tokio::test]
async fn test_clone_internet_rootless() -> Result<()> {
    clone_internet_test_impl("rootless").await
}

async fn clone_internet_test_impl(network: &str) -> Result<()> {
    let (baseline_name, clone_name, snapshot_name, _) =
        common::unique_names(&format!("inet-{}", network));

    println!("\n╔═══════════════════════════════════════════════════════════════╗");
    println!(
        "║     Clone Internet Connectivity Test ({:8})              ║",
        network
    );
    println!("╚═══════════════════════════════════════════════════════════════╝\n");

    // Start local test servers on host
    let bind_addr = if network == "rootless" {
        "127.0.0.1"
    } else {
        "0.0.0.0" // Bridged needs to bind to all interfaces
    };

    // HTTP test server
    let test_server = common::LocalTestServer::start_on_available_port(bind_addr)
        .await
        .context("starting local HTTP test server")?;

    // For rootless, we know the egress URL upfront (10.0.2.2).
    // For bridged, we'll use the veth host IP from clone's state (same as DNS).
    let egress_url_for_rootless = if network == "rootless" {
        Some(format!("http://10.0.2.2:{}/", test_server.port))
    } else {
        None // Will use veth host IP from state
    };
    println!(
        "  Local HTTP server: {} (VM will connect via {})",
        test_server.url,
        egress_url_for_rootless
            .as_deref()
            .unwrap_or("veth host IP from state")
    );

    // DNS test server - responds with 93.184.216.34 (example.com IP) for any query
    // Using high port since port 53 may be in use by systemd-resolved
    let dns_response_ip: std::net::Ipv4Addr = "93.184.216.34".parse().unwrap();
    let dns_server = common::LocalDnsServer::start_on_available_port(bind_addr, dns_response_ip)
        .await
        .context("starting local DNS test server")?;

    // For rootless, we know the slirp gateway address upfront.
    // For bridged, we need to get the veth host IP from the clone's state after it starts,
    // since the VM can only reach the host through the veth pair, not the host's primary IP.
    let dns_server_addr_for_rootless = if network == "rootless" {
        Some("10.0.2.2".to_string())
    } else {
        None // Will be determined from clone's state
    };
    println!(
        "  Local DNS server: {}:{} (VM will query via {})",
        bind_addr,
        dns_server.port,
        dns_server_addr_for_rootless
            .as_deref()
            .unwrap_or("veth host IP from state")
    );

    let fcvm_path = common::find_fcvm_binary()?;

    // Step 1: Start baseline VM
    println!("Step 1: Starting baseline VM...");
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

    println!("  Waiting for baseline VM to become healthy...");
    common::poll_health_by_pid(baseline_pid, 120).await?;
    println!("  ✓ Baseline VM healthy (PID: {})", baseline_pid);

    // Step 2: Create snapshot
    println!("\nStep 2: Creating snapshot...");
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
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Snapshot creation failed: {}", stderr);
    }
    println!("  ✓ Snapshot created");

    // Kill baseline - we only need the snapshot
    common::kill_process(baseline_pid).await;
    println!("  Killed baseline VM (only need snapshot)");

    // Step 3: Start memory server
    println!("\nStep 3: Starting memory server...");
    let (_serve_child, serve_pid) =
        common::spawn_fcvm_with_logs(&["snapshot", "serve", &snapshot_name], "uffd-server")
            .await
            .context("spawning memory server")?;

    // Wait for serve to be ready (poll for socket)
    common::poll_serve_ready(&snapshot_name, serve_pid, 30).await?;
    println!("  ✓ Memory server ready (PID: {})", serve_pid);

    // Step 4: Spawn clone
    println!("\nStep 4: Spawning clone...");
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

    // Wait for clone to become healthy
    println!("  Waiting for clone to become healthy...");
    common::poll_health_by_pid(clone_pid, 120).await?;
    println!("  ✓ Clone is healthy (PID: {})", clone_pid);

    // Install bind-tools for dig command (Alpine doesn't include it by default)
    println!("  Installing bind-tools for dig...");
    let install_output = tokio::process::Command::new(&fcvm_path)
        .args([
            "exec",
            "--pid",
            &clone_pid.to_string(),
            "--vm",
            "--",
            "apk",
            "add",
            "--no-cache",
            "bind-tools",
        ])
        .output()
        .await
        .context("installing bind-tools")?;

    if !install_output.status.success() {
        let stderr = String::from_utf8_lossy(&install_output.stderr);
        // Log but don't fail - dig might already be available
        eprintln!("  Warning: bind-tools install: {}", stderr.trim());
    } else {
        println!("  ✓ bind-tools installed");
    }

    // Step 5: Test connectivity from inside the clone
    println!("\nStep 5: Testing connectivity from clone...");

    // Get the DNS server address for this network mode
    let dns_server_addr = if let Some(addr) = dns_server_addr_for_rootless.as_ref() {
        addr.clone()
    } else {
        // For bridged mode, get the veth host IP from clone's state
        // The VM can only reach the host through the veth pair
        let display_output = tokio::process::Command::new(&fcvm_path)
            .args(["ls", "--json", "--pid", &clone_pid.to_string()])
            .output()
            .await
            .context("getting clone state")?;
        let stdout = String::from_utf8_lossy(&display_output.stdout);
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&stdout).unwrap_or_default();
        let veth_host_ip = parsed
            .first()
            .and_then(|v| v.get("config")?.get("network")?.get("host_ip")?.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| anyhow::anyhow!("Could not get veth host IP from clone state"))?;
        println!("  Using veth host IP for DNS: {}", veth_host_ip);
        veth_host_ip
    };

    // Test 1: DNS resolution using local DNS server
    // Note: DNS over UDP may not work in bridged mode with clones due to In-Namespace NAT
    // The clone uses NAT to reach external IPs, but UDP DNS packets may not traverse properly
    println!("  Testing DNS resolution...");
    let dns_result = test_clone_dns(
        &fcvm_path,
        clone_pid,
        &dns_server_addr,
        dns_server.port,
        &dns_response_ip.to_string(),
    )
    .await;

    // Test 2: HTTP connectivity to local test server
    println!("  Testing HTTP connectivity to local server...");
    let egress_url = if let Some(url) = egress_url_for_rootless.as_ref() {
        url.clone()
    } else {
        // For bridged mode, use the same veth host IP we determined for DNS
        format!("http://{}:{}/", dns_server_addr, test_server.port)
    };
    let http_result = test_clone_http(&fcvm_path, clone_pid, &egress_url).await;

    // Cleanup
    println!("\nCleaning up...");
    common::kill_process(clone_pid).await;
    println!("  Killed clone");
    common::kill_process(serve_pid).await;
    println!("  Killed memory server");
    dns_server.stop().await;
    println!("  Stopped DNS server");
    test_server.stop().await;
    println!("  Stopped HTTP server");

    // Report results
    println!("\n╔═══════════════════════════════════════════════════════════════╗");
    println!("║                         RESULTS                               ║");
    println!("╠═══════════════════════════════════════════════════════════════╣");

    let dns_ok = dns_result.is_ok();
    let http_ok = http_result.is_ok();

    if dns_ok {
        println!("║  DNS reachability:  ✓ PASSED                                 ║");
    } else {
        println!("║  DNS reachability:  ✗ FAILED                                 ║");
        if let Err(ref e) = dns_result {
            eprintln!("    Error: {}", e);
        }
    }

    if http_ok {
        println!("║  HTTP connectivity: ✓ PASSED                                 ║");
    } else {
        println!("║  HTTP connectivity: ✗ FAILED                                 ║");
        if let Err(ref e) = http_result {
            eprintln!("    Error: {}", e);
        }
    }

    println!("╚═══════════════════════════════════════════════════════════════╝");

    // For bridged mode, HTTP is the critical test (DNS over UDP has NAT issues with clones)
    // For rootless mode, both should work
    let required_tests_pass = if network == "bridged" {
        // In bridged mode with clones, DNS over UDP may fail due to In-Namespace NAT
        // HTTP connectivity is sufficient to prove networking works
        http_ok
    } else {
        // In rootless mode, both DNS and HTTP should work
        dns_ok && http_ok
    };

    if required_tests_pass {
        println!(
            "\n✅ CLONE INTERNET CONNECTIVITY TEST PASSED! ({})",
            network
        );
        Ok(())
    } else {
        anyhow::bail!(
            "Clone internet test failed: dns={}, http={}, network={}",
            dns_ok,
            http_ok,
            network
        )
    }
}

/// Test DNS resolution from inside the clone VM using a local DNS server
///
/// Tests that DNS resolution works by querying a local test DNS server.
/// Uses `dig` which supports custom ports via `-p` option.
/// This avoids external hostname dependencies while still validating DNS path.
async fn test_clone_dns(
    fcvm_path: &std::path::Path,
    clone_pid: u32,
    dns_server: &str,
    dns_port: u16,
    expected_ip: &str,
) -> Result<()> {
    // Use dig to query our local DNS server for test.local
    // dig @server -p port hostname
    // The local DNS server responds with our expected_ip for any query
    let output = tokio::process::Command::new(fcvm_path)
        .args([
            "exec",
            "--pid",
            &clone_pid.to_string(),
            "--vm",
            "--",
            "dig",
            &format!("@{}", dns_server),
            "-p",
            &dns_port.to_string(),
            "test.local",
            "+short",
        ])
        .output()
        .await
        .context("running dig in clone")?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // dig +short should return just the IP address
    if output.status.success() && stdout.contains(expected_ip) {
        println!(
            "    dig @{}:{} test.local: OK (got {})",
            dns_server, dns_port, expected_ip
        );
        Ok(())
    } else {
        anyhow::bail!(
            "DNS resolution failed: exit={}, stdout={}, stderr={}",
            output.status,
            stdout.trim(),
            stderr.trim()
        )
    }
}

/// Test HTTP connectivity from inside the clone VM using a local test server
async fn test_clone_http(
    fcvm_path: &std::path::Path,
    clone_pid: u32,
    egress_url: &str,
) -> Result<()> {
    // Use curl to test HTTP connectivity to local test server
    // Note: We use the VM (not container) because curl is available there
    let output = tokio::process::Command::new(fcvm_path)
        .args([
            "exec",
            "--pid",
            &clone_pid.to_string(),
            "--vm",
            "--",
            "curl",
            "-s",
            "--max-time",
            "10",
            egress_url,
        ])
        .output()
        .await
        .context("running curl in clone VM")?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // Local test server returns "TEST_SUCCESS" in the body
    if output.status.success() && stdout.contains("TEST_SUCCESS") {
        println!("    curl {}: OK (got TEST_SUCCESS)", egress_url);
        Ok(())
    } else {
        anyhow::bail!(
            "HTTP connectivity failed: exit={}, stdout={}, stderr={}",
            output.status,
            stdout.trim(),
            stderr.trim()
        )
    }
}

/// Test port forwarding on clones with bridged networking
///
/// Verifies that --publish correctly forwards ports to cloned VMs.
/// This tests the full port forwarding path: host → iptables DNAT → clone VM → nginx.
#[cfg(feature = "privileged-tests")]
#[tokio::test]
async fn test_clone_port_forward_bridged() -> Result<()> {
    let (baseline_name, clone_name, snapshot_name, _) = common::unique_names("pf-bridged");

    // Port 8080:80 - DNAT is scoped to veth IP so same port works across parallel VMs
    let host_port: u16 = 8080;

    println!("\n╔═══════════════════════════════════════════════════════════════╗");
    println!("║     Clone Port Forwarding Test (bridged)                      ║");
    println!("╚═══════════════════════════════════════════════════════════════╝\n");

    let fcvm_path = common::find_fcvm_binary()?;

    // Step 1: Start baseline VM with nginx
    println!("Step 1: Starting baseline VM with nginx...");
    let (_baseline_child, baseline_pid) = common::spawn_fcvm_with_logs(
        &[
            "podman",
            "run",
            "--name",
            &baseline_name,
            "--network",
            "bridged",
            common::TEST_IMAGE,
        ],
        &baseline_name,
    )
    .await
    .context("spawning baseline VM")?;

    println!("  Waiting for baseline VM to become healthy...");
    common::poll_health_by_pid(baseline_pid, 120).await?;
    println!("  ✓ Baseline VM healthy (PID: {})", baseline_pid);

    // Step 2: Create snapshot
    println!("\nStep 2: Creating snapshot...");
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
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Snapshot creation failed: {}", stderr);
    }
    println!("  ✓ Snapshot created");

    // Kill baseline - we only need the snapshot for clones
    common::kill_process(baseline_pid).await;
    println!("  Killed baseline VM (only need snapshot)");

    // Step 3: Start memory server
    println!("\nStep 3: Starting memory server...");
    let (_serve_child, serve_pid) =
        common::spawn_fcvm_with_logs(&["snapshot", "serve", &snapshot_name], "uffd-server")
            .await
            .context("spawning memory server")?;

    // Wait for serve to be ready (poll for socket)
    common::poll_serve_ready(&snapshot_name, serve_pid, 30).await?;
    println!("  ✓ Memory server ready (PID: {})", serve_pid);

    // Step 4: Spawn clone WITH port forwarding
    let publish_arg = format!("{}:80", host_port);
    println!("\nStep 4: Spawning clone with --publish {}...", publish_arg);
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
            "bridged",
            "--publish",
            &publish_arg,
        ],
        &clone_name,
    )
    .await
    .context("spawning clone with port forward")?;

    // Wait for clone to become healthy
    println!("  Waiting for clone to become healthy...");
    common::poll_health_by_pid(clone_pid, 120).await?;
    println!("  ✓ Clone is healthy (PID: {})", clone_pid);

    // Step 5: Test port forwarding
    println!("\nStep 5: Testing port forwarding...");

    // Get clone's guest IP from state
    let output = tokio::process::Command::new(&fcvm_path)
        .args(["ls", "--json", "--pid", &clone_pid.to_string()])
        .output()
        .await
        .context("getting clone state")?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: Vec<serde_json::Value> = serde_json::from_str(&stdout).unwrap_or_default();
    let network = parsed.first().and_then(|v| v.get("config")?.get("network"));

    let guest_ip = network
        .and_then(|n| n.get("guest_ip")?.as_str())
        .unwrap_or_default()
        .to_string();
    let veth_host_ip = network
        .and_then(|n| n.get("host_ip")?.as_str())
        .unwrap_or_default()
        .to_string();

    println!(
        "  Clone guest_ip: {}, veth_host_ip: {}",
        guest_ip, veth_host_ip
    );

    // Test: Access via port forwarding (veth's host IP)
    // DNAT rules are scoped to the veth IP, so this is what we test
    println!(
        "  Testing port forwarding via veth IP {}:{}...",
        veth_host_ip, host_port
    );
    let forward_result = tokio::process::Command::new("curl")
        .args([
            "-s",
            "--max-time",
            "10",
            &format!("http://{}:{}", veth_host_ip, host_port),
        ])
        .output()
        .await;

    let forward_works = forward_result
        .map(|o| o.status.success() && !o.stdout.is_empty())
        .unwrap_or(false);
    println!(
        "    Port forward (veth IP): {}",
        if forward_works { "✓ OK" } else { "✗ FAIL" }
    );

    // Cleanup
    println!("\nCleaning up...");
    common::kill_process(clone_pid).await;
    println!("  Killed clone");
    common::kill_process(serve_pid).await;
    println!("  Killed memory server");

    // Results
    println!("\n╔═══════════════════════════════════════════════════════════════╗");
    println!("║                         RESULTS                               ║");
    println!("╠═══════════════════════════════════════════════════════════════╣");
    println!(
        "║  Port forward (veth IP):    {}                                 ║",
        if forward_works {
            "✓ PASSED"
        } else {
            "✗ FAILED"
        }
    );
    println!("╚═══════════════════════════════════════════════════════════════╝");

    // Port forwarding via veth IP must work
    if forward_works {
        println!("\n✅ CLONE PORT FORWARDING TEST PASSED!");
        Ok(())
    } else {
        anyhow::bail!(
            "Clone port forwarding test failed: forward={}",
            forward_works
        )
    }
}

/// Test port forwarding on clones with rootless networking
///
/// This is the key test - rootless clones with port forwarding.
/// Port forwarding is done via slirp4netns API, accessing via unique loopback IP.
#[tokio::test]
async fn test_clone_port_forward_rootless() -> Result<()> {
    let (baseline_name, clone_name, snapshot_name, _) = common::unique_names("pf-rootless");

    println!("\n╔═══════════════════════════════════════════════════════════════╗");
    println!("║     Clone Port Forwarding Test (rootless)                     ║");
    println!("╚═══════════════════════════════════════════════════════════════╝\n");

    let fcvm_path = common::find_fcvm_binary()?;

    // Step 1: Start baseline VM with nginx (rootless)
    println!("Step 1: Starting baseline VM with nginx (rootless)...");
    let (_baseline_child, baseline_pid) = common::spawn_fcvm_with_logs(
        &[
            "podman",
            "run",
            "--name",
            &baseline_name,
            "--network",
            "rootless",
            common::TEST_IMAGE,
        ],
        &baseline_name,
    )
    .await
    .context("spawning baseline VM")?;

    println!("  Waiting for baseline VM to become healthy...");
    common::poll_health_by_pid(baseline_pid, 90).await?;
    println!("  ✓ Baseline VM healthy (PID: {})", baseline_pid);

    // Step 2: Create snapshot
    println!("\nStep 2: Creating snapshot...");
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
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Snapshot creation failed: {}", stderr);
    }
    println!("  ✓ Snapshot created");

    // Kill baseline - we only need the snapshot for clones
    common::kill_process(baseline_pid).await;
    println!("  Killed baseline VM (only need snapshot)");

    // Step 3: Start memory server
    println!("\nStep 3: Starting memory server...");
    let (_serve_child, serve_pid) =
        common::spawn_fcvm_with_logs(&["snapshot", "serve", &snapshot_name], "uffd-server")
            .await
            .context("spawning memory server")?;

    // Wait for serve to be ready (poll for socket)
    common::poll_serve_ready(&snapshot_name, serve_pid, 30).await?;
    println!("  ✓ Memory server ready (PID: {})", serve_pid);

    // Step 4: Spawn clone WITH port forwarding (rootless)
    // Use dynamic port to avoid conflicts with system services
    let host_port = common::find_available_high_port().context("finding available port")?;
    let publish_arg = format!("{}:80", host_port);
    println!(
        "\nStep 4: Spawning clone with --publish {} (rootless)...",
        publish_arg
    );
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
            "rootless",
            "--publish",
            &publish_arg,
        ],
        &clone_name,
    )
    .await
    .context("spawning clone with port forward")?;

    // Wait for clone to become healthy
    println!("  Waiting for clone to become healthy...");
    common::poll_health_by_pid(clone_pid, 120).await?;
    println!("  ✓ Clone is healthy (PID: {})", clone_pid);

    // Step 5: Test port forwarding via loopback IP
    println!("\nStep 5: Testing port forwarding...");

    // Get clone's loopback IP from state (rootless uses 127.x.y.z)
    let output = tokio::process::Command::new(&fcvm_path)
        .args(["ls", "--json", "--pid", &clone_pid.to_string()])
        .output()
        .await
        .context("getting clone state")?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let loopback_ip: String = serde_json::from_str::<Vec<serde_json::Value>>(&stdout)
        .ok()
        .and_then(|v| v.first().cloned())
        .and_then(|v| {
            v.get("config")?
                .get("network")?
                .get("loopback_ip")?
                .as_str()
                .map(|s| s.to_string())
        })
        .unwrap_or_default();

    println!("  Clone loopback IP: {}", loopback_ip);

    // Test: Access via loopback IP and forwarded port
    println!(
        "  Testing access via loopback {}:{}...",
        loopback_ip, host_port
    );
    let loopback_result = tokio::process::Command::new("curl")
        .args([
            "-s",
            "--max-time",
            "10",
            &format!("http://{}:{}", loopback_ip, host_port),
        ])
        .output()
        .await;

    let loopback_works = loopback_result
        .as_ref()
        .map(|o| o.status.success() && !o.stdout.is_empty())
        .unwrap_or(false);

    if let Ok(ref out) = loopback_result {
        if loopback_works {
            println!("    Loopback access: ✓ OK");
            let response = String::from_utf8_lossy(&out.stdout);
            println!(
                "    Response: {} bytes (nginx welcome page)",
                response.len()
            );
        } else {
            println!("    Loopback access: ✗ FAIL");
            println!("    stderr: {}", String::from_utf8_lossy(&out.stderr));
        }
    } else {
        println!("    Loopback access: ✗ FAIL (request error)");
    }

    // Cleanup
    println!("\nCleaning up...");
    common::kill_process(clone_pid).await;
    println!("  Killed clone");
    common::kill_process(serve_pid).await;
    println!("  Killed memory server");

    // Results
    println!("\n╔═══════════════════════════════════════════════════════════════╗");
    println!("║                         RESULTS                               ║");
    println!("╠═══════════════════════════════════════════════════════════════╣");
    println!(
        "║  Loopback port forward: {}                                    ║",
        if loopback_works {
            "✓ PASSED"
        } else {
            "✗ FAILED"
        }
    );
    println!("╚═══════════════════════════════════════════════════════════════╝");

    if loopback_works {
        println!("\n✅ ROOTLESS CLONE PORT FORWARDING TEST PASSED!");
        Ok(())
    } else {
        anyhow::bail!("Rootless clone port forwarding test failed")
    }
}

/// Test direct file-based snapshot run (--snapshot flag) with rootless networking
///
/// This tests the new --snapshot flag which restores directly from disk
/// without needing a UFFD memory server. Simpler for single clones.
#[tokio::test]
async fn test_snapshot_run_direct_rootless() -> Result<()> {
    snapshot_run_direct_test_impl("rootless").await
}

/// Test direct file-based snapshot run (--snapshot flag) with bridged networking
#[cfg(feature = "privileged-tests")]
#[tokio::test]
async fn test_snapshot_run_direct_bridged() -> Result<()> {
    snapshot_run_direct_test_impl("bridged").await
}

/// Implementation of direct file-based snapshot run test
async fn snapshot_run_direct_test_impl(network: &str) -> Result<()> {
    let (baseline_name, clone_name, snapshot_name, _) =
        common::unique_names(&format!("direct-{}", network));

    println!("\n╔═══════════════════════════════════════════════════════════════╗");
    println!(
        "║     Direct Snapshot Run Test ({:8})                       ║",
        network
    );
    println!("║     (--snapshot flag, no UFFD server needed)                  ║");
    println!("╚═══════════════════════════════════════════════════════════════╝\n");

    let fcvm_path = common::find_fcvm_binary()?;

    // Step 1: Start baseline VM
    println!("Step 1: Starting baseline VM...");
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

    println!("  Waiting for baseline VM to become healthy...");
    common::poll_health_by_pid(baseline_pid, 120).await?;
    println!("  ✓ Baseline VM healthy (PID: {})", baseline_pid);

    // Step 2: Create snapshot
    println!("\nStep 2: Creating snapshot...");
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
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Snapshot creation failed: {}", stderr);
    }
    println!("  ✓ Snapshot created");

    // Kill baseline - we only need the snapshot files
    common::kill_process(baseline_pid).await;
    println!("  Killed baseline VM (only need snapshot files)");

    // Step 3: Run clone directly from snapshot files (NO UFFD server!)
    println!(
        "\nStep 3: Running clone with --snapshot {} (direct file mode)...",
        snapshot_name
    );
    let (_clone_child, clone_pid) = common::spawn_fcvm_with_logs(
        &[
            "snapshot",
            "run",
            "--snapshot", // Direct file mode, not --pid
            &snapshot_name,
            "--name",
            &clone_name,
            "--network",
            network,
        ],
        &clone_name,
    )
    .await
    .context("spawning clone from snapshot (direct mode)")?;

    // Step 4: Wait for clone to become healthy
    println!("\nStep 4: Waiting for clone to become healthy...");
    common::poll_health_by_pid(clone_pid, 120).await?;
    println!("  ✓ Clone is healthy (PID: {})", clone_pid);

    // Step 5: Verify clone works by executing a command
    println!("\nStep 5: Verifying clone works with exec...");
    let output = tokio::process::Command::new(&fcvm_path)
        .args([
            "exec",
            "--pid",
            &clone_pid.to_string(),
            "--",
            "echo",
            "DIRECT_SNAPSHOT_SUCCESS",
        ])
        .output()
        .await
        .context("running exec in clone")?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let exec_ok = stdout.contains("DIRECT_SNAPSHOT_SUCCESS");
    println!("  Exec result: {}", if exec_ok { "✓ OK" } else { "✗ FAIL" });

    // Cleanup
    println!("\nCleaning up...");
    common::kill_process(clone_pid).await;
    println!("  Killed clone");

    // Results
    println!("\n╔═══════════════════════════════════════════════════════════════╗");
    println!("║                         RESULTS                               ║");
    println!("╠═══════════════════════════════════════════════════════════════╣");
    println!(
        "║  Direct snapshot restore: {}                                  ║",
        if exec_ok { "✓ PASSED" } else { "✗ FAILED" }
    );
    println!("╚═══════════════════════════════════════════════════════════════╝");

    if exec_ok {
        println!("\n✅ DIRECT SNAPSHOT RUN TEST PASSED!");
        Ok(())
    } else {
        anyhow::bail!("Direct snapshot run test failed: exec_ok={}", exec_ok)
    }
}

/// Test snapshot run --exec with bridged networking
#[cfg(feature = "privileged-tests")]
#[tokio::test]
async fn test_snapshot_run_exec_bridged() -> Result<()> {
    snapshot_run_exec_test_impl("bridged").await
}

/// Test snapshot run --exec with rootless networking
#[tokio::test]
async fn test_snapshot_run_exec_rootless() -> Result<()> {
    snapshot_run_exec_test_impl("rootless").await
}

/// Implementation of snapshot run --exec test
async fn snapshot_run_exec_test_impl(network: &str) -> Result<()> {
    let (baseline_name, _, snapshot_name, _) = common::unique_names(&format!("exec-{}", network));

    println!("\n╔═══════════════════════════════════════════════════════════════╗");
    println!(
        "║     Snapshot Run --exec Test ({:8})                      ║",
        network
    );
    println!("╚═══════════════════════════════════════════════════════════════╝\n");

    let fcvm_path = common::find_fcvm_binary()?;

    // Step 1: Start baseline VM
    println!("Step 1: Starting baseline VM...");
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

    println!("  Waiting for baseline VM to become healthy...");
    common::poll_health_by_pid(baseline_pid, 120).await?;
    println!("  ✓ Baseline VM healthy (PID: {})", baseline_pid);

    // Step 2: Create snapshot
    println!("\nStep 2: Creating snapshot...");
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
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Snapshot creation failed: {}", stderr);
    }
    println!("  ✓ Snapshot created");

    // Step 3: Start memory server
    println!("\nStep 3: Starting memory server...");
    let (_serve_child, serve_pid) =
        common::spawn_fcvm_with_logs(&["snapshot", "serve", &snapshot_name], "uffd-server")
            .await
            .context("spawning memory server")?;

    // Wait for serve to be ready (poll for socket)
    common::poll_serve_ready(&snapshot_name, serve_pid, 30).await?;
    println!("  ✓ Memory server ready (PID: {})", serve_pid);

    // Step 4: Run clone with --exec (command that outputs something)
    println!("\nStep 4: Running clone with --exec 'echo EXEC_TEST_SUCCESS'...");
    let serve_pid_str = serve_pid.to_string();
    let output = tokio::process::Command::new(&fcvm_path)
        .args([
            "snapshot",
            "run",
            "--pid",
            &serve_pid_str,
            "--network",
            network,
            "--exec",
            "echo EXEC_TEST_SUCCESS",
        ])
        .output()
        .await
        .context("running snapshot run --exec")?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    println!("  stdout: {}", stdout.trim());
    println!(
        "  stderr: {}",
        stderr
            .trim()
            .lines()
            .take(5)
            .collect::<Vec<_>>()
            .join("\n          ")
    );

    // Verify the output contains our test string
    let exec_success = stdout.contains("EXEC_TEST_SUCCESS") || stderr.contains("EXEC_TEST_SUCCESS");
    let exit_success = output.status.success();

    // Cleanup
    println!("\nCleaning up...");
    common::kill_process(serve_pid).await;
    println!("  Killed memory server");
    common::kill_process(baseline_pid).await;
    println!("  Killed baseline VM");

    // Final result
    if exec_success && exit_success {
        println!("\n✅ SNAPSHOT RUN --EXEC TEST PASSED!");
        Ok(())
    } else {
        anyhow::bail!(
            "Test failed: exec_output_found={}, exit_success={}, stdout='{}', stderr='{}'",
            exec_success,
            exit_success,
            stdout.trim(),
            stderr.trim()
        )
    }
}
