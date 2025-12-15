//! Egress stress test - many clones, parallel exec
//!
//! This test:
//! 1. Starts a local HTTP server on the host
//! 2. Creates a baseline VM and snapshot
//! 3. Spawns multiple clones in parallel
//! 4. Runs parallel curl commands from each clone to the local HTTP server
//! 5. Verifies all requests succeed

mod common;

use anyhow::{Context, Result};
use std::process::Stdio;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::Semaphore;

/// Number of clones to spawn
const NUM_CLONES: usize = 10;

/// Number of parallel requests per clone
const REQUESTS_PER_CLONE: usize = 5;

/// Port for local HTTP server
const HTTP_SERVER_PORT: u16 = 18080;

/// Test egress stress with bridged networking using local HTTP server
///
/// Uses CONNMARK-based routing to ensure each clone's egress traffic is routed
/// back to the correct clone, even though they all share the same guest IP.
#[tokio::test]
async fn test_egress_stress_bridged() -> Result<()> {
    egress_stress_impl("bridged", NUM_CLONES, REQUESTS_PER_CLONE).await
}

/// Test egress stress with rootless networking using local HTTP server
#[tokio::test]
async fn test_egress_stress_rootless() -> Result<()> {
    egress_stress_impl("rootless", NUM_CLONES, REQUESTS_PER_CLONE).await
}

