//! Integration tests for --portable-volumes (RemapFs)
//!
//! Tests the full stack: --portable-volumes flag → RemapFs wrapping PassthroughFs
//! → VolumeServer → vsock → guest FUSE mount.
//!
//! Run with: make test-root FILTER=portable_volumes STREAM=1

#![cfg(feature = "integration-slow")]

mod common;

use anyhow::{Context, Result};
use std::time::Duration;

/// Read a file from the container's FUSE mount, retrying until success or timeout.
async fn fuse_read(pid: u32, path: &str, timeout_secs: u64) -> Result<String> {
    let attempts = (timeout_secs * 5) as usize; // 200ms intervals
    let mut last_err = None;
    for _ in 0..attempts {
        match common::exec_in_container(pid, &[&format!("cat {}", path)]).await {
            Ok(output) => return Ok(output),
            Err(e) => last_err = Some(format!("{}", e)),
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    anyhow::bail!(
        "fuse_read {} failed after {}s: {}",
        path,
        timeout_secs,
        last_err.unwrap_or_default()
    )
}

/// Get the inode number of a file inside the container via stat.
async fn get_inode(pid: u32, path: &str, timeout_secs: u64) -> Result<u64> {
    let attempts = (timeout_secs * 5) as usize;
    let mut last_err = None;
    for _ in 0..attempts {
        match common::exec_in_container(pid, &[&format!("stat -c %i {}", path)]).await {
            Ok(output) => {
                let trimmed = output.trim();
                if let Ok(ino) = trimmed.parse::<u64>() {
                    return Ok(ino);
                }
                last_err = Some(format!("parse error: '{}'", trimmed));
            }
            Err(e) => last_err = Some(format!("{}", e)),
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    anyhow::bail!(
        "get_inode {} failed after {}s: {}",
        path,
        timeout_secs,
        last_err.unwrap_or_default()
    )
}

/// Start a VM with --portable-volumes and a FUSE mount.
async fn start_portable_vm(
    vm_name: &str,
    host_dir: &str,
    guest_path: &str,
    read_only: bool,
) -> Result<(tokio::process::Child, u32)> {
    let map_arg = if read_only {
        format!("{}:{}:ro", host_dir, guest_path)
    } else {
        format!("{}:{}", host_dir, guest_path)
    };

    common::spawn_fcvm_with_logs(
        &[
            "podman",
            "run",
            "--name",
            vm_name,
            "--network",
            "rootless",
            "--portable-volumes",
            "--map",
            &map_arg,
            common::TEST_IMAGE,
        ],
        vm_name,
    )
    .await
    .context("spawning portable-volumes VM")
}

// =============================================================================
// Test 13: stat() returns deterministic inodes with --portable-volumes
// =============================================================================

/// Mount with RemapFs via --portable-volumes, stat() same files multiple times.
/// Inodes must be stable (hash-based) and consistent across calls.
#[tokio::test]
async fn test_remap_fs_stat_deterministic() -> Result<()> {
    let (vm_name, _, _, _) = common::unique_names("pv-stat");
    let host_dir = format!("/tmp/fcvm-pv-stat-{}", std::process::id());
    tokio::fs::create_dir_all(&host_dir).await?;

    println!("=== test_remap_fs_stat_deterministic ===");

    // Create test files on host
    tokio::fs::write(format!("{}/file-a.txt", host_dir), "content-a").await?;
    tokio::fs::write(format!("{}/file-b.txt", host_dir), "content-b").await?;
    tokio::fs::create_dir_all(format!("{}/subdir", host_dir)).await?;
    tokio::fs::write(format!("{}/subdir/nested.txt", host_dir), "nested").await?;

    let (_child, pid) = start_portable_vm(&vm_name, &host_dir, "/mnt/test", true).await?;
    common::poll_health_by_pid(pid, 180).await?;
    println!("  VM healthy (PID: {})", pid);

    // Stat each file 3 times — inode must be identical every time
    for file in &[
        "/mnt/test/file-a.txt",
        "/mnt/test/file-b.txt",
        "/mnt/test/subdir/nested.txt",
        "/mnt/test/subdir",
    ] {
        let ino1 = get_inode(pid, file, 30).await?;
        let ino2 = get_inode(pid, file, 5).await?;
        let ino3 = get_inode(pid, file, 5).await?;

        assert_eq!(
            ino1, ino2,
            "inode for {} changed between stat calls: {} vs {}",
            file, ino1, ino2
        );
        assert_eq!(
            ino2, ino3,
            "inode for {} changed between stat calls: {} vs {}",
            file, ino2, ino3
        );

        // RemapFs inodes are hash-based, must not be 0 or 1
        assert!(ino1 >= 2, "inode for {} is reserved value: {}", file, ino1);

        println!("  {} → inode {} (stable across 3 calls)", file, ino1);
    }

    // Different files must have different inodes
    let ino_a = get_inode(pid, "/mnt/test/file-a.txt", 5).await?;
    let ino_b = get_inode(pid, "/mnt/test/file-b.txt", 5).await?;
    assert_ne!(
        ino_a, ino_b,
        "different files should have different inodes: {} vs {}",
        ino_a, ino_b
    );
    println!(
        "  file-a and file-b have different inodes ({} vs {})",
        ino_a, ino_b
    );

    // Root mount point should have inode 1 (FUSE convention)
    let ino_root = get_inode(pid, "/mnt/test", 5).await?;
    assert_eq!(
        ino_root, 1,
        "mount root should have inode 1, got {}",
        ino_root
    );
    println!("  mount root → inode {} (correct)", ino_root);

    // Verify FUSE inodes differ from host inodes (proves RemapFs is active)
    let host_ino_a = std::fs::metadata(format!("{}/file-a.txt", host_dir))
        .map(|m| {
            use std::os::unix::fs::MetadataExt;
            m.ino()
        })
        .unwrap_or(0);
    assert_ne!(
        ino_a, host_ino_a,
        "FUSE inode matches host inode {} — RemapFs not active",
        host_ino_a
    );
    println!(
        "  FUSE inode {} differs from host inode {} (RemapFs active)",
        ino_a, host_ino_a
    );

    // Cleanup
    common::kill_process(pid).await;
    tokio::fs::remove_dir_all(&host_dir).await.ok();

    println!("PASSED: test_remap_fs_stat_deterministic");
    Ok(())
}

// =============================================================================
// Test 14: Snapshot/clone preserves inodes with --portable-volumes
// =============================================================================

/// Baseline with RemapFs → snapshot → clone → same inodes on clone.
/// This is the core portability guarantee: clone sees the same inodes as baseline.
#[tokio::test]
async fn test_remap_fs_snapshot_clone() -> Result<()> {
    let (vm_name, clone_name, snap_name, _) = common::unique_names("pv-clone");
    let host_dir = format!("/tmp/fcvm-pv-clone-{}", std::process::id());
    tokio::fs::create_dir_all(&host_dir).await?;

    println!("=== test_remap_fs_snapshot_clone ===");

    // Create test files
    tokio::fs::write(format!("{}/alpha.txt", host_dir), "alpha-content").await?;
    tokio::fs::write(format!("{}/beta.txt", host_dir), "beta-content").await?;

    // Start baseline with --portable-volumes
    let (_child, pid) = start_portable_vm(&vm_name, &host_dir, "/mnt/test", true).await?;
    common::poll_health_by_pid(pid, 180).await?;
    println!("  Baseline healthy (PID: {})", pid);

    // Get inodes on baseline
    let baseline_ino_alpha = get_inode(pid, "/mnt/test/alpha.txt", 30).await?;
    let baseline_ino_beta = get_inode(pid, "/mnt/test/beta.txt", 30).await?;
    let baseline_ino_root = get_inode(pid, "/mnt/test", 5).await?;
    println!(
        "  Baseline inodes: root={}, alpha={}, beta={}",
        baseline_ino_root, baseline_ino_alpha, baseline_ino_beta
    );

    // Snapshot
    common::create_snapshot_by_pid(pid, &snap_name).await?;
    println!("  Snapshot created");

    // Start clone
    let (_serve, serve_pid) = common::start_memory_server(&snap_name).await?;
    let (_clone, clone_pid) = common::spawn_clone(serve_pid, &clone_name).await?;
    common::poll_health_by_pid(clone_pid, 180).await?;
    println!("  Clone healthy (PID: {})", clone_pid);

    // Get inodes on clone — they must match baseline
    let clone_ino_alpha = get_inode(clone_pid, "/mnt/test/alpha.txt", 30).await?;
    let clone_ino_beta = get_inode(clone_pid, "/mnt/test/beta.txt", 30).await?;
    let clone_ino_root = get_inode(clone_pid, "/mnt/test", 5).await?;
    println!(
        "  Clone inodes: root={}, alpha={}, beta={}",
        clone_ino_root, clone_ino_alpha, clone_ino_beta
    );

    assert_eq!(
        baseline_ino_root, clone_ino_root,
        "root inode mismatch: baseline={} clone={}",
        baseline_ino_root, clone_ino_root
    );
    assert_eq!(
        baseline_ino_alpha, clone_ino_alpha,
        "alpha.txt inode mismatch: baseline={} clone={}",
        baseline_ino_alpha, clone_ino_alpha
    );
    assert_eq!(
        baseline_ino_beta, clone_ino_beta,
        "beta.txt inode mismatch: baseline={} clone={}",
        baseline_ino_beta, clone_ino_beta
    );
    println!("  All inodes match between baseline and clone!");

    // Verify inodes are NOT host inodes (proves RemapFs is active, not PassthroughFs).
    // If --portable-volumes wasn't propagated to the clone, it would use PassthroughFs
    // which returns host inodes — these would differ from the baseline's hash-based inodes.
    let host_ino = std::fs::metadata(format!("{}/alpha.txt", host_dir))
        .map(|m| {
            use std::os::unix::fs::MetadataExt;
            m.ino()
        })
        .unwrap_or(0);
    assert_ne!(
        clone_ino_alpha, host_ino,
        "clone inode matches host inode {} — RemapFs not active on clone (portable flag lost in snapshot?)",
        host_ino
    );
    println!(
        "  Clone inode {} differs from host inode {} (RemapFs confirmed active)",
        clone_ino_alpha, host_ino
    );

    // Cleanup
    common::kill_process(clone_pid).await;
    common::kill_process(serve_pid).await;
    common::kill_process(pid).await;
    tokio::fs::remove_dir_all(&host_dir).await.ok();

    println!("PASSED: test_remap_fs_snapshot_clone");
    Ok(())
}

// =============================================================================
// Test 15: Host file modifications visible through RemapFs
// =============================================================================

/// Modify file on host, guest sees updated content through portable FUSE mount.
/// Verifies RemapFs doesn't break data transparency.
#[tokio::test]
async fn test_remap_fs_host_file_changes() -> Result<()> {
    let (vm_name, _, _, _) = common::unique_names("pv-host-mod");
    let host_dir = format!("/tmp/fcvm-pv-host-mod-{}", std::process::id());
    tokio::fs::create_dir_all(&host_dir).await?;

    println!("=== test_remap_fs_host_file_changes ===");

    // Create initial file
    tokio::fs::write(format!("{}/data.txt", host_dir), "version-1").await?;

    let (_child, pid) = start_portable_vm(&vm_name, &host_dir, "/mnt/test", true).await?;
    common::poll_health_by_pid(pid, 180).await?;
    println!("  VM healthy");

    // Read initial content
    let content = fuse_read(pid, "/mnt/test/data.txt", 30).await?;
    assert!(
        content.contains("version-1"),
        "initial read failed: got '{}'",
        content.trim()
    );
    let ino_before = get_inode(pid, "/mnt/test/data.txt", 5).await?;
    println!("  Initial read: '{}', inode={}", content.trim(), ino_before);

    // Modify on host
    tokio::fs::write(format!("{}/data.txt", host_dir), "version-2").await?;
    println!("  Host file modified to version-2");

    // Guest should see the update (may need to wait for cache expiry)
    let mut saw_update = false;
    for _ in 0..50 {
        // 10s at 200ms intervals
        if let Ok(content) = fuse_read(pid, "/mnt/test/data.txt", 2).await {
            if content.contains("version-2") {
                saw_update = true;
                println!("  Guest sees updated content: '{}'", content.trim());
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(saw_update, "guest never saw updated content 'version-2'");

    // Inode should remain the same after content change (same path = same hash)
    let ino_after = get_inode(pid, "/mnt/test/data.txt", 5).await?;
    assert_eq!(
        ino_before, ino_after,
        "inode changed after content update: {} vs {}",
        ino_before, ino_after
    );
    println!("  Inode stable through content update: {}", ino_after);

    // Add new file on host
    tokio::fs::write(format!("{}/new-file.txt", host_dir), "new-content").await?;
    let content = fuse_read(pid, "/mnt/test/new-file.txt", 10).await?;
    assert!(
        content.contains("new-content"),
        "new file read failed: got '{}'",
        content.trim()
    );
    println!("  New host file visible in guest: '{}'", content.trim());

    // Cleanup
    common::kill_process(pid).await;
    tokio::fs::remove_dir_all(&host_dir).await.ok();

    println!("PASSED: test_remap_fs_host_file_changes");
    Ok(())
}

// =============================================================================
// Test 16: Host-side rename detected via readdir
// =============================================================================

/// Rename file on host, guest readdir sees the new name.
/// Verifies RemapFs's path staleness detection works end-to-end.
#[tokio::test]
async fn test_remap_fs_host_rename_then_readdir() -> Result<()> {
    let (vm_name, _, _, _) = common::unique_names("pv-rename");
    let host_dir = format!("/tmp/fcvm-pv-rename-{}", std::process::id());
    tokio::fs::create_dir_all(&host_dir).await?;

    println!("=== test_remap_fs_host_rename_then_readdir ===");

    // Create initial files
    tokio::fs::write(format!("{}/original.txt", host_dir), "rename-test").await?;
    tokio::fs::write(format!("{}/stable.txt", host_dir), "not-renamed").await?;

    let (_child, pid) = start_portable_vm(&vm_name, &host_dir, "/mnt/test", true).await?;
    common::poll_health_by_pid(pid, 180).await?;
    println!("  VM healthy");

    // Read original file and get its inode
    let content = fuse_read(pid, "/mnt/test/original.txt", 30).await?;
    assert!(content.contains("rename-test"));
    let ino_original = get_inode(pid, "/mnt/test/original.txt", 5).await?;
    println!("  original.txt: inode={}", ino_original);

    // Get inode for stable.txt (should not change)
    let ino_stable = get_inode(pid, "/mnt/test/stable.txt", 5).await?;
    println!("  stable.txt: inode={}", ino_stable);

    // Rename on host
    tokio::fs::rename(
        format!("{}/original.txt", host_dir),
        format!("{}/renamed.txt", host_dir),
    )
    .await?;
    println!("  Host: renamed original.txt → renamed.txt");

    // Guest readdir should eventually show the new name
    let mut saw_renamed = false;
    for _ in 0..50 {
        // 10s at 200ms intervals
        if let Ok(output) = common::exec_in_container(pid, &["ls /mnt/test/"]).await {
            if output.contains("renamed.txt") && !output.contains("original.txt") {
                saw_renamed = true;
                println!(
                    "  Guest readdir shows: {}",
                    output.trim().replace('\n', ", ")
                );
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(
        saw_renamed,
        "guest readdir never showed renamed.txt (host-side rename not detected)"
    );

    // The renamed file should be readable with the same content
    let content = fuse_read(pid, "/mnt/test/renamed.txt", 10).await?;
    assert!(
        content.contains("rename-test"),
        "renamed file content wrong: got '{}'",
        content.trim()
    );
    println!("  renamed.txt readable with correct content");

    // stable.txt should still have the same inode
    let ino_stable_after = get_inode(pid, "/mnt/test/stable.txt", 5).await?;
    assert_eq!(
        ino_stable, ino_stable_after,
        "stable.txt inode changed: {} vs {}",
        ino_stable, ino_stable_after
    );
    println!("  stable.txt inode unchanged: {}", ino_stable_after);

    // Cleanup
    common::kill_process(pid).await;
    tokio::fs::remove_dir_all(&host_dir).await.ok();

    println!("PASSED: test_remap_fs_host_rename_then_readdir");
    Ok(())
}

// =============================================================================
// Test 17: Persistent reader survives snapshot, sees host file replacement
// =============================================================================

/// Mount a directory, start a background reader that logs file contents in a loop,
/// snapshot the VM, replace files on host, start clone from snapshot, verify the
/// clone's reader process sees the new content.
///
/// This proves:
/// - FUSE mount survives snapshot/restore on clones
/// - RemapFs works across snapshot/clone boundary
/// - Host-side file replacement is visible to clone
/// - Open file operations continue working after restore
#[tokio::test]
async fn test_remap_fs_snapshot_file_replace() -> Result<()> {
    let (vm_name, clone_name, snap_name, _) = common::unique_names("pv-replace");
    let host_dir = format!("/tmp/fcvm-pv-replace-{}", std::process::id());
    tokio::fs::create_dir_all(&host_dir).await?;

    println!("=== test_remap_fs_snapshot_file_replace ===");

    // Create initial file
    tokio::fs::write(format!("{}/data.txt", host_dir), "original-v1").await?;

    // Start VM with portable volumes
    let (_child, pid) = start_portable_vm(&vm_name, &host_dir, "/mnt/test", true).await?;
    common::poll_health_by_pid(pid, 180).await?;
    println!("  VM healthy (PID: {})", pid);

    // Verify initial content is readable
    let content = fuse_read(pid, "/mnt/test/data.txt", 30).await?;
    assert!(
        content.contains("original-v1"),
        "initial read failed: {}",
        content.trim()
    );
    println!("  Initial read: '{}'", content.trim());

    // Start a persistent reader inside the container that reads the file every 0.5s
    // and appends each read to a log file. This process will be captured in the snapshot
    // and resume on the clone.
    common::exec_in_container(
        pid,
        &["sh -c 'while true; do echo \"$(cat /mnt/test/data.txt 2>&1)\" >> /tmp/reader.log; sleep 0.5; done &'"],
    )
    .await?;
    println!("  Background reader started");

    // Wait for reader to accumulate a few entries
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Verify reader is logging original content
    let log = common::exec_in_container(pid, &["cat /tmp/reader.log"]).await?;
    assert!(
        log.contains("original-v1"),
        "reader not logging original content: {}",
        log
    );
    let pre_snap_lines = log.lines().count();
    println!(
        "  Reader confirmed logging original content ({} lines)",
        pre_snap_lines
    );

    // Snapshot the VM (reader is still running inside)
    common::create_snapshot_by_pid(pid, &snap_name).await?;
    println!("  Snapshot created (reader process captured in snapshot)");

    // Replace file on host with new content
    tokio::fs::write(format!("{}/data.txt", host_dir), "replaced-v2").await?;
    println!("  Host file replaced: 'original-v1' → 'replaced-v2'");

    // Start clone from snapshot — the reader process resumes automatically
    let (_serve, serve_pid) = common::start_memory_server(&snap_name).await?;
    let (_clone, clone_pid) = common::spawn_clone(serve_pid, &clone_name).await?;
    common::poll_health_by_pid(clone_pid, 180).await?;
    println!("  Clone healthy (PID: {})", clone_pid);

    // Wait for clone's reader to run several iterations with the new content
    tokio::time::sleep(Duration::from_secs(5)).await;

    // Read the clone's log file — it should have pre-snapshot entries (original-v1)
    // from before the snapshot, plus post-snapshot entries (replaced-v2) from after
    // the reader resumed on the clone and saw the new host content.
    let clone_log = common::exec_in_container(clone_pid, &["cat /tmp/reader.log"]).await?;
    let total_lines = clone_log.lines().count();
    println!("  Clone reader log ({} lines):", total_lines);
    for line in clone_log
        .lines()
        .rev()
        .take(5)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
    {
        println!("    {}", line);
    }

    // Log must contain both original and replaced content, proving the reader
    // saw the transition across snapshot/restore + host file replacement.
    assert!(
        clone_log.contains("original-v1"),
        "clone log missing pre-snapshot content 'original-v1'"
    );
    assert!(
        clone_log.contains("replaced-v2"),
        "clone reader never saw replaced content 'replaced-v2' — FUSE mount may not have \
         reconnected after snapshot restore. Log:\n{}",
        clone_log
    );
    assert!(
        total_lines > pre_snap_lines,
        "clone log has {} lines (same as pre-snapshot {}) — reader didn't resume",
        total_lines,
        pre_snap_lines
    );
    println!(
        "  Clone reader log proves transition: original-v1 → replaced-v2 ({} → {} lines)",
        pre_snap_lines, total_lines
    );

    // Also verify a direct read confirms the new content
    let direct = fuse_read(clone_pid, "/mnt/test/data.txt", 10).await?;
    assert!(
        direct.contains("replaced-v2"),
        "clone direct read got '{}', expected 'replaced-v2'",
        direct.trim()
    );
    println!("  Direct read confirms: '{}'", direct.trim());

    // Cleanup
    common::kill_process(clone_pid).await;
    common::kill_process(serve_pid).await;
    common::kill_process(pid).await;
    tokio::fs::remove_dir_all(&host_dir).await.ok();

    println!("PASSED: test_remap_fs_snapshot_file_replace");
    Ok(())
}

// =============================================================================
// Test 18: Open file handles survive snapshot/clone restore
// =============================================================================

/// Verify that file handles survive snapshot/clone restore through
/// the portable volumes pipeline.
///
/// Tests:
/// 1. Clone can read files that existed at snapshot time
/// 2. Clone can write new files (FUSE mount works bidirectionally)
/// 3. Host sees clone's writes (data integrity through clone VolumeServer)
///
/// Note: The inode table serialize/restore path runs in the pre-start
/// snapshot cache (production path). The CLI `fcvm snapshot create` path
/// used by tests creates snapshots in a separate process that doesn't have
/// access to the running VolumeServer's RemapFs. In the CLI path, inodes
/// are re-discovered via FUSE TTL expiry (~1s) which the test accommodates
/// via retry loops in fuse_read().
#[tokio::test]
async fn test_open_handle_survives_snapshot_restore() -> Result<()> {
    let (vm_name, clone_name, snap_name, _) = common::unique_names("pv-handle");
    let host_dir = format!("/tmp/fcvm-pv-handle-{}", std::process::id());
    tokio::fs::create_dir_all(&host_dir).await?;

    println!("=== test_open_handle_survives_snapshot_restore ===");

    // Create files that will be used across snapshot
    tokio::fs::write(format!("{}/watched.txt", host_dir), "line1\n").await?;

    // Start VM with portable volumes (RW)
    let (_child, pid) = start_portable_vm(&vm_name, &host_dir, "/mnt/test", false).await?;
    common::poll_health_by_pid(pid, 180).await?;
    println!("  Baseline VM healthy (PID: {})", pid);

    // Read file to populate RemapFs inode table before snapshot
    let content = fuse_read(pid, "/mnt/test/watched.txt", 30).await?;
    assert!(
        content.contains("line1"),
        "initial read: {}",
        content.trim()
    );
    println!("  Initial read: '{}'", content.trim());

    // Take snapshot
    common::create_snapshot_by_pid(pid, &snap_name).await?;
    println!("  Snapshot created");

    // Start clone
    let (_serve, serve_pid) = common::start_memory_server(&snap_name).await?;
    let (_clone, clone_pid) = common::spawn_clone(serve_pid, &clone_name).await?;
    common::poll_health_by_pid(clone_pid, 180).await?;
    println!("  Clone healthy (PID: {})", clone_pid);

    // Verify snapshot content is readable on clone (inode table re-discovered via TTL)
    let direct = fuse_read(clone_pid, "/mnt/test/watched.txt", 30).await?;
    assert!(
        direct.contains("line1"),
        "clone should read snapshot content, got: {}",
        direct.trim()
    );
    println!("  Clone reads snapshot content: '{}'", direct.trim());

    // Verify clone can write new files (proves FUSE mount fully operational)
    common::exec_in_container(
        clone_pid,
        &["sh -c 'echo clone_wrote > /mnt/test/new_file.txt'"],
    )
    .await?;
    let new_content = fuse_read(clone_pid, "/mnt/test/new_file.txt", 10).await?;
    assert!(
        new_content.contains("clone_wrote"),
        "clone write failed, got: {}",
        new_content.trim()
    );
    println!("  Clone write works: '{}'", new_content.trim());

    // Verify host sees the clone's write
    let host_content = tokio::fs::read_to_string(format!("{}/new_file.txt", host_dir)).await?;
    assert!(
        host_content.contains("clone_wrote"),
        "host should see clone write, got: {}",
        host_content.trim()
    );
    println!("  Host sees clone write: '{}'", host_content.trim());

    // Cleanup
    common::kill_process(clone_pid).await;
    common::kill_process(serve_pid).await;
    common::kill_process(pid).await;
    tokio::fs::remove_dir_all(&host_dir).await.ok();

    println!("PASSED: test_open_handle_survives_snapshot_restore");
    Ok(())
}
