use anyhow::{Context, Result};
use serde::Deserialize;
use std::process::Stdio;
use std::time::Instant;
use tokio::process::Command;
use tracing::info;

use crate::cli::TestArgs;
use crate::paths;

/// Helper to create fcvm subprocess command with --sub-process flag
/// All test harness commands that spawn fcvm should use this helper
/// to ensure subprocess logging is consistent (no duplicate timestamps)
fn fcvm_subprocess() -> Command {
    let current_exe = std::env::current_exe().expect("failed to get current executable path");
    let mut cmd = Command::new(current_exe);
    cmd.arg("--sub-process"); // Disable timestamps/level in subprocess
    cmd
}

/// Spawn tasks to read stdout/stderr and prefix each line with [vm-name]
/// Format: [vm-name] [target] message
/// where target comes from the subprocess output (e.g., "vm:", "firecracker:", "health-monitor:")
fn spawn_log_prefix_tasks(
    stdout: Option<tokio::process::ChildStdout>,
    stderr: Option<tokio::process::ChildStderr>,
    vm_name: String,
) {
    use tokio::io::{AsyncBufReadExt, BufReader};
    use tracing::info;

    if let Some(stdout) = stdout {
        let vm_name = vm_name.clone();
        tokio::spawn(async move {
            let reader = BufReader::new(stdout);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                // Line format from subprocess: "target: message" or just "message"
                // We want: "[vm-name] [target] message" or "[vm-name] message"
                if let Some((target, message)) = line.split_once(": ") {
                    info!(target: "test", "[{}] [{}] {}", vm_name, target, message);
                } else {
                    info!(target: "test", "[{}] {}", vm_name, line);
                }
            }
        });
    }

    if let Some(stderr) = stderr {
        tokio::spawn(async move {
            let reader = BufReader::new(stderr);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                // stderr typically for errors
                if let Some((target, message)) = line.split_once(": ") {
                    info!(target: "test", "[{}] [{}] {}", vm_name, target, message);
                } else {
                    info!(target: "test", "[{}] {}", vm_name, line);
                }
            }
        });
    }
}

