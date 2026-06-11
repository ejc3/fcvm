//! Extra disk and NFS integration tests
//!
//! Tests the --disk, --disk-dir, and --nfs flags for adding extra storage to VMs.

#![cfg(feature = "privileged-tests")]

mod common;

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use tempfile::TempDir;

/// Mount method for directory sharing tests
#[derive(Debug, Clone, Copy)]
enum MountMethod {
    /// --disk-dir: creates a raw disk image from directory contents
    DiskDir,
    /// --nfs: shares directory via NFS over network
    Nfs,
}

impl MountMethod {
    fn flag(&self) -> &'static str {
        match self {
            MountMethod::DiskDir => "--disk-dir",
            MountMethod::Nfs => "--nfs",
        }
    }

    fn name(&self) -> &'static str {
        match self {
            MountMethod::DiskDir => "diskdir",
            MountMethod::Nfs => "nfs",
        }
    }

    /// Extra VM arguments required for this mount method.
    /// NFS requires the nested kernel profile which has CONFIG_NFS_FS=y.
    fn extra_args(&self) -> Vec<&'static str> {
        match self {
            MountMethod::DiskDir => vec![],
            MountMethod::Nfs => vec!["--kernel-profile", "nested"],
        }
    }
}

/// Create a small ext4 disk image with a test file
async fn create_test_disk(path: &Path) -> Result<()> {
    // Create 64MB sparse file and format as ext4
    tokio::process::Command::new("truncate")
        .args(["-s", "64M", path.to_str().unwrap()])
        .status()
        .await?;
    tokio::process::Command::new("mkfs.ext4")
        .args(["-q", "-F", path.to_str().unwrap()])
        .status()
        .await?;

    // Mount temporarily, write test file, unmount
    // Use path-derived name to avoid collisions when tests run in parallel
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
    tokio::fs::write(format!("{}/test.txt", mount_dir), "hello\n").await?;
    tokio::process::Command::new("umount")
        .arg(&mount_dir)
        .status()
        .await?;
    tokio::fs::remove_dir(&mount_dir).await.ok();
    Ok(())
}

/// Test RW disk: mounted, readable, writable, blocks snapshots
#[tokio::test]
async fn test_extra_disk_rw() -> Result<()> {
    let (vm_name, _, _, _) = common::unique_names("disk-rw");
    let disk_path = PathBuf::from(format!("/tmp/fcvm-{}.raw", vm_name));
    create_test_disk(&disk_path).await?;

    // Start VM with disk at /data
    let disk_spec = format!("{}:/data", disk_path.display());
    let (mut child, pid) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &vm_name,
        "--network",
        "bridged",
        "--disk",
        &disk_spec,
        common::TEST_IMAGE,
    ])
    .await
    .context("spawn")?;

    common::poll_health_by_pid(pid, 120).await?;

    // Read test file from container
    let content = common::exec_in_container(pid, &["cat", "/data/test.txt"]).await?;
    assert!(content.contains("hello"), "read failed: {}", content);

    // Write new file in container
    let content =
        common::exec_in_container(pid, &["echo world > /data/new.txt && cat /data/new.txt"])
            .await?;
    assert!(content.contains("world"), "write failed: {}", content);

    // Snapshot should be blocked for RW disk
    let result = common::create_snapshot_by_pid(pid, "x").await;
    assert!(result.is_err(), "snapshot should fail for RW disk");
    let err = result.unwrap_err().to_string();
    assert!(err.contains("read-write"), "wrong error: {}", err);

    child.kill().await.ok();
    tokio::fs::remove_file(&disk_path).await.ok();
    Ok(())
}

/// Test RO disk: mounted, readable, allows snapshots/clones
#[tokio::test]
async fn test_extra_disk_ro_clone() -> Result<()> {
    let (vm_name, clone_name, snap_name, serve_name) = common::unique_names("disk-ro");
    let disk_path = PathBuf::from(format!("/tmp/fcvm-{}.raw", vm_name));
    create_test_disk(&disk_path).await?;

    // Start VM with RO disk at /data
    let disk_spec = format!("{}:/data:ro", disk_path.display());
    let (_child, pid) = common::spawn_fcvm_with_logs(
        &[
            "podman",
            "run",
            "--name",
            &vm_name,
            "--network",
            "bridged",
            "--disk",
            &disk_spec,
            common::TEST_IMAGE,
        ],
        &vm_name,
    )
    .await?;

    common::poll_health_by_pid(pid, 120).await?;

    // Read test file from container
    let content = common::exec_in_container(pid, &["cat", "/data/test.txt"]).await?;
    assert!(content.contains("hello"), "read failed");

    // Snapshot should succeed for RO disk
    common::create_snapshot_by_pid(pid, &snap_name).await?;

    // Start serve
    let (_serve, serve_pid) =
        common::spawn_fcvm_with_logs(&["snapshot", "serve", &snap_name], &serve_name).await?;
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    // Start clone
    let (_clone, clone_pid) = common::spawn_fcvm_with_logs(
        &[
            "snapshot",
            "run",
            "--pid",
            &serve_pid.to_string(),
            "--name",
            &clone_name,
        ],
        &clone_name,
    )
    .await?;

    common::poll_health_by_pid(clone_pid, 60).await?;

    // Read test file from clone's container
    let content = common::exec_in_container(clone_pid, &["cat", "/data/test.txt"]).await?;
    assert!(content.contains("hello"), "clone read failed");

    tokio::fs::remove_file(&disk_path).await.ok();
    Ok(())
}

