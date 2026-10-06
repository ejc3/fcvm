//! In-place VM reboot resilience (RFC #625 follow-on).
//!
//! A guest `reboot` must behave exactly like a disk-only clone cold boot: the VM
//! relaunches in place from the same provisioned disk and comes back healthy, with
//! the container's writable layer ("the work") preserved and its identity
//! regenerated. The fcvm process (and therefore its PID) stays stable across the
//! reboot, only the VMM child restarts.
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

/// True when `after` is a machine-id that differs from the one read before the reboot.
fn machine_id_regenerated(before: &str, after: &str) -> bool {
    let (before, after) = (before.trim(), after.trim());
    !before.is_empty() && !after.is_empty() && after != before
}

/// The regeneration witness must compare against a real pre-reboot machine-id. An empty baseline,
/// which a failed read produced through `unwrap_or_default()`, made any machine-id read after the
/// reboot count as regenerated, including one from a VM that never rebooted (CodeRabbit on #921).
#[test]
fn a_regenerated_machine_id_needs_a_real_baseline() {
    let before = "0123456789abcdef0123456789abcdef";
    let after = "fedcba9876543210fedcba9876543210\n";
    assert!(machine_id_regenerated(before, after));
    assert!(!machine_id_regenerated(before, before));
    assert!(!machine_id_regenerated(before, ""));
    assert!(
        !machine_id_regenerated("", after),
        "an empty pre-reboot machine-id witnesses nothing"
    );
    assert!(
        !machine_id_regenerated("\n", after),
        "a blank pre-reboot machine-id witnesses nothing"
    );
}

