use anyhow::{Context, Result};
use chrono::Utc;
use std::path::PathBuf;
use std::time::Instant;
use tokio::task::JoinHandle;
use tokio::time::Duration;
use tracing::{debug, info, warn};

use crate::paths;
use crate::state::{truncate_id, HealthStatus, StateManager};

/// Health check polling intervals
const HEALTH_POLL_STARTUP_INTERVAL: Duration = Duration::from_millis(100);
const HEALTH_POLL_HEALTHY_INTERVAL: Duration = Duration::from_secs(10);

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
    spawn_health_monitor_with_state_dir(vm_id, pid, paths::state_dir())
}

/// Same as `spawn_health_monitor` but with an explicit state directory.
/// Useful for tests to avoid relying on global base directory state.
pub fn spawn_health_monitor_with_state_dir(
    vm_id: String,
    pid: Option<u32>,
    state_dir: PathBuf,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let state_manager = StateManager::new(state_dir);

        // Get VM name from state for logging
        let vm_name = if let Ok(state) = state_manager.load_state(&vm_id).await {
            state.name.clone().unwrap_or_else(|| truncate_id(&vm_id, 8).to_string())
        } else {
            truncate_id(&vm_id, 8).to_string() // Fallback to short vm_id
        };

        // vm_name is already in the hierarchical target, so don't duplicate
        let _ = (&vm_name, &vm_id); // suppress unused warning
        info!(target: "health-monitor", pid = ?pid, "starting health monitor");

        // Adaptive polling: fast during startup, slow after healthy
        let mut poll_interval = HEALTH_POLL_STARTUP_INTERVAL;
        let mut is_healthy = false;

        // Throttle health check failure logs to once per second (simple local variable)
        let mut last_failure_log: Option<Instant> = None;

        loop {
            tokio::time::sleep(poll_interval).await;

            let health_status = match update_health_status_once(
                &state_manager,
                &vm_id,
                pid,
                &mut last_failure_log,
            )
            .await
            {
                Ok(status) => status,
                Err(e) => {
                    warn!(target: "health-monitor", error = %e, "health check iteration failed");
                    HealthStatus::Unknown
                }
            };

            // Switch to slower polling once healthy
            if health_status == HealthStatus::Healthy && !is_healthy {
                is_healthy = true;
                poll_interval = HEALTH_POLL_HEALTHY_INTERVAL;
                info!(target: "health-monitor", "VM healthy, switching to {:?} polling", HEALTH_POLL_HEALTHY_INTERVAL);
            }
        }
    })
}

/// Perform a single health check iteration and persist the result.
async fn update_health_status_once(
    state_manager: &StateManager,
    vm_id: &str,
    pid: Option<u32>,
    last_failure_log: &mut Option<Instant>,
) -> Result<HealthStatus> {
    let health_status = if let Some(pid) = pid {
        // First check if Firecracker process is still running
        if std::fs::metadata(format!("/proc/{}", pid)).is_err() {
            debug!(target: "health-monitor", pid = pid, "process not found");
            HealthStatus::Unreachable
        } else {
            // Process exists, now check if application is responding
            // Get network config and health check path from state
            let state = state_manager
                .load_state(vm_id)
                .await
                .context("loading state for health check")?;
            let health_path = &state.config.health_check_path;
            let net = &state.config.network;

            // Check for rootless mode (loopback_ip set)
            if let Some(loopback_ip) = &net.loopback_ip {
                let port = net.health_check_port.unwrap_or(80);
                debug!(target: "health-monitor", loopback_ip = %loopback_ip, port = port, "rootless health check via loopback");

                match check_http_health_loopback(loopback_ip, port, health_path).await {
                    Ok(true) => {
                        debug!(target: "health-monitor", "health check passed (rootless)");
                        *last_failure_log = None;
                        HealthStatus::Healthy
                    }
                    Ok(false) => {
                        warn!(target: "health-monitor", "health check returned false (unexpected)");
                        HealthStatus::Unhealthy
                    }
                    Err(e) => {
                        let should_log = match last_failure_log {
                            None => true,
                            Some(last_time) => last_time.elapsed() >= Duration::from_secs(1),
                        };
                        if should_log {
                            warn!(target: "health-monitor", error = %e, "health check failed (rootless)");
                            *last_failure_log = Some(Instant::now());
                        }
                        HealthStatus::Unhealthy
                    }
                }
            } else {
                // Bridged mode: use guest_ip + veth
                let guest_ip = net.guest_ip.as_deref();
                let veth_device = net.host_veth.as_deref();

                debug!(target: "health-monitor", guest_ip = ?guest_ip, veth = ?veth_device, "bridged health check via veth");

                if let (Some(guest_ip), Some(veth)) = (guest_ip, veth_device) {
                    match check_http_health_bridged(guest_ip, veth, health_path).await {
                        Ok(true) => {
                            debug!(target: "health-monitor", "health check passed (bridged)");
                            *last_failure_log = None;
                            HealthStatus::Healthy
                        }
                        Ok(false) => {
                            warn!(target: "health-monitor", "health check returned false (unexpected)");
                            HealthStatus::Unhealthy
                        }
                        Err(e) => {
                            let should_log = match last_failure_log {
                                None => true,
                                Some(last_time) => last_time.elapsed() >= Duration::from_secs(1),
                            };
                            if should_log {
                                warn!(target: "health-monitor", error = %e, "health check failed (bridged)");
                                *last_failure_log = Some(Instant::now());
                            }
                            HealthStatus::Unhealthy
                        }
                    }
                } else {
                    // No network config yet
                    if guest_ip.is_none() {
                        warn!(target: "health-monitor", "cannot check health: no guest_ip in config");
                    }
                    if veth_device.is_none() {
                        warn!(target: "health-monitor", "cannot check health: no host_veth in config");
                    }
                    HealthStatus::Unknown
                }
            }
        }
    } else {
        HealthStatus::Unknown
    };

    // Update state file
    let mut state = state_manager
        .load_state(vm_id)
        .await
        .context("loading state for health update")?;
    state.health_status = health_status;
    state.last_updated = Utc::now();
    state_manager
        .save_state(&state)
        .await
        .context("saving updated health state")?;

    Ok(health_status)
}

