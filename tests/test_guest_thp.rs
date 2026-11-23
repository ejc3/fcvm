//! `--guest-thp`: the guest's transparent huge page mode, applied at boot and at
//! every restore, and not part of the snapshot cache key.

#![cfg(feature = "integration-fast")]

mod common;

use anyhow::{Context, Result};
use std::time::{Duration, Instant};

/// The guest kernel's own THP mode, which a VM run without `--guest-thp` keeps. The guest
/// kernels set no `CONFIG_TRANSPARENT_HUGEPAGE_*` default of their own (kernel/*.conf), so it
/// is the Firecracker base config's: `always` on aarch64 and `madvise` on x86_64, where CI
/// read `madvise` (c228fb7a, Host-x64).
#[cfg(target_arch = "aarch64")]
const KERNEL_DEFAULT_THP: &str = "always";
#[cfg(target_arch = "x86_64")]
const KERNEL_DEFAULT_THP: &str = "madvise";

/// The selected mode, as the guest's sysfs file shows it (`always [madvise] never`).
async fn guest_thp(pid: u32) -> Result<String> {
    let out =
        common::exec_in_vm(pid, &["cat", "/sys/kernel/mm/transparent_hugepage/enabled"]).await?;
    let start = out.find('[').context("no selected mode")? + 1;
    let end = start + out[start..].find(']').context("no selected mode")?;
    Ok(out[start..end].to_string())
}

/// The guest's `vm.compaction_proactiveness`. fc-agent keeps it at 0 while the huge page
/// mode is `never`, unless the guest reports free pages, and at the kernel's own value
/// (20 unless the kernel says otherwise) in any other mode.
async fn guest_compaction_proactiveness(pid: u32) -> Result<String> {
    Ok(
        common::exec_in_vm(pid, &["cat", "/proc/sys/vm/compaction_proactiveness"])
            .await?
            .trim()
            .to_string(),
    )
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

/// A guest whose balloon reports free pages keeps the kernel's proactive compaction with
/// huge pages off: the reports are of 2 MiB blocks, which the proactive pass gathers out
/// of freed 4 KiB pages. fc-agent reads the balloon's negotiated feature from the
/// guest's sysfs, at boot and at the restore from the pre-start snapshot.
///
/// RED BEFORE THE FIX: `never` set the knob to 0 in this guest too.
#[tokio::test]
async fn test_a_guest_that_reports_free_pages_keeps_proactive_compaction() -> Result<()> {
    let (vm, _, _, _) = common::unique_names("guest-thp-fpr");
    let (mut child, pid) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &vm,
        "--network",
        "rootless",
        "--guest-thp",
        "never",
        "--balloon",
        "0",
        "--free-page-reporting",
        common::TEST_IMAGE,
    ])
    .await?;
    common::poll_health_by_pid(pid, 180).await?;
    assert_eq!(guest_thp(pid).await?, "never");
    assert_ne!(
        guest_compaction_proactiveness(pid).await?,
        "0",
        "a guest that reports free pages lost the proactive compaction that gathers them"
    );
    common::kill_process(pid).await;
    let _ = child.wait().await;
    Ok(())
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
    assert_eq!(
        guest_compaction_proactiveness(pid).await?,
        "0",
        "a guest without huge pages compacts memory ahead of demand"
    );

    common::create_snapshot_by_pid(pid, &snapshot).await?;
    let (mut serve, serve_pid) = common::start_memory_server(&snapshot).await?;

    let (mut a, a_pid) = common::spawn_clone(serve_pid, &clone_a).await?;
    common::poll_health_by_pid(a_pid, 120).await?;
    assert_eq!(
        guest_thp(a_pid).await?,
        "never",
        "a clone inherits the snapshot's mode"
    );
    assert_eq!(guest_compaction_proactiveness(a_pid).await?, "0");

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
    assert_ne!(
        guest_compaction_proactiveness(b_pid).await?,
        "0",
        "a clone that turned huge pages on did not get the kernel's proactive compaction back"
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
/// kernel's own mode ([`KERNEL_DEFAULT_THP`]), as its cold boot would. Without a
/// health-check URL the first run takes only the pre-start snapshot, which the second
/// restores. The test is about that snapshot, so both runs opt back in to snapshots,
/// which the SnapshotDisabled CI lanes switch off with FCVM_NO_SNAPSHOT.
#[tokio::test]
async fn test_a_cache_hit_without_guest_thp_runs_in_the_kernel_mode() -> Result<()> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos();
    // A container command no snapshot holds yet, so the first run is a miss.
    let cmd = format!("echo guest-thp-{nanos}; exec sleep 3600");
    let (miss_name, hit_name, _, _) = common::unique_names("guest-thp-hit");

    let (mut miss, miss_pid, miss_log) =
        common::spawn_fcvm_snapshots_enabled_with_env_and_log_path(
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
            &[],
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

    let (mut hit, hit_pid, hit_log) = common::spawn_fcvm_snapshots_enabled_with_env_and_log_path(
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
        &[],
    )
    .await?;
    common::poll_health_by_pid(hit_pid, 180).await?;
    let log = std::fs::read_to_string(&hit_log)?;
    anyhow::ensure!(
        log.contains("snapshot hit! Restoring"),
        "the second run did not restore the first run's snapshot; its log is {}",
        hit_log.display()
    );
    assert_eq!(guest_thp(hit_pid).await?, KERNEL_DEFAULT_THP);
    common::kill_process(hit_pid).await;
    let _ = hit.wait().await;
    Ok(())
}