/// Spawn task to read stdout, prefix lines with [vm-name], and check for ready message
/// Returns a channel receiver that will send () when ready message is found
fn spawn_log_prefix_with_ready_check(
    stdout: Option<tokio::process::ChildStdout>,
    vm_name: String,
    ready_message: &str,
) -> tokio::sync::oneshot::Receiver<()> {
    use tokio::io::{AsyncBufReadExt, BufReader};
    use tracing::info;

    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let ready_msg = ready_message.to_string();

    if let Some(stdout) = stdout {
        tokio::spawn(async move {
            let reader = BufReader::new(stdout);
            let mut lines = reader.lines();
            let mut ready_tx = Some(ready_tx);

            while let Ok(Some(line)) = lines.next_line().await {
                // Check for ready message
                if let Some(tx) = ready_tx.take() {
                    if line.contains(&ready_msg) {
                        let _ = tx.send(());
                    } else {
                        // Restore tx if not sent yet
                        ready_tx = Some(tx);
                    }
                }

                // Prefix and print
                if let Some((target, message)) = line.split_once(": ") {
                    info!(target: "test", "[{}] [{}] {}", vm_name, target, message);
                } else {
                    info!(target: "test", "[{}] {}", vm_name, line);
                }
            }
        });
    }

    ready_rx
}

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
                stress_args.network,
            )
            .await
        }
        TestCommands::Sanity(sanity_args) => cmd_sanity_test(sanity_args).await,
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
    network: crate::cli::NetworkMode,
) -> Result<()> {
    info!("Starting stress test");

    let network_str = match network {
        crate::cli::NetworkMode::Bridged => "bridged",
        crate::cli::NetworkMode::Rootless => "rootless",
    };

    println!("fcvm stress test");
    println!("================");
    println!("Snapshot: {}", snapshot);
    println!("Clones: {}", num_clones);
    println!("Batch size: {}", batch_size);
    println!("Timeout: {}s", timeout);
    println!("Baseline VM: {}", baseline_name);
    println!("Network mode: {}", network_str);
    println!();

    // Step 1: Cleanup existing processes
    println!("Cleaning up existing processes...");
    cleanup_all_firecracker().await?;

    // Step 2: Optionally create fresh baseline VM and snapshot
    let baseline_vm = if clean {
        println!("Starting fresh baseline VM...");

        let (vm_proc, pid) = start_baseline_vm(baseline_name, timeout, network_str)
            .await
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
    let (server_proc, serve_pid) = start_memory_server(snapshot).await?;
    println!("✓ Memory server ready (PID: {})", serve_pid);
    println!();

    // Step 5: Run stress test
    let metrics = run_stress_test(serve_pid, num_clones, batch_size, timeout, network_str).await?;

    // Step 6: Print summary
    print_summary(&metrics);

    // Step 7: Check for failures
    let failed_count = metrics
        .iter()
        .filter(|m| m.error.is_some() || m.health_time_ms.is_none())
        .count();
    let success_count = metrics.len() - failed_count;

    // Step 8: Cleanup
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

    // Return error if any VMs failed
    if failed_count > 0 {
        anyhow::bail!(
            "Stress test failed: {}/{} VMs failed to become healthy",
            failed_count,
            failed_count + success_count
        );
    }

    Ok(())
}

#[derive(Debug)]
struct CloneMetrics {
    name: String,
    pid: Option<u32>,                          // fcvm process PID
    fcvm_child: Option<tokio::process::Child>, // Keep the fcvm process handle
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

    // Clean up all fcvm network namespaces
    // First, get list of all fcvm-* namespaces
    let output = Command::new("ip")
        .args(["netns", "list"])
        .output()
        .await?;

    if output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.lines() {
            let ns_name = line.split_whitespace().next().unwrap_or("");
            if ns_name.starts_with("fcvm-") {
                let _ = Command::new("sudo")
                    .args(["ip", "netns", "del", ns_name])
                    .output()
                    .await;
            }
        }
    }

    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
    Ok(())
}

async fn start_baseline_vm(vm_name: &str, timeout: u64, network: &str) -> Result<(tokio::process::Child, u32)> {
    println!("  Starting VM '{}'...", vm_name);

    // Note: Don't use sudo - stress test command itself is run with sudo
    let mut cmd = fcvm_subprocess();
    cmd.arg("podman")
        .arg("run")
        .arg("--name")
        .arg(vm_name)
        .arg("--network")
        .arg(network)
        .arg("nginx:alpine")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut proc = cmd.spawn().context("spawning baseline VM")?;
    let fcvm_pid = proc.id().expect("process must have PID");
    println!("  fcvm process PID: {}", fcvm_pid);

    // Spawn tasks to read and prefix output
    spawn_log_prefix_tasks(proc.stdout.take(), proc.stderr.take(), vm_name.to_string());

    // Wait for VM to be healthy
    println!(
        "  Waiting for VM to become healthy (timeout: {}s)...",
        timeout
    );
    let start = Instant::now();
    let timeout_duration = tokio::time::Duration::from_secs(timeout);

    let state_manager = crate::state::StateManager::new(crate::paths::state_dir());

    while start.elapsed() < timeout_duration {
        tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

        // Check if fcvm process is still running
        match proc.try_wait() {
            Ok(Some(status)) => {
                anyhow::bail!("fcvm process exited with status: {}", status);
            }
            Ok(None) => {
                // Still running, check actual health status
            }
            Err(e) => {
                anyhow::bail!("Failed to check process status: {}", e);
            }
        }

        // Check actual health status from state file
        if let Ok(states) = state_manager.list_vms().await {
            if let Some(state) = states.iter().find(|s| s.pid == Some(fcvm_pid)) {
                if state.health_status == crate::state::HealthStatus::Healthy {
                    let elapsed = start.elapsed().as_secs_f64();
                    println!("  ✓ VM became healthy in {:.1}s", elapsed);
                    break;
                }
            }
        }
    }

    // Final check - did we actually become healthy?
    if let Ok(states) = state_manager.list_vms().await {
        if let Some(state) = states.iter().find(|s| s.pid == Some(fcvm_pid)) {
            if state.health_status != crate::state::HealthStatus::Healthy {
                anyhow::bail!(
                    "VM did not become healthy within {}s timeout (status: {:?})",
                    timeout,
                    state.health_status
                );
            }
        }
    }

    Ok((proc, fcvm_pid))
}

