//! Integration tests for clone restore fixes:
//! - Clock sync after snapshot restore (chronyd + MMDS time sync), to well under a second
//! - A cold-booted VM agrees with the host's clock once healthy
//! - A snapshot's source agrees with the host's clock once `snapshot create` returns
//! - A clone agrees with the host's clock after a restore that stalls in the guest
//! - exact restore cleanup re-establishes gateway and container connectivity
//! - --no-swap creates dedicated cgroup with memory.swap.max=0
//! - --no-dirty-tracking passes track_dirty_pages=false

#![cfg(feature = "integration-slow")]
#![cfg_attr(not(feature = "privileged-tests"), allow(unused_imports))]

mod common;

use anyhow::{Context, Result};
use std::time::Duration;

/// After snapshot restore, the VM clock should be within a few seconds of host
/// time (not stuck at snapshot time). This verifies the host-time clock step
/// in fc-agent/src/restore.rs.
#[cfg(feature = "privileged-tests")]
#[tokio::test]
async fn test_clock_synced_after_clone_restore() -> Result<()> {
    let fixture = common::SnapshotFixture::new("clocksync", "bridged").await?;

    // Wait 3 seconds so snapshot time drifts from real time
    println!("  Waiting 3s for clock drift...");
    tokio::time::sleep(Duration::from_secs(3)).await;

    // Clone
    let (_, clone_name, _, _) = common::unique_names("clocksync-c");
    let (_clone_child, clone_pid) = common::spawn_fcvm_with_logs(
        &[
            "snapshot",
            "run",
            "--pid",
            &fixture.serve_pid.to_string(),
            "--name",
            &clone_name,
        ],
        &clone_name,
    )
    .await?;
    common::poll_health_by_pid(clone_pid, 120).await?;
    println!("  ✓ Clone healthy (PID: {})", clone_pid);

    // Check clock inside clone VM — should be within 5s of host time
    let host_epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();

    let vm_time = common::exec_in_vm(clone_pid, &["date", "+%s"]).await?;
    let vm_epoch: u64 = vm_time.trim().parse().context("parsing VM epoch")?;
    let drift = (host_epoch as i64 - vm_epoch as i64).unsigned_abs();

    println!(
        "  Host epoch: {}, VM epoch: {}, drift: {}s",
        host_epoch, vm_epoch, drift
    );
    assert!(
        drift < 5,
        "VM clock drifted {}s from host after restore — clock sync failed",
        drift
    );
    println!("  ✓ Clock synced (drift={}s)", drift);

    // Cleanup
    common::kill_process(clone_pid).await;
    fixture.cleanup().await;
    Ok(())
}

/// A restored clone's clock agrees with the host's to well under a second from
/// the moment it is healthy (issue 1194). Each sample brackets one guest read of
/// CLOCK_REALTIME between two host reads, so the exec's own latency cannot be
/// mistaken for an offset: the guest's value has to fall inside the bracket,
/// widened by `CLOCK_TOLERANCE` on both sides.
///
/// `CLOCK_TOLERANCE` covers what the step cannot compensate: the host stamps its clock
/// when the guest connects to the restore document's listener, and the guest
/// adds the time from its read of that document to its step, so only the
/// document's transfer over vsock is uncounted. That is milliseconds; 100 ms
/// leaves room for a busy host. The defect was the host sending whole seconds,
/// which left the guest behind by the second's fraction at the stamp, uniformly
/// between 0 and 1 s, so a run that restores at a fraction above 0.1 s fails
/// here. The deterministic pins are `the_clock_lands_on_the_hosts_instant_to_the_nanosecond`
/// (fc-agent) and the boot-plan listener tests.
#[tokio::test]
async fn test_clone_clock_matches_the_host_to_a_tenth_of_a_second() -> Result<()> {
    let fixture = common::SnapshotFixture::new("clockoffset", "rootless").await?;
    // The snapshot's clock falls behind while it waits, so the restore has to step it.
    tokio::time::sleep(Duration::from_secs(3)).await;

    let (_, clone_name, _, _) = common::unique_names("clockoffset-c");
    let (_clone_child, clone_pid) = common::spawn_fcvm_with_logs(
        &[
            "snapshot",
            "run",
            "--pid",
            &fixture.serve_pid.to_string(),
            "--name",
            &clone_name,
        ],
        &clone_name,
    )
    .await?;
    let measured: Result<f64> = async {
        common::poll_health_by_pid(clone_pid, 120).await?;
        worst_clock_offset(clone_pid).await
    }
    .await;
    common::kill_process(clone_pid).await;
    fixture.cleanup().await;

    let worst = measured?;
    assert!(
        worst <= CLOCK_TOLERANCE.as_secs_f64(),
        "the restored clone's clock was {:.1} ms off the host's (tolerance {} ms)",
        worst * 1000.0,
        CLOCK_TOLERANCE.as_millis()
    );
    Ok(())
}

