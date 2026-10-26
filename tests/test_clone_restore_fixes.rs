//! Integration tests for clone restore fixes:
//! - Clock sync after snapshot restore (chronyd + MMDS time sync), to well under a second
//! - exact restore cleanup re-establishes gateway and container connectivity
//! - --no-swap creates dedicated cgroup with memory.swap.max=0
//! - --no-dirty-tracking passes track_dirty_pages=false

#![cfg(feature = "integration-slow")]
#![cfg_attr(not(feature = "privileged-tests"), allow(unused_imports))]

mod common;

use anyhow::{Context, Result};
use std::time::Duration;

/// After snapshot restore, the VM clock should be within a few seconds of host
/// time (not stuck at snapshot time). This verifies the MMDS clock sync +
/// chronyc makestep path in fc-agent/src/restore.rs.
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
/// widened by `TOLERANCE` on both sides.
///
/// `TOLERANCE` covers what the step cannot compensate: the host stamps its clock
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
    const TOLERANCE: Duration = Duration::from_millis(100);
    const SAMPLES: usize = 20;

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
    let measured: Result<Vec<(f64, f64)>> = async {
        common::poll_health_by_pid(clone_pid, 120).await?;
        let mut samples = Vec::with_capacity(SAMPLES);
        for _ in 0..SAMPLES {
            let before = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?;
            let guest = common::exec_in_vm(clone_pid, &["date", "+%s.%N"]).await?;
            let after = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?;
            let guest: f64 = guest.trim().parse().context("parsing the guest's clock")?;
            // Signed distance from the bracket, zero inside it.
            let offset = if guest < before.as_secs_f64() {
                guest - before.as_secs_f64()
            } else if guest > after.as_secs_f64() {
                guest - after.as_secs_f64()
            } else {
                0.0
            };
            samples.push((offset, (after - before).as_secs_f64()));
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        Ok(samples)
    }
    .await;
    common::kill_process(clone_pid).await;
    fixture.cleanup().await;

    let samples = measured?;
    for (index, (offset, bracket)) in samples.iter().enumerate() {
        println!(
            "  sample {index:2}: guest outside the host bracket by {:+8.1} ms (bracket {:.1} ms)",
            offset * 1000.0,
            bracket * 1000.0
        );
    }
    let worst = samples
        .iter()
        .map(|(offset, _)| offset.abs())
        .fold(0.0, f64::max);
    assert!(
        worst <= TOLERANCE.as_secs_f64(),
        "the restored clone's clock was {:.1} ms off the host's (tolerance {} ms)",
        worst * 1000.0,
        TOLERANCE.as_millis()
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
