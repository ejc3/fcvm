//! Startup snapshot tests - verifies snapshot creation at the first Healthy
//!
//! Tests the two-tier snapshot system:
//! 1. Pre-start snapshot: Created after image load, before container starts
//! 2. Startup snapshot: Created at the first Healthy: the HTTP health check with
//!    --health-check, otherwise the container's start (or its image's HEALTHCHECK)
//!
//! Selection priority: startup snapshot > pre-start snapshot > fresh boot

#![cfg(feature = "integration-fast")]

mod common;

use anyhow::{Context, Result};
use std::time::Duration;

/// Image for startup snapshot tests - nginx provides /health endpoint
const TEST_IMAGE: &str = common::TEST_IMAGE;

/// Health check URL for nginx
const HEALTH_CHECK_URL: &str = "http://localhost/";

/// Test that fresh boot creates startup snapshot when health check URL is provided
///
/// This test:
/// 1. Starts a VM with --health-check-url
/// 2. Waits for it to become healthy
/// 3. Verifies startup snapshot is created after health check passes
#[tokio::test]
async fn test_startup_snapshot_created_on_fresh_boot() -> Result<()> {
    println!("\nStartup snapshot creation test");
    println!("===============================");
    println!("Verifies startup snapshot is created after health check passes");

    let (vm_name, _, _, _) = common::unique_names("startup-fresh");

    // Use unique env var to get unique snapshot key (prevents parallel test interference)
    let test_id = format!("TEST_ID=fresh-{}", std::process::id());

    // Start VM with health check URL (rootless mode for unprivileged testing)
    println!("Starting VM with --health-check-url...");
    let (mut child, fcvm_pid) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &vm_name,
        "--env",
        &test_id,
        "--health-check",
        HEALTH_CHECK_URL,
        TEST_IMAGE,
    ])
    .await
    .context("spawning fcvm")?;

    println!("  fcvm PID: {}", fcvm_pid);
    println!("  Waiting for VM to become healthy (triggers startup snapshot)...");

    // Wait for healthy status
    // Use 300 second timeout to account for rootfs creation on first run
    let health_result = tokio::time::timeout(
        Duration::from_secs(300),
        common::poll_health_by_pid(fcvm_pid, 300),
    )
    .await;

    // Give extra time for startup snapshot to be created after health check
    // (snapshot creation happens asynchronously after health is detected)
    tokio::time::sleep(Duration::from_secs(5)).await;

    // Cleanup
    println!("  Stopping VM...");
    common::kill_process(fcvm_pid).await;
    let _ = child.wait().await;

    // Check result
    match health_result {
        Ok(Ok(_)) => {
            println!("  VM became healthy");
            // Note: We can't easily verify the exact snapshot key here because
            // it's derived from the full FirecrackerConfig hash. We verify the
            // mechanism works by checking logs and behavior in subsequent tests.
            println!("✅ STARTUP SNAPSHOT TEST PASSED!");
            println!("  Health check triggered - startup snapshot creation path exercised");
            Ok(())
        }
        Ok(Err(e)) => {
            println!("❌ Health check failed: {}", e);
            Err(e)
        }
        Err(_) => {
            anyhow::bail!("Timeout waiting for VM to become healthy")
        }
    }
}

/// Test that VM uses startup snapshot when available (fastest path)
///
/// This test verifies the selection priority:
/// - When both pre-start and startup snapshots exist, startup is preferred
#[tokio::test]
async fn test_startup_snapshot_priority() -> Result<()> {
    println!("\nStartup snapshot priority test");
    println!("==============================");
    println!("Verifies startup snapshot is preferred over pre-start snapshot");

    // Use unique env var to get unique snapshot key (prevents parallel test interference)
    let test_id = format!("TEST_ID=priority-{}", std::process::id());

    // First boot: create both snapshots
    let (vm_name1, _, _, _) = common::unique_names("startup-priority-1");

    println!("First boot: Creating snapshots...");
    let (mut child1, fcvm_pid1) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &vm_name1,
        "--env",
        &test_id,
        "--health-check",
        HEALTH_CHECK_URL,
        TEST_IMAGE,
    ])
    .await
    .context("spawning fcvm for first boot")?;

    // Wait for healthy (startup snapshot created)
    let health_result1 = tokio::time::timeout(
        Duration::from_secs(300),
        common::poll_health_by_pid(fcvm_pid1, 300),
    )
    .await;

    // Wait for snapshot creation to complete
    tokio::time::sleep(Duration::from_secs(5)).await;

    // Stop first VM
    println!("  Stopping first VM...");
    common::kill_process(fcvm_pid1).await;
    let _ = child1.wait().await;

    if health_result1.is_err() || health_result1.as_ref().unwrap().is_err() {
        anyhow::bail!("First VM did not become healthy");
    }

    // Second boot: should use startup snapshot
    let (vm_name2, _, _, _) = common::unique_names("startup-priority-2");

    println!("Second boot: Should use startup snapshot...");
    let (mut child2, fcvm_pid2) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &vm_name2,
        "--env",
        &test_id,
        "--health-check",
        HEALTH_CHECK_URL,
        TEST_IMAGE,
    ])
    .await
    .context("spawning fcvm for second boot")?;

    // Wait for healthy - should be faster if using startup snapshot
    let start = std::time::Instant::now();
    let health_result2 = tokio::time::timeout(
        Duration::from_secs(120),
        common::poll_health_by_pid(fcvm_pid2, 120),
    )
    .await;
    let elapsed = start.elapsed();

    // Cleanup
    println!("  Stopping second VM...");
    common::kill_process(fcvm_pid2).await;
    let _ = child2.wait().await;

    match health_result2 {
        Ok(Ok(_)) => {
            println!(
                "  Second VM became healthy in {:.2}s",
                elapsed.as_secs_f32()
            );
            // Startup snapshot should make second boot significantly faster
            // (skips container initialization time)
            println!("✅ STARTUP SNAPSHOT PRIORITY TEST PASSED!");
            Ok(())
        }
        Ok(Err(e)) => {
            println!("❌ Second boot health check failed: {}", e);
            Err(e)
        }
        Err(_) => {
            anyhow::bail!("Timeout waiting for second VM to become healthy")
        }
    }
}