/// A restored clone whose guest stalls after reading the restore document still
/// agrees with the host's clock once healthy (issue 1375). The restore used to step
/// to the host time in that document plus the time since the restore sequence
/// started, so a stall between the two left the clone behind by the stall: about
/// 130 ms on staging after a 26 s cold restore, and here the guest failpoint holds
/// fc-agent for `STALL` at that spot. The step now reads the host's clock right
/// before it.
///
/// The golden cold-boots with the failpoint armed (`FCVM_NO_SNAPSHOT=1`, so no
/// pre-start snapshot keyed by the failpoint is restored or left in the cache), its
/// snapshot carries the armed fc-agent, and only the clone's restore hits the point.
#[tokio::test]
async fn test_clone_clock_matches_the_host_after_a_stalled_restore() -> Result<()> {
    const STALL: Duration = Duration::from_secs(3);
    let (golden_name, clone_name, snapshot_name, _) = common::unique_names("stallclock");
    let stall = format!("restore.post_epoch_read:sleep:{}", STALL.as_millis());

    let (mut golden_child, golden_pid, _golden_log) = common::spawn_fcvm_with_env_and_log_path(
        &[
            "podman",
            "run",
            "--name",
            &golden_name,
            "--network",
            "rootless",
            common::TEST_IMAGE,
        ],
        &[
            ("FCVM_GUEST_FAILPOINT", stall.as_str()),
            ("FCVM_NO_SNAPSHOT", "1"),
        ],
    )
    .await
    .context("spawning the golden VM with the restore stall armed")?;
    let created: Result<()> = async {
        common::poll_health_by_pid(golden_pid, 180).await?;
        let output = tokio::process::Command::new(common::find_fcvm_binary()?)
            .args([
                "snapshot",
                "create",
                "--pid",
                &golden_pid.to_string(),
                "--tag",
                &snapshot_name,
            ])
            .kill_on_drop(true)
            .output()
            .await?;
        anyhow::ensure!(
            output.status.success(),
            "snapshot create failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(())
    }
    .await;
    common::kill_process(golden_pid).await;
    let _ = golden_child.wait().await;
    if let Err(error) = created {
        let _ = common::delete_snapshot(&snapshot_name).await;
        return Err(error);
    }

    let spawned = common::spawn_fcvm_with_env_and_log_path(
        &[
            "snapshot",
            "run",
            "--snapshot",
            &snapshot_name,
            "--name",
            &clone_name,
        ],
        &[],
    )
    .await
    .context("spawning the restored clone");
    let (mut clone_child, clone_pid, clone_log) = match spawned {
        Ok(spawned) => spawned,
        Err(error) => {
            let _ = common::delete_snapshot(&snapshot_name).await;
            return Err(error);
        }
    };
    let measured: Result<f64> = async {
        common::poll_health_by_pid(clone_pid, 120).await?;
        worst_clock_offset(clone_pid).await
    }
    .await;
    common::kill_process(clone_pid).await;
    let _ = clone_child.wait().await;
    let deleted = common::delete_snapshot(&snapshot_name).await;

    let worst = measured?;
    deleted?;
    let log = std::fs::read_to_string(&clone_log).unwrap_or_default();
    assert!(
        log.contains("FAILPOINT restore.post_epoch_read reached"),
        "the clone's restore never hit the stall, so nothing was tested ({})",
        clone_log.display()
    );
    assert!(
        worst <= CLOCK_TOLERANCE.as_secs_f64(),
        "after a restore stalled {} ms past its read of the restore document, the clone's \
         clock was {:.1} ms off the host's (tolerance {} ms)",
        STALL.as_millis(),
        worst * 1000.0,
        CLOCK_TOLERANCE.as_millis()
    );
    Ok(())
}

/// How far a guest's clock may sit outside the host bracket of a sample
/// (`worst_clock_offset`).
const CLOCK_TOLERANCE: Duration = Duration::from_millis(100);

/// Sample the guest's CLOCK_REALTIME 20 times, 100 ms apart, each read bracketed
/// between two host reads, and return the largest distance in seconds by which the
/// guest's value fell outside its bracket (zero inside it). The bracket keeps the
/// exec's own latency from being mistaken for an offset.
async fn worst_clock_offset(pid: u32) -> Result<f64> {
    const SAMPLES: usize = 20;
    let mut samples = Vec::with_capacity(SAMPLES);
    for _ in 0..SAMPLES {
        samples.push(clock_offset(pid).await?);
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    for (index, (offset, bracket)) in samples.iter().enumerate() {
        println!(
            "  sample {index:2}: guest outside the host bracket by {:+8.1} ms (bracket {:.1} ms)",
            offset * 1000.0,
            bracket * 1000.0
        );
    }
    Ok(samples
        .iter()
        .map(|(offset, _)| offset.abs())
        .fold(0.0, f64::max))
}

/// One bracketed sample: the signed distance in seconds by which the guest's
/// CLOCK_REALTIME fell outside two host reads around it (zero inside them), and the
/// bracket's width.
async fn clock_offset(pid: u32) -> Result<(f64, f64)> {
    let before = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?;
    let guest = common::exec_in_vm(pid, &["date", "+%s.%N"]).await?;
    let after = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?;
    let guest: f64 = guest.trim().parse().context("parsing the guest's clock")?;
    let offset = if guest < before.as_secs_f64() {
        guest - before.as_secs_f64()
    } else if guest > after.as_secs_f64() {
        guest - after.as_secs_f64()
    } else {
        0.0
    };
    Ok((offset, (after - before).as_secs_f64()))
}

/// A cold-booted Firecracker VM agrees with the host's clock from the moment it is
/// healthy (issue 1354). The guest used to step its clock to the host time stamped
/// into MMDS while the boot plan was built, before the VM booted, so it came up
/// 0.86 to 1.14 s behind the host and stayed there until chronyd corrected it, about
/// 12 s later. The guest now reads the host's clock over vsock, stamped when it
/// connects. Sampling starts right after healthy, while chronyd has not yet
/// corrected anything.
#[tokio::test]
async fn test_cold_boot_clock_matches_the_host() -> Result<()> {
    let (vm_name, _, _, _) = common::unique_names("bootclock");
    let (_child, pid) = common::spawn_fcvm_with_logs(
        &[
            "podman",
            "run",
            "--name",
            &vm_name,
            "--network",
            "rootless",
            "--no-snapshot",
            common::TEST_IMAGE,
        ],
        &vm_name,
    )
    .await?;
    let measured: Result<f64> = async {
        common::poll_health_by_pid(pid, 180).await?;
        worst_clock_offset(pid).await
    }
    .await;
    common::kill_process(pid).await;

    let worst = measured?;
    assert!(
        worst <= CLOCK_TOLERANCE.as_secs_f64(),
        "the cold-booted VM's clock was {:.1} ms off the host's after healthy \
         (tolerance {} ms)",
        worst * 1000.0,
        CLOCK_TOLERANCE.as_millis()
    );
    Ok(())
}

/// The source of a snapshot agrees with the host's clock as soon as `snapshot
/// create` returns. On the arm64 `nested` (NV2) profile the Firecracker fork freezes
/// the guest's counters while the snapshot pauses the VM, so a source resumed without
/// a step is behind by the pause until chronyd corrects it: without the step the first
/// sample was 1561 ms behind after a 1.6 s pause. The host failpoint lengthens the
/// pause by 300 ms, so the lag is over the tolerance however fast the save is.
///
/// The baseline cold-boots (`--no-snapshot`). The sampling starts right after the
/// create returns.
#[tokio::test]
async fn test_source_clock_matches_the_host_after_snapshot_create() -> Result<()> {
    let (baseline_name, _, snapshot_name, _) = common::unique_names("srcclock");
    let (_child, baseline_pid) = common::spawn_fcvm_with_logs(
        &[
            "podman",
            "run",
            "--name",
            &baseline_name,
            "--network",
            "rootless",
            "--kernel-profile",
            "nested",
            "--no-snapshot",
            common::TEST_IMAGE,
        ],
        &baseline_name,
    )
    .await?;
    let measured: Result<f64> = async {
        common::poll_health_by_pid(baseline_pid, 180).await?;
        let output = tokio::process::Command::new(common::find_fcvm_binary()?)
            .args([
                "snapshot",
                "create",
                "--pid",
                &baseline_pid.to_string(),
                "--tag",
                &snapshot_name,
            ])
            .env("FCVM_FAILPOINT", "snapshot.post_save_pre_resume:sleep:300")
            .kill_on_drop(true)
            .output()
            .await?;
        anyhow::ensure!(
            output.status.success(),
            "snapshot create failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        worst_clock_offset(baseline_pid).await
    }
    .await;
    common::kill_process(baseline_pid).await;
    // Nothing restores this snapshot; a create that never ran leaves nothing to delete.
    let deleted = common::delete_snapshot(&snapshot_name).await;

    let worst = measured?;
    deleted?;
    assert!(
        worst <= CLOCK_TOLERANCE.as_secs_f64(),
        "the snapshot's source was {:.1} ms off the host's clock after `snapshot create` \
         returned (tolerance {} ms)",
        worst * 1000.0,
        CLOCK_TOLERANCE.as_millis()
    );
    Ok(())
}

/// A restored clone runs a chronyd started after its restore, never the one in
/// the snapshot. The restore steps the clock behind chronyd's back, and chronyd
/// misses a forward step under 10 s, so the snapshot's daemon kept whatever
/// correction it was making and applied it to the stepped clock: a clone of a
/// source whose chronyd was still correcting a snapshot pause came up 0.42 to
/// 2.19 s off the host. The source keeps its daemon (it is resumed, not
/// restored), so its chronyd PID is the snapshot's.
#[tokio::test]
async fn test_clone_runs_a_chronyd_started_after_the_restore() -> Result<()> {
    let fixture = common::SnapshotFixture::new("chronydpid", "rootless").await?;
    // Every chronyd PID in the guest, from /proc/<pid>/comm. Not pidof: exec's
    // PATH is minimal and the probe must not depend on a tool being on it. The
    // helper already runs its words as one `sh -c` script.
    let chronyd_pid = |pid: u32| async move {
        let out =
            common::exec_in_vm(pid, &["/usr/bin/grep -lx chronyd /proc/[0-9]*/comm; true"]).await?;
        let pids: Vec<&str> = out
            .split_whitespace()
            .filter_map(|path| path.strip_prefix("/proc/")?.strip_suffix("/comm"))
            .collect();
        Ok::<_, anyhow::Error>(pids.join(" "))
    };
    let snapshot_daemon = chronyd_pid(fixture.baseline_pid).await;

    let (_, clone_name, _, _) = common::unique_names("chronydpid-c");
    let (_clone_child, clone_pid) = common::spawn_fcvm_with_logs(
        &[
            "snapshot",
            "run",
            "--pid",
            &fixture.serve_pid.to_string(),
            "--name",
            &clone_name,
        ],
        &clone_name,
    )
    .await?;
    let clone_daemon: Result<String> = async {
        common::poll_health_by_pid(clone_pid, 120).await?;
        // The fresh daemon is started off the readiness path, after the restore ACK.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let daemon = chronyd_pid(clone_pid).await?;
            if !daemon.is_empty() || std::time::Instant::now() > deadline {
                return Ok(daemon);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    .await;
    common::kill_process(clone_pid).await;
    fixture.cleanup().await;

    let snapshot_daemon = snapshot_daemon?;
    let clone_daemon = clone_daemon?;
    println!("  snapshot's chronyd: {snapshot_daemon:?}, clone's chronyd: {clone_daemon:?}");
    assert!(
        !snapshot_daemon.is_empty() && !snapshot_daemon.contains(' '),
        "the source should run exactly one chronyd, got {snapshot_daemon:?}"
    );
    assert!(
        !clone_daemon.is_empty() && !clone_daemon.contains(' '),
        "the clone should run exactly one chronyd within 10 s of healthy, got {clone_daemon:?}"
    );
    assert_ne!(
        clone_daemon, snapshot_daemon,
        "the clone still runs the snapshot's chronyd"
    );
    Ok(())
}

/// After snapshot restore, cookie-bound stale-socket cleanup must leave the
/// clone with a usable gateway and loopback service. Every non-loopback socket,
/// including a gateway peer, belongs to the snapshot generation and is removed;
/// this verifies the post-cleanup network is re-established successfully.
#[cfg(feature = "privileged-tests")]
#[tokio::test]
async fn test_restore_network_cleanup_reestablishes_gateway() -> Result<()> {
    let fixture = common::SnapshotFixture::new("ssfilter", "bridged").await?;

    let (_, clone_name, _, _) = common::unique_names("ssfilter-c");
    let (_clone_child, clone_pid) = common::spawn_fcvm_with_logs(
        &[
            "snapshot",
            "run",
            "--pid",
            &fixture.serve_pid.to_string(),
            "--name",
            &clone_name,
        ],
        &clone_name,
    )
    .await?;
    common::poll_health_by_pid(clone_pid, 120).await?;
    println!("  ✓ Clone healthy (PID: {})", clone_pid);

    // Verify gateway route exists after restore
    let route_out = common::exec_in_vm(clone_pid, &["ip", "route", "show"]).await?;
    println!("  Routes: {}", route_out.trim());
    assert!(
        route_out.contains("10.0.2.1") || route_out.contains("default"),
        "gateway route should exist after restore"
    );

    // Verify container networking works — nginx must respond on localhost
    let container_out = common::exec_in_container(
        clone_pid,
        &[
            "wget",
            "-q",
            "-O",
            "-",
            "--timeout=5",
            "http://127.0.0.1:80/",
        ],
    )
    .await
    .context("nginx should be reachable after exact restore cleanup")?;
    assert!(
        container_out.contains("nginx") || container_out.contains("Welcome"),
        "nginx should respond after restore"
    );
    println!("  ✓ Container nginx responding after restore");

    // Cleanup
    common::kill_process(clone_pid).await;
    fixture.cleanup().await;
    Ok(())
}

/// --no-swap should create a dedicated cgroup under /sys/fs/cgroup/fcvm.slice/
/// with memory.swap.max=0 for the Firecracker process.
#[cfg(feature = "privileged-tests")]
#[tokio::test]
async fn test_no_swap_creates_cgroup() -> Result<()> {
    let fixture = common::SnapshotFixture::new("noswap", "bridged").await?;

    // Clone WITH --no-swap
    let (_, clone_name, _, _) = common::unique_names("noswap-c");
    println!("  Spawning clone with --no-swap...");
    let (_clone_child, clone_pid) = common::spawn_fcvm_with_logs(
        &[
            "snapshot",
            "run",
            "--pid",
            &fixture.serve_pid.to_string(),
            "--name",
            &clone_name,
            "--no-swap",
        ],
        &clone_name,
    )
    .await?;
    common::poll_health_by_pid(clone_pid, 120).await?;
    println!("  ✓ Clone healthy with --no-swap (PID: {})", clone_pid);

    // Find the Firecracker process (child of the clone fcvm process)
    let fc_pid_out = tokio::process::Command::new("pgrep")
        .args([
            "-f",
            "firecracker.*api-sock",
            "--parent",
            &clone_pid.to_string(),
        ])
        .output()
        .await?;
    let fc_pid_str = String::from_utf8_lossy(&fc_pid_out.stdout);
    let fc_pid: u32 = fc_pid_str
        .trim()
        .lines()
        .next()
        .context("no firecracker child found")?
        .parse()
        .context("parse fc pid")?;
    println!("  Firecracker PID: {}", fc_pid);

    // Check that Firecracker is in a fcvm.slice cgroup with swap disabled
    let cgroup = std::fs::read_to_string(format!("/proc/{}/cgroup", fc_pid))
        .context("reading firecracker cgroup")?;
    let cgroup_path = cgroup
        .lines()
        .find_map(|l| l.strip_prefix("0::"))
        .context("no cgroup v2 entry")?;
    println!("  Firecracker cgroup: {}", cgroup_path);

    assert!(
        cgroup_path.contains("fcvm.slice"),
        "Firecracker should be in fcvm.slice, got: {}",
        cgroup_path
    );

    let swap_max =
        std::fs::read_to_string(format!("/sys/fs/cgroup{}/memory.swap.max", cgroup_path))
            .context("reading memory.swap.max")?;
    assert_eq!(
        swap_max.trim(),
        "0",
        "memory.swap.max should be 0, got: {}",
        swap_max.trim()
    );
    println!("  ✓ Firecracker in fcvm.slice with memory.swap.max=0");

    // Cleanup
    common::kill_process(clone_pid).await;
    fixture.cleanup().await;
    Ok(())
}

/// --no-dirty-tracking should disable KVM dirty page tracking. Verify by
/// checking that the Firecracker log shows track_dirty_pages=false.
#[cfg(feature = "privileged-tests")]
#[tokio::test]
async fn test_no_dirty_tracking_clone() -> Result<()> {
    let fixture = common::SnapshotFixture::new("nodirty", "bridged").await?;

    // Clone WITH --no-dirty-tracking
    let (_, clone_name, _, _) = common::unique_names("nodirty-c");
    println!("  Spawning clone with --no-dirty-tracking...");
    let (_clone_child, clone_pid, log_path) = common::spawn_fcvm_with_log_path(
        &[
            "snapshot",
            "run",
            "--pid",
            &fixture.serve_pid.to_string(),
            "--name",
            &clone_name,
            "--no-dirty-tracking",
        ],
        &clone_name,
    )
    .await?;
    common::poll_health_by_pid(clone_pid, 120).await?;
    println!(
        "  ✓ Clone healthy with --no-dirty-tracking (PID: {})",
        clone_pid
    );

    // Verify the clone actually works (exec something)
    let out = common::exec_in_container(clone_pid, &["echo", "no-dirty-works"]).await?;
    assert!(
        out.contains("no-dirty-works"),
        "exec should work on no-dirty-tracking clone"
    );
    println!("  ✓ Container exec works");

    // Verify track_dirty_pages=false in the fcvm debug log.
    // The snapshot load info line includes: track_dirty_pages=false
    let log_content = tokio::fs::read_to_string(&log_path)
        .await
        .unwrap_or_default();
    assert!(
        log_content.contains("track_dirty_pages=false"),
        "log should show track_dirty_pages=false in snapshot load line. Relevant lines: {}",
        log_content
            .lines()
            .filter(|l| l.contains("snapshot load"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    println!("  ✓ track_dirty_pages=false confirmed in log");

    // Cleanup
    common::kill_process(clone_pid).await;
    fixture.cleanup().await;
    Ok(())
}
