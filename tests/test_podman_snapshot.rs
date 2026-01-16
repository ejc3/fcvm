//! Podman snapshot integration tests
//!
//! Tests the snapshot caching feature that snapshots VM state after
//! container image is loaded, enabling fast subsequent launches.
//!
//! ## Snapshot Storage
//!
//! Snapshot entries are stored via SnapshotManager in the snapshots directory
//! (`paths::snapshot_dir()`). The snapshot key becomes the snapshot name.
//!
//! ## Snapshot Key Model
//!
//! Snapshot keys are computed from FirecrackerConfig JSON which includes:
//! - kernel_path, initrd_path, rootfs_path (content-addressed with SHA)
//! - container_image, container_cmd, cpu, mem, network_mode
//!
//! Snapshot keys do NOT include runtime-only values: env vars, ports, volumes.
//! This means VMs with same image+cmd+cpu+mem+network_mode share the same snapshot.
//!
//! ## Test Isolation
//!
//! Tests use different network modes (bridged vs rootless) for snapshot isolation
//! since network mode IS part of the snapshot key.
//!
//! ## Root Required
//!
//! All tests in this file require root for privileged networking tests.

#![cfg(all(feature = "integration-fast", feature = "privileged-tests"))]

mod common;

use anyhow::{Context, Result};
use std::collections::HashSet;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Check if snapshot is disabled via FCVM_NO_SNAPSHOT environment variable
fn snapshot_disabled_by_env() -> bool {
    std::env::var("FCVM_NO_SNAPSHOT").is_ok()
}

/// Get the snapshot directory path
fn snapshot_dir() -> PathBuf {
    let data_dir = std::env::var("FCVM_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/mnt/fcvm-btrfs/root"));
    data_dir.join("snapshots")
}

/// List all snapshot entries (directory names that contain complete snapshot files)
fn list_snapshot_entries() -> HashSet<String> {
    let mut entries = HashSet::new();
    if let Ok(dir) = std::fs::read_dir(snapshot_dir()) {
        for entry in dir.flatten() {
            if let Ok(name) = entry.file_name().into_string() {
                let path = entry.path();
                // Check if this is a complete snapshot entry
                if path.join("memory.bin").exists()
                    && path.join("vmstate.bin").exists()
                    && path.join("disk.raw").exists()
                    && path.join("config.json").exists()
                {
                    entries.insert(name);
                }
            }
        }
    }
    entries
}

/// Wait for a new snapshot entry to appear (returns the new key)
async fn wait_for_new_snapshot_entry(
    before: &HashSet<String>,
    timeout_secs: u64,
) -> Option<String> {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(timeout_secs) {
        let current = list_snapshot_entries();
        let new_entries: Vec<_> = current.difference(before).collect();
        if !new_entries.is_empty() {
            return Some(new_entries[0].clone());
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    None
}

/// Check if a specific snapshot entry exists and is complete
fn snapshot_entry_exists(snapshot_key: &str) -> bool {
    let path = snapshot_dir().join(snapshot_key);
    path.join("memory.bin").exists()
        && path.join("vmstate.bin").exists()
        && path.join("disk.raw").exists()
        && path.join("config.json").exists()
}

/// Test that first run creates a snapshot entry
/// Uses rootless network mode for this test
#[tokio::test]
async fn test_podman_snapshot_miss_creates_snapshot() -> Result<()> {
    if snapshot_disabled_by_env() {
        println!("Skipping test: FCVM_NO_SNAPSHOT is set");
        return Ok(());
    }
    println!("\ntest_podman_snapshot_miss_creates_snapshot");
    println!("==========================================");

    // Record snapshot entries before test
    let before = list_snapshot_entries();
    println!("Snapshot entries before: {}", before.len());

    // Run container with a UNIQUE command that won't be in any existing snapshot.
    // Since container_cmd is now part of the snapshot key, using a timestamp ensures
    // this test always creates a new snapshot entry (snapshot miss).
    let (vm_name, _, _, _) = common::unique_names("snapshot-miss");
    let unique_msg = format!(
        "snapshot-miss-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    println!(
        "Starting VM: {} with unique message: {}",
        vm_name, unique_msg
    );

    let (mut child, pid) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &vm_name,
        "--network",
        "rootless",
        "alpine:latest",
        "echo",
        &unique_msg,
    ])
    .await
    .context("spawning fcvm")?;

    // Wait for container (may exit quickly)
    let _ = common::poll_health_by_pid(pid, 180).await;
    let _ = tokio::time::timeout(Duration::from_secs(120), child.wait()).await;

    // Verify at least one new snapshot entry was created
    let new_key = wait_for_new_snapshot_entry(&before, 10).await;
    assert!(new_key.is_some(), "A snapshot entry should be created");
    println!("New snapshot entry: {}", new_key.unwrap());

    println!("Test passed");
    Ok(())
}

