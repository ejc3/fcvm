//! Vsock CONNECT reliability stress test
//!
//! Proves that `fcvm exec` (which uses vsock CONNECT to reach the guest exec server)
//! works 100% reliably and stays flat/fast through every lifecycle stage:
//!
//! 1. Baseline VM (direct boot)
//! 2. Clone from snapshot (snapshot restore)
//! 3. Clone of clone (double snapshot restore)
//! 4. Parallel clones from same serve
//! 5. Clone surviving sibling death
//!
//! Also tests kill-on-drop resilience: killing an exec mid-flight must not
//! break subsequent execs.

#![cfg(feature = "integration-slow")]

mod common;

use anyhow::{Context, Result};
use std::time::{Duration, Instant};

/// Number of sequential exec iterations per stress round
const EXEC_ITERATIONS: usize = 20;

/// Maximum acceptable time for a single exec (milliseconds)
const MAX_EXEC_MS: u128 = 2000;

/// Number of kill-on-drop iterations per round
const KILL_ON_DROP_ITERATIONS: usize = 5;

/// Run rapid sequential execs against a VM, assert all succeed and are fast.
///
/// Returns (success_count, avg_ms, max_ms).
async fn exec_stress(pid: u32, label: &str) -> Result<(usize, f64, f64)> {
    let fcvm_path = common::find_fcvm_binary()?;
    let pid_str = pid.to_string();
    let mut times_ms = Vec::with_capacity(EXEC_ITERATIONS);

    println!(
        "\n  --- Exec stress: {} ({} iterations) ---",
        label, EXEC_ITERATIONS
    );

    for i in 0..EXEC_ITERATIONS {
        let t0 = Instant::now();
        let output = tokio::time::timeout(
            Duration::from_secs(5),
            tokio::process::Command::new(&fcvm_path)
                .args(["exec", "--pid", &pid_str, "--vm", "--", "echo", "ok"])
                .output(),
        )
        .await;

        let elapsed_ms = t0.elapsed().as_millis() as f64;

        match output {
            Ok(Ok(out)) if out.status.success() => {
                let stdout = String::from_utf8_lossy(&out.stdout);
                assert!(
                    stdout.trim() == "ok",
                    "exec #{} returned unexpected output: {:?}",
                    i,
                    stdout.trim()
                );
                if elapsed_ms > 500.0 {
                    println!("    SLOW #{}: {:.0}ms", i, elapsed_ms);
                }
                times_ms.push(elapsed_ms);
            }
            Ok(Ok(out)) => {
                let stderr = String::from_utf8_lossy(&out.stderr);
                panic!(
                    "exec #{} failed: rc={} stderr={} ({:.0}ms)",
                    i,
                    out.status.code().unwrap_or(-1),
                    &stderr[..stderr.len().min(200)],
                    elapsed_ms
                );
            }
            Ok(Err(e)) => {
                panic!("exec #{} spawn error: {} ({:.0}ms)", i, e, elapsed_ms);
            }
            Err(_) => {
                panic!("exec #{} TIMEOUT after {:.0}ms", i, elapsed_ms);
            }
        }

        assert!(
            (elapsed_ms as u128) < MAX_EXEC_MS,
            "exec #{} took {:.0}ms (max {}ms)",
            i,
            elapsed_ms,
            MAX_EXEC_MS
        );
    }

    let avg = times_ms.iter().sum::<f64>() / times_ms.len() as f64;
    let max = times_ms.iter().cloned().fold(0.0_f64, f64::max);
    let min = times_ms.iter().cloned().fold(f64::MAX, f64::min);
    println!(
        "  Results: {}/{} OK  min={:.0}ms  avg={:.0}ms  max={:.0}ms",
        times_ms.len(),
        EXEC_ITERATIONS,
        min,
        avg,
        max
    );

    Ok((times_ms.len(), avg, max))
}

