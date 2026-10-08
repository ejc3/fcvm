//! Diff snapshot tests - verifies automatic diff-based snapshot creation
//!
//! Tests the diff snapshot optimization:
//! 1. First snapshot (pre-start) = Full snapshot
//! 2. Subsequent snapshots (startup, user from clone) = Diff snapshot
//! 3. Diff is merged onto base immediately after creation
//!
//! Only one Full snapshot is ever created. All subsequent snapshots use reflink copy
//! of base memory.bin, then create and merge a diff.

#![cfg(feature = "integration-fast")]

mod common;

use anyhow::{Context, Result};
use std::time::Duration;

/// Image for diff snapshot tests - nginx provides /health endpoint
const TEST_IMAGE: &str = common::TEST_IMAGE;

/// Health check URL for nginx
const HEALTH_CHECK_URL: &str = "http://localhost/";

/// Get the snapshot directory path
fn snapshot_dir() -> std::path::PathBuf {
    let data_dir = std::env::var("FCVM_DATA_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("/mnt/fcvm-btrfs"));
    data_dir.join("snapshots")
}

/// Read log file and search for diff snapshot indicators
async fn check_log_for_diff_snapshot(log_path: &str) -> (bool, bool, bool) {
    let log_content = tokio::fs::read_to_string(log_path)
        .await
        .unwrap_or_default();

    let has_full = log_content.contains("creating full snapshot")
        || log_content.contains("snapshot_type=\"Full\"");
    let has_diff = log_content.contains("creating diff snapshot")
        || log_content.contains("snapshot_type=\"Diff\"");
    let has_merge = log_content.contains("merging diff snapshot onto base")
        || log_content.contains("diff merge complete");

    (has_full, has_diff, has_merge)
}

/// Test that pre-start snapshot is Full and startup snapshot is Diff
///
/// This test verifies the core diff snapshot optimization:
/// 1. Pre-start snapshot is Full (no base exists yet)
/// 2. Startup snapshot uses reflink copy of pre-start's memory.bin as base
/// 3. Startup creates a Diff snapshot
/// 4. Diff is merged onto base
#[tokio::test]
async fn test_diff_snapshot_prestart_full_startup_diff() -> Result<()> {
    // This test verifies diff snapshot types (Full vs Diff), which requires
    // snapshot creation to be enabled. Skip when FCVM_NO_SNAPSHOT is set.
    if std::env::var("FCVM_NO_SNAPSHOT")
        .map(|v| !v.is_empty())
        .unwrap_or(false)
    {
        println!("Skipping test: FCVM_NO_SNAPSHOT is set");
        return Ok(());
    }

    println!("\nDiff Snapshot: Pre-start Full, Startup Diff");
    println!("=============================================");

    let (vm_name, _, _, _) = common::unique_names("diff-prestart-startup");

    // Use unique env var to get unique snapshot key
    let test_id = format!("TEST_ID=diff-test-{}", std::process::id());

    // Start VM with health check URL to trigger both pre-start and startup snapshots
    println!("Starting VM with --health-check (triggers both pre-start and startup)...");
    let (mut child, fcvm_pid, log_path) = common::spawn_fcvm_with_log_path(
        &[
            "podman",
            "run",
            "--name",
            &vm_name,
            "--env",
            &test_id,
            "--health-check",
            HEALTH_CHECK_URL,
            TEST_IMAGE,
        ],
        &vm_name,
    )
    .await
    .context("spawning fcvm")?;

    println!("  fcvm PID: {}", fcvm_pid);
    println!("  Log file: {}", log_path.display());
    println!("  Waiting for VM to become healthy (creates pre-start, then startup)...");

    // Wait for healthy status (triggers startup snapshot creation)
    let health_result = tokio::time::timeout(
        Duration::from_secs(300),
        common::poll_health_by_pid(fcvm_pid, 300),
    )
    .await;

    // Give extra time for startup snapshot to be created and merged
    tokio::time::sleep(Duration::from_secs(10)).await;

    // Cleanup
    println!("  Stopping VM...");
    common::kill_process(fcvm_pid).await;
    let _ = child.wait().await;

    // Check result
    match health_result {
        Ok(Ok(_)) => {
            println!("  VM became healthy");

            let (has_full, has_diff, has_merge) =
                check_log_for_diff_snapshot(log_path.to_str().unwrap()).await;

            println!("\n  Log analysis (from {}):", log_path.display());
            println!("    Full snapshot created: {}", has_full);
            println!("    Diff snapshot created: {}", has_diff);
            println!("    Diff merge performed:  {}", has_merge);

            // Both Full (pre-start) and Diff (startup) should be created
            if has_full && has_diff && has_merge {
                println!("\n✅ DIFF SNAPSHOT TEST PASSED!");
                println!("  Pre-start = Full snapshot");
                println!("  Startup = Diff snapshot (merged onto base)");
                return Ok(());
            } else {
                let mut missing = vec![];
                if !has_full {
                    missing.push("Full snapshot (pre-start)");
                }
                if !has_diff {
                    missing.push("Diff snapshot (startup)");
                }
                if !has_merge {
                    missing.push("Diff merge");
                }
                // Print log content for debugging
                let log_content = tokio::fs::read_to_string(&log_path)
                    .await
                    .unwrap_or_else(|e| format!("(failed to read log: {})", e));
                let snapshot_lines: Vec<&str> = log_content
                    .lines()
                    .filter(|l| l.contains("snapshot") && !l.contains("Snapshot miss"))
                    .collect();
                println!("\n  Snapshot-related log lines:");
                for line in &snapshot_lines {
                    println!("    {}", line);
                }
                anyhow::bail!(
                    "Expected Full + Diff + Merge but missing: {}",
                    missing.join(", ")
                );
            }
        }
        Ok(Err(e)) => {
            println!("❌ Health check failed: {}", e);
            Err(e)
        }
        Err(_) => {
            anyhow::bail!("Timeout waiting for VM to become healthy")
        }
    }
}

/// Test that second run (from startup snapshot) is much faster due to diff optimization
///
/// Because startup snapshot was created as a diff and merged:
/// - Second run loads the same memory.bin (full data)
/// - No additional snapshot creation needed (hits startup cache)
#[tokio::test]
async fn test_diff_snapshot_cache_hit_fast() -> Result<()> {
    println!("\nDiff Snapshot: Cache Hit Performance");
    println!("=====================================");

    // Use unique env var to get unique snapshot key
    let test_id = format!("TEST_ID=diff-perf-{}", std::process::id());

    // First boot: creates pre-start (Full) and startup (Diff, merged)
    let (vm_name1, _, _, _) = common::unique_names("diff-perf-1");

    println!("First boot: Creating Full + Diff snapshots...");
    let start1 = std::time::Instant::now();
    let (mut child1, fcvm_pid1) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &vm_name1,
        "--env",
        &test_id,
        "--health-check",
        HEALTH_CHECK_URL,
        TEST_IMAGE,
    ])
    .await
    .context("spawning fcvm for first boot")?;

    // Wait for healthy (startup snapshot created)
    let health_result1 = tokio::time::timeout(
        Duration::from_secs(300),
        common::poll_health_by_pid(fcvm_pid1, 300),
    )
    .await;

    // Wait for snapshot creation to complete
    tokio::time::sleep(Duration::from_secs(5)).await;
    let duration1 = start1.elapsed();

    // Stop first VM
    println!("  First boot completed in {:.1}s", duration1.as_secs_f32());
    common::kill_process(fcvm_pid1).await;
    let _ = child1.wait().await;

    if health_result1.is_err() || health_result1.as_ref().unwrap().is_err() {
        anyhow::bail!("First VM did not become healthy");
    }

    // Second boot: should hit startup snapshot (merged diff)
    let (vm_name2, _, _, _) = common::unique_names("diff-perf-2");

    println!("Second boot: Should use merged startup snapshot...");
    let start2 = std::time::Instant::now();
    let (mut child2, fcvm_pid2) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &vm_name2,
        "--env",
        &test_id,
        "--health-check",
        HEALTH_CHECK_URL,
        TEST_IMAGE,
    ])
    .await
    .context("spawning fcvm for second boot")?;

    // Wait for healthy - should be much faster
    let health_result2 = tokio::time::timeout(
        Duration::from_secs(60),
        common::poll_health_by_pid(fcvm_pid2, 60),
    )
    .await;
    let duration2 = start2.elapsed();

    // Cleanup
    println!("  Second boot completed in {:.1}s", duration2.as_secs_f32());
    common::kill_process(fcvm_pid2).await;
    let _ = child2.wait().await;

    match health_result2 {
        Ok(Ok(_)) => {
            let speedup = duration1.as_secs_f64() / duration2.as_secs_f64();
            println!("\n  Performance:");
            println!(
                "    First boot:  {:.1}s (creates Full + Diff)",
                duration1.as_secs_f32()
            );
            println!(
                "    Second boot: {:.1}s (uses merged snapshot)",
                duration2.as_secs_f32()
            );
            println!("    Speedup:     {:.1}x", speedup);

            println!("\n✅ DIFF SNAPSHOT CACHE HIT TEST PASSED!");
            Ok(())
        }
        Ok(Err(e)) => {
            println!("❌ Second boot health check failed: {}", e);
            Err(e)
        }
        Err(_) => {
            anyhow::bail!("Timeout waiting for second VM to become healthy")
        }
    }
}

