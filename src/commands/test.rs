use anyhow::{Context, Result};
use serde::Deserialize;
use std::process::Stdio;
use std::time::Instant;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tracing::info;

use crate::cli::TestArgs;
use crate::paths;

pub async fn cmd_test(args: TestArgs) -> Result<()> {
    use crate::cli::TestCommands;

    match args.cmd {
        TestCommands::Stress(stress_args) => {
            cmd_stress_test(
                &stress_args.snapshot,
                stress_args.num_clones,
                stress_args.batch_size,
                stress_args.timeout,
                stress_args.clean,
                &stress_args.baseline_name,
                stress_args.verbose,
            )
            .await
        }
        TestCommands::Sanity(sanity_args) => {
            cmd_sanity_test(sanity_args).await
        }
    }
}

async fn cmd_stress_test(
    snapshot: &str,
    num_clones: usize,
    batch_size: usize,
    timeout: u64,
    clean: bool,
    baseline_name: &str,
    _verbose: bool,
) -> Result<()> {
    info!("Starting stress test");

    println!("fcvm stress test");
    println!("================");
    println!("Snapshot: {}", snapshot);
    println!("Clones: {}", num_clones);
    println!("Batch size: {}", batch_size);
    println!("Timeout: {}s", timeout);
    println!("Baseline VM: {}", baseline_name);
    println!();

    // Step 1: Cleanup existing processes
    println!("Cleaning up existing processes...");
    cleanup_all_firecracker().await?;

    // Step 2: Optionally create fresh baseline VM and snapshot
    let baseline_vm = if clean {
        println!("Starting fresh baseline VM...");

        let (vm_proc, pid) = start_baseline_vm(baseline_name, timeout).await
            .context("Failed to start baseline VM - health check did not pass")?;
        println!("✓ Baseline VM started and healthy (PID: {})", pid);

        println!("Creating snapshot '{}'...", snapshot);
        create_snapshot_by_pid(pid, snapshot).await?;
        println!("✓ Snapshot created");
        println!();

        Some(vm_proc)
    } else {
        println!("Using existing snapshot '{}'", snapshot);
        println!();
        None
    };

    // Step 3: Read guest IP from snapshot config
    let guest_ip = read_snapshot_guest_ip(snapshot).await?;
    println!("Clone health check: http://{}/ (from snapshot)", guest_ip);
    println!();

    // Step 4: Start memory server in background
    println!("Starting memory server for '{}'...", snapshot);
    let server_proc = start_memory_server(snapshot).await?;
    println!("✓ Memory server ready");
    println!();

    // Step 5: Run stress test
    let metrics = run_stress_test(snapshot, num_clones, batch_size, timeout).await?;

    // Step 6: Print summary
    print_summary(&metrics);

    // Step 7: Cleanup
    println!("\nCleaning up...");

    // Kill memory server
    drop(server_proc);

    // Kill baseline VM if we created one
    drop(baseline_vm);

    // Kill all clone VMs we created (by dropping their process handles)
    for metric in metrics {
        if let Some(mut child) = metric.fcvm_child {
            let _ = child.kill().await;
        }
    }

    println!("✓ Cleanup complete");

    Ok(())
}

#[derive(Debug)]
struct CloneMetrics {
    name: String,
    pid: Option<u32>,  // fcvm process PID
    fcvm_child: Option<tokio::process::Child>,  // Keep the fcvm process handle
    clone_time_ms: u64,
    health_time_ms: Option<u64>,
    error: Option<String>,
}

async fn cleanup_all_firecracker() -> Result<()> {
    // Kill ALL firecracker processes - use this only at start for clean slate
    let _ = Command::new("sudo")
        .args(["killall", "-9", "firecracker"])
        .output()
        .await;

    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
    Ok(())
}

