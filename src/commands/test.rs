use anyhow::{Context, Result};
use serde::Deserialize;
use std::process::Stdio;
use std::time::Instant;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tracing::info;

use crate::cli::TestArgs;
use crate::paths;
use crate::state::StateManager;

pub async fn cmd_test(args: TestArgs) -> Result<()> {
    use crate::cli::TestCommands;

    match args.cmd {
        TestCommands::Stress(stress_args) => {
            cmd_stress_test(
                &stress_args.snapshot,
                stress_args.num_clones,
                stress_args.batch_size,
                &stress_args.health_check_path,
                stress_args.timeout,
                stress_args.clean,
                &stress_args.baseline_name,
                stress_args.verbose,
            )
            .await
        }
    }
}

async fn cmd_stress_test(
    snapshot: &str,
    num_clones: usize,
    batch_size: usize,
    health_check_path: &str,
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
    println!("Health check path: {}", health_check_path);
    println!("Timeout: {}s", timeout);
    println!("Baseline VM: {}", baseline_name);
    println!();

    // Step 1: Cleanup existing processes
    println!("Cleaning up existing processes...");
    cleanup_all().await?;

    // Step 2: Optionally create fresh baseline VM and snapshot
    let baseline_vm = if clean {
        println!("Starting fresh baseline VM...");

        let (vm_proc, baseline_ip) = start_baseline_vm(baseline_name, health_check_path, timeout).await
            .context("Failed to start baseline VM - health check did not pass")?;
        println!("✓ Baseline VM started and healthy at {}", baseline_ip);

        println!("Creating snapshot '{}'...", snapshot);
        create_snapshot(baseline_name, snapshot).await?;
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
    let health_url = format!("http://{}{}", guest_ip, health_check_path);
    println!("Clone health check URL: {} (from snapshot)", health_url);
    println!();

    // Step 4: Start memory server in background
    println!("Starting memory server for '{}'...", snapshot);
    let server_proc = start_memory_server(snapshot).await?;
    println!("✓ Memory server ready");
    println!();

    // Step 5: Run stress test
    let metrics = run_stress_test(snapshot, num_clones, batch_size, &health_url, timeout).await?;

    // Step 6: Print summary
    print_summary(&metrics);

    // Step 7: Cleanup
    println!("\nCleaning up...");
    drop(server_proc); // Kill server
    drop(baseline_vm); // Kill baseline VM
    cleanup_all().await?;
    println!("✓ Cleanup complete");

    Ok(())
}

#[derive(Debug)]
struct CloneMetrics {
    name: String,
    clone_time_ms: u64,
    health_time_ms: Option<u64>,
    tap_device: Option<String>,
    error: Option<String>,
}

async fn cleanup_all() -> Result<()> {
    // Only kill firecracker (not fcvm to avoid killing ourselves!)
    let _ = Command::new("sudo")
        .args(&["killall", "-9", "firecracker"])
        .output()
        .await;

    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
    Ok(())
}

async fn start_baseline_vm(
    vm_name: &str,
    health_check_path: &str,
    timeout: u64,
) -> Result<(tokio::process::Child, String)> {
    println!("  Starting VM '{}'...", vm_name);

    let mut cmd = Command::new("sudo");
    cmd.arg("./target/release/fcvm")
        .arg("podman")
        .arg("run")
        .arg("--name")
        .arg(vm_name)
        .arg("--mode")
        .arg("rootless")
        .arg("nginx:alpine")
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    let proc = cmd.spawn().context("spawning baseline VM")?;

    // Wait for VM state file to be created
    tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;

    // Read actual guest IP from VM state
    let state_manager = StateManager::new(paths::state_dir());
    let vm_state = state_manager.load_state_by_name(vm_name).await?;

    // Extract guest IP from network config
    let guest_ip = extract_guest_ip_from_network_config(&vm_state.config.network)?;
    println!("  VM assigned IP: {}", guest_ip);

    // Poll health check using fcvm ls
    println!("  Waiting for VM to become healthy (timeout: {}s)...", timeout);
    poll_health_check(vm_name, timeout).await?;

    Ok((proc, guest_ip))
}

fn extract_guest_ip_from_network_config(network_config: &serde_json::Value) -> Result<String> {
    network_config
        .get("guest_ip")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow::anyhow!("guest_ip not found in network config"))
}

