use anyhow::{Context, Result};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio::time::Duration;
use tracing::{debug, info, warn};

use crate::paths;
use crate::state::{HealthStatus, StateManager};

/// Spawn a background health monitoring task for a VM
///
/// The task polls the VM process health at adaptive intervals:
/// - 100ms during startup (until healthy)
/// - 10s after VM is healthy
///
/// Health check tests HTTP connectivity using reqwest to the guest IP.
///
/// Returns a JoinHandle that can be used to cancel the task.
/// The task runs until cancelled or until the tokio runtime shuts down.
pub fn spawn_health_monitor(vm_id: String, pid: Option<u32>) -> JoinHandle<()> {
    tokio::spawn(async move {
        let state_manager = StateManager::new(paths::state_dir());

        // Get VM name from state for logging
        let vm_name = if let Ok(state) = state_manager.load_state(&vm_id).await {
            state.name.clone().unwrap_or_else(|| vm_id[..8].to_string())
        } else {
            vm_id[..8].to_string() // Fallback to short vm_id
        };

        info!(target: "health-monitor", vm_name = %vm_name, vm_id = %vm_id, pid = ?pid, "starting health monitor");

        // Adaptive polling: 100ms during startup, 10s after healthy
        let mut poll_interval = Duration::from_millis(100);
        let mut is_healthy = false;

        // Throttle health check failure logs to once per second
        let last_failure_log: Arc<Mutex<Option<Instant>>> = Arc::new(Mutex::new(None));

        loop {
            tokio::time::sleep(poll_interval).await;

            let health_status = if let Some(pid) = pid {
                // First check if Firecracker process is still running
                if std::fs::metadata(format!("/proc/{}", pid)).is_err() {
                    debug!(target: "health-monitor", vm_name = %vm_name, vm_id = %vm_id, pid = pid, "process not found");
                    HealthStatus::Unreachable
                } else {
                    // Process exists, now check if application is responding
                    // Get guest IP, host veth device, and health check path from state
                    if let Ok(state) = state_manager.load_state(&vm_id).await {
                        let guest_ip = state
                            .config
                            .network
                            .get("guest_ip")
                            .and_then(|v| v.as_str());

                        // Get veth device from network config (required for namespace isolation)
                        let veth_device = state
                            .config
                            .network
                            .get("host_veth")
                            .and_then(|v| v.as_str());

                        debug!(target: "health-monitor", vm_name = %vm_name, vm_id = %vm_id, guest_ip = ?guest_ip, veth = ?veth_device, "network config for health check");

                        if let (Some(guest_ip), Some(veth)) = (guest_ip, veth_device) {
                            // Try HTTP request via veth device
                            let health_path = &state.config.health_check_path;
                            match check_http_health(guest_ip, veth, health_path).await {
                                Ok(true) => {
                                    debug!(target: "health-monitor", vm_name = %vm_name, vm_id = %vm_id, "health check passed");
                                    // Reset throttle on success
                                    *last_failure_log.lock().await = None;
                                    HealthStatus::Healthy
                                }
                                Ok(false) => {
                                    // This case is unreachable since check_http_health only returns Ok(true) or Err
                                    warn!(target: "health-monitor", vm_name = %vm_name, vm_id = %vm_id, "health check returned false (unexpected)");
                                    HealthStatus::Unhealthy
                                }
                                Err(e) => {
                                    // Throttle failure logs to once per second
                                    let mut last_log = last_failure_log.lock().await;
                                    let should_log = match *last_log {
                                        None => true,
                                        Some(last_time) => last_time.elapsed() >= Duration::from_secs(1),
                                    };

                                    if should_log {
                                        warn!(target: "health-monitor", vm_name = %vm_name, vm_id = %vm_id, error = %e, "health check failed");
                                        *last_log = Some(Instant::now());
                                    }
                                    HealthStatus::Unhealthy
                                }
                            }
                        } else {
                            // No network config yet, log what's missing
                            if guest_ip.is_none() {
                                warn!(target: "health-monitor", vm_name = %vm_name, vm_id = %vm_id, "cannot check health: no guest_ip in config");
                            }
                            if veth_device.is_none() {
                                warn!(target: "health-monitor", vm_name = %vm_name, vm_id = %vm_id, "cannot check health: no host_veth in config");
                            }
                            HealthStatus::Unknown
                        }
                    } else {
                        warn!(target: "health-monitor", vm_name = %vm_name, vm_id = %vm_id, "failed to load VM state for health check");
                        HealthStatus::Unknown
                    }
                }
            } else {
                HealthStatus::Unknown
            };

            // Update state file
            if let Ok(mut state) = state_manager.load_state(&vm_id).await {
                state.health_status = health_status;
                state.last_updated = chrono::Utc::now();
                match state_manager.save_state(&state).await {
                    Ok(_) => {
                        debug!(target: "health-monitor", vm_name = %vm_name, vm_id = %vm_id, health_status = ?health_status, "state saved")
                    }
                    Err(e) => {
                        warn!(target: "health-monitor", vm_name = %vm_name, vm_id = %vm_id, error = %e, "failed to save state")
                    }
                }
            } else {
                warn!(target: "health-monitor", vm_name = %vm_name, vm_id = %vm_id, "failed to load state for updating health");
            }

            // Switch to slower polling once healthy
            if health_status == HealthStatus::Healthy && !is_healthy {
                is_healthy = true;
                poll_interval = Duration::from_secs(10);
                info!(target: "health-monitor", vm_name = %vm_name, vm_id = %vm_id, "VM healthy, switching to 10s polling");
            }
        }
    })
}

