use anyhow::{Context, Result};
use std::path::PathBuf;
use std::time::Instant;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio::time::Duration;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::paths;
use crate::state::{truncate_id, HealthStatus, StateManager};

/// Health check polling intervals
const HEALTH_POLL_STARTUP_INTERVAL: Duration = Duration::from_millis(100);
const HEALTH_POLL_HEALTHY_INTERVAL: Duration = Duration::from_secs(10);

/// Spawn a background health monitoring task for a VM
///
/// The task polls the VM health at adaptive intervals:
/// - 100ms during startup (until healthy)
/// - 10s after VM is healthy
///
/// Health is determined by:
/// 1. Firecracker process existence
/// 2. container-health file (written by fc-agent reporting podman's health status)
/// 3. container-ready file (fallback for containers without healthcheck)
///
/// Returns a JoinHandle that can be used to cancel the task.
/// The task runs until cancelled or until the tokio runtime shuts down.
pub fn spawn_health_monitor(vm_id: String, pid: Option<u32>) -> JoinHandle<()> {
    spawn_health_monitor_with_cancel(vm_id, pid, paths::state_dir(), None)
}

/// Spawn a health monitor with a cancellation token for graceful shutdown.
///
/// When the token is cancelled, the health monitor will stop after completing
/// its current iteration (no partial state updates).
pub fn spawn_health_monitor_with_cancel(
    vm_id: String,
    pid: Option<u32>,
    state_dir: PathBuf,
    cancel_token: Option<CancellationToken>,
) -> JoinHandle<()> {
    spawn_health_monitor_full(vm_id, pid, state_dir, cancel_token, None)
}

/// Spawn a health monitor with full configuration options.
///
/// Parameters:
/// - `vm_id`: The VM identifier
/// - `pid`: Optional process ID for the VM
/// - `state_dir`: Directory for state files
/// - `cancel_token`: Optional token for graceful shutdown
/// - `startup_healthy_tx`: Optional oneshot channel to signal when health first becomes Healthy.
///   This is used to trigger startup snapshot creation. Only fires once, when transitioning
///   from non-healthy to healthy state.
pub fn spawn_health_monitor_full(
    vm_id: String,
    pid: Option<u32>,
    state_dir: PathBuf,
    cancel_token: Option<CancellationToken>,
    startup_healthy_tx: Option<oneshot::Sender<()>>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let state_manager = StateManager::new(state_dir);

        // Get VM name from state for logging
        let vm_name = if let Ok(state) = state_manager.load_state(&vm_id).await {
            state
                .name
                .clone()
                .unwrap_or_else(|| truncate_id(&vm_id, 8).to_string())
        } else {
            truncate_id(&vm_id, 8).to_string() // Fallback to short vm_id
        };

        // vm_name is already in the hierarchical target, so don't duplicate
        let _ = (&vm_name, &vm_id); // suppress unused warning
        info!(target: "health-monitor", pid = ?pid, "starting health monitor");

        // Adaptive polling: fast during startup, slow after healthy
        let mut poll_interval = HEALTH_POLL_STARTUP_INTERVAL;
        let mut is_healthy = false;

        // Oneshot channel for startup snapshot notification (can only fire once)
        let mut startup_tx = startup_healthy_tx;

        // Throttle health check failure logs to once per second (simple local variable)
        let mut last_failure_log: Option<Instant> = None;
        let mut first_check = true;

        loop {
            // Check for cancellation before sleeping
            if let Some(ref token) = cancel_token {
                if token.is_cancelled() {
                    info!(target: "health-monitor", "cancellation requested, stopping");
                    break;
                }
            }

            // Skip initial sleep - check immediately on first iteration
            // This saves ~100ms on clone startup
            if first_check {
                first_check = false;
            } else {
                // Sleep with cancellation support
                if let Some(ref token) = cancel_token {
                    tokio::select! {
                        _ = tokio::time::sleep(poll_interval) => {}
                        _ = token.cancelled() => {
                            info!(target: "health-monitor", "cancellation requested during sleep, stopping");
                            break;
                        }
                    }
                } else {
                    tokio::time::sleep(poll_interval).await;
                }
            }

            let health_status = match update_health_status_once(
                &state_manager,
                &vm_id,
                pid,
                &mut last_failure_log,
            )
            .await
            {
                Ok((status, _exit_code)) => status,
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

                // Signal startup snapshot trigger (fires only once)
                if let Some(tx) = startup_tx.take() {
                    info!(target: "health-monitor", "signaling startup snapshot trigger");
                    let _ = tx.send(()); // Ignore error if receiver dropped
                }
            }

            // Stop monitoring if container has stopped
            if health_status == HealthStatus::Stopped {
                info!(target: "health-monitor", "container stopped, ending health monitor");
                break;
            }
        }
    })
}