/// Shared test logic for read-only directory mounts (--disk-dir or --nfs)
async fn test_dir_mount_ro(method: MountMethod) -> Result<()> {
    let (vm_name, _, _, _) = common::unique_names(&format!("{}-ro", method.name()));

    // Create a temp directory with test files
    let source_dir = TempDir::new()?;
    tokio::fs::write(
        source_dir.path().join("hello.txt"),
        "hello from dir mount\n",
    )
    .await?;
    tokio::fs::create_dir_all(source_dir.path().join("subdir")).await?;
    tokio::fs::write(
        source_dir.path().join("subdir/nested.txt"),
        "nested content\n",
    )
    .await?;

    // Start VM with directory mount (read-only)
    let mount_spec = format!("{}:/mydata:ro", source_dir.path().display());
    let mut args = vec![
        "podman",
        "run",
        "--name",
        &vm_name,
        "--network",
        "bridged",
        method.flag(),
        &mount_spec,
    ];
    args.extend(method.extra_args());
    args.push(common::TEST_IMAGE);
    let (mut child, pid) = common::spawn_fcvm(&args).await.context("spawn")?;

    common::poll_health_by_pid(pid, 120).await?;

    // Read top-level file
    let content = common::exec_in_container(pid, &["cat", "/mydata/hello.txt"]).await?;
    assert!(
        content.contains("hello from dir mount"),
        "{:?} read top-level failed: {}",
        method,
        content
    );

    // Read nested file
    let content = common::exec_in_container(pid, &["cat", "/mydata/subdir/nested.txt"]).await?;
    assert!(
        content.contains("nested content"),
        "{:?} read nested failed: {}",
        method,
        content
    );

    child.kill().await.ok();
    Ok(())
}

/// Test --disk-dir read-only: creates disk image from directory contents
#[tokio::test]
async fn test_disk_dir_ro() -> Result<()> {
    test_dir_mount_ro(MountMethod::DiskDir).await
}

/// Test --nfs read-only: shares directory via NFS
#[tokio::test]
async fn test_nfs_ro() -> Result<()> {
    test_dir_mount_ro(MountMethod::Nfs).await
}

/// Shared test logic for read-write directory mounts (--disk-dir or --nfs)
async fn test_dir_mount_rw(method: MountMethod) -> Result<()> {
    let (vm_name, _, _, _) = common::unique_names(&format!("{}-rw", method.name()));

    // Create a temp directory with initial content
    let source_dir = TempDir::new()?;
    tokio::fs::write(source_dir.path().join("original.txt"), "original content\n").await?;

    // Start VM with directory mount (read-write, no :ro suffix)
    let mount_spec = format!("{}:/mydata", source_dir.path().display());
    let mut args = vec![
        "podman",
        "run",
        "--name",
        &vm_name,
        "--network",
        "bridged",
        method.flag(),
        &mount_spec,
    ];
    args.extend(method.extra_args());
    args.push(common::TEST_IMAGE);
    let (mut child, pid) = common::spawn_fcvm(&args).await.context("spawn")?;

    common::poll_health_by_pid(pid, 120).await?;

    // Read original file
    let content = common::exec_in_container(pid, &["cat", "/mydata/original.txt"]).await?;
    assert!(
        content.contains("original content"),
        "{:?} read original failed: {}",
        method,
        content
    );

    // Write new file
    let content = common::exec_in_container(
        pid,
        &["echo 'written in vm' > /mydata/newfile.txt && cat /mydata/newfile.txt"],
    )
    .await?;
    assert!(
        content.contains("written in vm"),
        "{:?} write failed: {}",
        method,
        content
    );

    // Verify the write persists within the VM session
    let content = common::exec_in_container(pid, &["cat", "/mydata/newfile.txt"]).await?;
    assert!(
        content.contains("written in vm"),
        "{:?} re-read failed: {}",
        method,
        content
    );

    child.kill().await.ok();
    Ok(())
}

