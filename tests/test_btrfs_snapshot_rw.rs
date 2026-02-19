//! Verifies btrfs storage is read-write after snapshot restore.
//!
//! Root cause: if the disk is copied AFTER VM resume, it contains post-resume
//! writes that don't match the snapshot's memory state. On restore, btrfs
//! detects the inconsistency and goes read-only. The fix: copy the disk
//! while the VM is still paused (memory and disk are from the same instant).
//!
//! Test flow:
//! 1. Cold boot with --user and --kernel-profile btrfs
//! 2. Wait for healthy → pre-start snapshot created (disk copied while paused)
//! 3. Kill VM
//! 4. Warm start from snapshot
//! 5. Verify btrfs mount is rw (not ro) — no remount needed
//! 6. Verify podman exec works (requires writable storage lock)

#![cfg(feature = "privileged-tests")]

mod common;

use anyhow::{Context, Result};
use std::time::Duration;

#[tokio::test]
async fn test_btrfs_rw_after_snapshot_restore() -> Result<()> {
    println!("\nBtrfs read-write after snapshot restore");
    println!("=======================================");

    let (vm_name, _, _, _) = common::unique_names("btrfs-rw");
    // Use the real user UID (not root) to match rootless container mode.
    // Under sudo, getuid() returns 0 but SUDO_UID has the real user.
    // Using uid 0 with --privileged causes fc-agent to use --cap-add instead
    // of --privileged (user namespace limitation), which can't mount sysfs.
    let uid: u32 = std::env::var("SUDO_UID")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| nix::unistd::getuid().as_raw());
    let gid: u32 = std::env::var("SUDO_GID")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| nix::unistd::getgid().as_raw());
    let user_spec = format!("{}:{}", uid, gid);
    let username = nix::unistd::User::from_uid(nix::unistd::Uid::from_raw(uid))
        .ok()
        .flatten()
        .map(|u| u.name)
        .unwrap_or_else(|| format!("u{}", uid));

    // Phase 1: Cold boot — creates pre-start snapshot
    println!(
        "Phase 1: Cold boot with --kernel-profile btrfs --user {}...",
        user_spec
    );
    let (mut child, fcvm_pid) = common::spawn_fcvm_with_logs(
        &[
            "podman",
            "run",
            "--name",
            &vm_name,
            "--kernel-profile",
            "btrfs",
            "--user",
            &user_spec,
            "--privileged",
            common::ALPINE_IMAGE,
            "sleep",
            "300",
        ],
        &vm_name,
    )
    .await
    .context("spawning cold boot VM")?;

    println!("  fcvm PID: {}", fcvm_pid);
    println!("  Waiting for healthy...");

    let healthy = tokio::time::timeout(
        Duration::from_secs(600),
        common::poll_health_by_pid(fcvm_pid, 600),
    )
    .await;

    match healthy {
        Ok(Ok(_)) => println!("  Cold boot healthy"),
        Ok(Err(e)) => {
            common::kill_process(fcvm_pid).await;
            let _ = child.wait().await;
            return Err(e).context("cold boot failed to become healthy");
        }
        Err(_) => {
            common::kill_process(fcvm_pid).await;
            let _ = child.wait().await;
            anyhow::bail!("cold boot timed out waiting for healthy");
        }
    }

    // Verify btrfs storage driver and rw on cold boot.
    // The btrfs mount at /var/lib/containers/storage is set up asynchronously
    // by fc-agent during boot, so we need to poll until it's ready.
    println!("  Checking btrfs mount (cold boot)...");
    let mount_info = {
        let mount_start = std::time::Instant::now();
        loop {
            if mount_start.elapsed() > Duration::from_secs(60) {
                anyhow::bail!(
                    "cold boot: btrfs mount at /var/lib/containers/storage not ready after 60s"
                );
            }
            match common::exec_in_vm(
                fcvm_pid,
                &[
                    "findmnt",
                    "-n",
                    "-o",
                    "FSTYPE,OPTIONS",
                    "/var/lib/containers/storage",
                ],
            )
            .await
            {
                Ok(info) if info.contains("btrfs") => break info,
                _ => tokio::time::sleep(Duration::from_millis(500)).await,
            }
        }
    };
    println!("  Mount: {}", mount_info.trim());
    assert!(
        mount_info.contains("btrfs") && mount_info.contains("rw"),
        "cold boot: expected btrfs rw, got: {}",
        mount_info.trim()
    );

    // Wait for snapshot
    tokio::time::sleep(Duration::from_secs(10)).await;

    // Kill cold boot VM
    println!("  Killing cold boot VM...");
    common::kill_process(fcvm_pid).await;
    let _ = child.wait().await;
    tokio::time::sleep(Duration::from_secs(3)).await;

    // Phase 2: Warm start — restores from snapshot
    println!("\nPhase 2: Warm start from snapshot...");
    let (mut child2, fcvm_pid2) = common::spawn_fcvm_with_logs(
        &[
            "podman",
            "run",
            "--name",
            &vm_name,
            "--kernel-profile",
            "btrfs",
            "--user",
            &user_spec,
            "--privileged",
            common::ALPINE_IMAGE,
            "sleep",
            "300",
        ],
        &format!("{}-warm", vm_name),
    )
    .await
    .context("spawning warm start VM")?;

    println!("  fcvm PID: {}", fcvm_pid2);
    println!("  Waiting for healthy...");

    let healthy2 = tokio::time::timeout(
        Duration::from_secs(120),
        common::poll_health_by_pid(fcvm_pid2, 120),
    )
    .await;

    match healthy2 {
        Ok(Ok(_)) => println!("  Warm start healthy"),
        Ok(Err(e)) => {
            common::kill_process(fcvm_pid2).await;
            let _ = child2.wait().await;
            return Err(e).context("warm start failed to become healthy");
        }
        Err(_) => {
            common::kill_process(fcvm_pid2).await;
            let _ = child2.wait().await;
            anyhow::bail!("warm start timed out waiting for healthy");
        }
    }

    // Verify btrfs is rw after snapshot restore (no remount hack needed).
    // Poll for btrfs mount readiness in case restore takes a moment.
    println!("  Checking btrfs mount (warm start)...");
    let mount_info2 = {
        let mount_start = std::time::Instant::now();
        loop {
            if mount_start.elapsed() > Duration::from_secs(60) {
                anyhow::bail!(
                    "warm start: btrfs mount at /var/lib/containers/storage not ready after 60s"
                );
            }
            match common::exec_in_vm(
                fcvm_pid2,
                &[
                    "findmnt",
                    "-n",
                    "-o",
                    "FSTYPE,OPTIONS",
                    "/var/lib/containers/storage",
                ],
            )
            .await
            {
                Ok(info) if info.contains("btrfs") => break info,
                _ => tokio::time::sleep(Duration::from_millis(500)).await,
            }
        }
    };
    println!("  Mount: {}", mount_info2.trim());
    assert!(
        mount_info2.contains("btrfs") && mount_info2.contains("rw"),
        "warm start: expected btrfs rw, got: {}",
        mount_info2.trim()
    );
    assert!(
        !mount_info2.split(',').any(|o| o.trim() == "ro"),
        "warm start: btrfs should NOT be ro, got: {}",
        mount_info2.trim()
    );

    // Verify podman exec works (requires writable storage lock)
    println!("  Verifying podman exec works (warm start)...");
    let exec_result = common::exec_in_vm(
        fcvm_pid2,
        &[
            "runuser",
            "-u",
            &username,
            "--",
            "podman",
            "exec",
            "fcvm-container",
            "echo",
            "warm-ok",
        ],
    )
    .await?;
    println!("  podman exec: {}", exec_result.trim());
    assert!(
        exec_result.contains("warm-ok"),
        "podman exec should work after snapshot restore, got: {}",
        exec_result.trim()
    );

    // Cleanup
    println!("  Stopping VM...");
    common::kill_process(fcvm_pid2).await;
    let _ = child2.wait().await;

    println!("  PASSED: btrfs rw after snapshot restore (disk copied while paused)");
    Ok(())
}
