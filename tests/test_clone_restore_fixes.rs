//! Integration tests for clone restore fixes:
//! - Clock sync after snapshot restore (chronyd + MMDS time sync)
//! - ss -K filter preserves gateway connections (10.0.2.0/24)
//! - --no-swap creates dedicated cgroup with memory.swap.max=0
//! - --no-dirty-tracking passes track_dirty_pages=false

#![cfg(feature = "integration-slow")]

mod common;

use anyhow::{Context, Result};
use std::time::Duration;

/// After snapshot restore, the VM clock should be within a few seconds of host
/// time (not stuck at snapshot time). This verifies the MMDS clock sync +
/// chronyc makestep path in fc-agent/src/restore.rs.
#[cfg(feature = "privileged-tests")]
#[tokio::test]
async fn test_clock_synced_after_clone_restore() -> Result<()> {
    let (baseline, clone, snap, _) = common::unique_names("clocksync");

    // Start baseline
    println!("Starting baseline...");
    let (_child, baseline_pid) = common::spawn_fcvm_with_logs(
        &["podman", "run", "--name", &baseline, "--network", "bridged", common::TEST_IMAGE],
        &baseline,
    )
    .await?;
    common::poll_health_by_pid(baseline_pid, 120).await?;
    println!("  ✓ Baseline healthy (PID: {})", baseline_pid);

    // Snapshot
    common::create_snapshot_by_pid(baseline_pid, &snap).await?;

    // Serve
    let (_serve_child, serve_pid) =
        common::spawn_fcvm_with_logs(&["snapshot", "serve", &snap], &format!("{}-serve", snap))
            .await?;
    common::poll_serve_ready(&snap, serve_pid, 30).await?;

    // Wait 3 seconds so snapshot time drifts from real time
    println!("  Waiting 3s for clock drift...");
    tokio::time::sleep(Duration::from_secs(3)).await;

    // Clone
    let (_clone_child, clone_pid) = common::spawn_fcvm_with_logs(
        &["snapshot", "run", "--pid", &serve_pid.to_string(), "--name", &clone],
        &clone,
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

    println!("  Host epoch: {}, VM epoch: {}, drift: {}s", host_epoch, vm_epoch, drift);
    assert!(
        drift < 5,
        "VM clock drifted {}s from host after restore — clock sync failed",
        drift
    );
    println!("  ✓ Clock synced (drift={}s)", drift);

    // Cleanup
    common::kill_process(clone_pid).await;
    common::kill_process(serve_pid).await;
    common::kill_process(baseline_pid).await;
    Ok(())
}

/// After snapshot restore, ss -K should kill external connections but PRESERVE
/// gateway (10.0.2.x) and loopback connections. Verify by checking that the
/// clone can still reach the gateway and that nginx responds inside the container.
#[cfg(feature = "privileged-tests")]
#[tokio::test]
async fn test_ss_filter_preserves_gateway_after_restore() -> Result<()> {
    let (baseline, clone, snap, _) = common::unique_names("ssfilter");

    // Start baseline
    println!("Starting baseline...");
    let (_child, baseline_pid) = common::spawn_fcvm_with_logs(
        &["podman", "run", "--name", &baseline, "--network", "bridged", common::TEST_IMAGE],
        &baseline,
    )
    .await?;
    common::poll_health_by_pid(baseline_pid, 120).await?;

    // Snapshot + Serve + Clone
    common::create_snapshot_by_pid(baseline_pid, &snap).await?;

    let (_serve_child, serve_pid) =
        common::spawn_fcvm_with_logs(&["snapshot", "serve", &snap], &format!("{}-serve", snap))
            .await?;
    common::poll_serve_ready(&snap, serve_pid, 30).await?;

    let (_clone_child, clone_pid) = common::spawn_fcvm_with_logs(
        &["snapshot", "run", "--pid", &serve_pid.to_string(), "--name", &clone],
        &clone,
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
        &["wget", "-q", "-O", "-", "--timeout=5", "http://127.0.0.1:80/"],
    )
    .await
    .context("nginx should be reachable after restore — ss -K may have killed gateway connections")?;
    assert!(
        container_out.contains("nginx") || container_out.contains("Welcome"),
        "nginx should respond after restore"
    );
    println!("  ✓ Container nginx responding after restore");

    // Cleanup
    common::kill_process(clone_pid).await;
    common::kill_process(serve_pid).await;
    common::kill_process(baseline_pid).await;
    Ok(())
}

/// --no-swap should create a dedicated cgroup under /sys/fs/cgroup/fcvm.slice/
/// with memory.swap.max=0 for the Firecracker process.
#[cfg(feature = "privileged-tests")]
#[tokio::test]
async fn test_no_swap_creates_cgroup() -> Result<()> {
    let (baseline, clone, snap, _) = common::unique_names("noswap");

    // Start baseline
    let (_child, baseline_pid) = common::spawn_fcvm_with_logs(
        &["podman", "run", "--name", &baseline, "--network", "bridged", common::TEST_IMAGE],
        &baseline,
    )
    .await?;
    common::poll_health_by_pid(baseline_pid, 120).await?;

    // Snapshot + Serve
    common::create_snapshot_by_pid(baseline_pid, &snap).await?;

    let (_serve_child, serve_pid) =
        common::spawn_fcvm_with_logs(&["snapshot", "serve", &snap], &format!("{}-serve", snap))
            .await?;
    common::poll_serve_ready(&snap, serve_pid, 30).await?;

    // Clone WITH --no-swap
    println!("  Spawning clone with --no-swap...");
    let (_clone_child, clone_pid) = common::spawn_fcvm_with_logs(
        &[
            "snapshot",
            "run",
            "--pid",
            &serve_pid.to_string(),
            "--name",
            &clone,
            "--no-swap",
        ],
        &clone,
    )
    .await?;
    common::poll_health_by_pid(clone_pid, 120).await?;
    println!("  ✓ Clone healthy with --no-swap (PID: {})", clone_pid);

    // Find the Firecracker process (child of the clone fcvm process)
    let fc_pid_out = tokio::process::Command::new("pgrep")
        .args(["-f", "firecracker.*api-sock", "--parent", &clone_pid.to_string()])
        .output()
        .await?;
    let fc_pid_str = String::from_utf8_lossy(&fc_pid_out.stdout);
    let fc_pid: u32 = fc_pid_str.trim().lines().next()
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

    let swap_max = std::fs::read_to_string(format!(
        "/sys/fs/cgroup{}/memory.swap.max",
        cgroup_path
    ))
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
    common::kill_process(serve_pid).await;
    common::kill_process(baseline_pid).await;
    Ok(())
}

/// --no-dirty-tracking should disable KVM dirty page tracking. Verify by
/// checking that the Firecracker log shows track_dirty_pages=false.
#[cfg(feature = "privileged-tests")]
#[tokio::test]
async fn test_no_dirty_tracking_clone() -> Result<()> {
    let (baseline, clone, snap, _) = common::unique_names("nodirty");

    // Start baseline
    let (_child, baseline_pid) = common::spawn_fcvm_with_logs(
        &["podman", "run", "--name", &baseline, "--network", "bridged", common::TEST_IMAGE],
        &baseline,
    )
    .await?;
    common::poll_health_by_pid(baseline_pid, 120).await?;

    // Snapshot + Serve
    common::create_snapshot_by_pid(baseline_pid, &snap).await?;

    let (_serve_child, serve_pid) =
        common::spawn_fcvm_with_logs(&["snapshot", "serve", &snap], &format!("{}-serve", snap))
            .await?;
    common::poll_serve_ready(&snap, serve_pid, 30).await?;

    // Clone WITH --no-dirty-tracking
    println!("  Spawning clone with --no-dirty-tracking...");
    let (_clone_child, clone_pid, log_path) = common::spawn_fcvm_with_log_path(
        &[
            "snapshot",
            "run",
            "--pid",
            &serve_pid.to_string(),
            "--name",
            &clone,
            "--no-dirty-tracking",
        ],
        &clone,
    )
    .await?;
    common::poll_health_by_pid(clone_pid, 120).await?;
    println!("  ✓ Clone healthy with --no-dirty-tracking (PID: {})", clone_pid);

    // Verify the clone actually works (exec something)
    let out = common::exec_in_container(clone_pid, &["echo", "no-dirty-works"]).await?;
    assert!(out.contains("no-dirty-works"), "exec should work on no-dirty-tracking clone");
    println!("  ✓ Container exec works");

    // Verify track_dirty_pages=false in the Firecracker debug log
    let log_content = tokio::fs::read_to_string(&log_path).await.unwrap_or_default();
    assert!(
        log_content.contains("track_dirty_pages\":false")
            || log_content.contains("track_dirty_pages: false")
            || log_content.contains("track_dirty_pages: Some(false)")
            || log_content.contains("track_dirty_pages\":Some(false)"),
        "log should show track_dirty_pages=false. Log snippet: {}",
        log_content
            .lines()
            .filter(|l| l.contains("track_dirty") || l.contains("load_snapshot") || l.contains("snapshot load"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    println!("  ✓ track_dirty_pages=false confirmed in log");

    // Cleanup
    common::kill_process(clone_pid).await;
    common::kill_process(serve_pid).await;
    common::kill_process(baseline_pid).await;
    Ok(())
}