/// Check if HTTP service is responding using curl with interface binding
///
/// IMPORTANT: For clones with the same guest_ip, we MUST bind to the specific
/// veth interface to reach the correct VM. Without interface binding, Linux routing
/// will always pick the first veth in the routing table, causing all health checks
/// to go to the same VM.
///
/// We use curl --interface to ensure each health check reaches its specific VM,
/// even when multiple VMs share the same IP address (from snapshot clones).
async fn check_http_health(guest_ip: &str, veth_device: &str, health_path: &str) -> Result<bool> {
    let url = format!("http://{}{}", guest_ip, health_path);

    // Use curl with --interface to bind to specific veth device
    // This is the simplest and most reliable way to route to a specific VM
    // when multiple VMs share the same IP
    //
    // Add --write-out to capture detailed timing information:
    // - time_namelookup: DNS resolution time
    // - time_connect: TCP connection establishment time
    // - time_starttransfer: Time to first byte
    // - time_total: Total request time
    let output = tokio::process::Command::new("curl")
        .args([
            "--interface", veth_device,
            "--max-time", "1",
            "--silent",
            "--fail",
            "--show-error",
            "--write-out", "\\nTIMING: dns=%{time_namelookup}s connect=%{time_connect}s ttfb=%{time_starttransfer}s total=%{time_total}s",
            &url,
        ])
        .output()
        .await
        .context("executing curl")?;

    if output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);

        // Extract timing information from curl output
        if let Some(timing_line) = stdout.lines().find(|line| line.starts_with("TIMING:")) {
            debug!(
                target: "health-monitor",
                interface = veth_device,
                guest_ip = guest_ip,
                timing = timing_line.strip_prefix("TIMING: ").unwrap_or(""),
                "health check succeeded with timing"
            );
        } else {
            debug!("Health check succeeded via {}", veth_device);
        }

        Ok(true)
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("Timeout") || stderr.contains("timeout") {
            anyhow::bail!("Health check timed out after 1 second via {}", veth_device)
        } else if stderr.contains("Connection refused") {
            anyhow::bail!("Connection refused to {} via {}", guest_ip, veth_device)
        } else {
            anyhow::bail!("Failed to connect to {} via {}: {}", guest_ip, veth_device, stderr.trim())
        }
    }
}
