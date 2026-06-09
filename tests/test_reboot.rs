//! In-place VM reboot resilience (RFC #625 follow-on).
//!
//! A guest `reboot` must behave exactly like a disk-only clone cold boot: the VM
//! relaunches in place from the same provisioned disk and comes back healthy, with
//! the container's writable layer ("the work") preserved and its identity
//! regenerated. The fcvm process (and therefore its PID) stays stable across the
//! reboot — only the Firecracker child restarts.
//!
//! Both VM lifecycle paths are covered:
//!   * fresh `podman run` boot (`--no-snapshot` pins the run_vm_loop path)
//!   * snapshot-restored clone (`snapshot run --snapshot`, the snapshot.rs path)

#![cfg(feature = "integration-slow")]

mod common;

use anyhow::{Context, Result};
use std::time::{Duration, Instant};

/// True if the process is alive and not a zombie.
fn process_alive(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .map(|s| !s.contains(") Z "))
        .unwrap_or(false)
}

/// Reboot the guest and assert the SAME fcvm process relaunches it in place:
/// machine-id regenerates (positive witness of the re-boot), health recovers,
/// and the container's writable layer survives.
async fn reboot_and_assert_relaunch(pid: u32, token: &str) -> Result<()> {
    let mid_before = common::exec_in_vm(pid, &["cat", "/etc/machine-id"])
        .await
        .unwrap_or_default();

    // `reboot` goes through systemd, which starts fcvm-reboot-notify.service
    // (WantedBy=reboot.target) -> fc-agent --notify-reboot -> host relaunches
    // Firecracker in place. The exec dies mid-command as the VM resets.
    let _ = common::exec_in_vm(pid, &["reboot"]).await;

    let deadline = Instant::now() + Duration::from_secs(150);
    let mut recovered = false;
    while Instant::now() < deadline {
        assert!(
            process_alive(pid),
            "fcvm process (pid {pid}) must stay alive across an in-place reboot"
        );
        if let Ok(mid) = common::exec_in_vm(pid, &["cat", "/etc/machine-id"]).await {
            let mid = mid.trim().to_string();
            if !mid.is_empty() && mid != mid_before.trim() {
                recovered = true;
                break;
            }
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    assert!(
        recovered,
        "VM did not relaunch with a regenerated machine-id after reboot \
         (in-place reboot relaunch failed)"
    );

    // Health recovers through the normal monitor path.
    common::poll_health_by_pid(pid, 60).await?;

    // The container restarts shortly AFTER the VM relaunches (fc-agent regenerates
    // identity before `podman start`), so poll until the captured container is
    // running again and carries the preserved writable layer.
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        let last = match common::exec_in_container(pid, &["cat", "/work.txt"]).await {
            Ok(out) if out.contains(token) => break,
            Ok(out) => out,
            Err(e) => e.to_string(),
        };
        assert!(
            Instant::now() < deadline,
            "container did not come back with preserved work after reboot; last: {last}"
        );
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    Ok(())
}

/// Write a marker into the container's writable layer and verify it.
/// The exec helper already wraps argv in `sh -c "<joined>"`, so pass the redirect
/// as plain tokens (a nested `sh -c` would double-wrap).
async fn write_work_marker(pid: u32, token: &str) -> Result<()> {
    common::exec_in_container(pid, &["echo", token, ">", "/work.txt"]).await?;
    let got = common::exec_in_container(pid, &["cat", "/work.txt"]).await?;
    anyhow::ensure!(
        got.contains(token),
        "container should hold the marker file, got: {got}"
    );
    Ok(())
}

/// Fresh-boot path: `podman run --no-snapshot` pins the run_vm_loop lifecycle
/// (no snapshot-cache divert), so this exercises the podman reboot branch.
#[tokio::test]
async fn test_vm_reboot_comes_back_healthy_and_preserves_work() -> Result<()> {
    let (name, _clone, _snap, _serve) = common::unique_names("reboot");

    let (mut child, pid) = common::spawn_fcvm_with_logs(
        &[
            "podman",
            "run",
            "--name",
            &name,
            "--no-snapshot",
            "nginx:alpine",
        ],
        "reboot-base",
    )
    .await?;
    common::poll_health_by_pid(pid, 120).await?;

    let token = format!("reboot-token-{}", std::process::id());
    write_work_marker(pid, &token).await?;

    reboot_and_assert_relaunch(pid, &token).await?;

    common::kill_process(pid).await;
    let _ = child.kill().await;
    Ok(())
}

/// Data-disk preservation: an in-place reboot must NOT rebuild --disk-dir
/// images from the host directory — guest writes to the data disk live only in
/// the per-VM image and would be silently destroyed (confirmed review finding).
#[tokio::test]
async fn test_vm_reboot_preserves_disk_dir_writes() -> Result<()> {
    let (name, _clone, _snap, _serve) = common::unique_names("reboot-disk");

    // Host directory seeding the data disk.
    let host_dir =
        std::path::PathBuf::from(format!("/tmp/fcvm-reboot-disk-{}", std::process::id()));
    std::fs::create_dir_all(&host_dir)?;
    std::fs::write(host_dir.join("seed.txt"), "seed\n")?;

    let disk_spec = format!("{}:/data", host_dir.display());
    let (mut child, pid) = common::spawn_fcvm_with_logs(
        &[
            "podman",
            "run",
            "--name",
            &name,
            "--no-snapshot",
            "--disk-dir",
            &disk_spec,
            "nginx:alpine",
        ],
        "reboot-disk-base",
    )
    .await?;
    common::poll_health_by_pid(pid, 120).await?;

    // Write onto the data disk from inside the VM (lives ONLY in disk-dir-0.raw),
    // and fsync via remount-cycle-free sync so it reaches the image.
    let token = format!("disk-token-{}", std::process::id());
    common::exec_in_vm(pid, &["echo", &token, ">", "/data/guest-write.txt"]).await?;
    common::exec_in_vm(pid, &["sync"]).await?;
    let before = common::exec_in_vm(pid, &["cat", "/data/guest-write.txt"]).await?;
    assert!(
        before.contains(&token),
        "guest write missing before reboot: {before}"
    );

    // Reboot; wait for the relaunch (machine-id change is the positive witness).
    let mid_before = common::exec_in_vm(pid, &["cat", "/etc/machine-id"])
        .await
        .unwrap_or_default();
    let _ = common::exec_in_vm(pid, &["reboot"]).await;
    let deadline = Instant::now() + Duration::from_secs(150);
    loop {
        assert!(
            process_alive(pid),
            "fcvm process must stay alive across the reboot"
        );
        if let Ok(mid) = common::exec_in_vm(pid, &["cat", "/etc/machine-id"]).await {
            if !mid.trim().is_empty() && mid.trim() != mid_before.trim() {
                break;
            }
        }
        assert!(
            Instant::now() < deadline,
            "VM did not relaunch after reboot"
        );
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    common::poll_health_by_pid(pid, 60).await?;

    // THE assertion: the guest's data-disk write survived the relaunch (the disk
    // image was re-attached, not rebuilt from the host directory).
    let after = common::exec_in_vm(pid, &["cat", "/data/guest-write.txt"]).await?;
    assert!(
        after.contains(&token),
        "guest write to --disk-dir was destroyed by the reboot relaunch: {after}"
    );
    // The seeded file is still there too.
    let seed = common::exec_in_vm(pid, &["cat", "/data/seed.txt"]).await?;
    assert!(
        seed.contains("seed"),
        "seed file missing after reboot: {seed}"
    );

    common::kill_process(pid).await;
    let _ = child.kill().await;
    let _ = std::fs::remove_dir_all(&host_dir);
    Ok(())
}

/// Snapshot-restore path: a clone restored via `snapshot run --snapshot` must
/// also relaunch in place on guest reboot (the snapshot.rs run loop).
#[tokio::test]
async fn test_restored_clone_reboot_comes_back_healthy() -> Result<()> {
    let (name, clone_name, snap, _serve) = common::unique_names("reboot-clone");

    // Baseline VM; write the marker BEFORE the snapshot so the captured disk
    // carries it into the clone.
    let (mut child, pid) = common::spawn_fcvm_with_logs(
        &["podman", "run", "--name", &name, "nginx:alpine"],
        "reboot-clone-base",
    )
    .await?;
    common::poll_health_by_pid(pid, 120).await?;

    let token = format!("reboot-clone-token-{}", std::process::id());
    write_work_marker(pid, &token).await?;

    common::create_snapshot_by_pid(pid, &snap)
        .await
        .context("creating full snapshot")?;

    // Source no longer needed; the clone restores from the snapshot files.
    common::kill_process(pid).await;
    let _ = child.kill().await;

    // Direct-file restore (no serve process needed).
    let (mut clone_child, clone_pid) = common::spawn_fcvm_with_logs(
        &[
            "snapshot",
            "run",
            "--snapshot",
            &snap,
            "--name",
            &clone_name,
        ],
        "reboot-clone-c1",
    )
    .await?;
    common::poll_health_by_pid(clone_pid, 120).await?;

    // The restored clone carries the marker (memory+disk restore).
    let restored = common::exec_in_container(clone_pid, &["cat", "/work.txt"]).await?;
    assert!(
        restored.contains(&token),
        "restored clone missing the marker file: {restored}"
    );

    reboot_and_assert_relaunch(clone_pid, &token).await?;

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
