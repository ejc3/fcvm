//! Snapshot and clone integration tests
//!
//! Tests the full snapshot/clone workflow:
//! 1. Start a baseline VM
//! 2. Create a snapshot
//! 3. Start memory server
//! 4. Spawn clones from snapshot (concurrently)
//! 5. Verify clones become healthy (concurrently)

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

/// Stress test with 100 clones using rootless networking
/// Run this test in isolation: cargo test --test test_snapshot_clone test_snapshot_clone_stress_100 -- --ignored
#[tokio::test]
#[ignore]
async fn test_snapshot_clone_stress_100() -> Result<()> {
    snapshot_clone_test_impl("rootless", 100).await
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
    let snapshot_name = format!("test-snapshot-{}", network);
    let baseline_name = format!("baseline-{}", network);
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
        &["podman", "run", "--name", &baseline_name, "--network", network, common::TEST_IMAGE],
        &baseline_name,
    )
    .await
    .context("spawning baseline VM")?;

    // Wait for healthy
    println!("  Waiting for baseline VM to become healthy...");
    common::poll_health_by_pid(baseline_pid, 60).await?;
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

    let (_serve_child, serve_pid) = common::spawn_fcvm_with_logs(
        &["snapshot", "serve", &snapshot_name],
        "uffd-server",
    )
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
        let clone_name = format!("clone-{}-{}", network, i);
        let network = network.to_string();
        let results = Arc::clone(&results);
        let clone_pids = Arc::clone(&clone_pids);
        let serve_pid_str = serve_pid.to_string();

        let handle = tokio::spawn(async move {
            let spawn_start = Instant::now();

            let result = common::spawn_fcvm_with_logs(
                &["snapshot", "run", "--pid", &serve_pid_str, "--name", &clone_name, "--network", &network],
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
                        Duration::from_secs(60),
                        common::poll_health_by_pid(clone_pid, 60),
                    )
                    .await;

                    let (health_time, error) = match health_result {
                        Ok(Ok(_)) => (Some(health_start.elapsed().as_secs_f64()), None),
                        Ok(Err(e)) => (None, Some(format!("health check failed: {}", e))),
                        Err(_) => (None, Some("health check timeout".to_string())),
                    };

                    results.lock().await.push(CloneResult {
                        name: clone_name,
                        pid: clone_pid,
                        spawn_time_ms: spawn_ms,
                        health_time_secs: health_time,
                        error,
                    });
                }
                Err(e) => {
                    results.lock().await.push(CloneResult {
                        name: clone_name,
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

/// Test cloning while baseline VM is still running
///
/// This tests for vsock socket path conflicts: when cloning from a running baseline,
/// both the baseline and clone need separate vsock sockets. Without mount namespace
/// isolation, Firecracker would try to bind to the same socket path stored in vmstate.bin.
#[tokio::test]
async fn test_clone_while_baseline_running() -> Result<()> {
    let snapshot_name = "test-clone-running";
    let baseline_name = "baseline-running";

    println!("\n╔═══════════════════════════════════════════════════════════════╗");
    println!("║     Clone While Baseline Running Test                         ║");
    println!("╚═══════════════════════════════════════════════════════════════╝\n");

    let fcvm_path = common::find_fcvm_binary()?;

    // Step 1: Start baseline VM (bridged mode for faster startup)
    println!("Step 1: Starting baseline VM...");
    let (_baseline_child, baseline_pid) = common::spawn_fcvm_with_logs(
        &["podman", "run", "--name", baseline_name, "--network", "bridged", common::TEST_IMAGE],
        baseline_name,
    )
    .await
    .context("spawning baseline VM")?;

    println!("  Waiting for baseline VM to become healthy...");
    common::poll_health_by_pid(baseline_pid, 60).await?;
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
            snapshot_name,
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
    let (_serve_child, serve_pid) = common::spawn_fcvm_with_logs(
        &["snapshot", "serve", snapshot_name],
        "uffd-server",
    )
    .await
    .context("spawning memory server")?;

    // Wait for serve to be ready (poll for socket)
    common::poll_serve_ready(snapshot_name, serve_pid, 30).await?;
    println!("  ✓ Memory server ready (PID: {})", serve_pid);

    // Step 5: Clone WHILE baseline is still running (this is the key test!)
    println!("\nStep 5: Spawning clone while baseline is STILL RUNNING...");
    println!("  (This tests vsock socket isolation via mount namespace)");

    let clone_name = "clone-running";
    let serve_pid_str = serve_pid.to_string();
    let (_clone_child, clone_pid) = common::spawn_fcvm_with_logs(
        &["snapshot", "run", "--pid", &serve_pid_str, "--name", clone_name, "--network", "bridged"],
        clone_name,
    )
    .await
    .context("spawning clone while baseline running")?;

    // Step 6: Wait for clone to become healthy
    println!("\nStep 6: Waiting for clone to become healthy...");
    let clone_health_result = tokio::time::timeout(
        Duration::from_secs(60),
        common::poll_health_by_pid(clone_pid, 60),
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
    let snapshot_name = format!("test-internet-{}", network);
    let baseline_name = format!("baseline-internet-{}", network);

    println!("\n╔═══════════════════════════════════════════════════════════════╗");
    println!(
        "║     Clone Internet Connectivity Test ({:8})              ║",
        network
    );
    println!("╚═══════════════════════════════════════════════════════════════╝\n");

    let fcvm_path = common::find_fcvm_binary()?;

    // Step 1: Start baseline VM
    println!("Step 1: Starting baseline VM...");
    let (_baseline_child, baseline_pid) = common::spawn_fcvm_with_logs(
        &["podman", "run", "--name", &baseline_name, "--network", network, common::TEST_IMAGE],
        &baseline_name,
    )
    .await
    .context("spawning baseline VM")?;

    println!("  Waiting for baseline VM to become healthy...");
    common::poll_health_by_pid(baseline_pid, 60).await?;
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
    let (_serve_child, serve_pid) = common::spawn_fcvm_with_logs(
        &["snapshot", "serve", &snapshot_name],
        "uffd-server",
    )
    .await
    .context("spawning memory server")?;

    // Wait for serve to be ready (poll for socket)
    common::poll_serve_ready(&snapshot_name, serve_pid, 30).await?;
    println!("  ✓ Memory server ready (PID: {})", serve_pid);

    // Step 4: Spawn clone
    println!("\nStep 4: Spawning clone...");
    let clone_name = format!("clone-internet-{}", network);
    let serve_pid_str = serve_pid.to_string();
    let (_clone_child, clone_pid) = common::spawn_fcvm_with_logs(
        &["snapshot", "run", "--pid", &serve_pid_str, "--name", &clone_name, "--network", network],
        &clone_name,
    )
    .await
    .context("spawning clone")?;

    // Wait for clone to become healthy
    println!("  Waiting for clone to become healthy...");
    common::poll_health_by_pid(clone_pid, 60).await?;
    println!("  ✓ Clone is healthy (PID: {})", clone_pid);

    // Step 5: Test internet connectivity from inside the clone
    // We'll use fcvm exec to run commands inside the VM
    println!("\nStep 5: Testing internet connectivity from clone...");

    // Test 1: DNS resolution using nslookup/host (available in alpine)
    println!("  Testing DNS resolution...");
    let dns_result = test_clone_dns(&fcvm_path, clone_pid).await;

    // Test 2: HTTP connectivity using wget (available in alpine)
    println!("  Testing HTTP connectivity...");
    let http_result = test_clone_http(&fcvm_path, clone_pid).await;

    // Cleanup
    println!("\nCleaning up...");
    common::kill_process(clone_pid).await;
    println!("  Killed clone");
    common::kill_process(serve_pid).await;
    println!("  Killed memory server");

    // Report results
    println!("\n╔═══════════════════════════════════════════════════════════════╗");
    println!("║                         RESULTS                               ║");
    println!("╠═══════════════════════════════════════════════════════════════╣");

    let dns_ok = dns_result.is_ok();
    let http_ok = http_result.is_ok();

    if dns_ok {
        println!("║  DNS resolution:    ✓ PASSED                                 ║");
    } else {
        println!("║  DNS resolution:    ✗ FAILED                                 ║");
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

    if dns_ok && http_ok {
        println!("\n✅ CLONE INTERNET CONNECTIVITY TEST PASSED! ({})", network);
        Ok(())
    } else {
        anyhow::bail!(
            "Clone internet test failed: dns={}, http={}",
            dns_ok,
            http_ok
        )
    }
}

/// Test DNS resolution from inside the clone VM
async fn test_clone_dns(fcvm_path: &std::path::Path, clone_pid: u32) -> Result<()> {
    // Use nslookup to test DNS - available in the VM
    // We'll resolve a well-known domain
    let output = tokio::process::Command::new(fcvm_path)
        .args([
            "exec",
            "--pid",
            &clone_pid.to_string(),
            "--",
            "nslookup",
            "google.com",
        ])
        .output()
        .await
        .context("running nslookup in clone")?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    if output.status.success() && (stdout.contains("Address") || stdout.contains("Name:")) {
        println!("    nslookup google.com: OK");
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

/// Test HTTP connectivity from inside the clone VM
async fn test_clone_http(fcvm_path: &std::path::Path, clone_pid: u32) -> Result<()> {
    // Use curl to test HTTP - available in the VM
    // We'll fetch a small file from a reliable source
    let output = tokio::process::Command::new(fcvm_path)
        .args([
            "exec",
            "--pid",
            &clone_pid.to_string(),
            "--",
            "curl",
            "-s",
            "--connect-timeout",
            "10",
            "http://httpbin.org/ip",
        ])
        .output()
        .await
        .context("running curl in clone")?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    if output.status.success() && stdout.contains("origin") {
        println!("    curl httpbin.org/ip: OK (got response)");
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

/// Test snapshot run --exec with bridged networking
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
    let snapshot_name = format!("test-exec-{}", network);
    let baseline_name = format!("baseline-exec-{}", network);

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
        &["podman", "run", "--name", &baseline_name, "--network", network, common::TEST_IMAGE],
        &baseline_name,
    )
    .await
    .context("spawning baseline VM")?;

    println!("  Waiting for baseline VM to become healthy...");
    common::poll_health_by_pid(baseline_pid, 60).await?;
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
    let (_serve_child, serve_pid) = common::spawn_fcvm_with_logs(
        &["snapshot", "serve", &snapshot_name],
        "uffd-server",
    )
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
    println!("  stderr: {}", stderr.trim().lines().take(5).collect::<Vec<_>>().join("\n          "));

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