/// Test --disk-dir read-write: can write to ephemeral disk
#[tokio::test]
async fn test_disk_dir_rw() -> Result<()> {
    test_dir_mount_rw(MountMethod::DiskDir).await
}

/// Test --nfs read-write: can write to NFS share
#[tokio::test]
async fn test_nfs_rw() -> Result<()> {
    test_dir_mount_rw(MountMethod::Nfs).await
}

/// Regression test: NFS shares must survive the snapshot→clone path.
///
/// Three independent breaks used to make this impossible (#630 follow-on):
///   1. snapshot metadata didn't record NFS shares, so the restore path never
///      re-created the host export (the baseline's /etc/exports.d entry dies
///      with the baseline);
///   2. a clone's namespace owns the gateway IP on its internal bridge, so the
///      guest's NFS traffic to gateway:2049 terminated in-namespace instead of
///      reaching the host's nfsd (fixed by the gateway DNAT + masquerade);
///   3. fc-agent re-fetched the plan from MMDS after restore, but the
///      restore-epoch PUT replaces the whole MMDS store — the fetch 404'd and
///      the remount silently no-oped (fixed by caching the boot-time plan).
///
/// The nested kernel profile is required for CONFIG_NFS_FS, which also means
/// the baseline itself converges through the restore path before we even
/// snapshot it — so this exercises NFS across two restore generations.
#[tokio::test]
async fn test_nfs_clone_restore() -> Result<()> {
    let (vm_name, clone_name, snap, _serve) = common::unique_names("nfs-clone");

    // Shared directory with initial content.
    let source_dir = TempDir::new()?;
    tokio::fs::write(source_dir.path().join("original.txt"), "original content\n").await?;
    let mount_spec = format!("{}:/mydata", source_dir.path().display());

    // Baseline with an NFS share (nested profile: CONFIG_NFS_FS=y).
    let (mut child, pid) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &vm_name,
        "--network",
        "bridged",
        "--kernel-profile",
        "nested",
        "--nfs",
        &mount_spec,
        common::TEST_IMAGE,
    ])
    .await
    .context("spawning NFS baseline")?;
    common::poll_health_by_pid(pid, 180).await?;

    // The baseline's NFS works (it already went through one restore via the
    // NV2 snapshot-cache convergence) — and leave a marker for the clone.
    let content = common::exec_in_container(pid, &["cat", "/mydata/original.txt"]).await?;
    assert!(
        content.contains("original content"),
        "baseline NFS read failed: {}",
        content
    );
    common::exec_in_container(pid, &["echo 'from baseline' > /mydata/baseline.txt"]).await?;

    // Snapshot the baseline, then retire it (the clone reuses its guest IP).
    common::create_snapshot_by_pid(pid, &snap)
        .await
        .context("creating snapshot of NFS baseline")?;
    common::kill_process(pid).await;
    let _ = child.kill().await;

    // Restore a clone from the snapshot files.
    let (mut clone_child, clone_pid) = common::spawn_fcvm(&[
        "snapshot",
        "run",
        "--snapshot",
        &snap,
        "--name",
        &clone_name,
    ])
    .await
    .context("restoring NFS clone")?;
    common::poll_health_by_pid(clone_pid, 120).await?;

    // The clone sees the share: both pre-snapshot files are readable...
    let content = common::exec_in_container(clone_pid, &["cat", "/mydata/original.txt"]).await?;
    assert!(
        content.contains("original content"),
        "clone NFS read failed (export/remount missing after restore): {}",
        content
    );
    let content = common::exec_in_container(clone_pid, &["cat", "/mydata/baseline.txt"]).await?;
    assert!(
        content.contains("from baseline"),
        "clone missing baseline's NFS write: {}",
        content
    );

    // ...and clone writes round-trip through the host's nfsd to the source dir.
    common::exec_in_container(clone_pid, &["echo 'from clone' > /mydata/clone.txt"]).await?;
    let host_side = tokio::fs::read_to_string(source_dir.path().join("clone.txt"))
        .await
        .context("clone NFS write did not reach the host directory")?;
    assert!(
        host_side.contains("from clone"),
        "clone NFS write corrupted: {}",
        host_side
    );

    common::kill_process(clone_pid).await;
    let _ = clone_child.kill().await;
    let _ = std::fs::remove_dir_all(
        std::env::var("FCVM_DATA_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| std::path::PathBuf::from("/mnt/fcvm-btrfs/root"))
            .join("snapshots")
            .join(&snap),
    );
    Ok(())
}
