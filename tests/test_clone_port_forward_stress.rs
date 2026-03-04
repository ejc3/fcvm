//! Stress test for clone port forwarding.
//!
//! This test exercises the pasta port forwarding path under stress:
//! - Multiple clones spawned from the same snapshot
//! - Concurrent rapid HTTP requests to all clones simultaneously
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
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Instant;

/// Number of clones to spawn
const NUM_CLONES: usize = 3;

/// Number of HTTP requests per clone
const REQUESTS_PER_CLONE: usize = 20;

/// Stress test: multiple clones with port forwarding, concurrent HTTP requests
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
        "║     {} clones × {} requests each (concurrent)              ║",
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
    let baseline_ip = common::get_loopback_ip(baseline_pid).await?;
    println!("  Baseline loopback IP: {}", baseline_ip);

    let baseline_check = common::curl_check(&baseline_ip, host_port, 5).await;
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

    // Step 4: Spawn all clones concurrently
    println!("\nStep 4: Spawning {} clones concurrently...", NUM_CLONES);
    let serve_pid_str = serve_pid.to_string();

    struct CloneInfo {
        name: String,
        pid: u32,
        loopback_ip: String,
        _child: tokio::process::Child,
    }

    // Spawn all clones concurrently using JoinSet
    let mut spawn_set = tokio::task::JoinSet::new();

    for i in 0..NUM_CLONES {
        let clone_name = format!("pf-stress-clone-{}-{}", i, std::process::id());
        println!("  Spawning clone {} ({})...", i, clone_name);
        let pid_str = serve_pid_str.clone();
        spawn_set.spawn(async move {
            let (child, clone_pid) = common::spawn_fcvm_with_logs(
                &["snapshot", "run", "--pid", &pid_str, "--name", &clone_name],
                &clone_name,
            )
            .await
            .context(format!("spawning clone {}", clone_name))?;

            common::poll_health_by_pid(clone_pid, 120).await?;
            let loopback_ip = common::get_loopback_ip(clone_pid).await?;
            Ok::<_, anyhow::Error>((clone_name, clone_pid, loopback_ip, child))
        });
    }

    let mut clones: Vec<CloneInfo> = Vec::new();
    while let Some(result) = spawn_set.join_next().await {
        let (name, pid, loopback_ip, child) = result??;
        println!(
            "  ✓ Clone {} healthy (PID: {}, IP: {})",
            name, pid, loopback_ip
        );
        clones.push(CloneInfo {
            name,
            pid,
            loopback_ip,
            _child: child,
        });
    }

    // Step 5: Concurrent HTTP requests to all clones simultaneously
    println!(
        "\nStep 5: Sending {} HTTP requests to each of {} clones (concurrently)...",
        REQUESTS_PER_CLONE, NUM_CLONES
    );

    let total_success = Arc::new(AtomicU32::new(0));
    let total_zero_bytes = Arc::new(AtomicU32::new(0));
    let total_errors = Arc::new(AtomicU32::new(0));

    let start = Instant::now();

    // Spawn concurrent tasks for all clones
    let mut handles = Vec::new();
    for clone in &clones {
        let ip = clone.loopback_ip.clone();
        let name = clone.name.clone();
        let success = Arc::clone(&total_success);
        let zero = Arc::clone(&total_zero_bytes);
        let errors = Arc::clone(&total_errors);

        let handle = tokio::spawn(async move {
            let mut clone_success = 0u32;
            let mut clone_zero = 0u32;
            let mut clone_error = 0u32;

            for req in 0..REQUESTS_PER_CLONE {
                let result = common::curl_check(&ip, host_port, 5).await;

                if result.success && result.body_len > 0 {
                    clone_success += 1;
                    success.fetch_add(1, Ordering::Relaxed);
                } else if result.success && result.body_len == 0 {
                    clone_zero += 1;
                    zero.fetch_add(1, Ordering::Relaxed);
                    println!("    ⚠ Clone {} request {}: 0-byte response!", name, req);
                } else {
                    clone_error += 1;
                    errors.fetch_add(1, Ordering::Relaxed);
                    if clone_error <= 3 {
                        println!(
                            "    ✗ Clone {} request {}: error ({})",
                            name, req, result.error
                        );
                    }
                }
            }

            (name, clone_success, clone_zero, clone_error)
        });
        handles.push(handle);
    }

    // Wait for all concurrent tasks to complete
    for handle in handles {
        let (name, ok, zero, err) = handle.await?;
        println!(
            "  Clone {}: {}/{} OK, {} zero-byte, {} errors",
            name, ok, REQUESTS_PER_CLONE, zero, err
        );
    }

    let elapsed = start.elapsed();
    let total_requests = (NUM_CLONES * REQUESTS_PER_CLONE) as u32;
    let success = total_success.load(Ordering::Relaxed);
    let zero_bytes = total_zero_bytes.load(Ordering::Relaxed);
    let errs = total_errors.load(Ordering::Relaxed);

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
        "║  Total requests:    {:4}  ({:.1}s concurrent)                ║",
        total_requests,
        elapsed.as_secs_f64()
    );
    println!(
        "║  Successful:        {:4}                                      ║",
        success
    );
    println!(
        "║  Zero-byte:         {:4}  (pasta poisoning pattern)           ║",
        zero_bytes
    );
    println!(
        "║  Errors:            {:4}                                      ║",
        errs
    );
    println!("╚═══════════════════════════════════════════════════════════════╝");

    if zero_bytes > 0 {
        anyhow::bail!(
            "PASTA POISONING DETECTED: {} out of {} requests returned 0 bytes. \
             This confirms that wait_for_port_forwarding() during restore \
             (before guest exists) corrupts pasta's forwarding state.",
            zero_bytes,
            total_requests
        );
    }

    if errs > 0 {
        anyhow::bail!(
            "Port forwarding errors: {} out of {} requests failed",
            errs,
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
