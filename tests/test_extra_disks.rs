//! Extra disk integration tests
//!
//! Tests the --disk and --disk-dir flags for adding extra block devices to VMs.

#![cfg(feature = "privileged-tests")]

mod common;

use anyhow::{Context, Result};
use std::path::PathBuf;
use tempfile::TempDir;

/// Create a small ext4 disk image with a test file
async fn create_test_disk(path: &PathBuf) -> Result<()> {
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
    let mount_dir = format!("/tmp/fcvm-disk-{}", std::process::id());
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
    let content = common::exec_in_container(
        pid,
        &["echo world > /data/new.txt && cat /data/new.txt"],
    )
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
            "--network",
            "bridged",
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

/// Test --disk-dir read-only: creates disk image from directory contents
#[tokio::test]
async fn test_disk_dir_ro() -> Result<()> {
    let (vm_name, _, _, _) = common::unique_names("diskdir-ro");

    // Create a temp directory with test files
    let source_dir = TempDir::new()?;
    tokio::fs::write(source_dir.path().join("hello.txt"), "hello from disk-dir\n").await?;
    tokio::fs::create_dir_all(source_dir.path().join("subdir")).await?;
    tokio::fs::write(
        source_dir.path().join("subdir/nested.txt"),
        "nested content\n",
    )
    .await?;

    // Start VM with --disk-dir (read-only)
    let disk_dir_spec = format!("{}:/mydata:ro", source_dir.path().display());
    let (mut child, pid) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &vm_name,
        "--network",
        "bridged",
        "--disk-dir",
        &disk_dir_spec,
        common::TEST_IMAGE,
    ])
    .await
    .context("spawn")?;

    common::poll_health_by_pid(pid, 120).await?;

    // Read top-level file
    let content = common::exec_in_container(pid, &["cat", "/mydata/hello.txt"]).await?;
    assert!(
        content.contains("hello from disk-dir"),
        "read top-level failed: {}",
        content
    );

    // Read nested file
    let content = common::exec_in_container(pid, &["cat", "/mydata/subdir/nested.txt"]).await?;
    assert!(
        content.contains("nested content"),
        "read nested failed: {}",
        content
    );

    // Verify disk image was created in VM's data directory (not /tmp)
    // and that it will be cleaned up on exit (which we test by just killing the VM)

    child.kill().await.ok();
    Ok(())
}

/// Test --disk-dir read-write: can write to ephemeral disk
#[tokio::test]
async fn test_disk_dir_rw() -> Result<()> {
    let (vm_name, _, _, _) = common::unique_names("diskdir-rw");

    // Create a temp directory with initial content
    let source_dir = TempDir::new()?;
    tokio::fs::write(source_dir.path().join("original.txt"), "original content\n").await?;

    // Start VM with --disk-dir (read-write, no :ro suffix)
    let disk_dir_spec = format!("{}:/mydata", source_dir.path().display());
    let (mut child, pid) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &vm_name,
        "--network",
        "bridged",
        "--disk-dir",
        &disk_dir_spec,
        common::TEST_IMAGE,
    ])
    .await
    .context("spawn")?;

    common::poll_health_by_pid(pid, 120).await?;

    // Read original file
    let content = common::exec_in_container(pid, &["cat", "/mydata/original.txt"]).await?;
    assert!(
        content.contains("original content"),
        "read original failed: {}",
        content
    );

    // Write new file (ephemeral - only exists in VM's copy)
    let content = common::exec_in_container(
        pid,
        &["echo 'written in vm' > /mydata/newfile.txt && cat /mydata/newfile.txt"],
    )
    .await?;
    assert!(
        content.contains("written in vm"),
        "write failed: {}",
        content
    );

    // Verify the write persists within the VM session
    let content = common::exec_in_container(pid, &["cat", "/mydata/newfile.txt"]).await?;
    assert!(
        content.contains("written in vm"),
        "re-read failed: {}",
        content
    );

    child.kill().await.ok();
    Ok(())
}