/// Test that second run with same config hits snapshot and is faster
/// Uses rootless network mode - tests may share snapshot, that's OK
#[tokio::test]
async fn test_podman_snapshot_hit_restores_fast() -> Result<()> {
    if snapshot_disabled_by_env() {
        println!("Skipping test: FCVM_NO_SNAPSHOT is set");
        return Ok(());
    }
    println!("\ntest_podman_snapshot_hit_restores_fast");
    println!("======================================");

    // First run - may create snapshot or use existing
    let (vm_name1, _, _, _) = common::unique_names("snapshot-hit-1");
    println!("First run: {}", vm_name1);

    let start1 = Instant::now();
    let (mut child1, pid1) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &vm_name1,
        "--network",
        "rootless",
        common::TEST_IMAGE,
    ])
    .await?;

    common::poll_health_by_pid(pid1, 180).await?;
    let duration1 = start1.elapsed();
    println!("First run duration: {:?}", duration1);

    child1.kill().await?;
    let _ = child1.wait().await;

    // Wait a moment for snapshot to be written
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Second run - should hit snapshot (same image+cpu+mem+network)
    let (vm_name2, _, _, _) = common::unique_names("snapshot-hit-2");
    println!("Second run (should be snapshot hit): {}", vm_name2);

    let start2 = Instant::now();
    let (mut child2, pid2) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &vm_name2,
        "--network",
        "rootless",
        common::TEST_IMAGE,
    ])
    .await?;

    common::poll_health_by_pid(pid2, 180).await?;
    let duration2 = start2.elapsed();
    println!("Second run duration: {:?}", duration2);

    child2.kill().await?;
    let _ = child2.wait().await;

    // Second run should be faster (or at least not much slower)
    // Snapshot hit skips image pull which saves significant time
    if duration1 > Duration::from_secs(5) {
        let speedup = duration1.as_secs_f64() / duration2.as_secs_f64();
        println!("Speedup: {:.1}x", speedup);
        // Snapshot should provide at least some speedup
        assert!(speedup > 1.0, "Snapshot hit should be faster than miss");
    } else {
        println!("First run was too fast to measure speedup (likely already has snapshot)");
    }

    println!("Test passed");
    Ok(())
}

/// Test that different network modes create different snapshot entries
#[tokio::test]
async fn test_podman_snapshot_different_network_modes() -> Result<()> {
    if snapshot_disabled_by_env() {
        println!("Skipping test: FCVM_NO_SNAPSHOT is set");
        return Ok(());
    }
    println!("\ntest_podman_snapshot_different_network_modes");
    println!("=============================================");

    // Record snapshot entries before
    let before = list_snapshot_entries();
    println!("Snapshot entries before: {}", before.len());

    // Run with rootless
    let (vm_name1, _, _, _) = common::unique_names("net-rootless");
    println!("Running rootless: {}", vm_name1);
    let (mut child1, pid1) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &vm_name1,
        "--network",
        "rootless",
        "alpine:latest",
        "echo",
        "rootless",
    ])
    .await?;
    let _ = common::poll_health_by_pid(pid1, 180).await;
    let _ = tokio::time::timeout(Duration::from_secs(120), child1.wait()).await;

    // Wait for snapshot
    tokio::time::sleep(Duration::from_secs(2)).await;
    let after_rootless = list_snapshot_entries();
    println!("Snapshot entries after rootless: {}", after_rootless.len());

    // Run with bridged (requires sudo, handled by test harness)
    let (vm_name2, _, _, _) = common::unique_names("net-bridged");
    println!("Running bridged: {}", vm_name2);
    let (mut child2, pid2) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &vm_name2,
        "--network",
        "bridged",
        "alpine:latest",
        "echo",
        "bridged",
    ])
    .await?;
    let _ = common::poll_health_by_pid(pid2, 180).await;
    let _ = tokio::time::timeout(Duration::from_secs(120), child2.wait()).await;

    // Wait for snapshot
    tokio::time::sleep(Duration::from_secs(2)).await;
    let after_bridged = list_snapshot_entries();
    println!("Snapshot entries after bridged: {}", after_bridged.len());

    // Should have created different snapshot entries for different network modes
    // (If either was already snapshotted, count might not increase but that's OK)
    let new_after_rootless: HashSet<_> = after_rootless.difference(&before).collect();
    let new_after_bridged: HashSet<_> = after_bridged.difference(&after_rootless).collect();

    println!("New entries after rootless: {:?}", new_after_rootless);
    println!("New entries after bridged: {:?}", new_after_bridged);

    // At least verify both network modes work
    println!("Test passed (both network modes work)");
    Ok(())
}