async fn egress_stress_impl(network: &str, num_clones: usize, requests_per_clone: usize) -> Result<()> {
    let test_name = format!("egress-stress-{}", network);

    println!("\n╔═══════════════════════════════════════════════════════════════╗");
    println!(
        "║     Egress Stress Test ({:8}) - {} clones, {} req/clone     ║",
        network, num_clones, requests_per_clone
    );
    println!("╚═══════════════════════════════════════════════════════════════╝\n");

    // Step 0: Start local HTTP server
    println!("Step 0: Starting local HTTP server on port {}...", HTTP_SERVER_PORT);
    let http_server = start_http_server(HTTP_SERVER_PORT).await?;
    println!("  ✓ HTTP server started (PID: {})", http_server.id().unwrap_or(0));

    // Determine the URL that VMs will use to test egress
    // For bridged mode, we use the host's primary network interface IP. Traffic to this IP
    // goes through NAT (MASQUERADE), so CONNMARK-based routing ensures correct return path.
    // For rootless mode, slirp4netns handles all routing so local traffic works fine (10.0.2.2).
    let egress_url = match network {
        "rootless" => format!("http://10.0.2.2:{}/", HTTP_SERVER_PORT),
        "bridged" => {
            // Get host's primary interface IP (the IP used to reach external networks)
            // Traffic to this IP from VMs goes through NAT, so CONNMARK works
            let host_ip = get_host_primary_ip().await?;
            format!("http://{}:{}/", host_ip, HTTP_SERVER_PORT)
        }
        _ => anyhow::bail!("Unknown network type: {}", network),
    };
    println!("  VMs will reach server at: {}", egress_url);

    let fcvm_path = common::find_fcvm_binary()?;

    // Step 1: Start baseline VM
    let baseline_name = format!("{}-baseline", test_name);
    println!("\nStep 1: Starting baseline VM '{}'...", baseline_name);

    let mut baseline_child = tokio::process::Command::new(&fcvm_path)
        .args([
            "podman",
            "run",
            "--name",
            &baseline_name,
            "--network",
            network,
            "nginx:alpine",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawning baseline VM")?;

    let baseline_pid = baseline_child
        .id()
        .ok_or_else(|| anyhow::anyhow!("failed to get baseline PID"))?;

    spawn_log_consumer(baseline_child.stdout.take(), &baseline_name);
    spawn_log_consumer_stderr(baseline_child.stderr.take(), &baseline_name);

    common::poll_health_by_pid(baseline_pid, 120).await?;
    println!("  ✓ Baseline healthy");

    // For rootless, the URL is already correct. For bridged, we use external server.
    println!("  Final egress URL: {}", egress_url);

    // Verify baseline can reach the test server
    println!("  Verifying baseline egress...");
    let output = tokio::process::Command::new(&fcvm_path)
        .args([
            "exec",
            "--pid",
            &baseline_pid.to_string(),
            "--vm",
            "--",
            "curl",
            "-s",
            "--max-time",
            "10",
            "-o",
            "/dev/null",
            "-w",
            "%{http_code}",
            &egress_url,
        ])
        .output()
        .await?;

    let http_code = String::from_utf8_lossy(&output.stdout);
    if !output.status.success() || http_code.trim() != "200" {
        let stderr = String::from_utf8_lossy(&output.stderr);
        eprintln!("  Baseline egress failed: status={}, code='{}', stderr='{}'",
            output.status, http_code.trim(), stderr.lines().next().unwrap_or(""));
        common::kill_process(baseline_pid).await;
        stop_http_server(http_server).await;
        anyhow::bail!("Baseline egress verification failed");
    }
    println!("  ✓ Baseline egress works");

    // Step 2: Create snapshot
    let snapshot_name = format!("{}-snapshot", test_name);
    println!("\nStep 2: Creating snapshot '{}'...", snapshot_name);

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
        .context("creating snapshot")?;

    if !output.status.success() {
        common::kill_process(baseline_pid).await;
        stop_http_server(http_server).await;
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Snapshot creation failed: {}", stderr);
    }
    println!("  ✓ Snapshot created");

    // Kill baseline
    common::kill_process(baseline_pid).await;
    println!("  Killed baseline");
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Step 3: Start memory server
    println!("\nStep 3: Starting memory server...");
    let mut serve_child = tokio::process::Command::new(&fcvm_path)
        .args(["snapshot", "serve", &snapshot_name])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawning memory server")?;

    let serve_pid = serve_child
        .id()
        .ok_or_else(|| anyhow::anyhow!("failed to get serve PID"))?;

    spawn_log_consumer(serve_child.stdout.take(), "uffd-server");
    spawn_log_consumer_stderr(serve_child.stderr.take(), "uffd-server");

    // Wait for server to be ready
    tokio::time::sleep(Duration::from_secs(2)).await;
    println!("  ✓ Memory server ready (PID: {})", serve_pid);

    // Step 4: Spawn clones in parallel
    println!("\nStep 4: Spawning {} clones in parallel...", num_clones);
    let start_spawn = Instant::now();

    let mut clone_handles = Vec::new();
    for i in 0..num_clones {
        let fcvm = fcvm_path.clone();
        let name = format!("{}-clone-{}", test_name, i);
        let net = network.to_string();
        let spid = serve_pid;

        let handle = tokio::spawn(async move {
            let mut child = tokio::process::Command::new(&fcvm)
                .args([
                    "snapshot",
                    "run",
                    "--pid",
                    &spid.to_string(),
                    "--name",
                    &name,
                    "--network",
                    &net,
                ])
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()?;

            let pid = child
                .id()
                .ok_or_else(|| anyhow::anyhow!("no PID for clone"))?;

            spawn_log_consumer(child.stdout.take(), &name);
            spawn_log_consumer_stderr(child.stderr.take(), &name);

            Ok::<_, anyhow::Error>((name, pid))
        });

        clone_handles.push(handle);
    }

    // Collect results
    let mut clone_pids: Vec<(String, u32)> = Vec::new();
    for handle in clone_handles {
        match handle.await? {
            Ok((name, pid)) => {
                println!("  Spawned {} (PID: {})", name, pid);
                clone_pids.push((name, pid));
            }
            Err(e) => {
                eprintln!("  Failed to spawn clone: {}", e);
            }
        }
    }

    let spawn_duration = start_spawn.elapsed();
    println!(
        "  Spawned {} clones in {:.2}s",
        clone_pids.len(),
        spawn_duration.as_secs_f64()
    );

    // Step 5: Wait for clones to become healthy
    println!("\nStep 5: Waiting for clones to become healthy...");
    let start_health = Instant::now();

    let mut healthy_clones = Vec::new();
    for (name, pid) in &clone_pids {
        match common::poll_health_by_pid(*pid, 60).await {
            Ok(()) => {
                println!("  ✓ {} healthy", name);
                healthy_clones.push((*pid, name.clone()));
            }
            Err(e) => {
                eprintln!("  ✗ {} failed health check: {}", name, e);
            }
        }
    }

    let health_duration = start_health.elapsed();
    println!(
        "  {}/{} clones healthy in {:.2}s",
        healthy_clones.len(),
        clone_pids.len(),
        health_duration.as_secs_f64()
    );

    if healthy_clones.is_empty() {
        // Cleanup and fail
        for (_name, pid) in &clone_pids {
            common::kill_process(*pid).await;
        }
        common::kill_process(serve_pid).await;
        stop_http_server(http_server).await;
        anyhow::bail!("No clones became healthy");
    }

    // Step 6: Run parallel egress requests
    println!(
        "\nStep 6: Running {} parallel requests from {} clones...",
        requests_per_clone * healthy_clones.len(),
        healthy_clones.len()
    );

    let start_requests = Instant::now();
    let success_count = Arc::new(AtomicUsize::new(0));
    let failure_count = Arc::new(AtomicUsize::new(0));
    let semaphore = Arc::new(Semaphore::new(20)); // Limit concurrent requests

    let mut request_handles = Vec::new();

    for (pid, _name) in &healthy_clones {
        for _req_id in 0..requests_per_clone {
            let fcvm = fcvm_path.clone();
            let clone_pid = *pid;
            let sem = semaphore.clone();
            let success = success_count.clone();
            let failure = failure_count.clone();
            let url = egress_url.clone();

            let handle = tokio::spawn(async move {
                let _permit = sem.acquire().await.unwrap();

                let output = tokio::process::Command::new(&fcvm)
                    .args([
                        "exec",
                        "--pid",
                        &clone_pid.to_string(),
                        "--vm",
                        "--",
                        "curl",
                        "-s",
                        "--max-time",
                        "10",
                        "-o",
                        "/dev/null",
                        "-w",
                        "%{http_code}",
                        &url,
                    ])
                    .output()
                    .await;

                match output {
                    Ok(out) => {
                        let code = String::from_utf8_lossy(&out.stdout);
                        let stderr = String::from_utf8_lossy(&out.stderr);
                        if out.status.success() && code.trim() == "200" {
                            success.fetch_add(1, Ordering::Relaxed);
                        } else {
                            eprintln!("Request failed: status={}, stdout='{}', stderr='{}'",
                                out.status, code.trim(), stderr.lines().next().unwrap_or(""));
                            failure.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    Err(e) => {
                        eprintln!("Request error: {}", e);
                        failure.fetch_add(1, Ordering::Relaxed);
                    }
                }
            });

            request_handles.push(handle);
        }
    }

    // Wait for all requests to complete
    for handle in request_handles {
        let _ = handle.await;
    }

    let request_duration = start_requests.elapsed();
    let total_success = success_count.load(Ordering::Relaxed);
    let total_failure = failure_count.load(Ordering::Relaxed);
    let total_requests = total_success + total_failure;

    println!(
        "  Completed {} requests in {:.2}s ({:.1} req/s)",
        total_requests,
        request_duration.as_secs_f64(),
        total_requests as f64 / request_duration.as_secs_f64()
    );
    println!("  Success: {}, Failure: {}", total_success, total_failure);

    // Cleanup
    println!("\nCleaning up...");
    for (name, pid) in &clone_pids {
        common::kill_process(*pid).await;
        println!("  Killed {}", name);
    }
    common::kill_process(serve_pid).await;
    println!("  Killed memory server");
    stop_http_server(http_server).await;
    println!("  Stopped HTTP server");

    // Report results
    let success_rate = total_success as f64 / total_requests as f64 * 100.0;

    if success_rate >= 95.0 {
        println!(
            "\n✅ EGRESS STRESS TEST PASSED! (network: {}, success rate: {:.1}%)",
            network, success_rate
        );
        Ok(())
    } else {
        println!(
            "\n❌ EGRESS STRESS TEST FAILED! (network: {}, success rate: {:.1}%)",
            network, success_rate
        );
        anyhow::bail!(
            "Success rate {:.1}% below threshold 95%",
            success_rate
        )
    }
}

/// Start a simple HTTP server using Python
async fn start_http_server(port: u16) -> Result<tokio::process::Child> {
    // Use Python's built-in HTTP server
    let child = tokio::process::Command::new("python3")
        .args(["-m", "http.server", &port.to_string(), "--bind", "0.0.0.0"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("starting Python HTTP server")?;

    // Give it a moment to start
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Verify it's running
    let check = tokio::process::Command::new("curl")
        .args(["-s", "-o", "/dev/null", "-w", "%{http_code}", &format!("http://127.0.0.1:{}/", port)])
        .output()
        .await?;

    if String::from_utf8_lossy(&check.stdout).trim() != "200" {
        anyhow::bail!("HTTP server not responding on port {}", port);
    }

    Ok(child)
}

/// Stop the HTTP server
async fn stop_http_server(mut server: tokio::process::Child) {
    let _ = server.kill().await;
}

/// Get the host's primary network interface IP (used for reaching external networks)
/// This is the IP that VMs can reach via NAT
async fn get_host_primary_ip() -> Result<String> {
    // Use "ip route get 8.8.8.8" to find which interface/IP is used for external traffic
    let output = tokio::process::Command::new("ip")
        .args(["route", "get", "8.8.8.8"])
        .output()
        .await
        .context("running ip route get")?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    // Output looks like: "8.8.8.8 via 172.31.0.1 dev enp3s0 src 172.31.15.123 uid 0"
    // We want the IP after "src"
    for part in stdout.split_whitespace().collect::<Vec<_>>().windows(2) {
        if part[0] == "src" {
            return Ok(part[1].to_string());
        }
    }

    anyhow::bail!("Could not determine host primary IP from: {}", stdout)
}

/// Get host_ip from VM state for bridged networking
#[allow(dead_code)]
async fn get_host_ip_from_state(pid: u32) -> Result<String> {
    // Read state file to get host_ip
    let state_dir = "/mnt/fcvm-btrfs/state";

    // Find state file for this PID
    let output = tokio::process::Command::new("bash")
        .args(["-c", &format!(
            "grep -l '\"pid\": {}' {}/*.json 2>/dev/null | head -1",
            pid, state_dir
        )])
        .output()
        .await?;

    let state_file = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if state_file.is_empty() {
        // Try alternative: list files and check each
        let output = tokio::process::Command::new("bash")
            .args(["-c", &format!(
                "for f in {}/*.json; do if grep -q '\"pid\": {}' \"$f\" 2>/dev/null; then echo \"$f\"; break; fi; done",
                state_dir, pid
            )])
            .output()
            .await?;
        let state_file = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if state_file.is_empty() {
            anyhow::bail!("Could not find state file for PID {}", pid);
        }
    }

    // Read the state file
    let content = tokio::fs::read_to_string(&state_file).await
        .context("reading state file")?;

    // Parse JSON and extract host_ip
    let state: serde_json::Value = serde_json::from_str(&content)
        .context("parsing state JSON")?;

    let host_ip = state["config"]["network"]["host_ip"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("host_ip not found in state"))?;

    Ok(host_ip.to_string())
}

fn spawn_log_consumer(stdout: Option<tokio::process::ChildStdout>, name: &str) {
    if let Some(stdout) = stdout {
        let name = name.to_string();
        tokio::spawn(async move {
            let reader = BufReader::new(stdout);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                println!("[{}] {}", name, line);
            }
        });
    }
}

fn spawn_log_consumer_stderr(stderr: Option<tokio::process::ChildStderr>, name: &str) {
    if let Some(stderr) = stderr {
        let name = name.to_string();
        tokio::spawn(async move {
            let reader = BufReader::new(stderr);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                eprintln!("[{} ERR] {}", name, line);
            }
        });
    }
}
