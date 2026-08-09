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

/// End-to-end: a disk-only clone cold-boots from the captured disk, preserves the
/// container's writable layer (the "work"), and regenerates its identity.
///
/// This is also the shutdown→start lifecycle test: the source VM is SHUT DOWN
/// before the clone boots, so the sequence (capture, stop, start) must behave
/// identically to an in-place reboot — work preserved, identity regenerated,
/// healthy. (Same provisioned-marker path; see tests/test_reboot.rs.)
///
/// 1. Boot a baseline VM, write a marker file *inside the container*.
/// 2. `snapshot create --disk-only` (freeze → reflink → unfreeze).
/// 3. Shut the source down (stop).
/// 4. `snapshot run --snapshot <tag>` cold-boots a fresh clone (start).
/// 5. The clone's container has the marker file (work preserved), the clone boots
///    healthy, and its machine-id differs from the source (identity regenerated).
#[tokio::test]
async fn test_disk_only_clone_preserves_work_and_regenerates_identity() -> Result<()> {
    let (name, _clone, snap, _serve) = common::unique_names("disk-only-clone");

    // Baseline VM (rootless, long-running container so it restarts cleanly).
    let (mut child, pid) = common::spawn_fcvm_with_logs(
        &["podman", "run", "--name", &name, "nginx:alpine"],
        "disk-only-clone-base",
    )
    .await?;
    common::poll_health_by_pid(pid, 120).await?;

    // Write a marker into the container's writable layer (the captured "work").
    // The exec helper already wraps argv in `sh -c "<joined>"`, so pass the
    // redirect as plain tokens — don't add another `sh -c` (it double-wraps).
    let token = format!("clone-token-{}", std::process::id());
    common::exec_in_container(pid, &["echo", &token, ">", "/work.txt"]).await?;
    let src_file = common::exec_in_container(pid, &["cat", "/work.txt"]).await?;
    assert!(
        src_file.contains(&token),
        "source container should hold the marker file, got: {src_file}"
    );
    let src_machine_id = common::exec_in_vm(pid, &["cat", "/etc/machine-id"])
        .await
        .unwrap_or_default();

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

    // SHUT DOWN the source first — the clone must be fully independent of it
    // (true stop→start sequence, not a side-by-side copy).
    common::kill_process(pid).await;
    let _ = child.kill().await;

    // Cold-boot a clone from the disk-only tag (the "start").
    let clone_name = format!("{snap}-c1");
    let (mut clone_child, clone_pid) = common::spawn_fcvm_with_logs(
        &[
            "snapshot",
            "run",
            "--snapshot",
            &snap,
            "--name",
            &clone_name,
        ],
        "disk-only-clone-c1",
    )
    .await?;
    common::poll_health_by_pid(clone_pid, 120).await?;
    let clone_state = fcvm::state::StateManager::new(fcvm::paths::state_dir())
        .load_state_by_pid(clone_pid)
        .await
        .context("loading disk-only clone state")?;
    assert!(
        clone_state.lifecycle_ready,
        "disk-only clone must publish lifecycle readiness after cold-boot resources are installed"
    );

    // The clone's container must carry the preserved file.
    let clone_file = common::exec_in_container(clone_pid, &["cat", "/work.txt"]).await?;
    assert!(
        clone_file.contains(&token),
        "clone container missing the preserved file (work not carried over): {clone_file}"
    );

    // Identity must be regenerated: the clone's machine-id differs from the source.
    let clone_machine_id = common::exec_in_vm(clone_pid, &["cat", "/etc/machine-id"])
        .await
        .unwrap_or_default();
    assert!(
        !clone_machine_id.trim().is_empty(),
        "clone machine-id should be regenerated (non-empty)"
    );
    assert_ne!(
        src_machine_id.trim(),
        clone_machine_id.trim(),
        "clone machine-id must differ from source (identity regenerated)"
    );

    // Cleanup (source already shut down above).
    common::kill_process(clone_pid).await;
    let _ = clone_child.kill().await;
    let _ = std::fs::remove_dir_all(snapshot_dir().join(&snap));
    Ok(())
}