/// Run a single health check iteration (exposed for tests).
pub async fn run_health_check_once(
    vm_id: &str,
    pid: Option<u32>,
    state_dir: PathBuf,
) -> Result<HealthStatus> {
    let state_manager = StateManager::new(state_dir);
    let mut last_failure_log = None;
    update_health_status_once(&state_manager, vm_id, pid, &mut last_failure_log).await
}

/// Check if HTTP service is responding via loopback IP (rootless mode)
///
/// For rootless VMs, we use a unique loopback IP (127.x.y.z) with port forwarding
/// through slirp4netns to reach the guest.
async fn check_http_health_loopback(loopback_ip: &str, port: u16, health_path: &str) -> Result<bool> {
    let url = format!("http://{}:{}{}", loopback_ip, port, health_path);

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(1))
        .build()
        .context("building reqwest client")?;

    let start = Instant::now();

    match client.get(&url).send().await {
        Ok(response) => {
            let elapsed = start.elapsed();
            if response.status().is_success() {
                debug!(
                    target: "health-monitor",
                    loopback_ip = loopback_ip,
                    port = port,
                    status = %response.status(),
                    elapsed_ms = elapsed.as_millis(),
                    "health check succeeded (rootless)"
                );
                Ok(true)
            } else {
                anyhow::bail!(
                    "Health check failed with status {} via {}:{} ({}ms)",
                    response.status(),
                    loopback_ip,
                    port,
                    elapsed.as_millis()
                )
            }
        }
        Err(e) => {
            if e.is_timeout() {
                anyhow::bail!("Health check timed out after 1 second via {}:{}", loopback_ip, port)
            } else if e.is_connect() {
                anyhow::bail!("Connection refused to {}:{}", loopback_ip, port)
            } else {
                anyhow::bail!("Failed to connect to {}:{}: {}", loopback_ip, port, e)
            }
        }
    }
}

/// Check if HTTP service is responding using reqwest with interface binding (bridged mode)
///
/// IMPORTANT: For clones with the same guest_ip, we MUST bind to the specific
/// veth interface to reach the correct VM. Without interface binding, Linux routing
/// will always pick the first veth in the routing table, causing all health checks
/// to go to the same VM.
///
/// We use reqwest's .interface() method (which uses SO_BINDTODEVICE on Linux)
/// to ensure each health check reaches its specific VM, even when multiple VMs
/// share the same IP address (from snapshot clones).
async fn check_http_health_bridged(guest_ip: &str, veth_device: &str, health_path: &str) -> Result<bool> {
    let url = format!("http://{}{}", guest_ip, health_path);

    // Build a reqwest client bound to the specific veth device
    // This uses SO_BINDTODEVICE on Linux to ensure traffic goes through this interface
    let client = reqwest::Client::builder()
        .interface(veth_device)
        .timeout(Duration::from_secs(1))
        .build()
        .context("building reqwest client")?;

    let start = Instant::now();

    match client.get(&url).send().await {
        Ok(response) => {
            let elapsed = start.elapsed();
            if response.status().is_success() {
                debug!(
                    target: "health-monitor",
                    interface = veth_device,
                    guest_ip = guest_ip,
                    status = %response.status(),
                    elapsed_ms = elapsed.as_millis(),
                    "health check succeeded"
                );
                Ok(true)
            } else {
                anyhow::bail!(
                    "Health check failed with status {} via {} ({}ms)",
                    response.status(),
                    veth_device,
                    elapsed.as_millis()
                )
            }
        }
        Err(e) => {
            if e.is_timeout() {
                anyhow::bail!("Health check timed out after 1 second via {}", veth_device)
            } else if e.is_connect() {
                anyhow::bail!("Connection refused to {} via {}", guest_ip, veth_device)
            } else {
                anyhow::bail!("Failed to connect to {} via {}: {}", guest_ip, veth_device, e)
            }
        }
    }
}
