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
/// The task polls the VM process health at adaptive intervals:
/// - 100ms during startup (until healthy)
/// - 10s after VM is healthy
///
/// Health check tests HTTP connectivity using reqwest to the guest IP.
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
        // Track if container has no HEALTHCHECK - skip exec if so
        let mut skip_podman_healthcheck = false;

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
                &mut skip_podman_healthcheck,
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

/// Find the fcvm binary for exec commands.
fn find_fcvm_binary() -> Option<std::path::PathBuf> {
    // Try several possible locations
    // ./target/release/fcvm first for development/tests where we run from repo root
    let candidates = [
        std::path::PathBuf::from("./target/release/fcvm"),
        std::path::PathBuf::from("/usr/local/bin/fcvm"),
        std::path::PathBuf::from("/usr/bin/fcvm"),
    ];

    for path in candidates {
        if path.exists() {
            return Some(path);
        }
    }

    // Fall back to current exe if it looks like fcvm
    if let Ok(exe) = std::env::current_exe() {
        if exe.file_name().map(|n| n == "fcvm").unwrap_or(false) {
            return Some(exe);
        }
    }

    None
}

/// Check if the container is running via podman inspect.
///
/// Returns:
/// - `true` = container is running
/// - `false` = container not running yet (or inspect failed)
async fn check_container_running(pid: u32) -> bool {
    let exe = match find_fcvm_binary() {
        Some(e) => e,
        None => return false, // Can't find fcvm binary
    };

    let output = match tokio::process::Command::new(&exe)
        .args([
            "exec",
            "--pid",
            &pid.to_string(),
            "--vm", // Run in VM (not container) where podman is available
            "--",
            "podman",
            "inspect",
            "--format",
            "{{.State.Running}}",
            "fcvm-container",
        ])
        .output()
        .await
    {
        Ok(o) => o,
        Err(e) => {
            debug!(target: "health-monitor", error = %e, "podman inspect exec failed");
            return false;
        }
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        debug!(target: "health-monitor", stderr = %stderr, "podman inspect failed");
        return false;
    }

    let running = String::from_utf8_lossy(&output.stdout).trim().to_string();
    debug!(target: "health-monitor", running = %running, "container running status");

    running == "true"
}

/// Check podman healthcheck status by exec'ing into VM.
///
/// Returns:
/// - `Some(true)` = healthcheck exists and is healthy
/// - `Some(false)` = healthcheck exists and is unhealthy/starting
/// - `None` = no healthcheck defined (caller should skip future calls)
async fn check_podman_healthcheck(pid: u32) -> Option<bool> {
    // Use fcvm exec to run podman inspect inside the VM
    let exe = match find_fcvm_binary() {
        Some(e) => e,
        None => return Some(true), // Can't find fcvm binary, assume healthy
    };

    let output = match tokio::process::Command::new(&exe)
        .args([
            "exec",
            "--pid",
            &pid.to_string(),
            "--vm", // Run in VM (not container) where podman is available
            "--",
            "podman",
            "inspect",
            "--format",
            "{{.State.Health.Status}}",
            "fcvm-container",
        ])
        .output()
        .await
    {
        Ok(o) => o,
        Err(e) => {
            // Exec not available yet, don't assume healthy - keep checking
            debug!(target: "health-monitor", error = %e, "podman healthcheck exec failed, will retry");
            return Some(false);
        }
    };

    if !output.status.success() {
        // Container may not be running yet, don't assume healthy - keep checking
        let stderr = String::from_utf8_lossy(&output.stderr);
        debug!(target: "health-monitor", stderr = %stderr, "podman inspect failed, will retry");
        return Some(false);
    }

    let status = String::from_utf8_lossy(&output.stdout).trim().to_string();
    debug!(target: "health-monitor", podman_health = %status, "podman healthcheck status");

    match status.as_str() {
        "healthy" => Some(true),
        "" => None, // No healthcheck defined - skip future checks
        "unhealthy" => Some(false),
        "starting" => Some(false), // Still starting, keep polling
        _ => Some(true),           // Unknown status, assume healthy
    }
}