/// Reboot the guest and assert the SAME fcvm process relaunches it in place:
/// machine-id regenerates (positive witness of the re-boot), health recovers,
/// and the container's writable layer survives.
async fn reboot_and_assert_relaunch(pid: u32, token: &str) -> Result<()> {
    let mid_before = common::exec_in_vm(pid, &["cat", "/etc/machine-id"])
        .await
        .context("reading the machine-id before the reboot")?;
    anyhow::ensure!(
        !mid_before.trim().is_empty(),
        "the machine-id read before the reboot is empty, so a regenerated one cannot be told apart"
    );

    // `reboot` goes through systemd, which starts fcvm-reboot-notify.service
    // (WantedBy=reboot.target) -> fc-agent --notify-reboot -> host relaunches
    // the VMM in place. The exec dies mid-command as the VM resets.
    let _ = common::exec_in_vm(pid, &["reboot"]).await;

    let deadline = Instant::now() + Duration::from_secs(150);
    let mut recovered = false;
    while Instant::now() < deadline {
        assert!(
            process_alive(pid),
            "fcvm process (pid {pid}) must stay alive across an in-place reboot"
        );
        if let Ok(mid) = common::exec_in_vm(pid, &["cat", "/etc/machine-id"]).await {
            if machine_id_regenerated(&mid_before, &mid) {
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

/// A normal Cloud Hypervisor VM must consume reboot intent before its final exit.
// Host-Root CI provisions Cloud Hypervisor; container-test-all does not.
#[cfg(feature = "privileged-tests")]
#[tokio::test]
async fn test_cloud_hypervisor_reboot_recovers_and_then_exits() -> Result<()> {
    fcvm::commands::common::find_cloud_hypervisor()
        .context("CH reboot test requires the backend")?;
    let (name, _, _, _) = common::unique_names("ch-reboot");
    let (mut child, pid) = common::spawn_fcvm_with_logs(
        &[
            "podman",
            "run",
            "--name",
            &name,
            "--hypervisor",
            "cloud-hypervisor",
            "--no-snapshot",
            "nginx:alpine",
            "sh",
            "-c",
            "while [ ! -e /stop ]; do sleep 1; done",
        ],
        "ch-reboot-base",
    )
    .await?;
    let result = async {
        common::poll_health_by_pid(pid, 120).await?;
        let token = format!("ch-reboot-token-{pid}");
        write_work_marker(pid, &token).await?;
        reboot_and_assert_relaunch(pid, &token).await?;

        // The exec response may disappear when its container exits; the process
        // exit below proves the stop marker was consumed and the final boot ended.
        let _ = common::exec_in_container(pid, &["touch", "/stop"]).await;
        let status = tokio::time::timeout(Duration::from_secs(60), child.wait())
            .await
            .context("CH VM did not terminate after its container exited")??;
        anyhow::ensure!(status.success(), "CH VM final exit must be zero: {status}");
        Ok::<_, anyhow::Error>(())
    }
    .await;
    if child.try_wait()?.is_none() {
        common::kill_process(pid).await;
        let _ = child.kill().await;
    }
    result
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
        .context("reading the machine-id before the reboot")?;
    anyhow::ensure!(
        !mid_before.trim().is_empty(),
        "the machine-id read before the reboot is empty, so a regenerated one cannot be told apart"
    );
    let _ = common::exec_in_vm(pid, &["reboot"]).await;
    let deadline = Instant::now() + Duration::from_secs(150);
    loop {
        assert!(
            process_alive(pid),
            "fcvm process must stay alive across the reboot"
        );
        if let Ok(mid) = common::exec_in_vm(pid, &["cat", "/etc/machine-id"]).await {
            if machine_id_regenerated(&mid_before, &mid) {
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

/// A clone restored from a snapshot of a `--balloon` VM keeps its balloon device
/// across a guest reboot. The relaunch is a cold boot from the clone's disk, and it
/// used to attach no balloon device whatever the source VM had (#1052).
#[tokio::test]
async fn test_restored_clone_reboot_keeps_its_balloon() -> Result<()> {
    const BALLOON_MIB: u32 = 64;
    let balloon = BALLOON_MIB.to_string();
    let (name, clone_name, snap, _serve) = common::unique_names("reboot-balloon");

    // --no-snapshot makes the source a cold boot, so what its state records comes
    // from its own --balloon. A cache hit would copy the cached snapshot's record,
    // and a snapshot written by an older build has none.
    let (mut child, pid) = common::spawn_fcvm_with_logs(
        &[
            "podman",
            "run",
            "--name",
            &name,
            "--no-snapshot",
            "--balloon",
            &balloon,
            "nginx:alpine",
        ],
        "reboot-balloon-base",
    )
    .await?;
    let token = format!("reboot-balloon-token-{}", std::process::id());
    let snapshotted = async {
        common::poll_health_by_pid(pid, 120).await?;
        // Control for the instrument: the source reports the target it booted with.
        let source = common::balloon_stats_by_pid(pid)
            .await
            .context("reading the source VM's balloon")?;
        anyhow::ensure!(
            source.target_mib == BALLOON_MIB,
            "the source VM's balloon target is {} MiB, not the {BALLOON_MIB} it was started with",
            source.target_mib
        );
        // reboot_and_assert_relaunch looks for the marker in the clone.
        write_work_marker(pid, &token).await?;
        common::create_snapshot_by_pid(pid, &snap)
            .await
            .context("creating full snapshot")
    }
    .await;
    // The clone restores from the snapshot files, with the source gone.
    common::kill_process(pid).await;
    let _ = child.kill().await;

    let result = async {
        snapshotted?;
        let (mut clone_child, clone_pid) = common::spawn_fcvm_with_logs(
            &[
                "snapshot",
                "run",
                "--snapshot",
                &snap,
                "--name",
                &clone_name,
            ],
            "reboot-balloon-c1",
        )
        .await?;
        let checked = async {
            common::poll_health_by_pid(clone_pid, 120).await?;
            // Second control: the memory restore brings the device back from the
            // VMM state, so the clone has it before the reboot.
            let restored = common::balloon_stats_by_pid(clone_pid)
                .await
                .context("reading the restored clone's balloon before the reboot")?;
            anyhow::ensure!(
                restored.target_mib == BALLOON_MIB,
                "the restored clone's balloon target is {} MiB before the reboot, not {BALLOON_MIB}",
                restored.target_mib
            );

            // reboot_and_assert_relaunch reports a failed relaunch by panicking. It runs
            // as its own task, so the panic comes back here as an error and the clone
            // and the snapshot are still cleaned up below.
            let relaunch_token = token.clone();
            tokio::spawn(async move {
                reboot_and_assert_relaunch(clone_pid, &relaunch_token).await
            })
            .await
            .context("the relaunch check panicked")??;

            let rebooted = common::balloon_stats_by_pid(clone_pid)
                .await
                .context("reading the clone's balloon after the reboot")?;
            anyhow::ensure!(
                rebooted.target_mib == BALLOON_MIB,
                "the rebooted clone's balloon target is {} MiB, not the source's {BALLOON_MIB}",
                rebooted.target_mib
            );
            let state = fcvm::state::StateManager::new(fcvm::paths::state_dir())
                .load_state_by_pid(clone_pid)
                .await
                .context("loading the clone's state")?;
            anyhow::ensure!(
                state.config.balloon_mib == Some(BALLOON_MIB),
                "the clone's state records balloon target {:?}, not Some({BALLOON_MIB})",
                state.config.balloon_mib
            );
            Ok(())
        }
        .await;
        common::kill_process(clone_pid).await;
        let _ = clone_child.kill().await;
        checked
    }
    .await;
    let _ = std::fs::remove_dir_all(fcvm::paths::snapshot_dir().join(&snap));
    result
}

/// The document the host serves on a VM's boot-plan vsock port, or None when nothing
/// listens there. A guest that connects to that port reads the same document, with
/// the host time of its own connection.
async fn served_boot_plan(pid: u32) -> Result<Option<serde_json::Value>> {
    use tokio::io::AsyncReadExt;

    let state = fcvm::state::StateManager::new(fcvm::paths::state_dir())
        .load_state_by_pid(pid)
        .await
        .with_context(|| format!("loading the state of fcvm process {pid}"))?;
    let base = state
        .config
        .vsock_socket_path
        .context("the VM's state records no vsock socket path")?;
    let socket = format!(
        "{}_{}",
        base.display(),
        fcvm::commands::common::VSOCK_BOOTPLAN_PORT
    );
    let mut stream = match tokio::net::UnixStream::connect(&socket).await {
        Ok(stream) => stream,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            ) =>
        {
            return Ok(None)
        }
        Err(error) => return Err(error).with_context(|| format!("connecting to {socket}")),
    };
    let mut bytes = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut bytes))
        .await
        .with_context(|| format!("{socket} accepted and sent no complete document in 5s"))?
        .with_context(|| format!("reading {socket}"))?;
    serde_json::from_slice(&bytes)
        .map(Some)
        .with_context(|| format!("{socket} served something that is not JSON"))
}

/// A restored clone that rebooted keeps running after a snapshot of it, and that
/// snapshot restores.
///
/// A restore serves its epoch to the guest on the boot-plan vsock port. The agent
/// of a guest that then reboots has handled no epoch, and it reads that port
/// whenever its snapshot boundary is armed, which a snapshot does. If the
/// restore's listener were still up after the relaunch, a snapshot would hand the
/// relaunched guest the old epoch: it would run the restore sequence on a VM that
/// was not restored, fail, and shut down.
#[tokio::test]
async fn test_snapshot_of_a_relaunched_clone_leaves_it_running() -> Result<()> {
    let (name, clone_name, snap, _serve) = common::unique_names("relaunch-snap");
    let second_snap = format!("{snap}-b");
    let second_clone_name = format!("{clone_name}-b");
    let snapshots = fcvm::paths::snapshot_dir();
    let token = format!("relaunch-snap-token-{}", std::process::id());

    let (mut child, pid) = common::spawn_fcvm_with_logs(
        &["podman", "run", "--name", &name, "nginx:alpine"],
        "relaunch-snap-base",
    )
    .await?;
    let snapshotted = async {
        common::poll_health_by_pid(pid, 120).await?;
        // reboot_and_assert_relaunch looks for the marker in the clone.
        write_work_marker(pid, &token).await?;
        common::create_snapshot_by_pid(pid, &snap)
            .await
            .context("creating the source VM's snapshot")
    }
    .await;
    // The clones restore from the snapshot files, with the source gone.
    common::kill_process(pid).await;
    let _ = child.kill().await;

    let result = async {
        snapshotted?;
        let (mut clone_child, clone_pid) = common::spawn_fcvm_with_logs(
            &[
                "snapshot",
                "run",
                "--snapshot",
                &snap,
                "--name",
                &clone_name,
            ],
            "relaunch-snap-clone",
        )
        .await?;
        let checked = async {
            common::poll_health_by_pid(clone_pid, 120).await?;
            // Control: the restored clone's port serves the restore's epoch, so a
            // port that serves nothing after the relaunch is not a blind probe.
            let served = served_boot_plan(clone_pid).await?;
            anyhow::ensure!(
                served
                    .as_ref()
                    .and_then(|document| document.get("restore-epoch"))
                    .is_some_and(|epoch| epoch.is_string()),
                "control: the restored clone's boot-plan port serves no restore-epoch: {served:?}"
            );

            // reboot_and_assert_relaunch reports a failed relaunch by panicking. It runs
            // as its own task, so the panic comes back here as an error and the clone
            // and the snapshots are still cleaned up below.
            let relaunch_token = token.clone();
            tokio::spawn(
                async move { reboot_and_assert_relaunch(clone_pid, &relaunch_token).await },
            )
            .await
            .context("the relaunch check panicked")??;

            let mut wrong = Vec::new();
            if let Some(document) = served_boot_plan(clone_pid).await? {
                wrong.push(format!(
                    "after the relaunch the host still serves the restore's document on the \
                     boot-plan port: {document}"
                ));
            }
            if let Err(error) = common::create_snapshot_by_pid(clone_pid, &second_snap).await {
                wrong.push(format!(
                    "creating a snapshot of the relaunched clone: {error:#}"
                ));
                return Ok((wrong, false));
            }
            let until = Instant::now() + Duration::from_secs(5);
            while Instant::now() < until {
                if !process_alive(clone_pid) {
                    wrong.push(
                        "the clone's fcvm process exited within 5s of the snapshot of it"
                            .to_string(),
                    );
                    break;
                }
                if let Err(error) = common::exec_in_vm(clone_pid, &["/usr/bin/true"]).await {
                    wrong.push(format!(
                        "the relaunched clone stopped answering exec within 5s of the snapshot \
                         of it: {error:#}"
                    ));
                    break;
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            Ok((wrong, true))
        }
        .await;
        common::kill_process(clone_pid).await;
        let _ = clone_child.kill().await;
        let (mut wrong, snapshotted): (Vec<String>, bool) = checked?;

        // Without a snapshot of the relaunched clone there is nothing to restore.
        if snapshotted {
            let (mut second_child, second_pid) = common::spawn_fcvm_with_logs(
                &[
                    "snapshot",
                    "run",
                    "--snapshot",
                    &second_snap,
                    "--name",
                    &second_clone_name,
                ],
                "relaunch-snap-clone-b",
            )
            .await?;
            let restored = async {
                common::poll_health_by_pid(second_pid, 120).await?;
                let work = common::exec_in_container(second_pid, &["cat", "/work.txt"]).await?;
                anyhow::ensure!(
                    work.contains(&token),
                    "it does not hold the marker file: {work}"
                );
                Ok(())
            }
            .await;
            common::kill_process(second_pid).await;
            let _ = second_child.kill().await;
            if let Err(error) = restored {
                wrong.push(format!(
                    "a clone restored from the relaunched clone's snapshot: {error:#}"
                ));
            }
        }
        anyhow::ensure!(wrong.is_empty(), "{}", wrong.join("\n"));
        Ok(())
    }
    .await;
    for dir in [
        snap.clone(),
        second_snap.clone(),
        format!("{second_snap}.creating"),
    ] {
        let _ = std::fs::remove_dir_all(snapshots.join(dir));
    }
    result
}

/// A clone restored through a memory server stops depending on that server once
/// its guest has rebooted: the relaunch is a cold boot from the clone's disk. The
/// server's exit must leave it running.
///
/// The clone's process watches its server, so that a clone with unserved pages is
/// failed and not left frozen. If that watch outlived the relaunch, the server's
/// exit would fail a VM that has no page left for the server to serve.
#[tokio::test]
async fn test_memory_server_clone_outlives_its_server_after_a_reboot() -> Result<()> {
    let (name, clone_name, snap, _serve) = common::unique_names("reboot-served");
    let snapshots = fcvm::paths::snapshot_dir();
    let token = format!("reboot-served-token-{}", std::process::id());

    let (mut child, pid) = common::spawn_fcvm_with_logs(
        &["podman", "run", "--name", &name, "nginx:alpine"],
        "reboot-served-base",
    )
    .await?;
    let snapshotted = async {
        common::poll_health_by_pid(pid, 120).await?;
        // reboot_and_assert_relaunch looks for the marker in the clone.
        write_work_marker(pid, &token).await?;
        common::create_snapshot_by_pid(pid, &snap)
            .await
            .context("creating the source VM's snapshot")
    }
    .await;
    // The clone restores from the snapshot files, with the source gone.
    common::kill_process(pid).await;
    let _ = child.kill().await;

    let result = async {
        snapshotted?;
        let (mut serve_child, serve_pid) = common::start_memory_server(&snap).await?;
        let served = async {
            let (mut clone_child, clone_pid) = common::spawn_clone(serve_pid, &clone_name).await?;
            let checked = async {
                common::poll_health_by_pid(clone_pid, 120).await?;
                // Control: before the reboot the clone belongs to the server.
                let state = fcvm::state::StateManager::new(fcvm::paths::state_dir())
                    .load_state_by_pid(clone_pid)
                    .await
                    .context("loading the clone's state")?;
                anyhow::ensure!(
                    state.config.serve_pid == Some(serve_pid),
                    "control: the clone's state names memory server {:?}, not {serve_pid}",
                    state.config.serve_pid
                );

                // reboot_and_assert_relaunch reports a failed relaunch by panicking. It
                // runs as its own task, so the panic comes back here as an error and
                // the clone, the server and the snapshot are still cleaned up below.
                let relaunch_token = token.clone();
                tokio::spawn(async move {
                    reboot_and_assert_relaunch(clone_pid, &relaunch_token).await
                })
                .await
                .context("the relaunch check panicked")??;

                // The server stops. Its shutdown leaves this clone alone: the relaunch
                // took the clone out of the server's list.
                common::kill_process(serve_pid).await;
                let deadline = Instant::now() + Duration::from_secs(20);
                while process_alive(serve_pid) {
                    anyhow::ensure!(
                        Instant::now() < deadline,
                        "the memory server (pid {serve_pid}) is still running 20s after it was \
                         told to stop"
                    );
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }

                let until = Instant::now() + Duration::from_secs(5);
                while Instant::now() < until {
                    anyhow::ensure!(
                        process_alive(clone_pid),
                        "the clone's fcvm process exited within 5s of its memory server's exit"
                    );
                    common::exec_in_vm(clone_pid, &["/usr/bin/true"])
                        .await
                        .context(
                            "the relaunched clone stopped answering exec within 5s of its \
                             memory server's exit",
                        )?;
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
                Ok(())
            }
            .await;
            common::kill_process(clone_pid).await;
            let _ = clone_child.kill().await;
            checked
        }
        .await;
        common::kill_process(serve_pid).await;
        let _ = serve_child.kill().await;
        served
    }
    .await;
    let _ = std::fs::remove_dir_all(snapshots.join(&snap));
    result
}