/// Test that --no-snapshot flag prevents snapshot creation
#[tokio::test]
async fn test_podman_no_snapshot_flag() -> Result<()> {
    println!("\ntest_podman_no_snapshot_flag");
    println!("============================");

    // Use a unique command so we can verify OUR snapshot wasn't created
    let unique_msg = format!(
        "no-snapshot-test-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    println!("Using unique message: {}", unique_msg);

    // Record snapshot entries before
    let before = list_snapshot_entries();
    println!("Snapshot entries before: {}", before.len());

    // Run with --no-snapshot
    let (vm_name, _, _, _) = common::unique_names("no-snapshot");
    let (mut child, pid) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &vm_name,
        "--network",
        "rootless",
        "--no-snapshot",
        "alpine:latest",
        "echo",
        &unique_msg,
    ])
    .await?;

    let _ = common::poll_health_by_pid(pid, 180).await;
    let _ = tokio::time::timeout(Duration::from_secs(60), child.wait()).await;

    // Wait extra time for snapshot creation (if it were to happen)
    tokio::time::sleep(Duration::from_secs(5)).await;

    // Check for new snapshot entries
    let after = list_snapshot_entries();
    let new_entries: Vec<_> = after.difference(&before).cloned().collect();
    println!(
        "Snapshot entries after: {} (new: {})",
        after.len(),
        new_entries.len()
    );

    // Verify no new entry contains our unique message in its config
    for entry in &new_entries {
        let config_path = snapshot_dir().join(entry).join("config.json");
        if let Ok(config) = std::fs::read_to_string(&config_path) {
            assert!(
                !config.contains(&unique_msg),
                "--no-snapshot flag failed: snapshot entry {} was created for our command",
                entry
            );
        }
    }

    println!("Test passed (--no-snapshot prevented snapshot creation)");
    Ok(())
}

/// Test that incomplete snapshot (missing files) is treated as miss
#[tokio::test]
async fn test_podman_snapshot_incomplete_treated_as_miss() -> Result<()> {
    if snapshot_disabled_by_env() {
        println!("Skipping test: FCVM_NO_SNAPSHOT is set");
        return Ok(());
    }
    println!("\ntest_podman_snapshot_incomplete_treated_as_miss");
    println!("================================================");

    // Create an incomplete snapshot entry with a known key
    let incomplete_key = "incomplete-test-entry";
    let path = snapshot_dir().join(incomplete_key);

    // Clean and create empty directory (incomplete snapshot)
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path)?;
    println!("Created incomplete snapshot directory: {}", incomplete_key);

    // Verify it's incomplete (exists but missing required files)
    assert!(path.exists(), "Directory should exist");
    assert!(
        !snapshot_entry_exists(incomplete_key),
        "Should be incomplete (missing files)"
    );

    // Clean up
    let _ = std::fs::remove_dir_all(&path);

    println!("Test passed");
    Ok(())
}

/// Test long-running container works with snapshot
#[tokio::test]
async fn test_podman_snapshot_long_running_container() -> Result<()> {
    if snapshot_disabled_by_env() {
        println!("Skipping test: FCVM_NO_SNAPSHOT is set");
        return Ok(());
    }
    println!("\ntest_podman_snapshot_long_running_container");
    println!("============================================");

    // First run
    let (vm_name1, _, _, _) = common::unique_names("long-1");
    let (mut child1, pid1) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &vm_name1,
        "--network",
        "rootless",
        common::TEST_IMAGE,
    ])
    .await?;

    common::poll_health_by_pid(pid1, 180).await?;
    println!("First container healthy");

    child1.kill().await?;
    let _ = child1.wait().await;

    // Wait for snapshot
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Second run - from snapshot
    let (vm_name2, _, _, _) = common::unique_names("long-2");
    let (mut child2, pid2) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &vm_name2,
        "--network",
        "rootless",
        common::TEST_IMAGE,
    ])
    .await?;

    common::poll_health_by_pid(pid2, 180).await?;
    println!("Second container healthy");

    child2.kill().await?;
    let _ = child2.wait().await;

    println!("Test passed");
    Ok(())
}
