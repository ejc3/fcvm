use anyhow::{Context, Result};
use std::process::Stdio;
use std::time::Instant;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tracing::info;

use crate::cli::TestArgs;

pub async fn cmd_test(args: TestArgs) -> Result<()> {
    use crate::cli::TestCommands;

    match args.cmd {
        TestCommands::Stress(stress_args) => {
            cmd_stress_test(
                &stress_args.snapshot,
                stress_args.num_clones,
                stress_args.batch_size,
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
    verbose: bool,
) -> Result<()> {
    info!("Starting stress test");

    println!("fcvm stress test");
    println!("================");
    println!("Snapshot: {}", snapshot);
    println!("Clones: {}", num_clones);
    println!("Batch size: {}", batch_size);
    println!();

    // Step 1: Cleanup any existing VMs/servers
    println!("Cleaning up existing processes...");
    cleanup_all().await?;

    // Step 2: Start memory server in background
    println!("Starting memory server for '{}'...", snapshot);
    let server_proc = start_memory_server(snapshot, verbose).await?;
    println!("✓ Memory server ready");
    println!();

    // Step 3: Run stress test
    let metrics = run_stress_test(snapshot, num_clones, batch_size, verbose).await?;

    // Step 4: Print summary
    print_summary(&metrics);

    // Step 5: Cleanup
    println!("\nCleaning up...");
    drop(server_proc); // Kill server
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
    let _ = Command::new("sudo")
        .args(&["killall", "-9", "firecracker", "fcvm"])
        .output()
        .await;

    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
    Ok(())
}

async fn start_memory_server(snapshot: &str, verbose: bool) -> Result<tokio::process::Child> {
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
            if verbose {
                println!("  [server] {}", line);
            }
            if line.contains("UFFD server listening") {
                // Put stdout back (won't use it anymore but keep proc alive)
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
    verbose: bool,
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
        println!("  Waiting for nginx health checks...");
        let mut health_tasks = Vec::new();
        for m in &batch_metrics {
            if let Some(tap) = &m.tap_device {
                let tap = tap.clone();
                health_tasks.push(tokio::spawn(async move {
                    wait_for_nginx(&tap).await
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
                println!("  ✓ {}: nginx healthy in {}ms", m.name, ms);
            } else {
                println!("  ✗ {}: nginx health check timeout", m.name);
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
                if let Some(idx) = line.find("tap-") {
                    if let Some(end) = line[idx..].find(|c: char| c.is_whitespace() || c == ')') {
                        tap_device = Some(line[idx..idx+end].to_string());
                    }
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

async fn wait_for_nginx(tap_device: &str) -> Option<u64> {
    let start = Instant::now();
    let max_attempts = 30;

    for _ in 0..max_attempts {
        let output = Command::new("curl")
            .args(&[
                "-s",
                "-m", "1",
                "--interface", tap_device,
                "http://172.16.0.202",  // Guest IP
            ])
            .output()
            .await;

        if let Ok(output) = output {
            if output.status.success() {
                let body = String::from_utf8_lossy(&output.stdout);
                if body.to_lowercase().contains("nginx") {
                    return Some(start.elapsed().as_millis() as u64);
                }
            }
        }

        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
    }

    None
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