/// Simulate health monitor kill_on_drop: start exec, kill after 100ms, verify next exec works.
async fn kill_on_drop_stress(pid: u32, label: &str) -> Result<()> {
    let fcvm_path = common::find_fcvm_binary()?;
    let pid_str = pid.to_string();

    println!(
        "\n  --- Kill-on-drop: {} ({} iterations) ---",
        label, KILL_ON_DROP_ITERATIONS
    );

    for i in 0..KILL_ON_DROP_ITERATIONS {
        // Start a long-running exec, then kill it after 100ms
        let mut child = tokio::process::Command::new(&fcvm_path)
            .args(["exec", "--pid", &pid_str, "--vm", "--", "sleep", "10"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .context("spawning long exec")?;

        tokio::time::sleep(Duration::from_millis(100)).await;
        let _ = child.kill().await;
        let _ = child.wait().await;

        // Verify a normal exec still works immediately after
        let t0 = Instant::now();
        let output = tokio::time::timeout(
            Duration::from_secs(5),
            tokio::process::Command::new(&fcvm_path)
                .args(["exec", "--pid", &pid_str, "--vm", "--", "echo", "ok"])
                .output(),
        )
        .await;

        let elapsed_ms = t0.elapsed().as_millis();

        match output {
            Ok(Ok(out)) if out.status.success() => {
                let stdout = String::from_utf8_lossy(&out.stdout);
                assert!(
                    stdout.trim() == "ok",
                    "post-kill exec #{} returned unexpected output: {:?}",
                    i,
                    stdout.trim()
                );
            }
            Ok(Ok(out)) => {
                let stderr = String::from_utf8_lossy(&out.stderr);
                panic!(
                    "post-kill exec #{} failed: stderr={} ({:.0}ms)",
                    i,
                    &stderr[..stderr.len().min(200)],
                    elapsed_ms
                );
            }
            Ok(Err(e)) => panic!("post-kill exec #{} error: {}", i, e),
            Err(_) => panic!("post-kill exec #{} TIMEOUT after {}ms", i, elapsed_ms),
        }
    }

    println!("  All {} post-kill execs OK", KILL_ON_DROP_ITERATIONS);
    Ok(())
}

/// Full lifecycle stress test for vsock CONNECT reliability.
///
/// Tests exec through: baseline → clone → clone-of-clone → parallel clones → sibling death.
/// Every exec must succeed and complete in under 2 seconds.
#[cfg(feature = "privileged-tests")]
#[tokio::test]
async fn test_vsock_connect_lifecycle_stress_bridged() -> Result<()> {
    vsock_connect_lifecycle_stress("bridged").await
}

#[tokio::test]
async fn test_vsock_connect_lifecycle_stress_rootless() -> Result<()> {
    vsock_connect_lifecycle_stress("rootless").await
}

#[cfg(feature = "privileged-tests")]
#[tokio::test]
async fn test_vsock_connect_lifecycle_stress_routed() -> Result<()> {
    skip_if_no_ipv6!();
    vsock_connect_lifecycle_stress("routed").await
}

async fn vsock_connect_lifecycle_stress(network: &str) -> Result<()> {
    let (baseline_name, clone1_name, snapshot_name, _serve_name) =
        common::unique_names(&format!("vsock-{}", network));
    let clone2_name = format!("{}-c2", clone1_name);
    let clone3_name = format!("{}-c3", clone1_name);

    println!("\n╔═══════════════════════════════════════════════════════════════╗");
    println!(
        "║     Vsock CONNECT Lifecycle Stress Test ({:8})            ║",
        network
    );
    println!("╚═══════════════════════════════════════════════════════════════╝");

    // ========================================================================
    // STAGE 1: Baseline VM
    // ========================================================================
    println!("\n=== STAGE 1: Baseline VM ===");
    let (_baseline_child, baseline_pid) = common::spawn_fcvm_with_logs(
        &[
            "podman",
            "run",
            "--name",
            &baseline_name,
            "--network",
            network,
            "--health-check",
            "http://localhost/",
            common::TEST_IMAGE,
        ],
        &baseline_name,
    )
    .await
    .context("spawning baseline VM")?;

    common::poll_health_by_pid(baseline_pid, 120).await?;
    println!("  Baseline healthy (PID: {})", baseline_pid);

    exec_stress(baseline_pid, "baseline").await?;
    kill_on_drop_stress(baseline_pid, "baseline").await?;

    // ========================================================================
    // STAGE 2: Snapshot + Clone
    // ========================================================================
    println!("\n=== STAGE 2: Snapshot baseline + Clone ===");
    common::create_snapshot_by_pid(baseline_pid, &snapshot_name).await?;
    println!("  Snapshot created: {}", snapshot_name);

    let (_serve_child, serve_pid) = common::start_memory_server(&snapshot_name).await?;
    println!("  Memory server ready (PID: {})", serve_pid);

    let (_clone1_child, clone1_pid) = common::spawn_clone(serve_pid, &clone1_name, network).await?;
    common::poll_health_by_pid(clone1_pid, 120).await?;
    println!("  Clone1 healthy (PID: {})", clone1_pid);

    exec_stress(clone1_pid, "clone1").await?;
    kill_on_drop_stress(clone1_pid, "clone1").await?;

    // ========================================================================
    // STAGE 3: Clone of Clone
    // ========================================================================
    println!("\n=== STAGE 3: Snapshot clone1 + Clone-of-clone ===");
    let snap2_name = format!("{}-snap2", snapshot_name);
    common::create_snapshot_by_pid(clone1_pid, &snap2_name).await?;
    println!("  Snapshot of clone created: {}", snap2_name);

    let (_serve2_child, serve2_pid) = common::start_memory_server(&snap2_name).await?;
    println!("  Memory server 2 ready (PID: {})", serve2_pid);

    let (_clone2_child, clone2_pid) =
        common::spawn_clone(serve2_pid, &clone2_name, network).await?;
    common::poll_health_by_pid(clone2_pid, 120).await?;
    println!("  Clone-of-clone healthy (PID: {})", clone2_pid);

    exec_stress(clone2_pid, "clone-of-clone").await?;
    kill_on_drop_stress(clone2_pid, "clone-of-clone").await?;

    // ========================================================================
    // STAGE 4: Parallel clone from same serve
    // ========================================================================
    println!("\n=== STAGE 4: Parallel clone from same serve ===");
    let (_clone3_child, clone3_pid) = common::spawn_clone(serve_pid, &clone3_name, network).await?;
    common::poll_health_by_pid(clone3_pid, 120).await?;
    println!("  Clone3 healthy (PID: {})", clone3_pid);

    exec_stress(clone3_pid, "parallel-clone3").await?;

    // Verify clone1 still works while clone3 is alive
    exec_stress(clone1_pid, "clone1-concurrent").await?;

    // ========================================================================
    // STAGE 5: Kill a clone, verify sibling survives
    // ========================================================================
    println!("\n=== STAGE 5: Kill clone1, verify clone3 survives ===");
    common::kill_process(clone1_pid).await;
    println!("  Clone1 killed");

    // Brief pause for cleanup
    tokio::time::sleep(Duration::from_secs(1)).await;

    exec_stress(clone3_pid, "clone3-after-sibling-killed").await?;

    // ========================================================================
    // Cleanup
    // ========================================================================
    println!("\n--- Cleanup ---");
    common::kill_process(clone3_pid).await;
    common::kill_process(clone2_pid).await;
    common::kill_process(serve2_pid).await;
    common::kill_process(serve_pid).await;
    common::kill_process(baseline_pid).await;

    println!("\n╔═══════════════════════════════════════════════════════════════╗");
    println!("║     ALL VSOCK CONNECT LIFECYCLE STRESS TESTS PASSED         ║");
    println!("╚═══════════════════════════════════════════════════════════════╝");

    Ok(())
}