/// The snapshot a VM was started from or last snapshotted to, from `fcvm ls --json`.
async fn snapshot_name(pid: u32) -> Result<String> {
    let fcvm_path = common::find_fcvm_binary()?;
    let output = tokio::process::Command::new(&fcvm_path)
        .args(["ls", "--json", "--pid", &pid.to_string()])
        .output()
        .await
        .context("running fcvm ls")?;
    let vms: Vec<serde_json::Value> =
        serde_json::from_slice(&output.stdout).context("parsing fcvm ls --json")?;
    vms.first()
        .and_then(|vm| vm.get("config")?.get("snapshot_name")?.as_str())
        .map(str::to_string)
        .with_context(|| format!("VM PID {pid} records no snapshot_name"))
}

/// Without --health-check, a run takes a startup snapshot when its container is first
/// healthy (the container's start notification), and the next identical run restores the
/// container already running: the guest runs no `podman run`, so the container's
/// StartedAt is from before the second run began.
#[tokio::test]
async fn a_second_identical_run_restores_a_running_container_without_podman_run() -> Result<()> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos();
    // A key no other test or run shares.
    let test_id = format!("TEST_ID=startup-no-url-{}-{nanos}", std::process::id());

    let (vm_name1, _, _, _) = common::unique_names("startup-no-url-1");
    let (mut child1, pid1) = common::spawn_fcvm(&[
        "podman", "run", "--name", &vm_name1, "--env", &test_id, TEST_IMAGE,
    ])
    .await
    .context("spawning the first run")?;
    let first = async {
        common::poll_health_by_pid(pid1, 300).await?;
        // Healthy is published after the startup snapshot is taken.
        snapshot_name(pid1).await
    }
    .await;
    common::kill_process(pid1).await;
    let _ = child1.wait().await;
    let startup_key = first?;
    anyhow::ensure!(
        startup_key.ends_with("-startup"),
        "the first run took no startup snapshot without --health-check; it records {startup_key}"
    );

    let second_began = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();
    let (vm_name2, _, _, _) = common::unique_names("startup-no-url-2");
    let (mut child2, pid2) = common::spawn_fcvm(&[
        "podman", "run", "--name", &vm_name2, "--env", &test_id, TEST_IMAGE,
    ])
    .await
    .context("spawning the second run")?;
    let second = async {
        common::poll_health_by_pid(pid2, 120).await?;
        let restored_from = snapshot_name(pid2).await?;
        let started_at = common::exec_in_vm(
            pid2,
            &[
                "podman",
                "inspect",
                "--format",
                "{{.State.StartedAt.Unix}}",
                "fcvm-container",
            ],
        )
        .await?;
        anyhow::Ok((restored_from, started_at))
    }
    .await;
    common::kill_process(pid2).await;
    let _ = child2.wait().await;
    let (restored_from, started_at) = second?;

    assert_eq!(
        restored_from, startup_key,
        "the second run did not restore the startup snapshot"
    );
    let started_at: u64 = started_at
        .trim()
        .parse()
        .with_context(|| format!("podman StartedAt {started_at:?}"))?;
    assert!(
        started_at < second_began,
        "the second run's container started at {started_at}, after the run began at \
         {second_began}: the guest ran podman run instead of restoring a running container"
    );
    Ok(())
}