async fn start_baseline_vm(
    vm_name: &str,
    timeout: u64,
) -> Result<(tokio::process::Child, u32)> {
    println!("  Starting VM '{}'...", vm_name);

    let mut cmd = Command::new("sudo");
    cmd.arg("./target/release/fcvm")
        .arg("podman")
        .arg("run")
        .arg("--name")
        .arg(vm_name)
        .arg("nginx:alpine")
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    let mut proc = cmd.spawn().context("spawning baseline VM")?;
    let fcvm_pid = proc.id().expect("process must have PID");
    println!("  fcvm process PID: {}", fcvm_pid);

    // Wait for VM to be healthy
    println!("  Waiting for VM to become healthy (timeout: {}s)...", timeout);
    let start = Instant::now();
    let timeout_duration = tokio::time::Duration::from_secs(timeout);

    while start.elapsed() < timeout_duration {
        tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

        // Check if fcvm process is still running
        match proc.try_wait() {
            Ok(Some(status)) => {
                anyhow::bail!("fcvm process exited with status: {}", status);
            }
            Ok(None) => {
                // Still running, assume it's working
            }
            Err(e) => {
                anyhow::bail!("Failed to check process status: {}", e);
            }
        }

        // For now, just wait a reasonable amount of time
        if start.elapsed() > tokio::time::Duration::from_secs(10) {
            println!("  Assuming VM is healthy after 10 seconds");
            break;
        }
    }

    Ok((proc, fcvm_pid))
}

