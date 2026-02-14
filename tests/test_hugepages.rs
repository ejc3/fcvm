//! Integration tests for hugepage-backed VMs.
//!
//! These tests verify:
//! - VM boots with --hugepages flag
//! - Snapshot cache creates hugepage-backed snapshots
//! - Cache restore implicitly starts UFFD server (required for hugepage snapshots)
//! - Snapshot/clone workflow works with hugepages
//! - Memory alignment validation
//!
//! Requires pre-allocated hugepage pool on host:
//!   echo 512 | sudo tee /proc/sys/vm/nr_hugepages

#![cfg(feature = "privileged-tests")]

mod common;

use anyhow::{Context, Result};
use std::time::Duration;

/// VM memory size for hugepage tests (MiB).
/// Must be even (2MB-aligned) and small enough to fit in the hugepage pool.
const HP_TEST_MEM_MIB: u32 = 512;

/// Ensure enough FREE hugepages are available for the test VM.
///
/// Checks free_hugepages (not nr_hugepages) because stale Firecracker processes
/// from previous test runs may still be consuming hugepages from the pool.
async fn ensure_hugepages(mem_mib: u32) -> Result<()> {
    let nr = tokio::fs::read_to_string("/sys/kernel/mm/hugepages/hugepages-2048kB/nr_hugepages")
        .await
        .context("reading nr_hugepages")?;
    let total: u64 = nr.trim().parse().context("parsing nr_hugepages")?;

    let free =
        tokio::fs::read_to_string("/sys/kernel/mm/hugepages/hugepages-2048kB/free_hugepages")
            .await
            .context("reading free_hugepages")?;
    let free: u64 = free.trim().parse().context("parsing free_hugepages")?;

    let needed = (mem_mib as u64) / 2; // Each hugepage is 2MB
    let in_use = total - free;

    println!(
        "  Hugepages: {} total, {} free, {} in use (need {} for {}MB VM)",
        total, free, in_use, needed, mem_mib
    );

    anyhow::ensure!(
        total > 0,
        "No hugepages allocated. Run: echo 512 | sudo tee /proc/sys/vm/nr_hugepages"
    );
    anyhow::ensure!(
        free >= needed,
        "Not enough free hugepages: need {} but only {} free ({} in use by other processes). \
         Kill stale VMs or increase pool: echo {} | sudo tee /proc/sys/vm/nr_hugepages",
        needed,
        free,
        in_use,
        needed * 2
    );
    Ok(())
}

/// Test that a VM boots successfully with --hugepages flag
///
/// Verifies:
/// - huge_pages: "2M" is accepted by Firecracker's PUT /machine-config
/// - VM reaches healthy status
/// - Exec works in the container
#[tokio::test]
async fn test_hugepage_vm_boot() -> Result<()> {
    println!("\nHugepage VM boot test");
    println!("=====================");
    ensure_hugepages(HP_TEST_MEM_MIB).await?;

    let (vm_name, _, _, _) = common::unique_names("hugepages-boot");
    let mem_str = HP_TEST_MEM_MIB.to_string();

    let (mut child, fcvm_pid) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &vm_name,
        "--network",
        "bridged",
        "--hugepages",
        "--mem",
        &mem_str,
        common::TEST_IMAGE,
    ])
    .await
    .context("spawning fcvm with --hugepages")?;

    println!("  fcvm PID: {}", fcvm_pid);
    println!("  Waiting for VM to become healthy...");

    let health_result = tokio::time::timeout(
        Duration::from_secs(300),
        common::poll_health_by_pid(fcvm_pid, 300),
    )
    .await;

    match &health_result {
        Ok(Ok(_)) => println!("  VM is healthy with hugepages!"),
        Ok(Err(e)) => println!("  Health check failed: {}", e),
        Err(_) => println!("  Health check timed out"),
    }

    // Run exec to verify container is functional
    if health_result.is_ok() && health_result.as_ref().unwrap().is_ok() {
        println!("  Running exec in hugepage VM...");
        let exec_output = common::exec_in_container(fcvm_pid, &["echo", "hugepages-ok"]).await?;
        assert!(
            exec_output.contains("hugepages-ok"),
            "exec output should contain 'hugepages-ok', got: {}",
            exec_output
        );
        println!("  Exec output: {}", exec_output.trim());
    }

    // Cleanup
    println!("  Stopping VM...");
    common::kill_process(fcvm_pid).await;
    let _ = child.wait().await;

    health_result
        .map_err(|_| anyhow::anyhow!("health check timed out"))?
        .context("health check failed")?;

    println!("  PASSED: hugepage VM boots and exec works");
    Ok(())
}

