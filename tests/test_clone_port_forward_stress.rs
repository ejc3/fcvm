//! Stress test for clone port forwarding.
//!
//! This test exercises the pasta port forwarding path under stress:
//! - Multiple clones spawned from the same snapshot
//! - Concurrent rapid HTTP requests to all clones simultaneously
//! - Checks for 0-byte responses (pasta connection tracking poisoning)
//!
//! Background: During snapshot restore, `post_start()` calls
//! `wait_for_port_forwarding()` BEFORE the VM snapshot is loaded.
//! pasta accepts the TCP connection (it's listening on loopback),
//! but can't forward to the non-existent guest. This may poison
//! pasta's internal forwarding state, causing subsequent connections
//! to return 0 bytes instead of actual data.

#![cfg(feature = "integration-slow")]

mod common;

use anyhow::{Context, Result};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Instant;

/// Number of clones to spawn
const NUM_CLONES: usize = 3;

/// Number of HTTP requests per clone
const REQUESTS_PER_CLONE: usize = 20;

/// Stress test: multiple clones with port forwarding, concurrent HTTP requests
///
/// Reproduces the "connect succeeded but 0 bytes" pattern seen in CI bench-vm
/// failures. The hypothesis: pasta's `wait_for_port_forwarding()` probe during
/// restore (before guest exists) poisons pasta's internal forwarding state.
#[tokio::test]
async fn test_clone_port_forward_stress_rootless() -> Result<()> {
    let (baseline_name, _, snapshot_name, _) = common::unique_names("pf-stress");

    println!("\n╔═══════════════════════════════════════════════════════════════╗");
    println!("║     Clone Port Forward Stress Test (rootless)                ║");
    println!(
        "║     {} clones × {} requests each (concurrent)              ║",
        NUM_CLONES, REQUESTS_PER_CLONE
    );
    println!("╚═══════════════════════════════════════════════════════════════╝\n");

    let fcvm_path = common::find_fcvm_binary()?;

    // Allocate port before baseline so it's baked into the snapshot
    let host_port = common::find_available_high_port().context("finding available port")?;
    let publish_arg = format!("{}:80", host_port);

    // Step 1: Start baseline VM with nginx + port forwarding
    println!(
        "Step 1: Starting baseline VM (nginx, --publish {})...",
        publish_arg
    );
    let (_baseline_child, baseline_pid) = common::spawn_fcvm_with_logs(
        &[
            "podman",
            "run",
            "--name",
            &baseline_name,
            "--network",
            "rootless",
            "--publish",
            &publish_arg,
            "--health-check",
            "http://localhost:80",
            common::TEST_IMAGE,
        ],
        &baseline_name,
    )
    .await
    .context("spawning baseline VM")?;

    println!("  Waiting for baseline VM to become healthy...");
    common::poll_health_by_pid(baseline_pid, 90).await?;
    println!("  ✓ Baseline VM healthy (PID: {})", baseline_pid);

    // Verify baseline port forwarding works before snapshotting
    let baseline_ip = common::get_loopback_ip(baseline_pid).await?;
    println!("  Baseline loopback IP: {}", baseline_ip);

    let baseline_check = common::curl_check(&baseline_ip, host_port, 5).await;
    println!(
        "  Baseline HTTP check: {} ({} bytes)",
        if baseline_check.success {
            "✓"
        } else {
            "✗ FAIL"
        },
        baseline_check.body_len
    );
    assert!(
        baseline_check.success && baseline_check.body_len > 0,
        "Baseline nginx must respond with data before snapshot"
    );

    // Step 2: Create snapshot
    println!("\nStep 2: Creating snapshot...");
    let output = tokio::process::Command::new(&fcvm_path)
        .args([
            "snapshot",
            "create",
            "--pid",
            &baseline_pid.to_string(),
            "--tag",
            &snapshot_name,
        ])
        .output()
        .await
        .context("running snapshot create")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Snapshot creation failed: {}", stderr);
    }
    println!("  ✓ Snapshot created");

    // Kill baseline - only need snapshot for clones
    common::kill_process(baseline_pid).await;
    println!("  Killed baseline VM");

    // Step 3: Start memory server
    println!("\nStep 3: Starting memory server...");
    let (_serve_child, serve_pid) =
        common::spawn_fcvm_with_logs(&["snapshot", "serve", &snapshot_name], "uffd-pf-stress")
            .await
            .context("spawning memory server")?;

    common::poll_serve_ready(serve_pid, 30).await?;
    println!("  ✓ Memory server ready (PID: {})", serve_pid);

    // Step 4: Spawn all clones concurrently
    println!("\nStep 4: Spawning {} clones concurrently...", NUM_CLONES);
    let serve_pid_str = serve_pid.to_string();

    struct CloneInfo {
        name: String,
        pid: u32,
        loopback_ip: String,
        _child: tokio::process::Child,
    }

    // Spawn all clones concurrently using JoinSet
    let mut spawn_set = tokio::task::JoinSet::new();

    for i in 0..NUM_CLONES {
        let clone_name = format!("pf-stress-clone-{}-{}", i, std::process::id());
        println!("  Spawning clone {} ({})...", i, clone_name);
        let pid_str = serve_pid_str.clone();
        spawn_set.spawn(async move {
            let (child, clone_pid) = common::spawn_fcvm_with_logs(
                &["snapshot", "run", "--pid", &pid_str, "--name", &clone_name],
                &clone_name,
            )
            .await
            .context(format!("spawning clone {}", clone_name))?;

            common::poll_health_by_pid(clone_pid, 120).await?;
            let loopback_ip = common::get_loopback_ip(clone_pid).await?;
            Ok::<_, anyhow::Error>((clone_name, clone_pid, loopback_ip, child))
        });
    }

    let mut clones: Vec<CloneInfo> = Vec::new();
    while let Some(result) = spawn_set.join_next().await {
        let (name, pid, loopback_ip, child) = result??;
        println!(
            "  ✓ Clone {} healthy (PID: {}, IP: {})",
            name, pid, loopback_ip
        );
        clones.push(CloneInfo {
            name,
            pid,
            loopback_ip,
            _child: child,
        });
    }

    // Step 5: Verify each clone's port forwarding before the stress storm.
    // One request per clone: verify_port_forwarding() already confirmed the L2
    // path, and pasta no longer retargets forwarding from overheard bridge
    // traffic, so the first request must succeed.
    println!("\nStep 5: Pre-storm verification of each clone...");
    for clone in &clones {
        let check =
            common::curl_check_with_diag(&clone.loopback_ip, host_port, 10, Some(clone.pid)).await;
        println!(
            "  Clone {} ({}:{}): {} ({} bytes)",
            clone.name,
            clone.loopback_ip,
            host_port,
            if check.success { "OK" } else { "FAIL" },
            check.body_len,
        );
        assert!(
            check.success && check.body_len > 0,
            "Pre-storm curl to clone {} failed: {}",
            clone.name,
            check.error
        );
    }

    // Step 6: Concurrent HTTP requests to all clones simultaneously
    println!(
        "\nStep 6: Sending {} HTTP requests to each of {} clones (concurrently)...",
        REQUESTS_PER_CLONE, NUM_CLONES
    );

    let total_success = Arc::new(AtomicU32::new(0));
    let total_zero_bytes = Arc::new(AtomicU32::new(0));
    let total_errors = Arc::new(AtomicU32::new(0));

    let start = Instant::now();

    // Spawn concurrent tasks for all clones
    let mut handles = Vec::new();
    for clone in &clones {
        let ip = clone.loopback_ip.clone();
        let name = clone.name.clone();
        let clone_pid = clone.pid;
        let success = Arc::clone(&total_success);
        let zero = Arc::clone(&total_zero_bytes);
        let errors = Arc::clone(&total_errors);

        let handle = tokio::spawn(async move {
            let mut clone_success = 0u32;
            let mut clone_zero = 0u32;
            let mut clone_error = 0u32;
            let force_diag = std::env::var("FCVM_FORCE_DIAG")
                .map(|v| !v.is_empty())
                .unwrap_or(false);

            for req in 0..REQUESTS_PER_CLONE {
                let req_start = Instant::now();
                let result = common::curl_check(&ip, host_port, 5).await;
                let req_ms = req_start.elapsed().as_millis();

                if result.success && result.body_len > 0 {
                    clone_success += 1;
                    success.fetch_add(1, Ordering::Relaxed);
                    if req % 5 == 0 || force_diag {
                        println!(
                            "    Clone {} req {}: OK {}b {}ms",
                            name, req, result.body_len, req_ms
                        );
                    }
                } else if result.success && result.body_len == 0 {
                    clone_zero += 1;
                    zero.fetch_add(1, Ordering::Relaxed);
                    println!("    ⚠ Clone {} request {}: 0-byte response!", name, req);
                } else {
                    clone_error += 1;
                    errors.fetch_add(1, Ordering::Relaxed);
                    if clone_error <= 3 {
                        println!(
                            "    ✗ Clone {} request {} to {}:{}: error ({})",
                            name, req, ip, host_port, result.error
                        );
                    }
                }

                // On first error (or forced via FCVM_FORCE_DIAG at req 3), dump extensive diagnostics
                let run_diag = clone_error == 1 || (force_diag && req == 3 && clone_error == 0);
                if run_diag {
                    let port_str = host_port.to_string();
                    let pid_str = clone_pid.to_string();

                    println!(
                        "    DIAG === clone {} (pid={}) first failure diagnostics ===",
                        name, clone_pid
                    );

                    // 1. Verbose curl to see exact failure point
                    if let Ok(out) = tokio::process::Command::new("curl")
                        .args([
                            "-v",
                            "--max-time",
                            "2",
                            &format!("http://{}:{}", ip, host_port),
                        ])
                        .output()
                        .await
                    {
                        let stderr = String::from_utf8_lossy(&out.stderr);
                        println!(
                            "    DIAG [verbose curl via pasta]:\n      {}",
                            stderr.lines().collect::<Vec<_>>().join("\n      ")
                        );
                    }

                    // 2. Listening sockets for our port
                    if let Ok(out) = tokio::process::Command::new("ss")
                        .args(["-tlnp"])
                        .output()
                        .await
                    {
                        let stdout = String::from_utf8_lossy(&out.stdout);
                        let matching: Vec<&str> = stdout
                            .lines()
                            .filter(|l| l.contains(&port_str) || l.starts_with("State"))
                            .collect();
                        println!("    DIAG [ss -tlnp]: {:?}", matching);
                    }

                    // 3. ALL TCP sockets (ESTABLISHED, TIME_WAIT, etc.)
                    if let Ok(out) = tokio::process::Command::new("ss")
                        .args(["-tanp"])
                        .output()
                        .await
                    {
                        let stdout = String::from_utf8_lossy(&out.stdout);
                        let matching: Vec<&str> = stdout
                            .lines()
                            .filter(|l| {
                                l.contains(&port_str)
                                    || l.contains(&ip)
                                    || l.starts_with("State")
                                    || l.contains("TIME-WAIT")
                            })
                            .collect();
                        println!("    DIAG [ss -tanp relevant]: {:?}", matching);
                    }

                    // 4. pasta process check
                    if let Ok(out) = tokio::process::Command::new("pgrep")
                        .args(["-a", "pasta"])
                        .output()
                        .await
                    {
                        let stdout = String::from_utf8_lossy(&out.stdout);
                        let matching: Vec<&str> = stdout
                            .lines()
                            .filter(|l| l.contains(&ip) || l.contains(&port_str))
                            .collect();
                        println!("    DIAG [pasta for this clone]: {:?}", matching);
                    }

                    // 5. Get holder_pid for nsenter diagnostics
                    let fcvm_path = common::find_fcvm_binary().unwrap();
                    let holder_pid = common::get_holder_pid_for_diag(&fcvm_path, clone_pid).await;
                    if let Some(hpid) = holder_pid {
                        let hpid_str = hpid.to_string();

                        // 6. nsenter curl — bypass pasta, curl guest directly through namespace
                        if let Ok(out) = tokio::process::Command::new("nsenter")
                            .args([
                                "-t",
                                &hpid_str,
                                "--net",
                                "curl",
                                "-sS",
                                "--max-time",
                                "2",
                                "http://10.0.2.100:80",
                            ])
                            .output()
                            .await
                        {
                            let body_len = out.stdout.len();
                            let stderr = String::from_utf8_lossy(&out.stderr);
                            let status = out.status;
                            println!("    DIAG [nsenter curl 10.0.2.100:80]: status={} body={} bytes stderr={}", status, body_len, stderr.trim());
                        }

                        // 7. ARP table in namespace
                        if let Ok(out) = tokio::process::Command::new("nsenter")
                            .args(["-t", &hpid_str, "--net", "ip", "neigh"])
                            .output()
                            .await
                        {
                            let stdout = String::from_utf8_lossy(&out.stdout);
                            println!(
                                "    DIAG [nsenter ip neigh]: {}",
                                stdout.lines().collect::<Vec<_>>().join(" | ")
                            );
                        }

                        // 8. Bridge state in namespace
                        if let Ok(out) = tokio::process::Command::new("nsenter")
                            .args(["-t", &hpid_str, "--net", "bridge", "link"])
                            .output()
                            .await
                        {
                            let stdout = String::from_utf8_lossy(&out.stdout);
                            println!(
                                "    DIAG [nsenter bridge link]: {}",
                                stdout.lines().collect::<Vec<_>>().join(" | ")
                            );
                        }

                        // 9. All TCP sockets inside the namespace (TIME_WAIT from pasta splice)
                        if let Ok(out) = tokio::process::Command::new("nsenter")
                            .args(["-t", &hpid_str, "--net", "ss", "-tanp"])
                            .output()
                            .await
                        {
                            let stdout = String::from_utf8_lossy(&out.stdout);
                            println!("    DIAG [nsenter ss -tanp (namespace)]:");
                            for line in stdout.lines() {
                                println!("      {}", line);
                            }
                        }

                        // 10. Ping guest from namespace
                        if let Ok(out) = tokio::process::Command::new("nsenter")
                            .args([
                                "-t",
                                &hpid_str,
                                "--net",
                                "ping",
                                "-c",
                                "1",
                                "-W",
                                "1",
                                "10.0.2.100",
                            ])
                            .output()
                            .await
                        {
                            let stdout = String::from_utf8_lossy(&out.stdout);
                            let status = out.status;
                            println!(
                                "    DIAG [nsenter ping 10.0.2.100]: status={} {}",
                                status,
                                stdout.lines().last().unwrap_or("")
                            );
                        }
                    } else {
                        println!(
                            "    DIAG [holder_pid]: could not determine holder PID for nsenter"
                        );
                    }

                    // 11. Check nginx inside the VM via exec
                    if let Ok(out) = tokio::process::Command::new(&fcvm_path)
                            .args(["exec", "--pid", &pid_str, "--vm", "--", "sh", "-c",
                                   "echo nginx_pids=$(pgrep nginx | tr '\\n' ','); curl -sS --max-time 2 http://localhost:80 | wc -c; echo nginx_conns=$(ss -tn | grep ':80 ' | wc -l)"])
                            .output().await
                        {
                            let stdout = String::from_utf8_lossy(&out.stdout);
                            let stderr = String::from_utf8_lossy(&out.stderr);
                            println!("    DIAG [exec in VM — nginx check]: stdout={} stderr={}", stdout.trim(), stderr.trim());
                        }

                    // 12. Check guest's conntrack / iptables
                    if let Ok(out) = tokio::process::Command::new(&fcvm_path)
                            .args(["exec", "--pid", &pid_str, "--vm", "--", "sh", "-c",
                                   "ss -tan | head -20; echo '---'; ip neigh; echo '---'; cat /proc/sys/net/ipv4/tcp_max_syn_backlog 2>/dev/null; echo '---'; cat /proc/sys/net/core/somaxconn 2>/dev/null"])
                            .output().await
                        {
                            let stdout = String::from_utf8_lossy(&out.stdout);
                            println!("    DIAG [exec in VM — tcp state]:");
                            for line in stdout.lines() {
                                println!("      {}", line);
                            }
                        }

                    // 13. dmesg inside VM for TCP errors
                    if let Ok(out) = tokio::process::Command::new(&fcvm_path)
                            .args(["exec", "--pid", &pid_str, "--vm", "--", "sh", "-c",
                                   "dmesg 2>/dev/null | grep -iE 'tcp|conntrack|drop|reset|syn|nf_' | tail -10 || echo 'no dmesg access'"])
                            .output().await
                        {
                            let stdout = String::from_utf8_lossy(&out.stdout);
                            if !stdout.trim().is_empty() {
                                println!("    DIAG [exec in VM — dmesg tcp]: {}", stdout.trim());
                            }
                        }

                    // 14. Raw TCP test from namespace via nc (bypasses HTTP)
                    if let Some(hpid) = holder_pid {
                        let hpid_str = hpid.to_string();
                        if let Ok(out) = tokio::process::Command::new("nsenter")
                                .args([
                                    "-t",
                                    &hpid_str,
                                    "--net",
                                    "sh",
                                    "-c",
                                    "echo -e 'GET / HTTP/1.0\\r\\nHost: test\\r\\n\\r\\n' | nc -w 2 10.0.2.100 80 | head -1",
                                ])
                                .output()
                                .await
                            {
                                let stdout = String::from_utf8_lossy(&out.stdout);
                                let stderr = String::from_utf8_lossy(&out.stderr);
                                println!(
                                    "    DIAG [nsenter nc 10.0.2.100:80]: stdout={} stderr={}",
                                    stdout.trim(),
                                    stderr.trim()
                                );
                            }

                        // 14b. /proc/net/sockstat inside namespace (socket counts)
                        if let Ok(out) = tokio::process::Command::new("nsenter")
                            .args(["-t", &hpid_str, "--net", "cat", "/proc/net/sockstat"])
                            .output()
                            .await
                        {
                            let stdout = String::from_utf8_lossy(&out.stdout);
                            println!("    DIAG [nsenter /proc/net/sockstat]: {}", stdout.trim());
                        }
                    }

                    // 15. Pasta fd count, /proc status, and memory (are fds/memory leaking?)
                    if let Ok(out) = tokio::process::Command::new("pgrep")
                        .args(["-a", "pasta"])
                        .output()
                        .await
                    {
                        let stdout = String::from_utf8_lossy(&out.stdout);
                        for line in stdout.lines() {
                            if line.contains(&ip) {
                                if let Some(pasta_pid) = line.split_whitespace().next() {
                                    // fd count
                                    if let Ok(fds) = tokio::process::Command::new("ls")
                                        .args([&format!("/proc/{}/fd", pasta_pid)])
                                        .output()
                                        .await
                                    {
                                        let fd_count =
                                            String::from_utf8_lossy(&fds.stdout).lines().count();
                                        println!(
                                            "    DIAG [pasta pid={} fd count]: {}",
                                            pasta_pid, fd_count
                                        );
                                    }
                                    // VmRSS and threads
                                    if let Ok(status) = tokio::process::Command::new("sh")
                                        .args([
                                            "-c",
                                            &format!(
                                                "grep -E 'VmRSS|Threads|FDSize' /proc/{}/status",
                                                pasta_pid
                                            ),
                                        ])
                                        .output()
                                        .await
                                    {
                                        let stdout = String::from_utf8_lossy(&status.stdout);
                                        println!(
                                            "    DIAG [pasta pid={} status]: {}",
                                            pasta_pid,
                                            stdout.lines().collect::<Vec<_>>().join(" | ")
                                        );
                                    }
                                }
                            }
                        }
                    }

                    // 15. tcpdump in namespace while doing a curl — capture the actual RST
                    if let Some(hpid) = holder_pid {
                        let hpid_str = hpid.to_string();
                        // Start tcpdump in background, do a curl via pasta, then collect
                        let tcpdump_hpid = hpid_str.clone();
                        let tcpdump_handle = tokio::spawn(async move {
                            tokio::process::Command::new("nsenter")
                                .args([
                                    "-t",
                                    &tcpdump_hpid,
                                    "--net",
                                    "timeout",
                                    "3",
                                    "tcpdump",
                                    "-i",
                                    "br0",
                                    "-c",
                                    "20",
                                    "-nn",
                                    "port",
                                    "80",
                                ])
                                .output()
                                .await
                        });
                        // Brief pause to let tcpdump start
                        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                        // Now do a curl via pasta (this should fail and tcpdump captures it)
                        let pasta_retry = common::curl_check(&ip, host_port, 2).await;
                        println!(
                            "    DIAG [pasta curl during tcpdump]: success={} body={} err={}",
                            pasta_retry.success,
                            pasta_retry.body_len,
                            pasta_retry.error.trim()
                        );
                        // Also try nsenter curl during tcpdump
                        if let Ok(out) = tokio::process::Command::new("nsenter")
                            .args([
                                "-t",
                                &hpid_str,
                                "--net",
                                "curl",
                                "-sS",
                                "--max-time",
                                "2",
                                "http://10.0.2.100:80",
                            ])
                            .output()
                            .await
                        {
                            println!(
                                "    DIAG [nsenter curl during tcpdump]: status={} body={} bytes",
                                out.status,
                                out.stdout.len()
                            );
                        }
                        // Collect tcpdump output
                        if let Ok(Ok(out)) = tcpdump_handle.await {
                            let stderr = String::from_utf8_lossy(&out.stderr);
                            println!("    DIAG [tcpdump br0 port 80]:");
                            for line in stderr.lines() {
                                println!("      {}", line);
                            }
                        }

                        // 16. conntrack entries in namespace
                        if let Ok(out) = tokio::process::Command::new("nsenter")
                                .args(["-t", &hpid_str, "--net", "sh", "-c",
                                       "cat /proc/net/nf_conntrack 2>/dev/null | grep ':0050 ' | head -10 || echo 'no conntrack'"])
                                .output().await
                            {
                                let stdout = String::from_utf8_lossy(&out.stdout);
                                if !stdout.trim().is_empty() {
                                    println!("    DIAG [nsenter conntrack port 80]: {}", stdout.trim());
                                }
                            }
                    }

                    println!("    DIAG === end diagnostics for clone {} ===", name);
                }
            }

            (name, clone_success, clone_zero, clone_error)
        });
        handles.push(handle);
    }

    // Wait for all concurrent tasks to complete
    for handle in handles {
        let (name, ok, zero, err) = handle.await?;
        println!(
            "  Clone {}: {}/{} OK, {} zero-byte, {} errors",
            name, ok, REQUESTS_PER_CLONE, zero, err
        );
    }

    let elapsed = start.elapsed();
    let total_requests = (NUM_CLONES * REQUESTS_PER_CLONE) as u32;
    let success = total_success.load(Ordering::Relaxed);
    let zero_bytes = total_zero_bytes.load(Ordering::Relaxed);
    let errs = total_errors.load(Ordering::Relaxed);

    // Cleanup
    println!("\nCleaning up...");
    for clone in &clones {
        common::kill_process(clone.pid).await;
    }
    common::kill_process(serve_pid).await;
    println!("  Killed all clones and memory server");

    // Results
    println!("\n╔═══════════════════════════════════════════════════════════════╗");
    println!("║                         RESULTS                               ║");
    println!("╠═══════════════════════════════════════════════════════════════╣");
    println!(
        "║  Total requests:    {:4}  ({:.1}s concurrent)                ║",
        total_requests,
        elapsed.as_secs_f64()
    );
    println!(
        "║  Successful:        {:4}                                      ║",
        success
    );
    println!(
        "║  Zero-byte:         {:4}  (pasta poisoning pattern)           ║",
        zero_bytes
    );
    println!(
        "║  Errors:            {:4}                                      ║",
        errs
    );
    println!("╚═══════════════════════════════════════════════════════════════╝");

    if zero_bytes > 0 {
        anyhow::bail!(
            "PASTA POISONING DETECTED: {} out of {} requests returned 0 bytes. \
             This confirms that wait_for_port_forwarding() during restore \
             (before guest exists) corrupts pasta's forwarding state.",
            zero_bytes,
            total_requests
        );
    }

    if errs > 0 {
        anyhow::bail!(
            "Port forwarding errors: {} out of {} requests failed",
            errs,
            total_requests
        );
    }

    println!("\n✅ CLONE PORT FORWARD STRESS TEST PASSED!");
    println!(
        "   All {} requests across {} clones returned valid data",
        total_requests, NUM_CLONES
    );
    Ok(())
}