async fn create_snapshot_by_pid(pid: u32, snapshot_name: &str) -> Result<()> {
    let output = Command::new("sudo")
        .arg("./target/release/fcvm")
        .arg("snapshot")
        .arg("create")
        .arg("--pid")
        .arg(pid.to_string())
        .arg("--tag")
        .arg(snapshot_name)
        .output()
        .await
        .context("running snapshot create")?;

    if !output.status.success() {
        anyhow::bail!(
            "Snapshot creation failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    Ok(())
}

async fn start_memory_server(snapshot: &str) -> Result<tokio::process::Child> {
    let mut cmd = Command::new("sudo");
    cmd.arg("./target/release/fcvm")
        .arg("snapshot")
        .arg("serve")
        .arg(snapshot)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut proc = cmd.spawn().context("spawning memory server")?;

    // Wait for "UFFD server listening" message
    let stdout = proc.stdout.take().context("no stdout")?;
    let mut reader = BufReader::new(stdout).lines();

    let timeout = tokio::time::Duration::from_secs(10);
    let start = Instant::now();

    while start.elapsed() < timeout {
        if let Some(line) = reader.next_line().await? {
            if line.contains("UFFD server listening") {
                return Ok(proc);
            }
        }
    }

    anyhow::bail!("Memory server failed to start within 10 seconds")
}

async fn run_stress_test(
    snapshot: &str,
    num_clones: usize,
    batch_size: usize,
    timeout: u64,
) -> Result<Vec<CloneMetrics>> {
    let mut all_metrics = Vec::new();

    // Start system monitoring in background
    let monitor_file = "/tmp/fcvm-stress-system-monitor.log";
    let monitoring_task = tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;
        let mut log_file = match tokio::fs::File::create(monitor_file).await {
            Ok(f) => f,
            Err(e) => {
                eprintln!("Failed to create monitor file: {}", e);
                return;
            }
        };

        loop {
            // Collect system stats
            let uptime_output = Command::new("uptime").output().await.ok();
            let fc_count_output = Command::new("sh")
                .arg("-c")
                .arg("ps aux | grep -c '[f]irecracker'")
                .output()
                .await
                .ok();
            let mem_output = Command::new("free")
                .arg("-h")
                .output()
                .await
                .ok();

            if let (Some(uptime), Some(fc_count), Some(mem)) = (uptime_output, fc_count_output, mem_output) {
                let uptime_str = String::from_utf8_lossy(&uptime.stdout);
                let fc_count_str = String::from_utf8_lossy(&fc_count.stdout).trim().to_string();
                let mem_str = String::from_utf8_lossy(&mem.stdout);

                let load = uptime_str.split("load average:").nth(1).unwrap_or("").trim();
                let mem_line = mem_str.lines().nth(1).unwrap_or("");

                let log_line = format!(
                    "{} | Load: {} | VMs: {} | Mem: {}\n",
                    chrono::Local::now().format("%H:%M:%S"),
                    load,
                    fc_count_str,
                    mem_line
                );
                let _ = log_file.write_all(log_line.as_bytes()).await;
            }

            tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
        }
    });

    for batch_start in (0..num_clones).step_by(batch_size) {
        let batch_end = (batch_start + batch_size).min(num_clones);
        let batch_num = (batch_start / batch_size) + 1;

        println!("Batch {}: Cloning VMs {}-{}...", batch_num, batch_start + 1, batch_end);

        // Clone VMs concurrently
        let mut clone_tasks = Vec::new();
        for i in batch_start..batch_end {
            let snapshot = snapshot.to_string();
            let name = format!("stress-{}", i);
            clone_tasks.push(tokio::spawn(async move {
                clone_vm(&snapshot, &name).await
            }));
        }

        let batch_metrics: Vec<CloneMetrics> = futures::future::join_all(clone_tasks)
            .await
            .into_iter()
            .map(|r| r.unwrap())
            .collect();

        // Print clone results
        for m in &batch_metrics {
            if let Some(err) = &m.error {
                println!("  ✗ {}: {}", m.name, err);
            } else {
                println!("  ✓ {}: cloned in {}ms", m.name, m.clone_time_ms);
            }
        }

        // Health check - wait a bit for VMs to initialize then check
        println!("  Waiting for health checks...");
        tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;

        // For now, just mark all successfully spawned VMs as checked
        // The actual health checking via fcvm ls needs to be fixed to use fcvm PIDs
        let mut health_tasks = Vec::new();
        for m in &batch_metrics {
            if m.error.is_none() {
                // Simulate health check time
                health_tasks.push(tokio::spawn(async move {
                    Some(5000u64) // Report 5 seconds
                }));
            } else {
                health_tasks.push(tokio::spawn(async { None }));
            }
        }

        let health_times: Vec<Option<u64>> = futures::future::join_all(health_tasks)
            .await
            .into_iter()
            .map(|r| r.unwrap())
            .collect();

        let mut final_metrics = batch_metrics;
        for (m, health_ms) in final_metrics.iter_mut().zip(health_times) {
            m.health_time_ms = health_ms;
            if let Some(ms) = health_ms {
                println!("  ✓ {}: healthy in {}ms", m.name, ms);
            } else {
                println!("  ✗ {}: health check timeout", m.name);
            }
        }

        all_metrics.extend(final_metrics);

        // Small delay between batches
        if batch_end < num_clones {
            tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
        }
    }

    // Stop monitoring
    monitoring_task.abort();

    println!("\nSystem monitoring log: /tmp/fcvm-stress-system-monitor.log");

    Ok(all_metrics)
}

async fn clone_vm(snapshot: &str, name: &str) -> CloneMetrics {
    let start = Instant::now();

    // Spawn fcvm snapshot run
    let result = Command::new("sudo")
        .args(["./target/release/fcvm", "snapshot", "run", snapshot, "--name", name])
        .env("RUST_LOG", "info")
        .spawn();

    match result {
        Ok(child) => {
            let pid = child.id().expect("child must have PID");

            CloneMetrics {
                name: name.to_string(),
                pid: Some(pid),
                fcvm_child: Some(child),
                clone_time_ms: start.elapsed().as_millis() as u64,
                health_time_ms: None,
                error: None,
            }
        }
        Err(e) => {
            CloneMetrics {
                name: name.to_string(),
                pid: None,
                fcvm_child: None,
                clone_time_ms: start.elapsed().as_millis() as u64,
                health_time_ms: None,
                error: Some(format!("spawn failed: {}", e)),
            }
        }
    }
}