/// Test that cache restore with hugepages uses UFFD (not File backend).
///
/// Firecracker rejects File backend for hugepage snapshots, so cache restore
/// must implicitly start a UFFD server. This test verifies:
/// 1. First run: VM boots with --hugepages, snapshot cache is created
/// 2. Second run: same args hit cache, VM still boots (proves UFFD restore worked)
///    (If File backend were used, Firecracker would return an error)
#[tokio::test]
async fn test_hugepage_cache_restore_uses_uffd() -> Result<()> {
    println!("\nHugepage cache restore test");
    println!("===========================");
    ensure_hugepages(HP_TEST_MEM_MIB).await?;

    let mem_str = HP_TEST_MEM_MIB.to_string();

    // Use unique env var so we get a unique cache key
    let test_id = format!("TEST_ID=hp-cache-{}", std::process::id());

    // First run: creates snapshot cache
    println!("  First run: creating hugepage snapshot cache...");
    let (vm_name1, _, _, _) = common::unique_names("hp-cache-1");
    let (mut child1, pid1) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &vm_name1,
        "--network",
        "bridged",
        "--hugepages",
        "--mem",
        &mem_str,
        "--env",
        &test_id,
        common::TEST_IMAGE,
    ])
    .await
    .context("spawning first hugepage VM")?;

    println!("  First VM PID: {}", pid1);
    common::poll_health_by_pid(pid1, 300).await?;
    println!("  First VM healthy, waiting for snapshot cache creation...");

    // Wait for snapshot cache to be created (happens after health check)
    tokio::time::sleep(Duration::from_secs(10)).await;

    // Kill first VM
    common::kill_process(pid1).await;
    let _ = child1.wait().await;
    println!("  First VM stopped");

    // Second run: should hit cache and use implicit UFFD server
    println!("  Second run: should hit hugepage cache...");
    let (vm_name2, _, _, _) = common::unique_names("hp-cache-2");
    let (mut child2, pid2) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &vm_name2,
        "--network",
        "bridged",
        "--hugepages",
        "--mem",
        &mem_str,
        "--env",
        &test_id,
        common::TEST_IMAGE,
    ])
    .await
    .context("spawning second hugepage VM (cache hit)")?;

    println!("  Second VM PID: {}", pid2);

    let health_result = tokio::time::timeout(
        Duration::from_secs(300),
        common::poll_health_by_pid(pid2, 300),
    )
    .await;

    match &health_result {
        Ok(Ok(_)) => println!("  Second VM healthy (UFFD restore worked!)"),
        Ok(Err(e)) => println!("  Second VM health check failed: {}", e),
        Err(_) => println!("  Second VM health check timed out"),
    }

    // Cleanup
    common::kill_process(pid2).await;
    let _ = child2.wait().await;

    health_result
        .map_err(|_| anyhow::anyhow!("cache restore health check timed out"))?
        .context("cache restore health check failed")?;

    println!("  PASSED: hugepage cache restore with implicit UFFD server works");
    Ok(())
}

