//! `--guest-thp`: the guest's transparent huge page mode, applied at boot and at
//! every restore, and not part of the snapshot cache key.

#![cfg(feature = "integration-fast")]

mod common;

use anyhow::{Context, Result};
use std::time::{Duration, Instant};

/// The selected mode, as the guest's sysfs file shows it (`always [madvise] never`).
async fn guest_thp(pid: u32) -> Result<String> {
    let out =
        common::exec_in_vm(pid, &["cat", "/sys/kernel/mm/transparent_hugepage/enabled"]).await?;
    let start = out.find('[').context("no selected mode")? + 1;
    let end = start + out[start..].find(']').context("no selected mode")?;
    Ok(out[start..end].to_string())
}

async fn wait_for_log(path: &std::path::Path, needle: &str, timeout: Duration) -> Result<()> {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if std::fs::read_to_string(path).is_ok_and(|log| log.contains(needle)) {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    anyhow::bail!("{} never logged {needle:?}", path.display())
}

/// A VM booted with `--guest-thp never` runs in that mode, a clone of its snapshot
/// inherits it, and a clone that names another mode gets that one.
#[tokio::test]
async fn test_guest_thp_is_applied_at_boot_and_inherited_or_overridden_by_clones() -> Result<()> {
    let (vm, clone_a, snapshot, _) = common::unique_names("guest-thp");
    let clone_b = format!("{clone_a}-b");
    let (mut child, pid) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &vm,
        "--network",
        "rootless",
        "--guest-thp",
        "never",
        common::TEST_IMAGE,
    ])
    .await?;
    common::poll_health_by_pid(pid, 180).await?;
    assert_eq!(guest_thp(pid).await?, "never", "the booted VM");

    common::create_snapshot_by_pid(pid, &snapshot).await?;
    let (mut serve, serve_pid) = common::start_memory_server(&snapshot).await?;

    let (mut a, a_pid) = common::spawn_clone(serve_pid, &clone_a).await?;
    common::poll_health_by_pid(a_pid, 120).await?;
    assert_eq!(
        guest_thp(a_pid).await?,
        "never",
        "a clone inherits the snapshot's mode"
    );

    let serve_pid_str = serve_pid.to_string();
    let (mut b, b_pid) = common::spawn_fcvm(&[
        "snapshot",
        "run",
        "--pid",
        &serve_pid_str,
        "--name",
        &clone_b,
        "--guest-thp",
        "madvise",
    ])
    .await?;
    common::poll_health_by_pid(b_pid, 120).await?;
    assert_eq!(
        guest_thp(b_pid).await?,
        "madvise",
        "a clone's own mode wins"
    );

    for (child, pid) in [
        (&mut b, b_pid),
        (&mut a, a_pid),
        (&mut serve, serve_pid),
        (&mut child, pid),
    ] {
        common::kill_process(pid).await;
        let _ = child.wait().await;
    }
    Ok(())
}

/// The mode is not in the snapshot cache key, so a run that gives none and restores
/// the snapshot of one that gave `never` must not inherit it: it runs in the guest
/// kernel's own mode, `always` in the default kernel profile
/// (`CONFIG_TRANSPARENT_HUGEPAGE_ALWAYS=y`), as its cold boot would. Without a
/// health-check URL the first run takes only the pre-start snapshot, which the second
/// restores.
#[tokio::test]
async fn test_a_cache_hit_without_guest_thp_runs_in_the_kernel_mode() -> Result<()> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos();
    // A container command no snapshot holds yet, so the first run is a miss.
    let cmd = format!("echo guest-thp-{nanos}; exec sleep 3600");
    let (miss_name, hit_name, _, _) = common::unique_names("guest-thp-hit");

    let (mut miss, miss_pid, miss_log) = common::spawn_fcvm_with_log_path(
        &[
            "podman",
            "run",
            "--name",
            &miss_name,
            "--network",
            "rootless",
            "--guest-thp",
            "never",
            common::TEST_IMAGE,
            "sh",
            "-c",
            &cmd,
        ],
        &miss_name,
    )
    .await?;
    common::poll_health_by_pid(miss_pid, 180).await?;
    assert_eq!(guest_thp(miss_pid).await?, "never");
    wait_for_log(
        &miss_log,
        "Pre-start snapshot created successfully",
        Duration::from_secs(120),
    )
    .await?;
    common::kill_process(miss_pid).await;
    let _ = miss.wait().await;

    let (mut hit, hit_pid, hit_log) = common::spawn_fcvm_with_log_path(
        &[
            "podman",
            "run",
            "--name",
            &hit_name,
            "--network",
            "rootless",
            common::TEST_IMAGE,
            "sh",
            "-c",
            &cmd,
        ],
        &hit_name,
    )
    .await?;
    common::poll_health_by_pid(hit_pid, 180).await?;
    let log = std::fs::read_to_string(&hit_log)?;
    anyhow::ensure!(
        log.contains("snapshot hit! Restoring"),
        "the second run did not restore the first run's snapshot; its log is {}",
        hit_log.display()
    );
    assert_eq!(guest_thp(hit_pid).await?, "always");
    common::kill_process(hit_pid).await;
    let _ = hit.wait().await;
    Ok(())
}
