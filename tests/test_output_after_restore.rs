//! Verifies that a container producing heavy stdout after snapshot restore doesn't deadlock.
//!
//! The output pipeline after restore:
//!   Container stdout (pipe, 64KB) → fc-agent stdout_task → mpsc channel (4096)
//!     → output_writer → vsock → host listener
//!
//! After snapshot restore, fc-agent must reconnect the vsock output connection.
//! If the reconnect is slow or the host accepts a stale connection, the pipeline
//! backs up: channel fills → send_line blocks → pipe fills → entrypoint blocks.
//!
//! This test:
//! 1. Cold boot with a container that sleeps initially
//! 2. Wait for pre-start snapshot
//! 3. Kill VM
//! 4. Warm start from snapshot
//! 5. Container immediately produces 50K lines of output
//! 6. Verify container is still responsive (not deadlocked)

#![cfg(feature = "privileged-tests")]

mod common;

use anyhow::{Context, Result};
use std::time::Duration;

#[tokio::test]
async fn test_heavy_output_after_snapshot_restore() -> Result<()> {
    println!("\nHeavy output after snapshot restore");
    println!("===================================");

    let (vm_name, _, _, _) = common::unique_names("output-restore");

    // Container: on first boot sleeps (for snapshot), on subsequent boots floods stdout.
    // We detect "subsequent boot" by checking if /tmp/booted exists (set on first boot).
    // After snapshot restore, /tmp/booted exists, so it floods.
    let cmd = concat!(
        "if [ -f /tmp/booted ]; then ",
        "  i=0; while [ $i -lt 50000 ]; do echo \"restored-line-$i\"; i=$((i+1)); done; ",
        "  echo FLOOD_DONE; ",
        "fi; ",
        "touch /tmp/booted; ",
        "sleep 300"
    );

    // Phase 1: Cold boot — creates pre-start snapshot
    println!("Phase 1: Cold boot...");
    let (mut child, fcvm_pid) = common::spawn_fcvm_with_logs(
        &[
            "podman",
            "run",
            "--name",
            &vm_name,
            common::ALPINE_IMAGE,
            "sh",
            "-c",
            cmd,
        ],
        &vm_name,
    )
    .await
    .context("spawning cold boot VM")?;

    println!("  fcvm PID: {}", fcvm_pid);
    println!("  Waiting for healthy...");

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

    // Wait for pre-start snapshot creation
    tokio::time::sleep(Duration::from_secs(10)).await;

    // Kill cold boot VM
    println!("  Killing cold boot VM...");
    common::kill_process(fcvm_pid).await;
    let _ = child.wait().await;
    tokio::time::sleep(Duration::from_secs(3)).await;

    // Phase 2: Warm start — container immediately floods stdout
    println!("\nPhase 2: Warm start (50K lines of output)...");
    let (mut child2, fcvm_pid2) = common::spawn_fcvm_with_logs(
        &[
            "podman",
            "run",
            "--name",
            &vm_name,
            common::ALPINE_IMAGE,
            "sh",
            "-c",
            cmd,
        ],
        &format!("{}-warm", vm_name),
    )
    .await
    .context("spawning warm start VM")?;

    println!("  fcvm PID: {}", fcvm_pid2);
    println!("  Waiting for healthy...");

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
            anyhow::bail!("warm start timed out — likely output pipe deadlock");
        }
    }

    // Wait for the flood to finish
    println!("  Waiting for output flood to complete...");
    tokio::time::sleep(Duration::from_secs(15)).await;

    // Verify container is still responsive (not deadlocked on pipe write)
    println!("  Checking container responsiveness...");
    let result = common::exec_in_container(fcvm_pid2, &["echo", "after-flood-ok"]).await?;
    println!("  Container response: {}", result.trim());
    assert!(
        result.contains("after-flood-ok"),
        "container should respond after 50K-line flood post-restore, got: {}",
        result.trim()
    );

    // Cleanup
    println!("  Stopping VM...");
    common::kill_process(fcvm_pid2).await;
    let _ = child2.wait().await;

    println!("  PASSED: heavy output after snapshot restore didn't deadlock");
    Ok(())
}
