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
use std::process::Stdio;
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

    let mut baseline_child = tokio::process::Command::new(&fcvm_path)
        .args([
            "podman",
            "run",
            "--name",
            &baseline_name,
            "--network",
            network,
            "nginx:alpine",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawning baseline VM")?;

    let baseline_pid = baseline_child
        .id()
        .ok_or_else(|| anyhow::anyhow!("failed to get baseline PID"))?;

    // Spawn log consumers
    spawn_log_consumer(baseline_child.stdout.take(), &baseline_name);
    spawn_log_consumer_stderr(baseline_child.stderr.take(), &baseline_name);

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

    let mut serve_child = tokio::process::Command::new(&fcvm_path)
        .args(["snapshot", "serve", &snapshot_name])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawning memory server")?;

    let serve_pid = serve_child
        .id()
        .ok_or_else(|| anyhow::anyhow!("failed to get serve PID"))?;

    // Spawn log consumers
    spawn_log_consumer(serve_child.stdout.take(), "uffd-server");
    spawn_log_consumer_stderr(serve_child.stderr.take(), "uffd-server");

    // Wait for serve process to be ready
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Verify serve process is still running
    match serve_child.try_wait() {
        Ok(Some(status)) => {
            anyhow::bail!("Memory server exited unexpectedly with status: {}", status);
        }
        Ok(None) => {}
        Err(e) => {
            anyhow::bail!("Failed to check serve process status: {}", e);
        }
    }
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
        let fcvm_path = fcvm_path.clone();
        let network = network.to_string();
        let results = Arc::clone(&results);
        let clone_pids = Arc::clone(&clone_pids);

        let handle = tokio::spawn(async move {
            let spawn_start = Instant::now();

            let result = tokio::process::Command::new(&fcvm_path)
                .args([
                    "snapshot",
                    "run",
                    "--pid",
                    &serve_pid.to_string(),
                    "--name",
                    &clone_name,
                    "--network",
                    &network,
                ])
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn();

            match result {
                Ok(mut child) => {
                    let clone_pid = child.id().unwrap_or(0);
                    spawn_log_consumer(child.stdout.take(), &clone_name);
                    spawn_log_consumer_stderr(child.stderr.take(), &clone_name);
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

fn spawn_log_consumer(stdout: Option<tokio::process::ChildStdout>, name: &str) {
    if let Some(stdout) = stdout {
        let name = name.to_string();
        tokio::spawn(async move {
            use tokio::io::{AsyncBufReadExt, BufReader};
            let reader = BufReader::new(stdout);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                eprintln!("[{}] {}", name, line);
            }
        });
    }
}

fn spawn_log_consumer_stderr(stderr: Option<tokio::process::ChildStderr>, name: &str) {
    if let Some(stderr) = stderr {
        let name = name.to_string();
        tokio::spawn(async move {
            use tokio::io::{AsyncBufReadExt, BufReader};
            let reader = BufReader::new(stderr);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                eprintln!("[{} ERR] {}", name, line);
            }
        });
    }
}
