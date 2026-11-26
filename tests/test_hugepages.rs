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
/// Waits up to 60s for hugepages to become free, since other parallel tests
/// may be releasing hugepages as they shut down their VMs.
async fn ensure_hugepages(mem_mib: u32) -> Result<()> {
    let needed = (mem_mib as u64) / 2; // Each hugepage is 2MB

    let nr = tokio::fs::read_to_string("/sys/kernel/mm/hugepages/hugepages-2048kB/nr_hugepages")
        .await
        .context("reading nr_hugepages")?;
    let total: u64 = nr.trim().parse().context("parsing nr_hugepages")?;
    anyhow::ensure!(
        total > 0,
        "No hugepages allocated. Run: echo 512 | sudo tee /proc/sys/vm/nr_hugepages"
    );

    // Wait for enough free hugepages (other tests may be releasing them)
    let start = std::time::Instant::now();
    let timeout = Duration::from_secs(60);
    loop {
        let free =
            tokio::fs::read_to_string("/sys/kernel/mm/hugepages/hugepages-2048kB/free_hugepages")
                .await
                .context("reading free_hugepages")?;
        let free: u64 = free.trim().parse().context("parsing free_hugepages")?;
        let in_use = total - free;

        if free >= needed {
            println!(
                "  Hugepages: {} total, {} free, {} in use (need {} for {}MB VM)",
                total, free, in_use, needed, mem_mib
            );
            return Ok(());
        }

        if start.elapsed() > timeout {
            anyhow::bail!(
                "Not enough free hugepages after {}s: need {} but only {} free ({} in use). \
                 Kill stale VMs or increase pool: echo {} | sudo tee /proc/sys/vm/nr_hugepages",
                timeout.as_secs(),
                needed,
                free,
                in_use,
                needed * 2
            );
        }

        if start.elapsed().as_secs().is_multiple_of(10) && start.elapsed().as_secs() > 0 {
            println!(
                "  Waiting for hugepages: {} free, need {} ({} in use)...",
                free, needed, in_use
            );
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// Wait for hugepages to become free after killing a VM.
///
/// Firecracker under `unshare` (rootless) or in a namespace (bridged) may take
/// a moment to fully exit and release hugepage-backed guest memory.
async fn wait_for_hugepages_free(needed: u64, timeout_secs: u64) -> Result<()> {
    let start = std::time::Instant::now();
    loop {
        let free =
            tokio::fs::read_to_string("/sys/kernel/mm/hugepages/hugepages-2048kB/free_hugepages")
                .await
                .context("reading free_hugepages")?;
        let free: u64 = free.trim().parse().context("parsing free_hugepages")?;
        if free >= needed {
            return Ok(());
        }
        if start.elapsed() > Duration::from_secs(timeout_secs) {
            anyhow::bail!(
                "Hugepages not freed after {}s: need {} but only {} free",
                timeout_secs,
                needed,
                free
            );
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
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

    // Kill first VM and wait for hugepages to be released
    common::kill_process(pid1).await;
    let _ = child1.wait().await;
    let needed_pages = (HP_TEST_MEM_MIB as u64) / 2;
    wait_for_hugepages_free(needed_pages, 30)
        .await
        .context("waiting for first VM hugepages to be released")?;
    println!("  First VM stopped, hugepages released");

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

    // Kill baseline (no longer needed) and wait for hugepages to be released
    common::kill_process(baseline_pid).await;
    let _ = baseline.wait().await;
    let needed_pages = (HP_TEST_MEM_MIB as u64) / 2;
    wait_for_hugepages_free(needed_pages, 30)
        .await
        .context("waiting for baseline VM hugepages to be released")?;
    println!("  Baseline killed, hugepages released");

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

/// Private huge pages of a process, in MiB: `Private_Hugetlb` of its smaps_rollup. A clone of
/// a minor-mode serve maps the serve's memfd, so only the huge pages it wrote, or that were
/// written for it, are private.
fn private_hugetlb_mib(pid: u32) -> Result<u64> {
    let rollup = std::fs::read_to_string(format!("/proc/{pid}/smaps_rollup"))
        .with_context(|| format!("reading /proc/{pid}/smaps_rollup"))?;
    let kb: u64 = rollup
        .lines()
        .find_map(|line| line.strip_prefix("Private_Hugetlb:"))
        .context("no Private_Hugetlb line")?
        .trim()
        .trim_end_matches("kB")
        .trim()
        .parse()
        .context("parsing Private_Hugetlb")?;
    Ok(kb / 1024)
}

/// Sets the balloon target of the VM `pid` runs and waits until the guest has reached it.
async fn balloon_to(pid: u32, target_mib: u32) -> Result<()> {
    let fcvm = common::find_fcvm_binary()?;
    let run = |args: Vec<String>| {
        let fcvm = fcvm.clone();
        async move {
            let out = tokio::process::Command::new(&fcvm)
                .arg("balloon")
                .args(&args)
                .output()
                .await
                .context("running fcvm balloon")?;
            anyhow::ensure!(
                out.status.success(),
                "fcvm balloon {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            let report: serde_json::Value = serde_json::from_slice(&out.stdout)
                .with_context(|| format!("fcvm balloon printed {:?}", out.stdout))?;
            Ok::<_, anyhow::Error>(report)
        }
    };
    run(vec![
        "--pid".into(),
        pid.to_string(),
        target_mib.to_string(),
    ])
    .await?;
    // The guest moves pages at its own pace; the hang guard names what it waited for.
    let guard = std::time::Instant::now();
    loop {
        let report = run(vec!["--pid".into(), pid.to_string()]).await?;
        if report["actual_mib"] == serde_json::json!(target_mib) {
            return Ok(());
        }
        anyhow::ensure!(
            guard.elapsed() < Duration::from_secs(120),
            "the balloon did not reach {target_mib} MiB in 120 s: {report}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Inflating the balloon of a clone on 2 MiB pages must not cost host memory.
///
/// A Linux guest gives the balloon 4 KiB pages, so almost no range covers a whole huge page
/// and the inflate frees nothing. Upstream Firecracker writes zeros over the huge pages a
/// range covers in part, which copies each one for a clone that still shared it with the
/// serve's memfd: measured on the pinned chain before its balloon commit, a 1536 MiB inflate
/// of a 2 GiB clone took 850 MiB of huge pages from the host. The pinned Firecracker zeroes
/// such a page only when the clone already has its own copy.
#[tokio::test]
async fn test_balloon_inflate_on_a_hugepage_minor_clone_copies_no_huge_pages() -> Result<()> {
    ensure_hugepages(HP_TEST_MEM_MIB).await?;
    let (baseline_name, clone_name, snap_name, serve_name) =
        common::unique_names("hp-balloon-clone");
    let mem = HP_TEST_MEM_MIB.to_string();
    let fcvm_path = common::find_fcvm_binary()?;

    let (mut baseline, baseline_pid) = common::spawn_fcvm(&[
        "podman",
        "run",
        "--name",
        &baseline_name,
        "--no-snapshot",
        "--hugepages",
        "--mem",
        &mem,
        "--balloon",
        "0",
        common::TEST_IMAGE,
    ])
    .await
    .context("spawning the baseline hugepage VM")?;
    let created: Result<()> = async {
        common::poll_health_by_pid(baseline_pid, 300).await?;
        let out = tokio::process::Command::new(&fcvm_path)
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
            .context("creating the snapshot")?;
        anyhow::ensure!(
            out.status.success(),
            "snapshot create failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        Ok(())
    }
    .await;
    common::kill_process(baseline_pid).await;
    let _ = baseline.wait().await;
    created?;
    wait_for_hugepages_free((HP_TEST_MEM_MIB as u64) / 2, 30).await?;

    let (mut serve_child, serve_pid) = common::spawn_fcvm_with_logs(
        &["snapshot", "serve", &snap_name, "--uffd-mode", "minor"],
        &serve_name,
    )
    .await
    .context("starting the minor-mode serve")?;
    // The clone finds the serve through its state file and its socket, which exist once it has
    // copied the snapshot into its backing memfd.
    common::poll_serve_ready(&snap_name, serve_pid, 120).await?;
    let (mut clone_child, clone_pid) = common::spawn_fcvm(&[
        "snapshot",
        "run",
        "--pid",
        &serve_pid.to_string(),
        "--name",
        &clone_name,
    ])
    .await
    .context("spawning the clone")?;

    let checked: Result<()> = async {
        common::poll_health_by_pid(clone_pid, 180).await?;
        let vmm = common::firecracker_child_of(clone_pid)
            .context("the clone has no Firecracker process")?;

        // 64 MiB the guest must still hold after the balloon has come and gone.
        let sum = |when: &'static str| async move {
            common::exec_in_vm(clone_pid, &["sh", "-c", "sha256sum /dev/shm/kept | cut -d' ' -f1"])
                .await
                .with_context(|| format!("checksum {when}"))
        };
        common::exec_in_vm(
            clone_pid,
            &["sh", "-c", "dd if=/dev/urandom of=/dev/shm/kept bs=1M count=64 2>/dev/null"],
        )
        .await?;
        let kept = sum("before the inflate").await?;

        let inflate_mib = HP_TEST_MEM_MIB * 5 / 8;
        let before = private_hugetlb_mib(vmm)?;
        balloon_to(clone_pid, inflate_mib).await?;
        let after = private_hugetlb_mib(vmm)?;
        println!(
            "  private huge pages: {before} MiB before a {inflate_mib} MiB inflate, {after} MiB after"
        );
        // The guest's own work during the inflate touches a few huge pages. Zeroing every
        // partly covered one copies most of what the balloon took.
        anyhow::ensure!(
            after <= before + u64::from(inflate_mib) / 4,
            "inflating the balloon by {inflate_mib} MiB made {} MiB of the clone's huge pages \
             private ({before} -> {after} MiB)",
            after - before
        );

        balloon_to(clone_pid, 0).await?;
        anyhow::ensure!(
            sum("after the deflate").await? == kept,
            "the guest's file changed across the inflate and deflate"
        );
        // Memory the guest takes back from the balloon and gives to a new file reads as the
        // file's bytes, zeros here, whatever the huge pages held meanwhile.
        let nonzero = common::exec_in_vm(
            clone_pid,
            &[
                "sh",
                "-c",
                "dd if=/dev/zero of=/dev/shm/zeros bs=1M count=192 2>/dev/null && \
                 tr -d '\\000' < /dev/shm/zeros | wc -c",
            ],
        )
        .await?;
        anyhow::ensure!(
            nonzero.trim() == "0",
            "a file of zeros written after the deflate holds {} non-zero bytes",
            nonzero.trim()
        );
        Ok(())
    }
    .await;

    common::kill_process(clone_pid).await;
    let _ = clone_child.wait().await;
    common::kill_process(serve_pid).await;
    let _ = serve_child.wait().await;
    let deleted = common::delete_snapshot(&snap_name).await;
    checked?;
    deleted
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