/// Perform a single health check iteration and persist the result.
async fn update_health_status_once(
    state_manager: &StateManager,
    vm_id: &str,
    pid: Option<u32>,
    last_failure_log: &mut Option<Instant>,
    skip_podman_healthcheck: &mut bool,
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
            // Process exists, now check application health
            let state = state_manager
                .load_state(vm_id)
                .await
                .context("loading state for health check")?;

            // Two modes:
            // 1. health_check_url = Some(url) -> HTTP check (app responds to HTTP)
            // 2. health_check_url = None -> Check if container is running via podman inspect
            let status = match &state.config.health_check_url {
                None => {
                    // No HTTP check - check if container is actually running
                    // Uses podman inspect to verify container state (not just process spawned)
                    if check_container_running(pid).await {
                        debug!(target: "health-monitor", "container is running, healthy");
                        *last_failure_log = None;
                        HealthStatus::Healthy
                    } else {
                        debug!(target: "health-monitor", "waiting for container to be running");
                        HealthStatus::Unknown
                    }
                }
                Some(url_str) => {
                    // HTTP health check
                    let url = url::Url::parse(url_str)
                        .with_context(|| format!("parsing health check URL: {}", url_str))?;
                    let health_path = url.path();
                    let net = &state.config.network;

                    // Rootless mode with holder_pid: use nsenter to curl guest directly
                    // This bypasses the complexity of slirp4netns port forwarding
                    if let Some(holder_pid) = state.holder_pid {
                        // Extract guest IP without CIDR suffix
                        let guest_ip = net
                            .guest_ip
                            .as_ref()
                            .map(|ip| ip.split('/').next().unwrap_or(ip))
                            .unwrap_or("192.168.1.2");
                        let port = 80; // Always use port 80 directly to guest
                        debug!(target: "health-monitor", holder_pid = holder_pid, guest_ip = %guest_ip, port = port, "HTTP health check via nsenter");

                        match check_http_health_nsenter(holder_pid, guest_ip, port, health_path)
                            .await
                        {
                            Ok(true) => {
                                debug!(target: "health-monitor", "health check passed");
                                *last_failure_log = None;
                                HealthStatus::Healthy
                            }
                            Ok(false) => {
                                warn!(target: "health-monitor", "health check returned false");
                                HealthStatus::Unhealthy
                            }
                            Err(e) => {
                                let should_log = match last_failure_log {
                                    None => true,
                                    Some(last_time) => {
                                        last_time.elapsed() >= Duration::from_secs(1)
                                    }
                                };
                                if should_log {
                                    debug!(target: "health-monitor", error = %e, "HTTP health check failed (nsenter)");
                                    *last_failure_log = Some(Instant::now());
                                }
                                HealthStatus::Unhealthy
                            }
                        }
                    } else {
                        // Bridged mode: transform URL to use guest IP if localhost is specified
                        // "localhost" from the host doesn't reach the VM - we need the guest's IP
                        let veth_device = net.host_veth.as_deref();

                        // Transform URL: if host is localhost/127.0.0.1, use guest IP instead
                        let effective_url = if url.host_str() == Some("localhost")
                            || url.host_str() == Some("127.0.0.1")
                        {
                            if let Some(guest_ip) = net.guest_ip.as_ref() {
                                // Strip CIDR suffix if present
                                let guest_ip = guest_ip.split('/').next().unwrap_or(guest_ip);
                                let port = url.port().unwrap_or(80);
                                format!("http://{}:{}{}", guest_ip, port, health_path)
                            } else {
                                url_str.to_string()
                            }
                        } else {
                            url_str.to_string()
                        };

                        debug!(target: "health-monitor", original_url = %url_str, effective_url = %effective_url, veth = ?veth_device, "HTTP health check via veth");

                        match check_http_health_bridged(&effective_url, veth_device).await {
                            Ok(true) => {
                                debug!(target: "health-monitor", "health check passed");
                                *last_failure_log = None;
                                HealthStatus::Healthy
                            }
                            Ok(false) => {
                                debug!(target: "health-monitor", "health check returned false");
                                HealthStatus::Unhealthy
                            }
                            Err(e) => {
                                let should_log = match last_failure_log {
                                    None => true,
                                    Some(last_time) => {
                                        last_time.elapsed() >= Duration::from_secs(1)
                                    }
                                };
                                if should_log {
                                    debug!(target: "health-monitor", error = %e, "HTTP health check failed");
                                    *last_failure_log = Some(Instant::now());
                                }
                                HealthStatus::Unhealthy
                            }
                        }
                    }
                }
            };

            // If base health check passed, also check podman healthcheck (AND logic)
            // Skip if we already know the container has no healthcheck
            let final_status = if status == HealthStatus::Healthy && !*skip_podman_healthcheck {
                match check_podman_healthcheck(pid).await {
                    Some(true) => {
                        debug!(target: "health-monitor", "all health checks passed");
                        HealthStatus::Healthy
                    }
                    Some(false) => {
                        debug!(target: "health-monitor", "podman healthcheck not healthy");
                        HealthStatus::Unhealthy
                    }
                    None => {
                        // No healthcheck defined - skip future checks
                        debug!(target: "health-monitor", "no podman healthcheck defined, skipping future checks");
                        *skip_podman_healthcheck = true;
                        HealthStatus::Healthy
                    }
                }
            } else {
                status
            };
            (final_status, None)
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
    let mut skip_podman_healthcheck = false;
    let (status, _exit_code) = update_health_status_once(
        &state_manager,
        vm_id,
        pid,
        &mut last_failure_log,
        &mut skip_podman_healthcheck,
    )
    .await?;
    Ok(status)
}