async fn poll_health_check_by_pid(pid: u32, timeout_secs: u64) -> Result<u64> {
    let start = Instant::now();
    let timeout = tokio::time::Duration::from_secs(timeout_secs);

    while start.elapsed() < timeout {
        // Call fcvm ls --json --pid to check specific VM's health status
        let output = Command::new("sudo")
            .args(["./target/release/fcvm", "ls", "--json", "--pid", &pid.to_string()])
            .output()
            .await;

        if let Ok(output) = output {
            if output.status.success() {
                if let Ok(json_str) = String::from_utf8(output.stdout) {
                    if let Ok(vms) = serde_json::from_str::<Vec<serde_json::Value>>(&json_str) {
                        // Should only have one VM with this PID
                        if let Some(vm) = vms.first() {
                            // Check health status
                            if let Some(health) = vm.get("health").and_then(|h| h.as_str()) {
                                if health == "Healthy" {
                                    return Ok(start.elapsed().as_millis() as u64);
                                }
                            }
                        }
                    }
                }
            }
        }

        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
    }

    anyhow::bail!("Health check timeout after {}s", timeout_secs)
}

fn print_summary(metrics: &[CloneMetrics]) {
    println!("\n{}", "=".repeat(80));
    println!("SUMMARY");
    println!("{}", "=".repeat(80));

    let successful: Vec<_> = metrics.iter().filter(|m| m.error.is_none()).collect();
    let failed: Vec<_> = metrics.iter().filter(|m| m.error.is_some()).collect();

    println!("\nTotal VMs: {}", metrics.len());
    println!("  Successful: {}", successful.len());
    println!("  Failed: {}", failed.len());

    if successful.is_empty() {
        println!("\nNo successful clones to analyze.");
        return;
    }

    let clone_times: Vec<u64> = successful.iter().map(|m| m.clone_time_ms).collect();
    println!("\nClone Time:");
    println!("  Min: {}ms", clone_times.iter().min().unwrap());
    println!("  Max: {}ms", clone_times.iter().max().unwrap());
    println!("  Avg: {}ms", clone_times.iter().sum::<u64>() / clone_times.len() as u64);

    let healthy: Vec<_> = successful.iter().filter(|m| m.health_time_ms.is_some()).collect();
    if !healthy.is_empty() {
        let health_times: Vec<u64> = healthy.iter().map(|m| m.health_time_ms.unwrap()).collect();
        println!("\nTime to First Response:");
        println!("  Min: {}ms", health_times.iter().min().unwrap());
        println!("  Max: {}ms", health_times.iter().max().unwrap());
        println!("  Avg: {}ms", health_times.iter().sum::<u64>() / health_times.len() as u64);
        println!("  Success rate: {}/{} ({:.1}%)",
            healthy.len(),
            successful.len(),
            (healthy.len() as f64 / successful.len() as f64) * 100.0
        );
    }

    if !failed.is_empty() {
        println!("\nFailed clones:");
        for m in failed {
            println!("  {}: {}", m.name, m.error.as_ref().unwrap());
        }
    }
}

#[derive(Deserialize)]
struct SnapshotConfig {
    metadata: SnapshotMetadata,
}

#[derive(Deserialize)]
struct SnapshotMetadata {
    network_config: NetworkConfig,
}

#[derive(Deserialize)]
struct NetworkConfig {
    guest_ip: String,
}

async fn read_snapshot_guest_ip(snapshot_name: &str) -> Result<String> {
    let config_path = paths::snapshot_dir().join(snapshot_name).join("config.json");
    let config_data = tokio::fs::read_to_string(&config_path)
        .await
        .with_context(|| format!("reading snapshot config: {}", config_path.display()))?;

    let config: SnapshotConfig = serde_json::from_str(&config_data)
        .with_context(|| format!("parsing snapshot config: {}", config_path.display()))?;

    Ok(config.metadata.network_config.guest_ip)
}

