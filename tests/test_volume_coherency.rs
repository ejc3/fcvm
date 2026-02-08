//! Volume coherency integration tests
//!
//! Verifies that read-only FUSE volume mounts work correctly:
//! 1. Baseline VM: host writes file, container reads it
//! 2. Clone VM: after snapshot/restore, host writes file, clone reads it

#![cfg(feature = "integration-slow")]

mod common;

use anyhow::{Context, Result};
use std::time::Duration;

/// Test read-only volume coherency on a baseline VM.
///
/// Host writes a file, container reads it through FUSE mount.
#[tokio::test]
async fn test_volume_coherency_baseline() -> Result<()> {
    let id = std::process::id();
    let vm_name = format!("vol-baseline-{}", id);
    let host_dir = format!("/tmp/fcvm-vol-test-{}", id);

    tokio::fs::create_dir_all(&host_dir).await?;

    // Start VM with read-only volume mount
    let map_arg = format!("{}:/mnt/test:ro", host_dir);
    let (_child, pid) = common::spawn_fcvm_with_logs(
        &[
            "podman",
            "run",
            "--name",
            &vm_name,
            "--network",
            "rootless",
            "--map",
            &map_arg,
            common::TEST_IMAGE,
        ],
        &vm_name,
    )
    .await
    .context("spawning VM")?;

    println!("Waiting for VM to become healthy...");
    common::poll_health_by_pid(pid, 180).await?;
    println!("VM healthy (PID: {})", pid);

    // Write a file on host
    let test_content = format!("coherency-test-{}", id);
    tokio::fs::write(format!("{}/test.txt", host_dir), &test_content).await?;
    println!("Wrote test.txt on host: {}", test_content);

    // Read from container with retry (up to 1s for cache coherency)
    let mut last_err = None;
    for attempt in 0..10 {
        match common::exec_in_container(pid, &["cat", "/mnt/test/test.txt"]).await {
            Ok(output) => {
                let trimmed = output.trim().to_string();
                if trimmed == test_content {
                    println!("Container read matches host write (attempt {})", attempt);
                    last_err = None;
                    break;
                } else {
                    last_err = Some(format!(
                        "content mismatch: expected '{}', got '{}'",
                        test_content, trimmed
                    ));
                }
            }
            Err(e) => {
                last_err = Some(format!("exec failed: {}", e));
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // Cleanup
    common::kill_process(pid).await;
    let _ = tokio::fs::remove_dir_all(&host_dir).await;

    if let Some(err) = last_err {
        anyhow::bail!("Volume coherency failed: {}", err);
    }

    println!("VOLUME COHERENCY BASELINE TEST PASSED!");
    Ok(())
}

/// Test read-only volume coherency after snapshot/clone.
///
/// 1. Start VM with volume mount
/// 2. Snapshot it
/// 3. Clone from snapshot (volumes reconstituted from snapshot metadata)
/// 4. Host writes new file, clone reads it
#[tokio::test]
async fn test_volume_coherency_clone() -> Result<()> {
    let id = std::process::id();
    let vm_name = format!("vol-clone-{}", id);
    let clone_name = format!("vol-clone-c-{}", id);
    let snapshot_name = format!("vol-snap-{}", id);
    let host_dir = format!("/tmp/fcvm-vol-clone-{}", id);

    tokio::fs::create_dir_all(&host_dir).await?;

    // Step 1: Start VM with read-only volume mount
    println!("Step 1: Starting baseline VM with volume...");
    let map_arg = format!("{}:/mnt/test:ro", host_dir);
    let (_child, baseline_pid) = common::spawn_fcvm_with_logs(
        &[
            "podman",
            "run",
            "--name",
            &vm_name,
            "--network",
            "rootless",
            "--map",
            &map_arg,
            common::TEST_IMAGE,
        ],
        &vm_name,
    )
    .await
    .context("spawning baseline VM")?;

    common::poll_health_by_pid(baseline_pid, 180).await?;
    println!("  Baseline VM healthy (PID: {})", baseline_pid);

    // Step 2: Create snapshot
    println!("Step 2: Creating snapshot...");
    common::create_snapshot_by_pid(baseline_pid, &snapshot_name).await?;
    println!("  Snapshot created");

    // Step 3: Start memory server
    println!("Step 3: Starting memory server...");
    let (_serve_child, serve_pid) = common::start_memory_server(&snapshot_name).await?;
    println!("  Memory server ready (PID: {})", serve_pid);

    // Step 4: Spawn clone (volumes reconstituted from snapshot metadata)
    println!("Step 4: Spawning clone...");
    let (_clone_child, clone_pid) = common::spawn_clone(serve_pid, &clone_name, "rootless").await?;

    common::poll_health_by_pid(clone_pid, 120).await?;
    println!("  Clone healthy (PID: {})", clone_pid);

    // Step 5: Write a NEW file on host (after clone started)
    let test_content = format!("clone-coherency-{}", id);
    tokio::fs::write(format!("{}/clone_test.txt", host_dir), &test_content).await?;
    println!("Step 5: Wrote clone_test.txt on host: {}", test_content);

    // Step 6: Read from clone container with retry
    // FUSE remount after clone restore takes time: watcher polls every 100ms,
    // then remount involves unmount + vsock reconnect + mount. Allow up to 5s.
    println!("Step 6: Reading from clone...");
    let mut last_err = None;
    for attempt in 0..25 {
        match common::exec_in_container(clone_pid, &["cat", "/mnt/test/clone_test.txt"]).await {
            Ok(output) => {
                let trimmed = output.trim().to_string();
                if trimmed == test_content {
                    println!("  Clone read matches host write (attempt {})", attempt);
                    last_err = None;
                    break;
                } else {
                    last_err = Some(format!(
                        "content mismatch: expected '{}', got '{}'",
                        test_content, trimmed
                    ));
                }
            }
            Err(e) => {
                last_err = Some(format!("exec failed: {}", e));
            }
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // Cleanup
    println!("Cleaning up...");
    common::kill_process(clone_pid).await;
    common::kill_process(serve_pid).await;
    common::kill_process(baseline_pid).await;
    let _ = tokio::fs::remove_dir_all(&host_dir).await;

    if let Some(err) = last_err {
        anyhow::bail!("Volume coherency after clone failed: {}", err);
    }

    println!("VOLUME COHERENCY CLONE TEST PASSED!");
    Ok(())
}