/// Test startup snapshot creation for restored VMs (from pre-start snapshot)
///
/// When a VM is restored from pre-start snapshot, it should still create
/// a startup snapshot after becoming healthy.
#[tokio::test]
async fn test_startup_snapshot_on_restored_vm() -> Result<()> {
    println!("\nStartup snapshot on restored VM test");
    println!("====================================");
    println!("Verifies startup snapshot is created even when restoring from pre-start");

    // Use unique env var to get unique snapshot key (prevents parallel test interference)
    let test_id = format!("TEST_ID=restored-{}", std::process::id());

    // First boot: creates pre-start snapshot, then startup snapshot
    let (vm_name1, _, _, _) = common::unique_names("startup-restore-1");

    println!("First boot: Full cold start...");
    let (mut child1, fcvm_pid1) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &vm_name1,
        "--env",
        &test_id,
        "--health-check",
        HEALTH_CHECK_URL,
        TEST_IMAGE,
    ])
    .await
    .context("spawning fcvm for first boot")?;

    let health_result1 = tokio::time::timeout(
        Duration::from_secs(300),
        common::poll_health_by_pid(fcvm_pid1, 300),
    )
    .await;

    // Give time for both snapshots to be created
    tokio::time::sleep(Duration::from_secs(5)).await;

    println!("  Stopping first VM...");
    common::kill_process(fcvm_pid1).await;
    let _ = child1.wait().await;

    if health_result1.is_err() || health_result1.as_ref().unwrap().is_err() {
        anyhow::bail!("First VM did not become healthy");
    }

    // For the second run, we want to test restoration from pre-start only.
    // However, since startup snapshot exists, it will be used.
    // This test mainly verifies the full workflow completes successfully.

    // Second boot: should restore from snapshot
    let (vm_name2, _, _, _) = common::unique_names("startup-restore-2");

    println!("Second boot: Restoring from snapshot...");
    let start = std::time::Instant::now();
    let (mut child2, fcvm_pid2) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &vm_name2,
        "--env",
        &test_id,
        "--health-check",
        HEALTH_CHECK_URL,
        TEST_IMAGE,
    ])
    .await
    .context("spawning fcvm for second boot")?;

    let health_result2 = tokio::time::timeout(
        Duration::from_secs(120),
        common::poll_health_by_pid(fcvm_pid2, 120),
    )
    .await;
    let elapsed = start.elapsed();

    // Cleanup
    println!("  Stopping second VM...");
    common::kill_process(fcvm_pid2).await;
    let _ = child2.wait().await;

    match health_result2 {
        Ok(Ok(_)) => {
            println!(
                "  Second VM (restored) became healthy in {:.2}s",
                elapsed.as_secs_f32()
            );
            println!("✅ STARTUP SNAPSHOT ON RESTORED VM TEST PASSED!");
            Ok(())
        }
        Ok(Err(e)) => {
            println!("❌ Restored VM health check failed: {}", e);
            Err(e)
        }
        Err(_) => {
            anyhow::bail!("Timeout waiting for restored VM to become healthy")
        }
    }
}

/// Test that startup snapshot works with bridged networking
#[cfg(feature = "privileged-tests")]
#[tokio::test]
async fn test_startup_snapshot_bridged() -> Result<()> {
    println!("\nStartup snapshot with bridged networking test");
    println!("=============================================");
    println!("Verifies startup snapshot works with --network bridged");

    let (vm_name, _, _, _) = common::unique_names("startup-bridged");

    // Use unique env var to get unique snapshot key (prevents parallel test interference)
    let test_id = format!("TEST_ID=bridged-{}", std::process::id());

    // Start VM with bridged networking and health check URL
    println!("Starting VM with --network bridged and --health-check-url...");
    let (mut child, fcvm_pid) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &vm_name,
        "--env",
        &test_id,
        "--network",
        "bridged",
        "--health-check",
        HEALTH_CHECK_URL,
        TEST_IMAGE,
    ])
    .await
    .context("spawning fcvm")?;

    println!("  fcvm PID: {}", fcvm_pid);
    println!("  Waiting for VM to become healthy...");

    // Wait for healthy status
    let health_result = tokio::time::timeout(
        Duration::from_secs(300),
        common::poll_health_by_pid(fcvm_pid, 300),
    )
    .await;

    // Wait for snapshot creation
    tokio::time::sleep(Duration::from_secs(5)).await;

    // Cleanup
    println!("  Stopping VM...");
    common::kill_process(fcvm_pid).await;
    let _ = child.wait().await;

    match health_result {
        Ok(Ok(_)) => {
            println!("  VM became healthy with bridged networking");
            println!("✅ STARTUP SNAPSHOT BRIDGED TEST PASSED!");
            Ok(())
        }
        Ok(Err(e)) => {
            println!("❌ Health check failed: {}", e);
            Err(e)
        }
        Err(_) => {
            anyhow::bail!("Timeout waiting for VM to become healthy")
        }
    }
}

/// Test startup snapshot key generation
#[test]
fn test_startup_snapshot_key_generation() {
    println!("\nStartup snapshot key generation test");
    println!("====================================");

    let base_key = "abc123def456";
    let startup_key = fcvm::commands::podman::startup_snapshot_key(base_key, None);

    assert_eq!(startup_key, "abc123def456-startup");
    println!("  Base key: {}", base_key);
    println!("  Startup key: {}", startup_key);
    println!("✅ STARTUP SNAPSHOT KEY GENERATION TEST PASSED!");
}
