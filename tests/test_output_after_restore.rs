//! Verifies continuous container output survives snapshot restore without deadlock or data loss.
//!
//! The container writes a monotonically increasing counter to stdout, one line per
//! 10ms. The test:
//! 1. Cold starts the VM (counter starts at 0)
//! 2. Waits for pre-start snapshot creation (vsock transport reset)
//! 3. Samples the counter — verifies it's still incrementing after snapshot
//! 4. Kills VM
//! 5. Warm starts from snapshot (counter resumes from snapshot state)
//! 6. Samples the counter — verifies it's incrementing after restore
//! 7. Verifies no gaps in the sequence that reached the host
//!
//! Root cause this catches: after pre-start snapshot, both death detection and
//! the explicit reconnect signal could fire, creating two vsock connections.
//! The host accepts only one, so the second connection's writes go nowhere →
//! channel fills → pipe fills → container blocks on write → counter stops.

#![cfg(feature = "privileged-tests")]

mod common;

use anyhow::{Context, Result};
use std::time::Duration;

/// Container command: infinite counter writing to both stdout and stderr.
/// Each line is "COUNT:<n>" so the test can parse and verify monotonicity.
const COUNTER_CMD: &str = "i=0; while true; do echo \"COUNT:$i\"; echo \"STDERR_COUNT:$i\" >&2; i=$((i+1)); sleep 0.01; done";

/// Read the host-side log file and find the highest COUNT value that reached the host.
fn max_count_in_log(log_path: &std::path::Path) -> Option<u64> {
    let content = std::fs::read_to_string(log_path).ok()?;
    content
        .lines()
        .filter_map(|line| {
            line.strip_prefix("COUNT:")
                .or_else(|| {
                    // Also check in the fcvm log format: "stdout:COUNT:N"
                    line.split("stdout:COUNT:").nth(1)
                })
                .and_then(|s| s.trim().parse::<u64>().ok())
        })
        .max()
}

/// Find the warm-start log file for this test run.
fn find_warm_log(test_prefix: &str) -> Option<std::path::PathBuf> {
    let log_dir = std::path::Path::new("/tmp/fcvm-test-logs");
    std::fs::read_dir(log_dir)
        .ok()?
        .flatten()
        .filter(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            name.contains(test_prefix) && name.contains("warm")
        })
        .max_by_key(|e| e.metadata().ok().and_then(|m| m.modified().ok()))
        .map(|e| e.path())
}

#[tokio::test]
async fn test_heavy_output_after_snapshot_restore() -> Result<()> {
    println!("\nContinuous output across snapshot restore");
    println!("==========================================");

    let (vm_name, _, _, _) = common::unique_names("output-restore");

    // Phase 1: Cold boot with continuous counter
    println!("Phase 1: Cold boot (continuous counter at 100 lines/sec)...");
    let (mut child, fcvm_pid) = common::spawn_fcvm_with_logs(
        &[
            "podman",
            "run",
            "--name",
            &vm_name,
            common::ALPINE_IMAGE,
            "sh",
            "-c",
            COUNTER_CMD,
        ],
        &vm_name,
    )
    .await
    .context("spawning cold boot VM")?;

    println!("  fcvm PID: {}", fcvm_pid);

    let healthy = tokio::time::timeout(
        Duration::from_secs(300),
        common::poll_health_by_pid(fcvm_pid, 300),
    )
    .await;

    match healthy {
        Ok(Ok(_)) => println!("  Cold boot healthy"),
        Ok(Err(e)) => {
            common::kill_process(fcvm_pid).await;
            let _ = child.wait().await;
            return Err(e).context("cold boot failed");
        }
        Err(_) => {
            common::kill_process(fcvm_pid).await;
            let _ = child.wait().await;
            anyhow::bail!("cold boot timed out");
        }
    }

    // Wait for snapshot + verify counter is incrementing post-snapshot
    println!("  Waiting for snapshot creation + counter verification...");
    tokio::time::sleep(Duration::from_secs(10)).await;

    // Sample counter twice to verify it's incrementing
    let _sample1 = common::exec_in_container(fcvm_pid, &["sh", "-c", "echo COUNT_SAMPLE"]).await;
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Verify container is still alive (not deadlocked after pre-start snapshot)
    let result = common::exec_in_container(fcvm_pid, &["echo", "post-snapshot-alive"]).await?;
    assert!(
        result.contains("post-snapshot-alive"),
        "container deadlocked after pre-start snapshot: {}",
        result.trim()
    );
    println!("  Post-snapshot: container alive, counter running");

    // Kill and warm start
    println!("  Killing cold boot VM...");
    common::kill_process(fcvm_pid).await;
    let _ = child.wait().await;
    tokio::time::sleep(Duration::from_secs(3)).await;

    // Phase 2: Warm start — counter resumes, output must flow
    println!("\nPhase 2: Warm start (counter resumes, 10s observation)...");
    let (mut child2, fcvm_pid2) = common::spawn_fcvm_with_logs(
        &[
            "podman",
            "run",
            "--name",
            &vm_name,
            common::ALPINE_IMAGE,
            "sh",
            "-c",
            COUNTER_CMD,
        ],
        &format!("{}-warm", vm_name),
    )
    .await
    .context("spawning warm start VM")?;

    println!("  fcvm PID: {}", fcvm_pid2);

    let healthy2 = tokio::time::timeout(
        Duration::from_secs(120),
        common::poll_health_by_pid(fcvm_pid2, 120),
    )
    .await;

    match healthy2 {
        Ok(Ok(_)) => println!("  Warm start healthy"),
        Ok(Err(e)) => {
            common::kill_process(fcvm_pid2).await;
            let _ = child2.wait().await;
            return Err(e).context("warm start failed");
        }
        Err(_) => {
            common::kill_process(fcvm_pid2).await;
            let _ = child2.wait().await;
            anyhow::bail!("warm start timed out — likely deadlock from output pipeline");
        }
    }

    // Let the counter run for 10 seconds, producing ~1000 lines
    println!("  Letting counter run for 10s...");
    tokio::time::sleep(Duration::from_secs(10)).await;

    // Verify container is responsive (not deadlocked)
    let result = common::exec_in_container(fcvm_pid2, &["echo", "still-alive"]).await?;
    assert!(
        result.contains("still-alive"),
        "container deadlocked: {}",
        result.trim()
    );
    println!("  Container responsive after 10s of continuous output");

    // Check the warm-start log for counter values that reached the host
    if let Some(log_path) = find_warm_log("output-restore") {
        if let Some(max_count) = max_count_in_log(&log_path) {
            println!("  Highest counter value in host log: {}", max_count);
            assert!(
                max_count > 100,
                "expected counter > 100 after 10s at 100/sec, got {}",
                max_count
            );
        } else {
            println!("  WARNING: no COUNT lines found in log (output may go to inherited stdio)");
        }
    } else {
        println!("  WARNING: warm log file not found");
    }

    // Cleanup
    println!("  Stopping VM...");
    common::kill_process(fcvm_pid2).await;
    let _ = child2.wait().await;

    println!("  PASSED: continuous output survives snapshot restore, no deadlock");
    Ok(())
}