/// Test that user snapshot from a clone uses parent lineage (Diff snapshot)
///
/// When a user creates a snapshot from a VM that was cloned from another snapshot,
/// the new snapshot should use the source snapshot as its parent (creating a Diff).
#[tokio::test]
async fn test_user_snapshot_from_clone_uses_parent() -> Result<()> {
    println!("\nDiff Snapshot: User Snapshot from Clone");
    println!("========================================");

    let (baseline_name, clone_name, snapshot1_name, _) = common::unique_names("user-parent");
    let snapshot2_name = format!("{}-user", snapshot1_name);

    let fcvm_path = common::find_fcvm_binary()?;

    // Step 1: Start baseline VM
    println!("Step 1: Starting baseline VM...");
    let (_baseline_child, baseline_pid) = common::spawn_fcvm_with_logs(
        &[
            "podman",
            "run",
            "--name",
            &baseline_name,
            "--network",
            "rootless",
            TEST_IMAGE,
        ],
        &baseline_name,
    )
    .await
    .context("spawning baseline VM")?;

    println!("  Waiting for baseline VM to become healthy...");
    common::poll_health_by_pid(baseline_pid, 120).await?;
    println!("  ✓ Baseline VM healthy (PID: {})", baseline_pid);

    // Step 2: Create first snapshot (this will be Full - baseline has no parent)
    println!("\nStep 2: Creating snapshot from baseline (should be Full)...");
    let output = tokio::process::Command::new(&fcvm_path)
        .args([
            "snapshot",
            "create",
            "--pid",
            &baseline_pid.to_string(),
            "--tag",
            &snapshot1_name,
        ])
        .output()
        .await
        .context("running snapshot create")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("First snapshot creation failed: {}", stderr);
    }
    println!("  ✓ First snapshot created: {}", snapshot1_name);

    // Kill baseline
    common::kill_process(baseline_pid).await;

    // Step 3: Clone from snapshot
    println!("\nStep 3: Creating clone from snapshot...");
    let (_clone_child, clone_pid) = common::spawn_fcvm_with_logs(
        &[
            "snapshot",
            "run",
            "--snapshot",
            &snapshot1_name,
            "--name",
            &clone_name,
        ],
        &clone_name,
    )
    .await
    .context("spawning clone")?;

    println!("  Waiting for clone to become healthy...");
    common::poll_health_by_pid(clone_pid, 120).await?;
    let clone_state = fcvm::state::StateManager::new(fcvm::paths::state_dir())
        .load_state_by_pid(clone_pid)
        .await
        .context("loading restored clone state")?;
    assert!(
        clone_state.lifecycle_ready,
        "full-memory clone must publish lifecycle readiness after restore supervision is installed"
    );
    println!("  ✓ Clone is healthy (PID: {})", clone_pid);

    // Fill 8 MiB of the clone's memory after its restore, so these pages are in the diff the next snapshot
    // merges, and keep their checksum.
    let written = common::exec_in_container(
        clone_pid,
        &["dd if=/dev/urandom of=/dev/shm/merged bs=1M count=8 2>/dev/null && sha256sum /dev/shm/merged | cut -d' ' -f1"],
    )
    .await
    .context("writing 8 MiB in the clone")?;
    let written = written.trim().to_string();
    assert_eq!(
        written.len(),
        64,
        "expected a sha256 of the clone's 8 MiB, got {written:?}"
    );

    // Step 4: Create user snapshot from clone (should use parent lineage)
    println!("\nStep 4: Creating user snapshot from clone (should use parent -> Diff)...");
    let output = tokio::process::Command::new(&fcvm_path)
        .args([
            "snapshot",
            "create",
            "--pid",
            &clone_pid.to_string(),
            "--tag",
            &snapshot2_name,
        ])
        .output()
        .await
        .context("running snapshot create from clone")?;

    let stderr = String::from_utf8_lossy(&output.stderr);

    if !output.status.success() {
        anyhow::bail!("User snapshot from clone failed: {}", stderr);
    }
    println!("  ✓ User snapshot created: {}", snapshot2_name);
    // What the merge did, for the log of this run.
    if let Some(line) = stderr
        .lines()
        .find(|line| line.contains("diff merge complete"))
    {
        println!("  {}", line.trim());
    }

    // Step 5: a clone of the user snapshot runs on the merged memory file, and holds the pages the diff carried.
    println!("\nStep 5: Restoring a clone from the user snapshot...");
    let clone2_name = format!("{}-2", clone_name);
    let (_clone2_child, clone2_pid) = common::spawn_fcvm_with_logs(
        &[
            "snapshot",
            "run",
            "--snapshot",
            &snapshot2_name,
            "--name",
            &clone2_name,
        ],
        &clone2_name,
    )
    .await
    .context("spawning a clone of the merged snapshot")?;
    common::poll_health_by_pid(clone2_pid, 120).await?;
    let read_back =
        common::exec_in_container(clone2_pid, &["sha256sum /dev/shm/merged | cut -d' ' -f1"])
            .await
            .context("reading the 8 MiB back in a clone of the merged snapshot")?;
    common::kill_process(clone2_pid).await;
    assert_eq!(
        read_back.trim(),
        written,
        "a clone of the merged snapshot does not hold the 8 MiB the first clone wrote before the snapshot"
    );
    println!("  ✓ A clone of the merged snapshot holds the first clone's 8 MiB");

    // Check if the snapshot was created as Diff (check stderr for logs)
    let created_diff =
        stderr.contains("creating diff snapshot") || stderr.contains("snapshot_type=\"Diff\"");
    let used_parent =
        stderr.contains("copying parent memory.bin as base") || stderr.contains("parent=");

    // Cleanup
    println!("\nCleaning up...");
    common::kill_process(clone_pid).await;
    println!("  Killed clone");

    // Verify second snapshot exists
    let snapshot2_dir = snapshot_dir().join(&snapshot2_name);
    assert!(
        snapshot2_dir.join("memory.bin").exists(),
        "Second snapshot not found at {}",
        snapshot2_dir.display()
    );
    assert!(
        !snapshot2_dir.join("memory.diff").exists(),
        "the merged diff was published with the snapshot at {}",
        snapshot2_dir.display()
    );

    // Verify diff was actually used
    assert!(
        used_parent,
        "User snapshot from clone should use parent lineage (stderr: {})",
        stderr
    );
    assert!(
        created_diff,
        "User snapshot from clone should be Diff (stderr: {})",
        stderr
    );

    println!("\n✅ USER SNAPSHOT FROM CLONE TEST PASSED!");
    Ok(())
}