/// Test full snapshot/clone workflow with hugepages.
///
/// 1. Boot VM with --hugepages
/// 2. Create user snapshot
/// 3. Start serve process
/// 4. Spawn clone from serve
/// 5. Verify clone is healthy and functional
#[tokio::test]
async fn test_hugepage_snapshot_clone() -> Result<()> {
    println!("\nHugepage snapshot/clone test");
    println!("============================");
    ensure_hugepages(HP_TEST_MEM_MIB).await?;

    let (baseline_name, clone_name, snap_name, serve_name) = common::unique_names("hp-snap-clone");
    let mem_str = HP_TEST_MEM_MIB.to_string();

    let fcvm_path = common::find_fcvm_binary()?;

    // 1. Boot baseline VM with hugepages
    println!("  Starting baseline VM with --hugepages...");
    let (mut baseline, baseline_pid) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &baseline_name,
        "--network",
        "bridged",
        "--hugepages",
        "--mem",
        &mem_str,
        common::TEST_IMAGE,
    ])
    .await
    .context("spawning baseline hugepage VM")?;

    common::poll_health_by_pid(baseline_pid, 300).await?;
    println!("  Baseline VM healthy (PID: {})", baseline_pid);

    // 2. Create snapshot (no sudo — CARGO_RUNNER handles that)
    println!("  Creating snapshot '{}'...", snap_name);
    let snap_output = tokio::process::Command::new(&fcvm_path)
        .args([
            "snapshot",
            "create",
            "--pid",
            &baseline_pid.to_string(),
            "--tag",
            &snap_name,
        ])
        .output()
        .await
        .context("creating snapshot")?;

    if !snap_output.status.success() {
        let stderr = String::from_utf8_lossy(&snap_output.stderr);
        // Kill baseline before bailing
        common::kill_process(baseline_pid).await;
        let _ = baseline.wait().await;
        anyhow::bail!("snapshot create failed: {}", stderr);
    }
    println!("  Snapshot created");

    // Kill baseline (no longer needed)
    common::kill_process(baseline_pid).await;
    let _ = baseline.wait().await;

    // 3. Start serve
    println!("  Starting serve for '{}'...", snap_name);
    let (mut serve_child, serve_pid) =
        common::spawn_fcvm_with_logs(&["snapshot", "serve", &snap_name], &serve_name)
            .await
            .context("starting serve")?;

    // Give serve a moment to start
    tokio::time::sleep(Duration::from_secs(2)).await;
    println!("  Serve started (PID: {})", serve_pid);

    // 4. Spawn clone
    println!("  Spawning clone from serve...");
    let (mut clone_child, clone_pid) = common::spawn_fcvm(&[
        "snapshot",
        "run",
        "--pid",
        &serve_pid.to_string(),
        "--name",
        &clone_name,
        "--network",
        "bridged",
    ])
    .await
    .context("spawning clone")?;

    println!("  Clone PID: {}", clone_pid);

    let health_result = tokio::time::timeout(
        Duration::from_secs(120),
        common::poll_health_by_pid(clone_pid, 120),
    )
    .await;

    match &health_result {
        Ok(Ok(_)) => {
            println!("  Clone healthy! Running exec...");
            let exec_output =
                common::exec_in_container(clone_pid, &["echo", "hugepage-clone-ok"]).await?;
            assert!(
                exec_output.contains("hugepage-clone-ok"),
                "exec output should contain 'hugepage-clone-ok', got: {}",
                exec_output
            );
            println!("  Clone exec: {}", exec_output.trim());
        }
        Ok(Err(e)) => println!("  Clone health failed: {}", e),
        Err(_) => println!("  Clone health timed out"),
    }

    // Cleanup (kill clone, serve)
    common::kill_process(clone_pid).await;
    let _ = clone_child.wait().await;
    common::kill_process(serve_pid).await;
    let _ = serve_child.wait().await;

    health_result
        .map_err(|_| anyhow::anyhow!("clone health timed out"))?
        .context("clone health failed")?;

    println!("  PASSED: hugepage snapshot/clone workflow works");
    Ok(())
}

/// Test that --hugepages with odd memory value is rejected.
///
/// Hugepages require 2MB-aligned memory, so mem_size_mib must be even.
#[tokio::test]
async fn test_hugepage_mem_validation() -> Result<()> {
    println!("\nHugepage memory validation test");
    println!("================================");

    let (vm_name, _, _, _) = common::unique_names("hp-validate");

    // Try to run with --hugepages --mem 2049 (odd MiB)
    println!("  Testing --hugepages --mem 2049 (should fail)...");
    let fcvm = common::find_fcvm_binary()?;
    let output = tokio::process::Command::new(&fcvm)
        .args([
            "podman",
            "run",
            "--name",
            &vm_name,
            "--network",
            "bridged",
            "--hugepages",
            "--mem",
            "2049",
            common::TEST_IMAGE,
        ])
        .output()
        .await
        .context("running fcvm with odd mem + hugepages")?;

    let stderr = String::from_utf8_lossy(&output.stderr);
    println!("  Exit status: {}", output.status);
    println!("  stderr: {}", stderr.trim());

    assert!(
        !output.status.success(),
        "fcvm should fail with odd memory + hugepages"
    );
    assert!(
        stderr.contains("not divisible by 2") || stderr.contains("hugepages"),
        "error should mention memory alignment, got: {}",
        stderr
    );

    println!("  PASSED: odd memory with hugepages correctly rejected");
    Ok(())
}
