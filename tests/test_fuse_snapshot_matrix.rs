//! FUSE + Snapshot matrix integration tests
//!
//! Tests all combinations of mount types through snapshot/restore cycles:
//! - --map (FUSE RW/RO) through snapshot/clone
//! - --map multi-volume through snapshot/clone
//! - --map + --disk combined through snapshot/clone
//! - Multiple clones with FUSE mounts
//! - Large file I/O through clone FUSE
//! - Continuous reads across multiple snapshots (zero failures)
//!
//! All VMs (baseline and clone) use reconnectable FUSE mounts: when a snapshot
//! resets vsock, the multiplexer automatically reconnects and re-sends pending
//! requests. The kernel FUSE session stays alive — no unmount/remount needed.
//! Clones reconnect to their own VolumeServer; stale inode references resolve
//! naturally because FUSE TTL (1s) expires during the clone boot sequence.

#![cfg(feature = "integration-slow")]

mod common;

use anyhow::{Context, Result};
#[cfg(feature = "privileged-tests")]
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Verify a file is readable from the container's FUSE mount, with retries.
/// Initial FUSE mount and clone boot can take several seconds.
/// After snapshot reconnection, reads resume quickly but we retry for safety.
async fn verify_fuse_read(
    pid: u32,
    guest_path: &str,
    filename: &str,
    expected: &str,
    timeout_secs: u64,
) -> Result<()> {
    let full_path = format!("{}/{}", guest_path, filename);
    let attempts = (timeout_secs * 5) as usize; // 200ms intervals
    let mut last_err = None;
    for attempt in 0..attempts {
        match common::exec_in_container(pid, &["cat", &full_path]).await {
            Ok(output) => {
                let trimmed = output.trim().to_string();
                if trimmed.contains(expected) {
                    if attempt > 0 {
                        println!(
                            "  Read verified after {} attempts ({:.1}s)",
                            attempt + 1,
                            (attempt as f64) * 0.2
                        );
                    }
                    return Ok(());
                }
                last_err = Some(format!(
                    "content mismatch: expected '{}' ({} bytes), got '{}' ({} bytes, hex={:02x?})",
                    expected,
                    expected.len(),
                    trimmed,
                    trimmed.len(),
                    trimmed.as_bytes()
                ));
            }
            Err(e) => {
                last_err = Some(format!("exec failed: {}", e));
            }
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    anyhow::bail!(
        "FUSE read of {} failed after {}s: {}",
        full_path,
        timeout_secs,
        last_err.unwrap_or_default()
    )
}

/// Write a file through the container's FUSE mount and verify it, with retries.
async fn verify_fuse_write(
    pid: u32,
    guest_path: &str,
    filename: &str,
    content: &str,
) -> Result<()> {
    let write_cmd = format!(
        "echo '{}' > {}/{} && cat {}/{}",
        content, guest_path, filename, guest_path, filename
    );
    let mut last_err = None;
    for attempt in 0..150 {
        // 30s total at 200ms intervals
        match common::exec_in_container(pid, &[&write_cmd]).await {
            Ok(output) => {
                if output.contains(content) {
                    if attempt > 0 {
                        println!(
                            "  Write verified after {} attempts ({:.1}s)",
                            attempt + 1,
                            (attempt as f64) * 0.2
                        );
                    }
                    return Ok(());
                }
                last_err = Some(format!(
                    "content mismatch: expected '{}', got '{}'",
                    content,
                    output.trim()
                ));
            }
            Err(e) => {
                last_err = Some(format!("exec failed: {}", e));
            }
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    anyhow::bail!(
        "FUSE write of {}/{} failed after 30s: {}",
        guest_path,
        filename,
        last_err.unwrap_or_default()
    )
}

/// Create a small ext4 disk image with a test file.
/// Reuses the pattern from test_extra_disks.rs.
#[cfg(feature = "privileged-tests")]
async fn create_test_disk(path: &Path) -> Result<()> {
    tokio::process::Command::new("truncate")
        .args(["-s", "64M", path.to_str().unwrap()])
        .status()
        .await?;
    tokio::process::Command::new("mkfs.ext4")
        .args(["-q", "-F", path.to_str().unwrap()])
        .status()
        .await?;

    // Mount temporarily, write test file, unmount
    let mount_dir = format!("/tmp/fcvm-disk-{:x}", {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        path.hash(&mut h);
        h.finish()
    });
    tokio::fs::create_dir_all(&mount_dir).await?;
    tokio::process::Command::new("mount")
        .args([path.to_str().unwrap(), &mount_dir])
        .status()
        .await?;
    tokio::fs::write(format!("{}/disk-test.txt", mount_dir), "disk-hello\n").await?;
    tokio::process::Command::new("umount")
        .arg(&mount_dir)
        .status()
        .await?;
    tokio::fs::remove_dir(&mount_dir).await.ok();
    Ok(())
}

// =============================================================================
// Test 1: RW FUSE through full snapshot/clone cycle
// =============================================================================

/// Full snapshot/clone cycle with RW FUSE mount.
///
/// Verifies: baseline write, snapshot, clone read+write, baseline still works after.
/// This is the most critical test for the freeze bug.
#[tokio::test]
async fn test_fuse_snapshot_matrix_rw_clone() -> Result<()> {
    let (vm_name, clone_name, snap_name, _) = common::unique_names("fuse-rw-snap");
    let host_dir = format!("/tmp/fcvm-fuse-rw-snap-{}", std::process::id());
    tokio::fs::create_dir_all(&host_dir).await?;

    println!("=== test_fuse_snapshot_matrix_rw_clone ===");

    // Step 1: Start baseline with RW FUSE mount
    println!("Step 1: Starting baseline VM with RW FUSE mount...");
    let map_arg = format!("{}:/mnt/test", host_dir);
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
    .context("spawning baseline VM")?;

    common::poll_health_by_pid(pid, 180).await?;
    println!("  Baseline healthy (PID: {})", pid);

    // Step 2: Write from container through FUSE (RW)
    println!("Step 2: Verifying baseline FUSE write...");
    verify_fuse_write(pid, "/mnt/test", "baseline.txt", "baseline-data").await?;
    // Verify file exists on host
    let content = tokio::fs::read_to_string(format!("{}/baseline.txt", host_dir)).await?;
    assert!(
        content.contains("baseline-data"),
        "Host file missing baseline write"
    );
    println!("  Baseline FUSE write verified");

    // Step 3: Create snapshot
    println!("Step 3: Creating snapshot...");
    common::create_snapshot_by_pid(pid, &snap_name).await?;
    println!("  Snapshot created");

    // Step 4: Verify baseline FUSE still works AFTER snapshot
    println!("Step 4: Verifying baseline FUSE after snapshot...");
    tokio::fs::write(format!("{}/after-snap.txt", host_dir), "after-snapshot").await?;
    verify_fuse_read(pid, "/mnt/test", "after-snap.txt", "after-snapshot", 30).await?;
    println!("  Baseline FUSE still works after snapshot");

    // Step 5: Start serve + clone
    println!("Step 5: Starting memory server and clone...");
    let (_serve, serve_pid) = common::start_memory_server(&snap_name).await?;
    let (_clone, clone_pid) = common::spawn_clone(serve_pid, &clone_name, "rootless").await?;
    common::poll_health_by_pid(clone_pid, 180).await?;
    println!("  Clone healthy (PID: {})", clone_pid);

    // Step 6: Verify clone can read through FUSE
    println!("Step 6: Verifying clone FUSE read...");
    tokio::fs::write(format!("{}/clone-test.txt", host_dir), "for-clone").await?;
    verify_fuse_read(clone_pid, "/mnt/test", "clone-test.txt", "for-clone", 30).await?;
    println!("  Clone FUSE read verified");

    // Step 7: Verify clone can write through FUSE (RW)
    println!("Step 7: Verifying clone FUSE write...");
    verify_fuse_write(clone_pid, "/mnt/test", "clone-wrote.txt", "clone-data").await?;
    let content = tokio::fs::read_to_string(format!("{}/clone-wrote.txt", host_dir)).await?;
    assert!(
        content.contains("clone-data"),
        "Host file missing clone write"
    );
    println!("  Clone FUSE write verified");

    // Cleanup
    println!("Cleaning up...");
    common::kill_process(clone_pid).await;
    common::kill_process(serve_pid).await;
    common::kill_process(pid).await;
    tokio::fs::remove_dir_all(&host_dir).await.ok();

    println!("PASSED: test_fuse_snapshot_matrix_rw_clone");
    Ok(())
}

// =============================================================================
// Test 2: RO FUSE through snapshot/clone cycle
// =============================================================================

/// RO FUSE mount through snapshot/clone.
///
/// Host writes file after clone starts, clone reads it through FUSE.
#[tokio::test]
async fn test_fuse_snapshot_matrix_ro_clone() -> Result<()> {
    let (vm_name, clone_name, snap_name, _) = common::unique_names("fuse-ro-snap");
    let host_dir = format!("/tmp/fcvm-fuse-ro-snap-{}", std::process::id());
    tokio::fs::create_dir_all(&host_dir).await?;

    println!("=== test_fuse_snapshot_matrix_ro_clone ===");

    // Write initial file
    tokio::fs::write(format!("{}/initial.txt", host_dir), "initial-data").await?;

    // Start baseline with RO FUSE
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
    .context("spawning baseline VM")?;

    common::poll_health_by_pid(pid, 180).await?;
    println!("  Baseline healthy");

    // Verify baseline reads
    verify_fuse_read(pid, "/mnt/test", "initial.txt", "initial-data", 30).await?;
    println!("  Baseline FUSE read verified");

    // Snapshot, serve, clone
    common::create_snapshot_by_pid(pid, &snap_name).await?;
    let (_serve, serve_pid) = common::start_memory_server(&snap_name).await?;
    let (_clone, clone_pid) = common::spawn_clone(serve_pid, &clone_name, "rootless").await?;
    common::poll_health_by_pid(clone_pid, 180).await?;
    println!("  Clone healthy");

    // Write NEW file on host after clone started
    tokio::fs::write(format!("{}/new-after-clone.txt", host_dir), "new-data").await?;

    // Clone should read the new file through FUSE
    verify_fuse_read(
        clone_pid,
        "/mnt/test",
        "new-after-clone.txt",
        "new-data",
        10,
    )
    .await?;
    println!("  Clone reads new host file through FUSE");

    // Cleanup
    common::kill_process(clone_pid).await;
    common::kill_process(serve_pid).await;
    common::kill_process(pid).await;
    tokio::fs::remove_dir_all(&host_dir).await.ok();

    println!("PASSED: test_fuse_snapshot_matrix_ro_clone");
    Ok(())
}

// =============================================================================
// Test 3: Baseline FUSE recovery after snapshot
// =============================================================================

/// Baseline VM's FUSE mount must survive snapshot create.
///
/// The snapshot causes a vsock reset; the reconnectable multiplexer
/// transparently reconnects. The kernel FUSE session is unaffected.
#[tokio::test]
async fn test_fuse_snapshot_matrix_rw_recovery() -> Result<()> {
    let (vm_name, _, snap_name, _) = common::unique_names("fuse-recovery");
    let host_dir = format!("/tmp/fcvm-fuse-recovery-{}", std::process::id());
    tokio::fs::create_dir_all(&host_dir).await?;

    println!("=== test_fuse_snapshot_matrix_rw_recovery ===");

    let map_arg = format!("{}:/mnt/test", host_dir);
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
    .context("spawning baseline VM")?;

    common::poll_health_by_pid(pid, 180).await?;

    // Write BEFORE snapshot
    println!("  Writing before snapshot...");
    verify_fuse_write(pid, "/mnt/test", "pre-snap.txt", "pre-snap-data").await?;

    // Take snapshot (triggers vsock reset + FUSE remount)
    println!("  Creating snapshot (triggers vsock reset)...");
    common::create_snapshot_by_pid(pid, &snap_name).await?;

    // Verify FUSE works AFTER snapshot (the critical assertion)
    // Multiplexer reconnects transparently — no remount needed
    println!("  Verifying FUSE after snapshot...");
    tokio::fs::write(format!("{}/post-snap.txt", host_dir), "post-snap-data").await?;
    verify_fuse_read(pid, "/mnt/test", "post-snap.txt", "post-snap-data", 30).await?;

    // Also verify write works after snapshot
    verify_fuse_write(pid, "/mnt/test", "post-write.txt", "post-write-data").await?;
    let content = tokio::fs::read_to_string(format!("{}/post-write.txt", host_dir)).await?;
    assert!(content.contains("post-write-data"));
    println!("  Baseline FUSE fully recovered after snapshot");

    // Cleanup
    common::kill_process(pid).await;
    tokio::fs::remove_dir_all(&host_dir).await.ok();

    println!("PASSED: test_fuse_snapshot_matrix_rw_recovery");
    Ok(())
}

// =============================================================================
// Test 4: I/O integrity through snapshot
// =============================================================================

/// Write file before snapshot, verify no corruption after.
#[tokio::test]
async fn test_fuse_snapshot_matrix_rw_io() -> Result<()> {
    let (vm_name, _, snap_name, _) = common::unique_names("fuse-io-snap");
    let host_dir = format!("/tmp/fcvm-fuse-io-snap-{}", std::process::id());
    tokio::fs::create_dir_all(&host_dir).await?;

    println!("=== test_fuse_snapshot_matrix_rw_io ===");

    let map_arg = format!("{}:/mnt/test", host_dir);
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
    .context("spawning baseline VM")?;

    common::poll_health_by_pid(pid, 180).await?;

    // Write a known-good file through FUSE
    let test_data = "integrity-check-1234567890-abcdefghijklmnop";
    tokio::fs::write(format!("{}/integrity.txt", host_dir), test_data).await?;
    verify_fuse_read(pid, "/mnt/test", "integrity.txt", test_data, 30).await?;
    println!("  Pre-snapshot data verified");

    // Take snapshot
    common::create_snapshot_by_pid(pid, &snap_name).await?;
    println!("  Snapshot created");

    // Verify data is not corrupted after snapshot
    verify_fuse_read(pid, "/mnt/test", "integrity.txt", test_data, 30).await?;
    println!("  Post-snapshot data integrity verified");

    // Cleanup
    common::kill_process(pid).await;
    tokio::fs::remove_dir_all(&host_dir).await.ok();

    println!("PASSED: test_fuse_snapshot_matrix_rw_io");
    Ok(())
}

// =============================================================================
// Test 5: Multi-volume FUSE through snapshot/clone
// =============================================================================

/// Two FUSE mounts (one RO, one RW) through snapshot/clone.
#[tokio::test]
async fn test_fuse_snapshot_matrix_multi_vol() -> Result<()> {
    let (vm_name, clone_name, snap_name, _) = common::unique_names("fuse-multi");
    let host_dir_a = format!("/tmp/fcvm-fuse-multi-a-{}", std::process::id());
    let host_dir_b = format!("/tmp/fcvm-fuse-multi-b-{}", std::process::id());
    tokio::fs::create_dir_all(&host_dir_a).await?;
    tokio::fs::create_dir_all(&host_dir_b).await?;

    println!("=== test_fuse_snapshot_matrix_multi_vol ===");

    // Pre-populate RO volume
    tokio::fs::write(format!("{}/ro-file.txt", host_dir_a), "read-only-data").await?;

    let map_a = format!("{}:/mnt/vol-ro:ro", host_dir_a);
    let map_b = format!("{}:/mnt/vol-rw", host_dir_b);
    let (_child, pid) = common::spawn_fcvm_with_logs(
        &[
            "podman",
            "run",
            "--name",
            &vm_name,
            "--network",
            "rootless",
            "--map",
            &map_a,
            "--map",
            &map_b,
            common::TEST_IMAGE,
        ],
        &vm_name,
    )
    .await
    .context("spawning baseline VM")?;

    common::poll_health_by_pid(pid, 180).await?;

    // Verify both mounts on baseline
    verify_fuse_read(pid, "/mnt/vol-ro", "ro-file.txt", "read-only-data", 30).await?;
    verify_fuse_write(pid, "/mnt/vol-rw", "rw-file.txt", "rw-data").await?;
    println!("  Baseline: both volumes verified");

    // Snapshot + clone
    common::create_snapshot_by_pid(pid, &snap_name).await?;
    let (_serve, serve_pid) = common::start_memory_server(&snap_name).await?;
    let (_clone, clone_pid) = common::spawn_clone(serve_pid, &clone_name, "rootless").await?;
    common::poll_health_by_pid(clone_pid, 180).await?;
    println!("  Clone healthy");

    // Verify both mounts on clone
    verify_fuse_read(
        clone_pid,
        "/mnt/vol-ro",
        "ro-file.txt",
        "read-only-data",
        10,
    )
    .await?;
    verify_fuse_write(clone_pid, "/mnt/vol-rw", "clone-rw.txt", "clone-rw-data").await?;
    let content = tokio::fs::read_to_string(format!("{}/clone-rw.txt", host_dir_b)).await?;
    assert!(content.contains("clone-rw-data"));
    println!("  Clone: both volumes verified");

    // Cleanup
    common::kill_process(clone_pid).await;
    common::kill_process(serve_pid).await;
    common::kill_process(pid).await;
    tokio::fs::remove_dir_all(&host_dir_a).await.ok();
    tokio::fs::remove_dir_all(&host_dir_b).await.ok();

    println!("PASSED: test_fuse_snapshot_matrix_multi_vol");
    Ok(())
}

// =============================================================================
// Test 6: Multiple clones with FUSE
// =============================================================================

/// 3 clones from same snapshot, all reading through FUSE.
#[tokio::test]
async fn test_fuse_snapshot_matrix_multi_clone() -> Result<()> {
    let (vm_name, _, snap_name, _) = common::unique_names("fuse-multi-clone");
    let host_dir = format!("/tmp/fcvm-fuse-multi-clone-{}", std::process::id());
    tokio::fs::create_dir_all(&host_dir).await?;
    tokio::fs::write(format!("{}/shared.txt", host_dir), "shared-data").await?;

    println!("=== test_fuse_snapshot_matrix_multi_clone ===");

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
    .context("spawning baseline VM")?;

    common::poll_health_by_pid(pid, 180).await?;
    verify_fuse_read(pid, "/mnt/test", "shared.txt", "shared-data", 30).await?;
    println!("  Baseline healthy and FUSE verified");

    // Snapshot + serve
    common::create_snapshot_by_pid(pid, &snap_name).await?;
    let (_serve, serve_pid) = common::start_memory_server(&snap_name).await?;
    println!("  Memory server ready");

    // Spawn 3 clones
    let mut clone_pids = Vec::new();
    for i in 0..3 {
        let clone_name = format!("{}-clone-{}", vm_name, i);
        let (_clone, clone_pid) = common::spawn_clone(serve_pid, &clone_name, "rootless").await?;
        common::poll_health_by_pid(clone_pid, 180).await?;
        println!("  Clone {} healthy (PID: {})", i, clone_pid);
        clone_pids.push(clone_pid);
    }

    // Write new file on host after all clones started
    let file_path = format!("{}/after-clones.txt", host_dir);
    tokio::fs::write(&file_path, "after-clone-data").await?;

    // All clones should read the new file
    for (i, &clone_pid) in clone_pids.iter().enumerate() {
        verify_fuse_read(
            clone_pid,
            "/mnt/test",
            "after-clones.txt",
            "after-clone-data",
            10,
        )
        .await
        .with_context(|| format!("clone {} FUSE read failed", i))?;
        println!("  Clone {} FUSE read verified", i);
    }

    // Cleanup
    for &clone_pid in &clone_pids {
        common::kill_process(clone_pid).await;
    }
    common::kill_process(serve_pid).await;
    common::kill_process(pid).await;
    tokio::fs::remove_dir_all(&host_dir).await.ok();

    println!("PASSED: test_fuse_snapshot_matrix_multi_clone");
    Ok(())
}

// =============================================================================
// Test 7: Large file through clone FUSE
// =============================================================================

/// 10MB file read/write through clone FUSE mount.
#[tokio::test]
async fn test_fuse_snapshot_matrix_large_file() -> Result<()> {
    let (vm_name, clone_name, snap_name, _) = common::unique_names("fuse-large");
    let host_dir = format!("/tmp/fcvm-fuse-large-{}", std::process::id());
    tokio::fs::create_dir_all(&host_dir).await?;

    println!("=== test_fuse_snapshot_matrix_large_file ===");

    // Create 10MB file on host
    let large_data: String = "A".repeat(10 * 1024 * 1024);
    tokio::fs::write(format!("{}/large.bin", host_dir), &large_data).await?;
    println!("  Created 10MB test file");

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
    .context("spawning baseline VM")?;

    common::poll_health_by_pid(pid, 180).await?;

    // Snapshot + clone
    common::create_snapshot_by_pid(pid, &snap_name).await?;
    let (_serve, serve_pid) = common::start_memory_server(&snap_name).await?;
    let (_clone, clone_pid) = common::spawn_clone(serve_pid, &clone_name, "rootless").await?;
    common::poll_health_by_pid(clone_pid, 180).await?;
    println!("  Clone healthy");

    // Verify clone reads the full 10MB file (check size via wc -c)
    let mut last_err = None;
    for attempt in 0..150 {
        // 30s total at 200ms intervals
        match common::exec_in_container(clone_pid, &["wc -c < /mnt/test/large.bin"]).await {
            Ok(output) => {
                let size: usize = output.trim().parse().unwrap_or(0);
                if size == 10 * 1024 * 1024 {
                    println!("  Clone read full 10MB file (attempt {})", attempt + 1);
                    last_err = None;
                    break;
                }
                last_err = Some(format!(
                    "size mismatch: expected {}, got {}",
                    10485760, size
                ));
            }
            Err(e) => {
                last_err = Some(format!("exec failed: {}", e));
            }
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // Cleanup
    common::kill_process(clone_pid).await;
    common::kill_process(serve_pid).await;
    common::kill_process(pid).await;
    tokio::fs::remove_dir_all(&host_dir).await.ok();

    if let Some(err) = last_err {
        anyhow::bail!("Large file read failed: {}", err);
    }

    println!("PASSED: test_fuse_snapshot_matrix_large_file");
    Ok(())
}

// =============================================================================
// Test 8: FUSE + block device combined (privileged)
// =============================================================================

/// Combined --map (RO FUSE) + --disk (RO block device) through snapshot/clone.
#[cfg(feature = "privileged-tests")]
#[tokio::test]
async fn test_fuse_snapshot_matrix_plus_disk() -> Result<()> {
    let (vm_name, clone_name, snap_name, _) = common::unique_names("fuse-disk");
    let host_dir = format!("/tmp/fcvm-fuse-disk-{}", std::process::id());
    let disk_path = PathBuf::from(format!("/tmp/fcvm-fuse-disk-{}.raw", std::process::id()));
    tokio::fs::create_dir_all(&host_dir).await?;

    println!("=== test_fuse_snapshot_matrix_plus_disk ===");

    // Setup
    tokio::fs::write(format!("{}/fuse-file.txt", host_dir), "fuse-data").await?;
    create_test_disk(&disk_path).await?;

    let map_arg = format!("{}:/mnt/fuse:ro", host_dir);
    let disk_arg = format!("{}:/mnt/disk:ro", disk_path.display());
    let (_child, pid) = common::spawn_fcvm_with_logs(
        &[
            "podman",
            "run",
            "--name",
            &vm_name,
            "--network",
            "bridged",
            "--map",
            &map_arg,
            "--disk",
            &disk_arg,
            common::TEST_IMAGE,
        ],
        &vm_name,
    )
    .await
    .context("spawning baseline VM")?;

    common::poll_health_by_pid(pid, 180).await?;

    // Verify both on baseline
    verify_fuse_read(pid, "/mnt/fuse", "fuse-file.txt", "fuse-data", 30).await?;
    let disk_content = common::exec_in_container(pid, &["cat /mnt/disk/disk-test.txt"]).await?;
    assert!(
        disk_content.contains("disk-hello"),
        "Baseline disk read failed"
    );
    println!("  Baseline: FUSE + disk verified");

    // Snapshot + clone
    common::create_snapshot_by_pid(pid, &snap_name).await?;
    let (_serve, serve_pid) = common::start_memory_server(&snap_name).await?;
    let (_clone, clone_pid) = common::spawn_clone(serve_pid, &clone_name, "bridged").await?;
    common::poll_health_by_pid(clone_pid, 180).await?;
    println!("  Clone healthy");

    // Verify both on clone
    verify_fuse_read(clone_pid, "/mnt/fuse", "fuse-file.txt", "fuse-data", 30).await?;
    let disk_content =
        common::exec_in_container(clone_pid, &["cat /mnt/disk/disk-test.txt"]).await?;
    assert!(
        disk_content.contains("disk-hello"),
        "Clone disk read failed"
    );
    println!("  Clone: FUSE + disk verified");

    // Cleanup
    common::kill_process(clone_pid).await;
    common::kill_process(serve_pid).await;
    common::kill_process(pid).await;
    tokio::fs::remove_dir_all(&host_dir).await.ok();
    tokio::fs::remove_file(&disk_path).await.ok();

    println!("PASSED: test_fuse_snapshot_matrix_plus_disk");
    Ok(())
}

// =============================================================================
// Test 9: Continuous read loop survives multiple snapshot/restore cycles
// =============================================================================

/// Runs a continuous read loop inside the VM container while taking multiple
/// snapshots. Each snapshot resets the vsock transport, but the reconnectable
/// multiplexer transparently reconnects and re-sends pending requests.
///
/// The kernel FUSE session stays alive — reads may hang briefly during
/// reconnection but MUST NOT fail. This test asserts ZERO read failures.
#[tokio::test]
async fn test_fuse_snapshot_matrix_continuous_read() -> Result<()> {
    let (vm_name, _, snap_base, _) = common::unique_names("fuse-continuous");
    let host_dir = format!("/tmp/fcvm-fuse-continuous-{}", std::process::id());
    tokio::fs::create_dir_all(&host_dir).await?;

    println!("=== test_fuse_snapshot_matrix_continuous_read ===");

    // Write sentinel file on host
    tokio::fs::write(format!("{}/sentinel.txt", host_dir), "ALIVE\n").await?;

    // Start baseline VM with RO FUSE mount
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
    .context("spawning baseline VM")?;

    common::poll_health_by_pid(pid, 180).await?;
    println!("  Baseline healthy (PID: {})", pid);

    // Verify FUSE works before starting the loop
    verify_fuse_read(pid, "/mnt/test", "sentinel.txt", "ALIVE", 30).await?;
    println!("  Initial FUSE read verified");

    // Start continuous read loop inside the container.
    // The script reads sentinel.txt every 100ms and logs OK/FAIL with a counter.
    // It runs until /tmp/read-loop-running is removed.
    let loop_script = concat!(
        "touch /tmp/read-loop-running && ",
        "i=0; ",
        "while [ -f /tmp/read-loop-running ]; do ",
        "i=$((i+1)); ",
        "if cat /mnt/test/sentinel.txt >/dev/null 2>&1; then ",
        "echo \"OK $i\" >> /tmp/read-results.log; ",
        "else ",
        "echo \"FAIL $i\" >> /tmp/read-results.log; ",
        "fi; ",
        "sleep 0.1; ",
        "done"
    );

    // Fire and forget - runs in background inside the container
    // We use sh -c '... &' to detach the loop from the exec session
    let bg_cmd = format!("sh -c '{} &'", loop_script);
    common::exec_in_container(pid, &[&bg_cmd])
        .await
        .context("starting read loop")?;
    println!("  Read loop started in container");

    // Let the loop accumulate some reads before first snapshot
    tokio::time::sleep(Duration::from_secs(3)).await;

    // Take 3 snapshots, each causing vsock reset + transparent reconnection
    for snap_i in 0..3 {
        let snap_name = format!("{}-s{}", snap_base, snap_i);
        println!("  Taking snapshot {} (triggers vsock reset)...", snap_i + 1);
        common::create_snapshot_by_pid(pid, &snap_name).await?;
        println!(
            "  Snapshot {} created, waiting for reconnection...",
            snap_i + 1
        );

        // Wait for multiplexer to reconnect and loop to resume
        tokio::time::sleep(Duration::from_secs(3)).await;

        // Verify the loop is still running by checking the log is growing
        let count_cmd = "wc -l < /tmp/read-results.log";
        let count_before = common::exec_in_container(pid, &[count_cmd])
            .await
            .unwrap_or_default()
            .trim()
            .parse::<u64>()
            .unwrap_or(0);

        tokio::time::sleep(Duration::from_secs(2)).await;

        let count_after = common::exec_in_container(pid, &[count_cmd])
            .await
            .unwrap_or_default()
            .trim()
            .parse::<u64>()
            .unwrap_or(0);

        assert!(
            count_after > count_before,
            "Read loop stalled after snapshot {}: count went from {} to {}",
            snap_i + 1,
            count_before,
            count_after
        );
        println!(
            "  Read loop alive after snapshot {} ({} -> {} reads)",
            snap_i + 1,
            count_before,
            count_after
        );
    }

    // Stop the loop and collect results
    common::exec_in_container(pid, &["rm -f /tmp/read-loop-running"])
        .await
        .ok();
    tokio::time::sleep(Duration::from_secs(1)).await;

    let results = common::exec_in_container(pid, &["cat /tmp/read-results.log"])
        .await
        .context("reading results log")?;

    let total = results.lines().count();
    let ok_count = results.lines().filter(|l| l.starts_with("OK")).count();
    let fail_count = results.lines().filter(|l| l.starts_with("FAIL")).count();

    println!(
        "  Results: {} total, {} OK, {} FAIL",
        total, ok_count, fail_count
    );

    // Core assertions:
    // 1. Loop ran enough iterations (at least 100 over ~20s at 10/sec)
    assert!(
        total >= 100,
        "Read loop didn't run enough iterations: {} (expected >= 100)",
        total
    );

    // 2. ZERO read failures. The reconnectable multiplexer handles vsock
    //    resets transparently — reads may hang briefly but must never fail.
    assert!(
        fail_count == 0,
        "Read failures detected: {} FAIL out of {} total. \
         Reconnectable multiplexer should prevent ALL read failures.",
        fail_count,
        total
    );
    println!("  All {} reads succeeded (100% success rate)", total);

    // Cleanup
    println!("Cleaning up...");
    common::kill_process(pid).await;
    tokio::fs::remove_dir_all(&host_dir).await.ok();

    println!("PASSED: test_fuse_snapshot_matrix_continuous_read");
    Ok(())
}

// =============================================================================
// Tests excluded: disk-dir + snapshot/clone
// =============================================================================
//
// --disk-dir + snapshot/clone is NOT YET IMPLEMENTED. The snapshot metadata
// doesn't include extra_disks, and the clone restore code doesn't copy
// disk-dir images from the base VM to the clone VM. Firecracker fails with
// "No such file or directory" for the missing disk image.
//
// When disk-dir snapshot support is added, tests should be added here for:
// - test_fuse_snapshot_matrix_plus_diskdir: FUSE + disk-dir combined
// - test_fuse_snapshot_matrix_diskdir_only: disk-dir alone through clone