async fn create_snapshot_by_pid(pid: u32, snapshot_name: &str) -> Result<()> {
    // Note: Don't use sudo - test command itself is run with sudo
    let output = fcvm_subprocess()
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

async fn start_memory_server(snapshot: &str) -> Result<(tokio::process::Child, u32)> {
    // Note: Don't use sudo - test command itself is run with sudo
    let mut cmd = fcvm_subprocess();
    cmd.arg("snapshot")
        .arg("serve")
        .arg(snapshot)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut proc = cmd.spawn().context("spawning memory server")?;
    let serve_pid = proc.id().context("getting serve process PID")?;

    // Spawn task to read stdout, prefix lines, and check for ready message
    let ready_rx = spawn_log_prefix_with_ready_check(
        proc.stdout.take(),
        format!("uffd-{}", snapshot),
        "UFFD server listening",
    );

    // Also handle stderr
    spawn_log_prefix_tasks(None, proc.stderr.take(), format!("uffd-{}", snapshot));

    // Wait for ready message with timeout
    let timeout = tokio::time::Duration::from_secs(10);
    match tokio::time::timeout(timeout, ready_rx).await {
        Ok(Ok(())) => Ok((proc, serve_pid)),
        Ok(Err(_)) => anyhow::bail!("Memory server stdout closed before ready message"),
        Err(_) => anyhow::bail!("Memory server failed to start within 10 seconds"),
    }
}

async fn run_stress_test(
    serve_pid: u32,
    num_clones: usize,
    batch_size: usize,
    timeout: u64,
    network: &str,
) -> Result<Vec<CloneMetrics>> {
    let mut all_metrics = Vec::new();

    // Start system monitoring in background
    let monitor_file = "/tmp/fcvm-stress-system-monitor.log";
    let uffd_pid = serve_pid;
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
            let mem_output = Command::new("free").arg("-h").output().await.ok();

            // Get UFFD process CPU usage using ps
            let uffd_cpu_output = Command::new("ps")
                .args(["-p", &uffd_pid.to_string(), "-o", "%cpu="])
                .output()
                .await
                .ok();

            if let (Some(uptime), Some(fc_count), Some(mem)) =
                (uptime_output, fc_count_output, mem_output)
            {
                let uptime_str = String::from_utf8_lossy(&uptime.stdout);
                let fc_count_str = String::from_utf8_lossy(&fc_count.stdout).trim().to_string();
                let mem_str = String::from_utf8_lossy(&mem.stdout);

                let uffd_cpu = uffd_cpu_output
                    .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                    .unwrap_or_else(|| "?".to_string());

                let load = uptime_str
                    .split("load average:")
                    .nth(1)
                    .unwrap_or("")
                    .trim();
                let mem_line = mem_str.lines().nth(1).unwrap_or("");

                let log_line = format!(
                    "{} | Load: {} | VMs: {} | UFFD CPU: {}% | Mem: {}\n",
                    chrono::Local::now().format("%H:%M:%S"),
                    load,
                    fc_count_str,
                    uffd_cpu,
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

        println!(
            "Batch {}: Cloning VMs {}-{}...",
            batch_num,
            batch_start + 1,
            batch_end
        );

        // Clone VMs concurrently
        let mut clone_tasks = Vec::new();
        for i in batch_start..batch_end {
            let name = format!("stress-{}", i);
            let network = network.to_string();
            clone_tasks.push(tokio::spawn(
                async move { clone_vm(serve_pid, &name, &network).await },
            ));
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

        // Health check - actually check each VM's health status
        println!("  Waiting for health checks...");

        let mut health_tasks = Vec::new();
        for m in &batch_metrics {
            if let (None, Some(pid)) = (&m.error, m.pid) {
                // VM started successfully, check its health
                health_tasks.push(tokio::spawn(async move {
                    poll_health_check_by_pid(pid, timeout).await.ok()
                }));
            } else {
                // VM failed to start, no health check needed
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
            } else if m.error.is_none() {
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

async fn clone_vm(serve_pid: u32, name: &str, network: &str) -> CloneMetrics {
    let start = Instant::now();

    // Spawn fcvm snapshot run using serve PID
    // Note: Don't use sudo - test command itself is run with sudo
    let result = fcvm_subprocess()
        .args([
            "snapshot",
            "run",
            "--pid",
            &serve_pid.to_string(),
            "--name",
            name,
            "--network",
            network,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();

    match result {
        Ok(mut child) => {
            let pid = child.id().expect("child must have PID");

            // Spawn tasks to read and prefix output
            spawn_log_prefix_tasks(child.stdout.take(), child.stderr.take(), name.to_string());

            CloneMetrics {
                name: name.to_string(),
                pid: Some(pid),
                fcvm_child: Some(child),
                clone_time_ms: start.elapsed().as_millis() as u64,
                health_time_ms: None,
                error: None,
            }
        }
        Err(e) => CloneMetrics {
            name: name.to_string(),
            pid: None,
            fcvm_child: None,
            clone_time_ms: start.elapsed().as_millis() as u64,
            health_time_ms: None,
            error: Some(format!("spawn failed: {}", e)),
        },
    }
}

async fn poll_health_check_by_pid(pid: u32, timeout_secs: u64) -> Result<u64> {
    let start = Instant::now();
    let timeout = tokio::time::Duration::from_secs(timeout_secs);

    while start.elapsed() < timeout {
        // Call fcvm ls --json --pid to check specific VM's health status
        // Note: Don't use sudo - test command itself is run with sudo
        let output = fcvm_subprocess()
            .args(["ls", "--json", "--pid", &pid.to_string()])
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
    println!(
        "  Avg: {}ms",
        clone_times.iter().sum::<u64>() / clone_times.len() as u64
    );

    let healthy: Vec<_> = successful
        .iter()
        .filter(|m| m.health_time_ms.is_some())
        .collect();
    if !healthy.is_empty() {
        let health_times: Vec<u64> = healthy.iter().map(|m| m.health_time_ms.unwrap()).collect();
        println!("\nTime to First Response:");
        println!("  Min: {}ms", health_times.iter().min().unwrap());
        println!("  Max: {}ms", health_times.iter().max().unwrap());
        println!(
            "  Avg: {}ms",
            health_times.iter().sum::<u64>() / health_times.len() as u64
        );
        println!(
            "  Success rate: {}/{} ({:.1}%)",
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
    network_config: TestNetworkConfig,
}

#[derive(Deserialize)]
struct TestNetworkConfig {
    guest_ip: Option<String>,
    loopback_ip: Option<String>,
}

async fn read_snapshot_guest_ip(snapshot_name: &str) -> Result<String> {
    let config_path = paths::snapshot_dir()
        .join(snapshot_name)
        .join("config.json");
    let config_data = tokio::fs::read_to_string(&config_path)
        .await
        .with_context(|| format!("reading snapshot config: {}", config_path.display()))?;

    let config: SnapshotConfig = serde_json::from_str(&config_data)
        .with_context(|| format!("parsing snapshot config: {}", config_path.display()))?;

    // For bridged mode, use guest_ip. For rootless mode, use loopback_ip.
    config
        .metadata
        .network_config
        .guest_ip
        .or(config.metadata.network_config.loopback_ip)
        .ok_or_else(|| anyhow::anyhow!("Snapshot has no guest_ip or loopback_ip configured"))
}

async fn cmd_sanity_test(args: crate::cli::SanityTestArgs) -> Result<()> {
    use std::time::Duration;

    let network_str = match args.network {
        crate::cli::NetworkMode::Bridged => "bridged",
        crate::cli::NetworkMode::Rootless => "rootless",
    };

    println!("fcvm sanity test");
    println!("================");
    println!("Starting a single VM to verify health checks work");
    println!("Image: {}", args.image);
    println!("Timeout: {}s", args.timeout);
    println!("Network mode: {}", network_str);
    println!();

    // Start the VM in background
    // Note: Don't use sudo here - the test command itself is run with sudo
    println!("Starting VM...");
    let mut child = fcvm_subprocess()
        .args(["podman", "run", "--name", "sanity-test-vm", "--network", network_str, &args.image])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawning fcvm podman run")?;

    let fcvm_pid = child.id().context("getting child PID")?;
    println!("  fcvm process started (PID: {})", fcvm_pid);

    // Stream subprocess stdout/stderr to tracing (like we do for Firecracker)
    // Use descriptive target that explains WHY fcvm was launched (sanity test baseline VM)
    if let Some(stdout) = child.stdout.take() {
        tokio::spawn(async move {
            use tokio::io::{AsyncBufReadExt, BufReader};
            let reader = BufReader::new(stdout);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                info!(target: "sanity-baseline-vm", "{}", line);
            }
        });
    }

    if let Some(stderr) = child.stderr.take() {
        tokio::spawn(async move {
            use tokio::io::{AsyncBufReadExt, BufReader};
            let reader = BufReader::new(stderr);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                info!(target: "sanity-baseline-vm", "{}", line);
            }
        });
    }

    // Wait for VM to appear in fcvm ls and become healthy
    println!("  Waiting for VM to appear in state and become healthy...");

    // Spawn task to poll health status
    let health_task = tokio::spawn(poll_health_check_by_pid(fcvm_pid, args.timeout));

    // Monitor child process for unexpected exits
    let monitor_task: tokio::task::JoinHandle<Result<()>> = tokio::spawn(async move {
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    return Err(anyhow::anyhow!(
                        "fcvm process exited unexpectedly with status: {}",
                        status
                    ));
                }
                Ok(None) => {
                    // Still running, continue
                }
                Err(e) => {
                    return Err(anyhow::anyhow!("Failed to check process status: {}", e));
                }
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    });

    // Wait for either health check to pass or process to exit
    let result = tokio::select! {
        health_result = health_task => {
            match health_result {
                Ok(Ok(ms)) => Ok(ms),
                Ok(Err(e)) => Err(e),
                Err(e) => Err(anyhow::anyhow!("Health check task panicked: {}", e)),
            }
        }
        monitor_result = monitor_task => {
            match monitor_result {
                Ok(Err(e)) => Err(e),
                Ok(Ok(_)) => unreachable!("Monitor task should never return Ok"),
                Err(e) => Err(anyhow::anyhow!("Monitor task panicked: {}", e)),
            }
        }
    };

    // Get guest IP from state for display
    let guest_ip = fcvm_subprocess()
        .args(["ls", "--json", "--pid", &fcvm_pid.to_string()])
        .output()
        .await
        .ok()
        .and_then(|output| serde_json::from_slice::<Vec<serde_json::Value>>(&output.stdout).ok())
        .and_then(|vms| vms.first().cloned())
        .and_then(|vm| vm["guest_ip"].as_str().map(String::from))
        .unwrap_or_else(|| "unknown".to_string());

    // Print results
    println!();
    match &result {
        Ok(ms) => {
            println!("✅ SANITY TEST PASSED!");
            println!("  VM became healthy in {:.1}s", *ms as f64 / 1000.0);
            println!("  Guest IP: {}", guest_ip);
            println!("  fcvm PID: {}", fcvm_pid);
            println!("  Health checks are working correctly!");
        }
        Err(e) => {
            println!("❌ SANITY TEST FAILED!");
            println!("  Error: {}", e);
            println!("  fcvm PID: {}", fcvm_pid);
            println!("  Guest IP: {}", guest_ip);
        }
    }

    // Always kill the fcvm process - need to get a handle to it
    // Since we moved `child` into the monitor task, we need to kill by PID
    println!("\nStopping fcvm process...");
    let _ = Command::new("kill")
        .arg("-9")
        .arg(fcvm_pid.to_string())
        .output()
        .await;

    result.map(|_| ())
}
