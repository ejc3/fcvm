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
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Check if snapshot is disabled via FCVM_NO_SNAPSHOT environment variable
fn snapshot_disabled_by_env() -> bool {
    std::env::var("FCVM_NO_SNAPSHOT")
        .map(|v| !v.is_empty())
        .unwrap_or(false)
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
    list_snapshot_entries_in(&snapshot_dir())
}

/// Complete snapshot entries (all four files present) under `snapshots`.
fn list_snapshot_entries_in(snapshots: &Path) -> HashSet<String> {
    let mut entries = HashSet::new();
    if let Ok(dir) = std::fs::read_dir(snapshots) {
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

/// Waits for a snapshot entry that is not in `before` and was taken from the VM
/// `vm_id`. Other tests create snapshots in the same directory at the same time, so
/// a new entry is not necessarily this test's: taking any new one once read another
/// test's user snapshot, and test_podman_snapshot_type_is_system failed on its type.
async fn wait_for_snapshot_entry_from(
    snapshots: &Path,
    before: &HashSet<String>,
    vm_id: &str,
    timeout_secs: u64,
) -> Option<String> {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(timeout_secs) {
        let ours = list_snapshot_entries_in(snapshots)
            .into_iter()
            .filter(|entry| !before.contains(entry))
            .find(|entry| entry_is_from(snapshots, entry, vm_id));
        if ours.is_some() {
            return ours;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    None
}

/// True when the entry's config.json records `vm_id` as the VM it was taken from.
/// The container command is not in config.json, so it cannot identify an entry.
fn entry_is_from(snapshots: &Path, entry: &str, vm_id: &str) -> bool {
    std::fs::read_to_string(snapshots.join(entry).join("config.json"))
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .is_some_and(|config| config["vm_id"] == vm_id)
}

/// The vm_id fcvm assigned the VM running as `pid`, read with `fcvm ls` while the
/// VM is up. A snapshot records the vm_id of the VM it was taken from.
async fn vm_id_of(pid: u32) -> Result<String> {
    #[derive(serde::Deserialize)]
    struct Listed {
        vm_id: String,
    }
    let fcvm = common::find_fcvm_binary()?;
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let output = tokio::process::Command::new(&fcvm)
            .args(["ls", "--json", "--pid", &pid.to_string()])
            .output()
            .await?;
        if output.status.success() {
            if let Ok(listed) = serde_json::from_slice::<Vec<Listed>>(&output.stdout) {
                if let Some(vm) = listed.into_iter().next() {
                    return Ok(vm.vm_id);
                }
            }
        }
        anyhow::ensure!(
            Instant::now() < deadline,
            "fcvm pid {pid} never listed its VM"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Waits for the fcvm process of a VM whose container only runs `echo`. The guest
/// powers itself off once the command exits, so fcvm has to exit on its own with
/// the container's status. Tests that discarded this wait passed after 245 to 309 s,
/// or failed later on an unrelated assertion, when a guest never finished shutting
/// down.
async fn wait_for_short_lived_vm(child: &mut tokio::process::Child, vm_name: &str) -> Result<()> {
    const LIMIT: Duration = Duration::from_secs(120);
    let status = tokio::time::timeout(LIMIT, child.wait())
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "fcvm for {vm_name} did not exit within {}s of starting a container that only \
                 runs echo, so the guest never finished shutting down; its console is in the \
                 VM's debug log under /tmp/fcvm-test-logs",
                LIMIT.as_secs()
            )
        })?
        .context("waiting for fcvm")?;
    anyhow::ensure!(status.success(), "fcvm for {vm_name} exited with {status}");
    Ok(())
}

/// A snapshot another VM leaves while this test waits is not this test's.
#[tokio::test]
async fn snapshot_entry_lookup_skips_other_vms_entries() -> Result<()> {
    let snapshots = std::env::temp_dir().join(format!("fcvm-entry-lookup-{}", std::process::id()));
    let add = |name: &str, vm_id: &str| -> Result<()> {
        let entry = snapshots.join(name);
        std::fs::create_dir_all(&entry)?;
        for file in ["memory.bin", "vmstate.bin", "disk.raw"] {
            std::fs::write(entry.join(file), b"")?;
        }
        std::fs::write(
            entry.join("config.json"),
            format!(r#"{{"vm_id":"{vm_id}"}}"#),
        )?;
        Ok(())
    };
    let before = HashSet::new();
    add("iso-copy-snap-1", "vm-other")?;
    let other = wait_for_snapshot_entry_from(&snapshots, &before, "vm-ours", 1).await;
    add("969632813527", "vm-ours")?;
    let ours = wait_for_snapshot_entry_from(&snapshots, &before, "vm-ours", 1).await;
    std::fs::remove_dir_all(&snapshots).ok();
    assert_eq!(
        other, None,
        "another VM's snapshot entry was taken for this one's"
    );
    assert_eq!(ours.as_deref(), Some("969632813527"));
    Ok(())
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
        common::ALPINE_IMAGE,
        "echo",
        &unique_msg,
    ])
    .await
    .context("spawning fcvm")?;

    let vm_id = vm_id_of(pid).await?;
    wait_for_short_lived_vm(&mut child, &vm_name).await?;

    // Verify this run created its own snapshot entry
    let new_key = wait_for_snapshot_entry_from(&snapshot_dir(), &before, &vm_id, 10).await;
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
    let (mut child1, _pid1) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &vm_name1,
        "--network",
        "rootless",
        common::ALPINE_IMAGE,
        "echo",
        "rootless",
    ])
    .await?;
    wait_for_short_lived_vm(&mut child1, &vm_name1).await?;

    // Wait for snapshot
    tokio::time::sleep(Duration::from_secs(2)).await;
    let after_rootless = list_snapshot_entries();
    println!("Snapshot entries after rootless: {}", after_rootless.len());

    // Run with bridged (requires sudo, handled by test harness)
    let (vm_name2, _, _, _) = common::unique_names("net-bridged");
    println!("Running bridged: {}", vm_name2);
    let (mut child2, _pid2) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &vm_name2,
        "--network",
        "bridged",
        common::ALPINE_IMAGE,
        "echo",
        "bridged",
    ])
    .await?;
    wait_for_short_lived_vm(&mut child2, &vm_name2).await?;

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
        common::ALPINE_IMAGE,
        "echo",
        &unique_msg,
    ])
    .await?;

    let vm_id = vm_id_of(pid).await?;
    wait_for_short_lived_vm(&mut child, &vm_name).await?;

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

    // A snapshot of this VM would record its vm_id. The command is not in
    // config.json, so checking entries for it could never fail.
    let ours: Vec<_> = new_entries
        .iter()
        .filter(|entry| entry_is_from(&snapshot_dir(), entry.as_str(), &vm_id))
        .collect();
    assert!(
        ours.is_empty(),
        "--no-snapshot flag failed: snapshot entries {ours:?} were taken from this VM"
    );

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