/// The bytes of one `/proc/meminfo` field, which the kernel prints in KiB.
fn meminfo_bytes(meminfo: &str, field: &str) -> Result<u64> {
    let line = meminfo
        .lines()
        .find(|line| line.split(':').next() == Some(field))
        .with_context(|| format!("no {field} in meminfo:\n{meminfo}"))?;
    let mut words = line.split_whitespace().skip(1);
    let kib: u64 = words
        .next()
        .with_context(|| format!("no value in {line:?}"))?
        .parse()
        .with_context(|| format!("parsing {line:?}"))?;
    anyhow::ensure!(words.next() == Some("kB"), "{line:?} is not in kB");
    Ok(kib * 1024)
}

/// Removes a snapshot directory when dropped, so a failed test does not leave
/// its memory file behind. Create it before `snapshot create`, which can
/// publish the snapshot and still exit non-zero; removing a directory that
/// does not exist does nothing.
struct RemoveSnapshotOnDrop(std::path::PathBuf);

impl Drop for RemoveSnapshotOnDrop {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A Full snapshot leaves the guest's all-zero pages as holes in memory.bin, and
/// a restore from that file is healthy.
///
/// The source boots cold with `--no-snapshot`, so it has no parent snapshot and
/// `snapshot create` writes a Full one. MemFree is the memory the guest's
/// allocator holds free. Most of it has not been touched since boot and reads as
/// zeros, so it becomes holes. The rest was used and freed during boot (the
/// initrd, early allocations) and still holds its bytes, so it is written as
/// data. The test therefore asks for holes covering at least half of MemFree,
/// read just before the snapshot. A Firecracker that writes zero pages as data
/// leaves no holes at all.
///
/// A snapshot of a clone restored from it is a Diff whose merge base is a copy of
/// this memory.bin, made with copy_file_range. On btrfs that clones the extents,
/// so the child keeps the parent's holes. When btrfs refuses the clone (the two
/// files' NODATACOW flags differ, or the data dir is not btrfs) the kernel falls
/// back to a byte copy and every hole becomes data, so the test also bounds the
/// child's holes.
///
/// The data is counted from SEEK_DATA and SEEK_HOLE runs, which is what the
/// snapshot wrote. That is not disk use: on btrfs with compression neither the
/// runs nor st_blocks give the disk, and only compsize does.
#[tokio::test]
async fn test_full_snapshot_stores_zero_pages_as_holes() -> Result<()> {
    let (vm_name, clone_name, tag, _) = common::unique_names("zero-holes");
    let fcvm_path = common::find_fcvm_binary()?;

    let (mut child, pid) = common::spawn_fcvm_with_logs(
        &[
            "podman",
            "run",
            "--name",
            &vm_name,
            "--no-snapshot",
            "--health-check",
            HEALTH_CHECK_URL,
            TEST_IMAGE,
        ],
        &vm_name,
    )
    .await
    .context("spawning the source VM")?;
    common::poll_health_by_pid(pid, 300).await?;

    let meminfo = common::exec_in_vm(pid, &["cat /proc/meminfo"])
        .await
        .context("reading the guest's meminfo")?;
    let mem_free = meminfo_bytes(&meminfo, "MemFree")?;

    let snapshot = snapshot_dir().join(&tag);
    let _remove_snapshot = RemoveSnapshotOnDrop(snapshot.clone());
    let output = tokio::process::Command::new(&fcvm_path)
        .args([
            "snapshot",
            "create",
            "--pid",
            &pid.to_string(),
            "--tag",
            &tag,
        ])
        .output()
        .await
        .context("running snapshot create")?;
    common::kill_process(pid).await;
    let _ = child.wait().await;
    let stderr = String::from_utf8_lossy(&output.stderr);
    anyhow::ensure!(output.status.success(), "snapshot create failed: {stderr}");
    for line in stderr.lines().filter(|line| {
        line.contains("snapshot created successfully")
            || line.contains("measured the snapshot's memory file")
    }) {
        println!("  {}", line.trim());
    }
    assert!(
        stderr.contains("creating full snapshot"),
        "a VM booted with --no-snapshot has no parent, so its snapshot must be Full: {stderr}"
    );
    assert!(
        stderr.contains("memory_data_bytes="),
        "snapshot create did not log the memory file's data: {stderr}"
    );

    let memory = std::fs::File::open(snapshot.join("memory.bin")).context("opening memory.bin")?;
    let len = memory.metadata()?.len();
    let data = fcvm::commands::common::data_run_bytes(&memory)?;
    let holes = len - data;
    println!(
        "  memory.bin: {} MiB long, {} MiB data, {} MiB holes; guest MemFree {} MiB",
        len >> 20,
        data >> 20,
        holes >> 20,
        mem_free >> 20
    );
    assert!(data > 0, "memory.bin holds no data at all");
    assert!(
        holes >= mem_free / 2,
        "memory.bin has {holes} bytes of holes, less than half of the guest's {mem_free} bytes \
         of free memory: zero pages were written as data"
    );

    let child_tag = format!("{tag}-child");
    let child_snapshot = snapshot_dir().join(&child_tag);
    let _remove_child_snapshot = RemoveSnapshotOnDrop(child_snapshot.clone());
    let (mut clone_child, clone_pid) = common::spawn_fcvm_with_logs(
        &["snapshot", "run", "--snapshot", &tag, "--name", &clone_name],
        &clone_name,
    )
    .await
    .context("spawning a clone of the snapshot")?;
    let clone_snapshot = async {
        common::poll_health_by_pid(clone_pid, 120)
            .await
            .context("a clone restored from a memory file with holes did not turn healthy")?;
        tokio::process::Command::new(&fcvm_path)
            .args([
                "snapshot",
                "create",
                "--pid",
                &clone_pid.to_string(),
                "--tag",
                &child_tag,
            ])
            .output()
            .await
            .context("running snapshot create on the clone")
    }
    .await;
    common::kill_process(clone_pid).await;
    let _ = clone_child.wait().await;
    let output = clone_snapshot?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    anyhow::ensure!(
        output.status.success(),
        "snapshot create on the clone failed: {stderr}"
    );
    for line in stderr.lines().filter(|line| {
        line.contains("snapshot created successfully")
            || line.contains("measured the snapshot's memory file")
    }) {
        println!("  {}", line.trim());
    }
    assert!(
        stderr.contains("creating diff snapshot"),
        "a snapshot of a restored clone must be a Diff over its parent: {stderr}"
    );

    let child_memory = std::fs::File::open(child_snapshot.join("memory.bin"))
        .context("opening the child's memory.bin")?;
    let child_len = child_memory.metadata()?.len();
    let child_data = fcvm::commands::common::data_run_bytes(&child_memory)?;
    let child_holes = child_len - child_data;
    println!(
        "  child memory.bin: {} MiB long, {} MiB data, {} MiB holes",
        child_len >> 20,
        child_data >> 20,
        child_holes >> 20
    );
    // The clone's diff writes every page it touched since the restore, zeros
    // included, so the child gains some data: 1.1 MiB on this 1 GiB guest, and
    // 4 to 8 MiB for the startup diffs of the other tests in this binary. A
    // merge base that writes the parent's holes out (about 630 MiB) leaves the
    // child none. Half the parent's holes allows about 40 times the largest
    // gain measured and still fails that copy.
    assert!(
        child_holes >= holes / 2,
        "the child's memory.bin has {child_holes} bytes of holes against its parent's {holes}: \
         the merge base copy wrote the parent's holes out as data"
    );
    Ok(())
}

/// Test that clones inherit the baseline VM's health check setting (or lack thereof).
///
/// When a baseline VM is started WITHOUT --health-check, clones should also have
/// health_check_url = None. Previously, cmd_snapshot_run would auto-assign
/// network_config.health_check_url (e.g., http://127.x.y.z:8080/) which gave clones
/// HTTP health checks they shouldn't have.
#[tokio::test]
async fn test_snapshot_clone_inherits_no_health_check() -> Result<()> {
    println!("\nSnapshot Clone: Inherits No Health Check");
    println!("=========================================");

    let (baseline_name, clone_name, snapshot_name, _) = common::unique_names("no-hc-inherit");

    let fcvm_path = common::find_fcvm_binary()?;

    // Step 1: Start baseline VM WITHOUT --health-check
    println!("Step 1: Starting baseline VM (no --health-check)...");
    let (_baseline_child, baseline_pid) = common::spawn_fcvm_with_logs(
        &[
            "podman",
            "run",
            "--name",
            &baseline_name,
            "--network",
            "rootless",
            TEST_IMAGE,
        ],
        &baseline_name,
    )
    .await
    .context("spawning baseline VM")?;

    println!("  Waiting for baseline VM to become healthy...");
    common::poll_health_by_pid(baseline_pid, 120).await?;
    println!("  Baseline VM healthy (PID: {})", baseline_pid);

    // Verify baseline has no health_check_url
    let output = tokio::process::Command::new(&fcvm_path)
        .args(["ls", "--json", "--pid", &baseline_pid.to_string()])
        .output()
        .await
        .context("running ls for baseline")?;

    #[derive(serde::Deserialize)]
    struct VmDisplay {
        #[serde(flatten)]
        vm: fcvm::state::VmState,
        #[allow(dead_code)]
        stale: bool,
    }

    let baseline_vms: Vec<VmDisplay> =
        serde_json::from_str(&String::from_utf8_lossy(&output.stdout))
            .context("parsing baseline ls output")?;
    assert!(
        baseline_vms[0].vm.config.health_check_url.is_none(),
        "Baseline VM should have no health_check_url, got: {:?}",
        baseline_vms[0].vm.config.health_check_url
    );
    println!("  Baseline health_check_url = None (correct)");

    // Step 2: Create snapshot from baseline
    println!("\nStep 2: Creating snapshot from baseline...");
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
    println!("  Snapshot created: {}", snapshot_name);

    // Verify snapshot metadata has health_check_url = null
    let snapshot_config_path = snapshot_dir().join(&snapshot_name).join("config.json");
    let snapshot_json = tokio::fs::read_to_string(&snapshot_config_path)
        .await
        .context("reading snapshot config.json")?;
    let snapshot_config: serde_json::Value =
        serde_json::from_str(&snapshot_json).context("parsing snapshot config")?;
    let metadata_hc = &snapshot_config["metadata"]["health_check_url"];
    assert!(
        metadata_hc.is_null(),
        "Snapshot metadata health_check_url should be null, got: {}",
        metadata_hc
    );
    println!("  Snapshot metadata health_check_url = null (correct)");

    // Kill baseline
    common::kill_process(baseline_pid).await;

    // Step 3: Clone from snapshot WITHOUT --health-check
    println!("\nStep 3: Creating clone from snapshot (no --health-check)...");
    let (_clone_child, clone_pid) = common::spawn_fcvm_with_logs(
        &[
            "snapshot",
            "run",
            "--snapshot",
            &snapshot_name,
            "--name",
            &clone_name,
        ],
        &clone_name,
    )
    .await
    .context("spawning clone")?;

    println!("  Waiting for clone to become healthy...");
    common::poll_health_by_pid(clone_pid, 120).await?;
    println!("  Clone is healthy (PID: {})", clone_pid);

    // Verify clone has no health_check_url
    let output = tokio::process::Command::new(&fcvm_path)
        .args(["ls", "--json", "--pid", &clone_pid.to_string()])
        .output()
        .await
        .context("running ls for clone")?;

    let clone_vms: Vec<VmDisplay> = serde_json::from_str(&String::from_utf8_lossy(&output.stdout))
        .context("parsing clone ls output")?;
    assert!(
        clone_vms[0].vm.config.health_check_url.is_none(),
        "Clone should have no health_check_url (inherited from baseline), got: {:?}",
        clone_vms[0].vm.config.health_check_url
    );
    println!("  Clone health_check_url = None (correct - inherited from baseline)");

    // Cleanup
    println!("\nCleaning up...");
    common::kill_process(clone_pid).await;

    println!("\n✅ SNAPSHOT CLONE INHERITS NO HEALTH CHECK TEST PASSED!");
    println!("  Clone correctly inherited baseline's None health_check_url");
    println!("  (Previously would get auto-assigned http://127.x.y.z:8080/)");
    Ok(())
}

/// Resolve a VM's vm_id from its fcvm process pid via `fcvm ls --json --pid`.
async fn vm_id_for_pid(pid: u32) -> Result<String> {
    let fcvm_path = common::find_fcvm_binary()?;
    let output = tokio::process::Command::new(&fcvm_path)
        .args(["ls", "--json", "--pid", &pid.to_string()])
        .output()
        .await
        .context("running fcvm ls")?;
    anyhow::ensure!(output.status.success(), "fcvm ls failed");
    let vms: serde_json::Value =
        serde_json::from_slice(&output.stdout).context("parsing fcvm ls json")?;
    vms.get(0)
        .and_then(|v| v.get("vm_id"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow::anyhow!("no vm found for pid {pid}"))
}

/// #608 regression: restoring a snapshot must not depend on its ANCESTORS'
/// vm-disks directories still existing.
///
/// The reported failure: a restore intermittently tried to open a cleaned-up
/// sibling's `vm-disks/<id>/disks/rootfs.raw` (the path embedded in vmstate.bin),
/// failing LoadSnapshot — or worse, silently opening a different live VM's disk.
/// The deterministic shape exercises the divergence-suspect chain end-to-end:
///
/// 1. Baseline A → full snapshot T1 (vmstate embeds A's disk path).
/// 2. Clone B restores T1 (drive patched to B's dir), then snapshot T2 FROM the
///    restored clone — T2's metadata carries original_vsock_vm_id=A, vm_id=B,
///    and its vmstate embeds B's patched path (the diff-chain case the #608
///    analysis suspected).
/// 3. Kill A and B — their vm-disks dirs are removed by normal cleanup (the
///    "sibling cleaned up" condition).
/// 4. Clone C restores T2: the #608 coverage guard must pass (vmstate's embedded
///    path covered by the bind-mount derived from T2's metadata) and C must boot
///    healthy with both ancestor dirs gone.
#[tokio::test]
async fn test_snapshot_restore_survives_ancestor_dir_cleanup() -> Result<()> {
    let (name_a, name_b, tag1, _serve) = common::unique_names("sib608");
    let tag2 = format!("{tag1}-t2");
    let name_c = format!("{name_b}-c");

    // 1. Baseline A.
    let (mut child_a, pid_a) = common::spawn_fcvm_with_logs(
        &["podman", "run", "--name", &name_a, TEST_IMAGE],
        "sib608-a",
    )
    .await?;
    common::poll_health_by_pid(pid_a, 120).await?;
    common::create_snapshot_by_pid(pid_a, &tag1)
        .await
        .context("creating T1 from baseline")?;

    // 2. Clone B from T1, then snapshot T2 FROM the restored clone.
    let (mut child_b, pid_b) = common::spawn_fcvm_with_logs(
        &["snapshot", "run", "--snapshot", &tag1, "--name", &name_b],
        "sib608-b",
    )
    .await?;
    common::poll_health_by_pid(pid_b, 120).await?;
    common::create_snapshot_by_pid(pid_b, &tag2)
        .await
        .context("creating T2 from restored clone")?;

    // 3. Kill BOTH ancestors; their per-VM data dirs (vm-disks/<id>) are removed
    //    by normal cleanup, reproducing the sibling-cleanup condition. The test
    //    then ENFORCES the condition rather than trusting cleanup timing: it
    //    waits for the dirs to disappear and force-removes any remnant, so clone
    //    C provably restores with both ancestor dirs gone.
    let vm_id_a = vm_id_for_pid(pid_a).await.context("vm_id for A")?;
    let vm_id_b = vm_id_for_pid(pid_b).await.context("vm_id for B")?;
    common::kill_process(pid_a).await;
    let _ = child_a.kill().await;
    common::kill_process(pid_b).await;
    let _ = child_b.kill().await;

    // Same base resolution as snapshot_dir(): FCVM_DATA_DIR, falling back to
    // fcvm's config default (/mnt/fcvm-btrfs) — NOT the Makefile's per-suite
    // override, which only exists when FCVM_DATA_DIR is exported anyway.
    let vm_disks = std::env::var("FCVM_DATA_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("/mnt/fcvm-btrfs"))
        .join("vm-disks");
    for id in [&vm_id_a, &vm_id_b] {
        let dir = vm_disks.join(id);
        // Normal cleanup removes it within a few seconds of SIGTERM.
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while dir.exists() && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        if dir.exists() {
            // Cleanup lagged (e.g. SIGKILL path) — force the condition the test
            // is about: the ancestor's directory must NOT exist at restore time.
            std::fs::remove_dir_all(&dir)
                .with_context(|| format!("force-removing ancestor dir {}", dir.display()))?;
        }
        assert!(
            !dir.exists(),
            "ancestor vm-disks dir must be gone before the restore: {}",
            dir.display()
        );
    }

    // 4. Clone C from T2 must restore and become healthy with the ancestors gone.
    let (mut child_c, pid_c) = common::spawn_fcvm_with_logs(
        &["snapshot", "run", "--snapshot", &tag2, "--name", &name_c],
        "sib608-c",
    )
    .await?;
    common::poll_health_by_pid(pid_c, 120)
        .await
        .context("#608: restore failed after ancestor vm-disks cleanup")?;

    // The restored clone must actually work (exec round-trip).
    let out = common::exec_in_vm(pid_c, &["echo", "sib608-ok"]).await?;
    assert!(out.contains("sib608-ok"), "exec into C failed: {out}");

    // Cleanup.
    common::kill_process(pid_c).await;
    let _ = child_c.kill().await;
    let _ = std::fs::remove_dir_all(snapshot_dir().join(&tag1));
    let _ = std::fs::remove_dir_all(snapshot_dir().join(&tag2));
    Ok(())
}