/// Same as `spawn_health_monitor` but with an explicit state directory.
/// Useful for tests to avoid relying on global base directory state.
pub fn spawn_health_monitor_with_state_dir(
    vm_id: String,
    pid: Option<u32>,
    state_dir: PathBuf,
) -> JoinHandle<()> {
    spawn_health_monitor_with_cancel(vm_id, pid, state_dir, None)
}

/// Perform a single health check iteration and persist the result.
async fn update_health_status_once(
    state_manager: &StateManager,
    vm_id: &str,
    pid: Option<u32>,
    last_failure_log: &mut Option<Instant>,
) -> Result<(HealthStatus, Option<i32>)> {
    let (health_status, exit_code) = if let Some(pid) = pid {
        // First check if Firecracker process is still running
        if std::fs::metadata(format!("/proc/{}", pid)).is_err() {
            debug!(target: "health-monitor", pid = pid, "process not found");
            // Process exited - check for container-exit file to get exit code
            let exit_file = paths::vm_runtime_dir(vm_id).join("container-exit");
            if exit_file.exists() {
                let exit_code = std::fs::read_to_string(&exit_file)
                    .ok()
                    .and_then(|s| s.trim().parse::<i32>().ok());
                info!(target: "health-monitor", exit_code = ?exit_code, "container stopped");
                (HealthStatus::Stopped, exit_code)
            } else {
                // Process gone but no exit file - VM crashed or was killed
                (HealthStatus::Unreachable, None)
            }
        } else {
            // Process exists, check container health status file (written by fc-agent via vsock)
            // fc-agent monitors podman's container health and reports it
            let health_file = paths::vm_runtime_dir(vm_id).join("container-health");
            let ready_file = paths::vm_runtime_dir(vm_id).join("container-ready");

            let status = if health_file.exists() {
                // fc-agent is reporting podman health status
                match std::fs::read_to_string(&health_file) {
                    Ok(content) => {
                        let health = content.trim();
                        match health {
                            "healthy" => {
                                debug!(target: "health-monitor", "podman reports healthy");
                                *last_failure_log = None;
                                HealthStatus::Healthy
                            }
                            "unhealthy" => {
                                debug!(target: "health-monitor", "podman reports unhealthy");
                                HealthStatus::Unhealthy
                            }
                            "starting" => {
                                debug!(target: "health-monitor", "podman health check starting");
                                HealthStatus::Unknown
                            }
                            _ => {
                                // No healthcheck defined, but container is running
                                debug!(target: "health-monitor", status = health, "container running (no healthcheck)");
                                *last_failure_log = None;
                                HealthStatus::Healthy
                            }
                        }
                    }
                    Err(e) => {
                        debug!(target: "health-monitor", error = %e, "failed to read health file");
                        HealthStatus::Unknown
                    }
                }
            } else if ready_file.exists() {
                // Legacy: container started but no health updates yet
                debug!(target: "health-monitor", "container-ready file exists, healthy");
                *last_failure_log = None;
                HealthStatus::Healthy
            } else {
                debug!(target: "health-monitor", "waiting for container health status");
                HealthStatus::Unknown
            };
            (status, None)
        }
    } else {
        (HealthStatus::Unknown, None)
    };

    // Update state file atomically (lock held across read-modify-write)
    state_manager
        .update_health_status(vm_id, health_status, exit_code)
        .await
        .context("updating health state atomically")?;

    Ok((health_status, exit_code))
}

/// Run a single health check iteration (exposed for tests).
pub async fn run_health_check_once(
    vm_id: &str,
    pid: Option<u32>,
    state_dir: PathBuf,
) -> Result<HealthStatus> {
    let state_manager = StateManager::new(state_dir);
    let mut last_failure_log = None;
    let (status, _exit_code) =
        update_health_status_once(&state_manager, vm_id, pid, &mut last_failure_log).await?;
    Ok(status)
}