async fn create_snapshot(vm_name: &str, snapshot_name: &str) -> Result<()> {
    let output = Command::new("./target/release/fcvm")
        .arg("snapshot")
        .arg("create")
        .arg(vm_name)
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
    health_check_url: &str,
    timeout: u64,
) -> Result<Vec<CloneMetrics>> {
    let mut all_metrics = Vec::new();

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
                println!("  ✓ {}: cloned in {}ms (TAP: {})",
                    m.name, m.clone_time_ms, m.tap_device.as_ref().unwrap_or(&"unknown".to_string()));
            }
        }

        // Health check
        println!("  Waiting for health checks...");
        let mut health_tasks = Vec::new();
        for m in &batch_metrics {
            if m.tap_device.is_some() {
                let vm_name = m.name.clone();
                health_tasks.push(tokio::spawn(async move {
                    poll_health_check(&vm_name, timeout).await.ok()
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

    Ok(all_metrics)
}

async fn clone_vm(snapshot: &str, name: &str) -> CloneMetrics {
    let start = Instant::now();

    let mut cmd = Command::new("sudo");
    cmd.arg("./target/release/fcvm")
        .arg("snapshot")
        .arg("run")
        .arg(snapshot)
        .arg("--name")
        .arg(name)
        .arg("--mode")
        .arg("rootless")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut proc = match cmd.spawn() {
        Ok(p) => p,
        Err(e) => {
            return CloneMetrics {
                name: name.to_string(),
                clone_time_ms: start.elapsed().as_millis() as u64,
                health_time_ms: None,
                tap_device: None,
                error: Some(format!("spawn failed: {}", e)),
            };
        }
    };

    let stdout = proc.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout).lines();

    let mut clone_time_ms = None;
    let mut tap_device = None;

    let timeout = tokio::time::Duration::from_secs(5);
    let deadline = Instant::now() + timeout;

    while Instant::now() < deadline {
        match tokio::time::timeout(tokio::time::Duration::from_millis(100), reader.next_line()).await {
            Ok(Ok(Some(line))) => {
                if line.contains("VM cloned successfully") && clone_time_ms.is_none() {
                    clone_time_ms = Some(start.elapsed().as_millis() as u64);
                }
                // Extract TAP device name: tap-vm-XXXXX (alphanumeric + hyphen only)
                if let Some(idx) = line.find("tap-vm-") {
                    let rest = &line[idx..];
                    let end = rest.find(|c: char| !c.is_alphanumeric() && c != '-')
                        .unwrap_or(rest.len());
                    tap_device = Some(rest[..end].to_string());
                }
                if clone_time_ms.is_some() && tap_device.is_some() {
                    break;
                }
            }
            Ok(Ok(None)) => break,
            Ok(Err(e)) => {
                return CloneMetrics {
                    name: name.to_string(),
                    clone_time_ms: start.elapsed().as_millis() as u64,
                    health_time_ms: None,
                    tap_device,
                    error: Some(format!("read error: {}", e)),
                };
            }
            Err(_) => continue, // timeout, keep trying
        }
    }

    if clone_time_ms.is_none() {
        let _ = proc.kill().await;
        return CloneMetrics {
            name: name.to_string(),
            clone_time_ms: start.elapsed().as_millis() as u64,
            health_time_ms: None,
            tap_device,
            error: Some("timeout waiting for VM to clone".to_string()),
        };
    }

    // Leave VM running for health check
    CloneMetrics {
        name: name.to_string(),
        clone_time_ms: clone_time_ms.unwrap(),
        health_time_ms: None,
        tap_device,
        error: None,
    }
}

async fn poll_health_check(vm_name: &str, timeout_secs: u64) -> Result<u64> {
    let start = Instant::now();
    let timeout = tokio::time::Duration::from_secs(timeout_secs);

    while start.elapsed() < timeout {
        // Use fcvm ls --json to check VM health status
        let mut cmd = Command::new("sudo");
        cmd.args(&["./target/release/fcvm", "ls", "--json"]);

        if let Ok(output) = cmd.output().await {
            if output.status.success() {
                if let Ok(stdout) = String::from_utf8(output.stdout) {
                    if let Ok(vms) = serde_json::from_str::<Vec<serde_json::Value>>(&stdout) {
                        // Find the specific VM by name and check if it's healthy
                        for vm in vms {
                            if let Some(name) = vm.get("name").and_then(|n| n.as_str()) {
                                if name == vm_name {
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
