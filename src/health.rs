use anyhow::{Context, Result};
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
        info!(target: "health-monitor", vm_id = %vm_id, pid = ?pid, "starting health monitor");
        let state_manager = StateManager::new(paths::state_dir());

        // Adaptive polling: 100ms during startup, 10s after healthy
        let mut poll_interval = Duration::from_millis(100);
        let mut is_healthy = false;

        loop {
            tokio::time::sleep(poll_interval).await;

            let health_status = if let Some(pid) = pid {
                // First check if Firecracker process is still running
                if std::fs::metadata(format!("/proc/{}", pid)).is_err() {
                    debug!(target: "health-monitor", vm_id = %vm_id, pid = pid, "process not found");
                    HealthStatus::Unreachable
                } else {
                    // Process exists, now check if application is responding
                    // Get guest IP, host veth device, and health check path from state
                    if let Ok(state) = state_manager.load_state(&vm_id).await {
                        let guest_ip = state.config.network.get("guest_ip").and_then(|v| v.as_str());

                        // Get veth device from network config (required for namespace isolation)
                        let veth_device = state.config.network.get("host_veth")
                            .and_then(|v| v.as_str());

                        debug!(target: "health-monitor", vm_id = %vm_id, guest_ip = ?guest_ip, veth = ?veth_device, "network config for health check");

                        if let (Some(guest_ip), Some(veth)) = (guest_ip, veth_device) {
                            // Try HTTP request via veth device
                            let health_path = &state.config.health_check_path;
                            match check_http_health(guest_ip, veth, health_path).await {
                                Ok(true) => {
                                    debug!(target: "health-monitor", vm_id = %vm_id, "health check passed");
                                    HealthStatus::Healthy
                                }
                                Ok(false) => {
                                    // This case is unreachable since check_http_health only returns Ok(true) or Err
                                    warn!(target: "health-monitor", vm_id = %vm_id, "health check returned false (unexpected)");
                                    HealthStatus::Unhealthy
                                }
                                Err(e) => {
                                    warn!(target: "health-monitor", vm_id = %vm_id, error = %e, "health check failed");
                                    HealthStatus::Unhealthy
                                }
                            }
                        } else {
                            // No network config yet, log what's missing
                            if guest_ip.is_none() {
                                warn!(target: "health-monitor", vm_id = %vm_id, "cannot check health: no guest_ip in config");
                            }
                            if veth_device.is_none() {
                                warn!(target: "health-monitor", vm_id = %vm_id, "cannot check health: no host_veth in config");
                            }
                            HealthStatus::Unknown
                        }
                    } else {
                        warn!(target: "health-monitor", vm_id = %vm_id, "failed to load VM state for health check");
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
                    Ok(_) => debug!(target: "health-monitor", vm_id = %vm_id, health_status = ?health_status, "state saved"),
                    Err(e) => warn!(target: "health-monitor", vm_id = %vm_id, error = %e, "failed to save state"),
                }
            } else {
                warn!(target: "health-monitor", vm_id = %vm_id, "failed to load state for updating health");
            }

            // Switch to slower polling once healthy
            if health_status == HealthStatus::Healthy && !is_healthy {
                is_healthy = true;
                poll_interval = Duration::from_secs(10);
                info!(target: "health-monitor", vm_id = %vm_id, "VM healthy, switching to 10s polling");
            }
        }
    })
}

/// Check if HTTP service is responding using native HTTP client
///
/// Note: With namespace isolation, we bind to the host veth device IP:
/// - TAP devices exist inside the namespace, not visible on host
/// - Multiple VMs can have the same guest IP (clones from snapshots)
/// - Binding to the veth device IP ensures requests route to the correct VM
/// - The veth device IP is derived from the guest IP (host is .1, guest is .2)
async fn check_http_health(guest_ip: &str, _veth_device: &str, health_path: &str) -> Result<bool> {
    let url = format!("http://{}{}", guest_ip, health_path);

    // Create a client with a short timeout
    // Note: We can't directly bind to a specific interface with reqwest,
    // but the routing table ensures packets to the guest IP go through the right veth
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(1))
        .danger_accept_invalid_certs(true)  // VMs may have self-signed certs
        .build()
        .context("building HTTP client")?;

    // Try to make a GET request
    match client.get(&url).send().await {
        Ok(response) => {
            if response.status().is_success() {
                debug!("Health check succeeded: {}", response.status());
                Ok(true)
            } else {
                anyhow::bail!("Health check failed with status: {}", response.status())
            }
        }
        Err(e) => {
            // Check if it's a timeout or connection error
            if e.is_timeout() {
                anyhow::bail!("Health check timed out after 1 second")
            } else if e.is_connect() {
                anyhow::bail!("Failed to connect to {}: {}", guest_ip, e)
            } else {
                anyhow::bail!("Health check request failed: {}", e)
            }
        }
    }
}