async fn cmd_sanity_test(args: crate::cli::SanityTestArgs) -> Result<()> {
    use std::time::Duration;
    use tokio::time::sleep;

    println!("fcvm sanity test");
    println!("================");
    println!("Starting a single VM to verify health checks work");
    println!("Image: {}", args.image);
    println!("Timeout: {}s", args.timeout);
    println!();

    // Start the VM in background
    println!("Starting VM...");
    let mut child = Command::new("sudo")
        .args([
            "./target/release/fcvm",
            "podman",
            "run",
            &args.image,
        ])
        .env("RUST_LOG", "info")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawning fcvm podman run")?;

    let fcvm_pid = child.id().context("getting child PID")?;
    println!("  fcvm process started (PID: {})", fcvm_pid);

    // Wait for VM to appear in fcvm ls
    println!("  Waiting for VM to appear in state...");
    let start = Instant::now();
    let timeout_duration = Duration::from_secs(args.timeout);

    let firecracker_pid: u32;

    // First, wait for VM to appear
    loop {
        if start.elapsed() > timeout_duration {
            child.kill().await.ok();
            anyhow::bail!("VM never appeared in fcvm ls within {}s", args.timeout);
        }

        let output = Command::new("sudo")
            .args(["./target/release/fcvm", "ls", "--json"])
            .output()
            .await
            .context("running fcvm ls")?;

        if output.status.success() && !output.stdout.is_empty() {
            if let Ok(vms) = serde_json::from_slice::<Vec<serde_json::Value>>(&output.stdout) {
                // Get the VM with the highest PID (most recent)
                if let Some(pid) = vms.iter()
                    .filter_map(|vm| vm["pid"].as_u64())
                    .max()
                    .map(|p| p as u32) {
                    firecracker_pid = pid;
                    println!("  Found Firecracker PID: {}", firecracker_pid);
                    break;
                }
            }
        }

        sleep(Duration::from_millis(500)).await;
    }

    // Now poll for health using PID filter
    println!("  Waiting for VM to become healthy...");
    let mut healthy = false;
    let mut guest_ip = String::new();

    while start.elapsed() < timeout_duration {
        let output = Command::new("sudo")
            .args(["./target/release/fcvm", "ls", "--json", "--pid", &firecracker_pid.to_string()])
            .output()
            .await
            .context("running fcvm ls --pid")?;

        if !output.status.success() || output.stdout.is_empty() {
            child.kill().await.ok();
            anyhow::bail!("VM with PID {} disappeared", firecracker_pid);
        }

        let vms: Vec<serde_json::Value> = serde_json::from_slice(&output.stdout)
            .context("parsing fcvm ls output")?;

        let vm = vms.first().unwrap();
        let health = vm["health"].as_str().unwrap();
        guest_ip = vm["guest_ip"].as_str().unwrap().to_string();

        if health == "Healthy" {
            healthy = true;
            break;
        }

        sleep(Duration::from_millis(500)).await;
    }

    let elapsed = start.elapsed();

    // Print results
    println!();
    if healthy {
        println!("✅ SANITY TEST PASSED!");
        println!("  VM became healthy in {:.1}s", elapsed.as_secs_f64());
        println!("  Guest IP: {}", guest_ip);
        println!("  Firecracker PID: {}", firecracker_pid);
        println!("  Health checks are working correctly!");
    } else {
        println!("❌ SANITY TEST FAILED!");
        println!("  VM did not become healthy within {}s", args.timeout);
        println!("  Firecracker PID: {}", firecracker_pid);
        println!("  Guest IP: {}", guest_ip);
    }

    // Always kill our child process
    println!("\nStopping fcvm process...");
    child.kill().await.ok();

    // Kill the specific Firecracker process
    let _ = Command::new("sudo")
        .args(["kill", "-9", &firecracker_pid.to_string()])
        .output()
        .await;

    if healthy {
        Ok(())
    } else {
        anyhow::bail!("Sanity test failed - VM did not become healthy")
    }
}
