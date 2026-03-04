//! Stress test for clone port forwarding.
//!
//! This test exercises the pasta port forwarding path under stress:
//! - Multiple clones spawned from the same snapshot
//! - Rapid repeated HTTP requests to each clone
//! - Checks for 0-byte responses (pasta connection tracking poisoning)
//!
//! Background: During snapshot restore, `post_start()` calls
//! `wait_for_port_forwarding()` BEFORE the VM snapshot is loaded.
//! pasta accepts the TCP connection (it's listening on loopback),
//! but can't forward to the non-existent guest. This may poison
//! pasta's internal forwarding state, causing subsequent connections
//! to return 0 bytes instead of actual data.

#![cfg(feature = "integration-slow")]

mod common;

use anyhow::{Context, Result};
use std::time::{Duration, Instant};

/// Number of clones to spawn
const NUM_CLONES: usize = 3;

/// Number of HTTP requests per clone
const REQUESTS_PER_CLONE: usize = 20;

/// Stress test: multiple clones with port forwarding, rapid HTTP requests
///
/// Reproduces the "connect succeeded but 0 bytes" pattern seen in CI bench-vm
/// failures. The hypothesis: pasta's `wait_for_port_forwarding()` probe during
/// restore (before guest exists) poisons pasta's internal forwarding state.
#[tokio::test]
async fn test_clone_port_forward_stress_rootless() -> Result<()> {
    let (baseline_name, _, snapshot_name, _) = common::unique_names("pf-stress");

    println!("\n╔═══════════════════════════════════════════════════════════════╗");
    println!("║     Clone Port Forward Stress Test (rootless)                ║");
    println!(
        "║     {} clones × {} requests each                           ║",
        NUM_CLONES, REQUESTS_PER_CLONE
    );
    println!("╚═══════════════════════════════════════════════════════════════╝\n");

    let fcvm_path = common::find_fcvm_binary()?;

    // Allocate port before baseline so it's baked into the snapshot
    let host_port = common::find_available_high_port().context("finding available port")?;
    let publish_arg = format!("{}:80", host_port);

    // Step 1: Start baseline VM with nginx + port forwarding
    println!(
        "Step 1: Starting baseline VM (nginx, --publish {})...",
        publish_arg
    );
    let (_baseline_child, baseline_pid) = common::spawn_fcvm_with_logs(
        &[
            "podman",
            "run",
            "--name",
            &baseline_name,
            "--network",
            "rootless",
            "--publish",
            &publish_arg,
            "--health-check",
            "http://localhost:80",
            common::TEST_IMAGE,
        ],
        &baseline_name,
    )
    .await
    .context("spawning baseline VM")?;

    println!("  Waiting for baseline VM to become healthy...");
    common::poll_health_by_pid(baseline_pid, 90).await?;
    println!("  ✓ Baseline VM healthy (PID: {})", baseline_pid);

    // Verify baseline port forwarding works before snapshotting
    let output = tokio::process::Command::new(&fcvm_path)
        .args(["ls", "--json", "--pid", &baseline_pid.to_string()])
        .output()
        .await
        .context("getting baseline state")?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let baseline_ip: String = serde_json::from_str::<Vec<serde_json::Value>>(&stdout)
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

    println!("  Baseline loopback IP: {}", baseline_ip);

    // Verify baseline nginx responds
    let baseline_check = curl_check(&baseline_ip, host_port, 5).await;
    println!(
        "  Baseline HTTP check: {} ({} bytes)",
        if baseline_check.success {
            "✓"
        } else {
            "✗ FAIL"
        },
        baseline_check.body_len
    );
    assert!(
        baseline_check.success && baseline_check.body_len > 0,
        "Baseline nginx must respond with data before snapshot"
    );

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

    // Kill baseline - only need snapshot for clones
    common::kill_process(baseline_pid).await;
    println!("  Killed baseline VM");

    // Step 3: Start memory server
    println!("\nStep 3: Starting memory server...");
    let (_serve_child, serve_pid) =
        common::spawn_fcvm_with_logs(&["snapshot", "serve", &snapshot_name], "uffd-pf-stress")
            .await
            .context("spawning memory server")?;

    common::poll_serve_ready(&snapshot_name, serve_pid, 30).await?;
    println!("  ✓ Memory server ready (PID: {})", serve_pid);

    // Step 4: Spawn clones sequentially (each gets unique loopback IP)
    println!("\nStep 4: Spawning {} clones...", NUM_CLONES);
    let serve_pid_str = serve_pid.to_string();

    struct CloneInfo {
        name: String,
        pid: u32,
        loopback_ip: String,
        _child: tokio::process::Child,
    }

    let mut clones: Vec<CloneInfo> = Vec::new();

    for i in 0..NUM_CLONES {
        let clone_name = format!("pf-stress-clone-{}-{}", i, std::process::id());
        println!("  Spawning clone {} ({})...", i, clone_name);

        let (child, clone_pid) = common::spawn_fcvm_with_logs(
            &[
                "snapshot",
                "run",
                "--pid",
                &serve_pid_str,
                "--name",
                &clone_name,
            ],
            &clone_name,
        )
        .await
        .context(format!("spawning clone {}", i))?;

        // Wait for healthy
        common::poll_health_by_pid(clone_pid, 120).await?;
        println!("  ✓ Clone {} healthy (PID: {})", i, clone_pid);

        // Get loopback IP
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

        println!("    Loopback IP: {}", loopback_ip);

        clones.push(CloneInfo {
            name: clone_name,
            pid: clone_pid,
            loopback_ip,
            _child: child,
        });
    }

    // Step 5: Rapid HTTP requests to all clones
    println!(
        "\nStep 5: Sending {} HTTP requests to each of {} clones...",
        REQUESTS_PER_CLONE, NUM_CLONES
    );

    let mut total_requests = 0u32;
    let mut total_success = 0u32;
    let mut total_zero_bytes = 0u32;
    let mut total_errors = 0u32;

    for clone in &clones {
        let mut clone_success = 0u32;
        let mut clone_zero = 0u32;
        let mut clone_error = 0u32;

        let start = Instant::now();

        for req in 0..REQUESTS_PER_CLONE {
            let result = curl_check(&clone.loopback_ip, host_port, 5).await;
            total_requests += 1;

            if result.success && result.body_len > 0 {
                clone_success += 1;
                total_success += 1;
            } else if result.success && result.body_len == 0 {
                // This is the pasta poisoning pattern: connect succeeds but 0 bytes
                clone_zero += 1;
                total_zero_bytes += 1;
                println!(
                    "    ⚠ Clone {} request {}: 0-byte response!",
                    clone.name, req
                );
            } else {
                clone_error += 1;
                total_errors += 1;
                if clone_error <= 3 {
                    println!(
                        "    ✗ Clone {} request {}: error ({})",
                        clone.name, req, result.error
                    );
                }
            }

            // Brief pause between requests (not too long - we want stress)
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        let elapsed = start.elapsed();
        println!(
            "  Clone {} ({}): {}/{} OK, {} zero-byte, {} errors ({:.1}s)",
            clone.name,
            clone.loopback_ip,
            clone_success,
            REQUESTS_PER_CLONE,
            clone_zero,
            clone_error,
            elapsed.as_secs_f64()
        );
    }

    // Cleanup
    println!("\nCleaning up...");
    for clone in &clones {
        common::kill_process(clone.pid).await;
    }
    common::kill_process(serve_pid).await;
    println!("  Killed all clones and memory server");

    // Results
    println!("\n╔═══════════════════════════════════════════════════════════════╗");
    println!("║                         RESULTS                               ║");
    println!("╠═══════════════════════════════════════════════════════════════╣");
    println!(
        "║  Total requests:    {:4}                                      ║",
        total_requests
    );
    println!(
        "║  Successful:        {:4}                                      ║",
        total_success
    );
    println!(
        "║  Zero-byte:         {:4}  (pasta poisoning pattern)           ║",
        total_zero_bytes
    );
    println!(
        "║  Errors:            {:4}                                      ║",
        total_errors
    );
    println!("╚═══════════════════════════════════════════════════════════════╝");

    if total_zero_bytes > 0 {
        anyhow::bail!(
            "PASTA POISONING DETECTED: {} out of {} requests returned 0 bytes. \
             This confirms that wait_for_port_forwarding() during restore \
             (before guest exists) corrupts pasta's forwarding state.",
            total_zero_bytes,
            total_requests
        );
    }

    if total_errors > 0 {
        anyhow::bail!(
            "Port forwarding errors: {} out of {} requests failed",
            total_errors,
            total_requests
        );
    }

    println!("\n✅ CLONE PORT FORWARD STRESS TEST PASSED!");
    println!(
        "   All {} requests across {} clones returned valid data",
        total_requests, NUM_CLONES
    );
    Ok(())
}

/// Result of a single HTTP check
struct CurlResult {
    success: bool,
    body_len: usize,
    error: String,
}

/// Make a single HTTP request and return the result
async fn curl_check(ip: &str, port: u16, timeout_secs: u32) -> CurlResult {
    let url = format!("http://{}:{}", ip, port);
    match tokio::process::Command::new("curl")
        .args(["-s", "--max-time", &timeout_secs.to_string(), &url])
        .output()
        .await
    {
        Ok(output) => CurlResult {
            success: output.status.success(),
            body_len: output.stdout.len(),
            error: if output.status.success() {
                String::new()
            } else {
                String::from_utf8_lossy(&output.stderr).to_string()
            },
        },
        Err(e) => CurlResult {
            success: false,
            body_len: 0,
            error: format!("curl failed: {}", e),
        },
    }
}