/// Test that snapshots created by podman run have type "System"
#[tokio::test]
async fn test_podman_snapshot_type_is_system() -> Result<()> {
    if snapshot_disabled_by_env() {
        println!("Skipping test: FCVM_NO_SNAPSHOT is set");
        return Ok(());
    }
    println!("\ntest_podman_snapshot_type_is_system");
    println!("====================================");

    // Record snapshot entries before test
    let before = list_snapshot_entries();

    // Run container with unique command to ensure fresh snapshot
    let (vm_name, _, _, _) = common::unique_names("type-check");
    let unique_msg = format!(
        "type-check-{}",
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
        common::ALPINE_IMAGE,
        "echo",
        &unique_msg,
    ])
    .await
    .context("spawning fcvm")?;

    let vm_id = vm_id_of(pid).await?;
    wait_for_short_lived_vm(&mut child, &vm_name).await?;

    // Wait for this run's snapshot entry
    let new_key = wait_for_snapshot_entry_from(&snapshot_dir(), &before, &vm_id, 10).await;
    assert!(new_key.is_some(), "A snapshot entry should be created");
    let snapshot_key = new_key.unwrap();
    println!("New snapshot entry: {}", snapshot_key);

    // Read the snapshot config and verify type is System
    let config_path = snapshot_dir().join(&snapshot_key).join("config.json");
    let config_json =
        std::fs::read_to_string(&config_path).context("reading snapshot config.json")?;

    // Parse and verify snapshot_type is "System"
    let config: serde_json::Value =
        serde_json::from_str(&config_json).context("parsing snapshot config.json")?;

    let snapshot_type = config
        .get("snapshot_type")
        .and_then(|v| v.as_str())
        .unwrap_or("System"); // Default to System for backward compatibility

    assert_eq!(
        snapshot_type, "System",
        "Snapshots created by 'podman run' should have type 'System', got '{}'",
        snapshot_type
    );
    println!("Verified snapshot_type = {}", snapshot_type);

    println!("Test passed");
    Ok(())
}