/// Check if HTTP service is responding via nsenter into the network namespace (rootless mode)
///
/// For rootless VMs, we use nsenter to enter the network namespace and curl
/// the guest directly. This bypasses the complexity of slirp4netns port forwarding.
///
/// The holder_pid is the PID of the namespace holder process (sleep infinity).
async fn check_http_health_nsenter(
    holder_pid: u32,
    guest_ip: &str,
    port: u16,
    health_path: &str,
) -> Result<bool> {
    let url = format!("http://{}:{}{}", guest_ip, port, health_path);

    let start = Instant::now();

    // Use nsenter to enter the namespace and curl the guest directly
    // --preserve-credentials keeps UID/GID mapping
    let output = tokio::process::Command::new("nsenter")
        .args([
            "-t",
            &holder_pid.to_string(),
            "-U",
            "-n",
            "--preserve-credentials",
            "--",
            "curl",
            "-s",
            "-o",
            "/dev/null",
            "-w",
            "%{http_code}",
            "--max-time",
            "1",
            &url,
        ])
        .output()
        .await
        .context("failed to run nsenter curl")?;

    let elapsed = start.elapsed();

    if output.status.success() {
        let status_code = String::from_utf8_lossy(&output.stdout);
        let status_code = status_code.trim();

        if status_code.starts_with('2') || status_code.starts_with('3') {
            debug!(
                target: "health-monitor",
                holder_pid = holder_pid,
                guest_ip = guest_ip,
                port = port,
                status = status_code,
                elapsed_ms = elapsed.as_millis(),
                "health check succeeded (nsenter)"
            );
            Ok(true)
        } else {
            anyhow::bail!(
                "Health check failed with status {} via nsenter to {}:{} ({}ms)",
                status_code,
                guest_ip,
                port,
                elapsed.as_millis()
            )
        }
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("timed out") || stderr.contains("Connection timed out") {
            anyhow::bail!(
                "Health check timed out via nsenter to {}:{}",
                guest_ip,
                port
            )
        } else if stderr.contains("Connection refused") {
            anyhow::bail!("Connection refused to {}:{} via nsenter", guest_ip, port)
        } else {
            anyhow::bail!(
                "Failed to connect to {}:{} via nsenter: {}",
                guest_ip,
                port,
                stderr.trim()
            )
        }
    }
}

/// Check if HTTP service is responding using reqwest with optional interface binding (bridged mode)
///
/// For baseline VMs, we bind to the specific veth interface since the guest IP
/// is reachable via that interface.
///
/// For clones with In-Namespace NAT, the health_check_url uses the veth inner IP
/// (e.g., 10.x.y.2) which is routed directly by the kernel, so interface binding
/// is optional (the kernel routes to the correct veth based on IP).
///
/// We use reqwest's .interface() method (which uses SO_BINDTODEVICE on Linux)
/// when a veth device is provided, ensuring traffic goes through that interface.
async fn check_http_health_bridged(url: &str, veth_device: Option<&str>) -> Result<bool> {
    // Build a reqwest client, optionally bound to the veth device
    let mut builder = reqwest::Client::builder().timeout(Duration::from_secs(1));

    if let Some(veth) = veth_device {
        builder = builder.interface(veth);
    }

    let client = builder.build().context("building reqwest client")?;

    let start = Instant::now();
    let iface_str = veth_device.unwrap_or("default");

    match client.get(url).send().await {
        Ok(response) => {
            let elapsed = start.elapsed();
            if response.status().is_success() {
                debug!(
                    target: "health-monitor",
                    interface = iface_str,
                    url = url,
                    status = %response.status(),
                    elapsed_ms = elapsed.as_millis(),
                    "health check succeeded"
                );
                Ok(true)
            } else {
                anyhow::bail!(
                    "Health check failed with status {} via {} ({}ms)",
                    response.status(),
                    iface_str,
                    elapsed.as_millis()
                )
            }
        }
        Err(e) => {
            if e.is_timeout() {
                anyhow::bail!("Health check timed out after 1 second via {}", iface_str)
            } else if e.is_connect() {
                anyhow::bail!("Connection refused to {} via {}", url, iface_str)
            } else {
                anyhow::bail!("Failed to connect to {} via {}: {}", url, iface_str, e)
            }
        }
    }
}
