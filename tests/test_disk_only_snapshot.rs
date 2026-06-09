//! Disk-only snapshot capture (RFC #625, P2).
//!
//! `fcvm snapshot create --disk-only` freezes the guest over the exec vsock,
//! reflinks only the disk (no vCPU pause, no memory dump), and unfreezes. This
//! verifies the capture produces a `DiskOnly` snapshot (disk.raw + config.json,
//! NO memory.bin/vmstate.bin) and that the source VM survives the freeze/unfreeze.

#![cfg(feature = "integration-slow")]

mod common;

use anyhow::{Context, Result};
use std::path::PathBuf;

fn snapshot_dir() -> PathBuf {
    let data_dir = std::env::var("FCVM_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/mnt/fcvm-btrfs/root"));
    data_dir.join("snapshots")
}

#[tokio::test]
async fn test_disk_only_capture_artifacts_and_source_survives() -> Result<()> {
    let (name, _clone, snap, _serve) = common::unique_names("disk-only");

    // Boot a baseline VM (rootless, no sudo).
    let (mut child, pid) = common::spawn_fcvm_with_logs(
        &["podman", "run", "--name", &name, "nginx:alpine"],
        "disk-only-base",
    )
    .await?;
    common::poll_health_by_pid(pid, 120).await?;

    // Capture disk-only.
    let fcvm_path = common::find_fcvm_binary()?;
    let output = tokio::process::Command::new(&fcvm_path)
        .args([
            "snapshot",
            "create",
            "--pid",
            &pid.to_string(),
            "--tag",
            &snap,
            "--disk-only",
        ])
        .output()
        .await
        .context("running snapshot create --disk-only")?;
    assert!(
        output.status.success(),
        "snapshot create --disk-only failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    // Artifacts: disk.raw + config.json present; NO memory image (disk-only).
    let dir = snapshot_dir().join(&snap);
    assert!(dir.join("disk.raw").exists(), "disk.raw must exist");
    assert!(dir.join("config.json").exists(), "config.json must exist");
    assert!(
        !dir.join("memory.bin").exists(),
        "memory.bin must NOT exist for a disk-only snapshot"
    );
    assert!(
        !dir.join("vmstate.bin").exists(),
        "vmstate.bin must NOT exist for a disk-only snapshot"
    );

    // kind == DiskOnly in the persisted config.
    let cfg_json = std::fs::read_to_string(dir.join("config.json"))?;
    let cfg: serde_json::Value =
        serde_json::from_str(&cfg_json).context("parsing snapshot config.json")?;
    assert_eq!(
        cfg["kind"],
        "DiskOnly",
        "snapshot kind must be DiskOnly, got {:?}",
        cfg.get("kind")
    );

    // The source VM must survive the freeze/unfreeze — exec still responds.
    let alive = common::exec_in_vm(pid, &["echo", "still-alive"]).await?;
    assert!(
        alive.contains("still-alive"),
        "source VM unresponsive after disk-only capture (unfreeze failed?): {alive}"
    );

    // Cleanup.
    common::kill_process(pid).await;
    let _ = child.kill().await;
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}
