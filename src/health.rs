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

/// Health check polling intervals.
/// During startup, the interval adapts to match how long each check takes
/// (minimum 100ms). Fast checks (podman inspect ~100ms) poll fast; slow
/// checks (nsenter+curl ~1-3s) poll slower to avoid hammering.
const HEALTH_POLL_MIN_INTERVAL: Duration = Duration::from_millis(100);
const HEALTH_POLL_MAX_INTERVAL: Duration = Duration::from_secs(5);
const HEALTH_POLL_HEALTHY_INTERVAL: Duration = Duration::from_secs(10);

/// Spawn a background health monitoring task for a VM
///
/// The task polls the VM process health at adaptive intervals:
/// - Adaptive during startup (matches check duration, min 100ms)
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

/// Ack sent by the startup-snapshot path once its pause/resume cycle is over
/// (snapshot created, skipped, or failed). Dropping it unblocks the monitor
/// too, so an aborted snapshot path can never wedge health reporting.
pub type StartupSnapshotAck = oneshot::Sender<()>;

/// Spawn a health monitor with full configuration options.
///
/// Parameters:
/// - `vm_id`: The VM identifier
/// - `pid`: Optional process ID for the VM
/// - `state_dir`: Directory for state files
/// - `cancel_token`: Optional token for graceful shutdown
/// - `startup_healthy_tx`: Optional channel to signal when health first becomes Healthy.
///   This triggers startup snapshot creation, which PAUSES the VM (the memory dump can
///   take tens of seconds under I/O load). A paused VM cannot answer forwarded ports or
///   execs, so the monitor sends an ack channel along with the signal and defers
///   publishing Healthy to the state file until the snapshot path acks (or drops the
///   ack): once Healthy is externally visible, the data plane is actually live.
pub fn spawn_health_monitor_full(
    vm_id: String,
    pid: Option<u32>,
    state_dir: PathBuf,
    cancel_token: Option<CancellationToken>,
    startup_healthy_tx: Option<oneshot::Sender<StartupSnapshotAck>>,
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
            truncate_id(&vm_id, 8).to_string()
        };

        // vm_name is already in the hierarchical target, so don't duplicate
        let _ = (&vm_name, &vm_id); // suppress unused warning
        info!(target: "health-monitor", pid = ?pid, "starting health monitor");

        // Adaptive polling: during startup, wait as long as the check took
        // (min 100ms). Once healthy, switch to 10s.
        let mut poll_interval = HEALTH_POLL_MIN_INTERVAL;
        let mut is_healthy = false;

        // Oneshot channel for startup snapshot notification (can only fire once)
        let mut startup_tx = startup_healthy_tx;

        // Throttle health check failure logs to once per second (simple local variable)
        let mut last_failure_log: Option<Instant> = None;
        let mut first_check = true;
        // Track if container has no HEALTHCHECK - skip exec if so
        let mut skip_podman_healthcheck = false;
        // Track consecutive failures for escalated logging
        let mut consecutive_failures: u32 = 0;

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

            let check_start = Instant::now();
            let checked = check_health_once(
                &state_manager,
                &vm_id,
                pid,
                &mut last_failure_log,
                &mut skip_podman_healthcheck,
            )
            .await;
            let check_duration = check_start.elapsed();
            let (health_status, exit_code, check_ok) = match checked {
                Ok((status, exit_code)) => (status, exit_code, true),
                Err(e) => {
                    warn!(target: "health-monitor", error = %e, "health check iteration failed");
                    (HealthStatus::Unknown, None, false)
                }
            };

            // First healthy transition: the healthy signal triggers startup-snapshot
            // creation, which pauses the VM. Run that pause/resume cycle to completion
            // BEFORE persisting Healthy, so no client (port-forward curl, exec, `fcvm
            // ls` poller) can observe Healthy while the vCPUs are paused. The snapshot
            // path acks when done; a dropped ack (abort/shutdown) unblocks us too.
            if health_status == HealthStatus::Healthy && !is_healthy {
                if let Some(tx) = startup_tx.take() {
                    let (ack_tx, ack_rx) = oneshot::channel::<()>();
                    if tx.send(ack_tx).is_ok() {
                        info!(target: "health-monitor",
                            "signaling startup snapshot trigger; deferring Healthy until its pause is over");
                        let acked = if let Some(ref token) = cancel_token {
                            tokio::select! {
                                res = ack_rx => res.is_ok(),
                                _ = token.cancelled() => {
                                    info!(target: "health-monitor", "cancellation requested during startup snapshot, stopping");
                                    break;
                                }
                            }
                        } else {
                            ack_rx.await.is_ok()
                        };
                        if !acked {
                            // The ack sender was dropped without completing: the
                            // startup-snapshot path was abandoned (guest-reboot
                            // relaunch clears startup_rx; shutdown paths return
                            // early). This Healthy observation predates that pause
                            // or reboot — persisting it would publish Healthy for
                            // a VM that is mid-reboot. Discard the observation and
                            // re-check real state next tick. The spent trigger is
                            // intentionally not re-armed: the relaunched VM takes
                            // no startup snapshot, so from now on Healthy
                            // observations persist ungated.
                            warn!(target: "health-monitor",
                                "startup snapshot ack dropped; discarding pre-pause Healthy observation and re-checking");
                            continue;
                        }
                    }
                }
            }

            // failpoint: hold between the first-healthy ack gate above and the
            // state-file persist — makes "client races the externally-visible
            // Healthy transition" (exec/curl just before Healthy lands) deterministic.
            if check_ok && health_status == HealthStatus::Healthy {
                failpoint::hit_async("health.pre_persist_healthy").await;
            }

            // Persist after the gate above. A failed check persists nothing (same as
            // before the check/persist split): the next iteration retries.
            let mut persisted = false;
            if check_ok {
                match state_manager
                    .update_health_status(&vm_id, health_status, exit_code)
                    .await
                {
                    Ok(_) => persisted = true,
                    Err(e) => {
                        warn!(target: "health-monitor", error = %e, "persisting health status failed");
                    }
                }
            }

            // Track consecutive failures for escalated logging
            if health_status == HealthStatus::Healthy {
                if consecutive_failures > 0 {
                    info!(target: "health-monitor",
                        consecutive_failures,
                        "health check recovered after {} consecutive failures",
                        consecutive_failures
                    );
                }
                consecutive_failures = 0;
            } else {
                consecutive_failures += 1;
                // Log at WARN every 10 failures (~30-50s) so CI logs show something
                if consecutive_failures == 10 || consecutive_failures.is_multiple_of(30) {
                    warn!(target: "health-monitor",
                        consecutive_failures,
                        status = ?health_status,
                        check_ms = check_duration.as_millis() as u64,
                        "health check still failing"
                    );
                }
            }

            // Adaptive polling: once healthy use 10s. During startup, wait as
            // long as the check took (min 100ms) — fast checks poll fast, slow
            // checks (nsenter+curl) poll slower to avoid hammering.
            if health_status == HealthStatus::Healthy {
                if !is_healthy {
                    is_healthy = true;
                    poll_interval = HEALTH_POLL_HEALTHY_INTERVAL;
                    info!(target: "health-monitor", "VM healthy, switching to {:?} polling", HEALTH_POLL_HEALTHY_INTERVAL);
                }
            } else if is_healthy {
                // VM was healthy but is no longer — revert to adaptive polling
                is_healthy = false;
                poll_interval =
                    check_duration.clamp(HEALTH_POLL_MIN_INTERVAL, HEALTH_POLL_MAX_INTERVAL);
                warn!(target: "health-monitor", "VM no longer healthy, reverting to {:?} polling", poll_interval);
            } else {
                // Still unhealthy — adapt interval to check duration
                poll_interval =
                    check_duration.clamp(HEALTH_POLL_MIN_INTERVAL, HEALTH_POLL_MAX_INTERVAL);
            }

            // Stop monitoring if container has stopped — but only once the Stopped
            // status actually reached the state file. If the persist failed, exiting
            // here would leave the state file stale (possibly Healthy) forever; keep
            // looping so the next tick retries the check and the write (mirrors the
            // pre-split behavior where a persist error yielded Unknown and the
            // monitor stayed alive).
            if health_status == HealthStatus::Stopped {
                if persisted {
                    info!(target: "health-monitor", "container stopped, ending health monitor");
                    break;
                }
                warn!(target: "health-monitor",
                    "container stopped but persisting Stopped failed; retrying next tick");
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
///
/// The health monitor runs inside the fcvm process that manages the VM, so the
/// running executable is the binary that wrote the state files the exec
/// subcommand reads. Prefer it over install or build locations to avoid
/// exec'ing a different (possibly stale) fcvm binary.
fn find_fcvm_binary() -> Option<std::path::PathBuf> {
    if let Ok(exe) = std::env::current_exe() {
        if exe.file_name().map(|n| n == "fcvm").unwrap_or(false) {
            return Some(exe);
        }
    }

    // Fallbacks for callers whose current_exe is not fcvm (e.g. test binaries):
    // repo build output first, then install locations.
    let candidates = [
        std::path::PathBuf::from("./target/release/fcvm"),
        std::path::PathBuf::from("/usr/local/bin/fcvm"),
        std::path::PathBuf::from("/usr/bin/fcvm"),
    ];

    candidates.into_iter().find(|path| path.exists())
}

/// Timeout for exec-based health checks (5 seconds)
const HEALTH_CHECK_EXEC_TIMEOUT: Duration = Duration::from_secs(5);

/// Build the env + runuser prefix for health check commands running as a user.
///
/// Must mirror fc-agent's `run_as_user_prefix()`: sets HOME and XDG_RUNTIME_DIR
/// so podman finds user-level config and runtime state.
fn user_cmd_prefix(name: &str, user_spec: Option<&str>) -> Vec<String> {
    // Extract UID from user spec (format: "UID:GID" or just "UID")
    // Extract UID from user spec. If missing, resolve from username via /etc/passwd.
    let uid = user_spec
        .and_then(|s| s.split(':').next())
        .map(|s| s.to_string())
        .unwrap_or_else(|| {
            // Fallback: look up UID from username
            nix::unistd::User::from_name(name)
                .ok()
                .flatten()
                .map(|u| u.uid.as_raw().to_string())
                .unwrap_or_else(|| "1000".to_string())
        });

    vec![
        "env".into(),
        format!("HOME=/home/{}", name),
        format!("XDG_RUNTIME_DIR=/run/user/{}", uid),
        "runuser".into(),
        "-u".into(),
        name.to_string(),
        "--".into(),
    ]
}

/// Check if the container is running via podman inspect.
///
/// Returns:
/// - `true` = container is running
/// - `false` = container not running yet (or inspect failed)
async fn check_container_running(
    pid: u32,
    username: Option<&str>,
    user_spec: Option<&str>,
) -> bool {
    let exe = match find_fcvm_binary() {
        Some(e) => e,
        None => return false, // Can't find fcvm binary
    };

    // Build the command to run inside the VM.
    // With --user, podman runs as the target user (rootless), so we need runuser.
    let mut cmd_args: Vec<String> = vec![
        "exec".into(),
        // Quiet: health-monitor subprocess. A benign "stream closed before exit" race
        // during teardown is downgraded to debug for quiet callers (#607); user-invoked
        // execs still surface a visible error.
        "--quiet".into(),
        "--pid".into(),
        pid.to_string(),
        "--vm".into(),
        "--".into(),
    ];
    if let Some(name) = username {
        cmd_args.extend(user_cmd_prefix(name, user_spec));
    }
    cmd_args.extend([
        "podman".into(),
        "inspect".into(),
        "--format".into(),
        "{{.State.Running}}".into(),
        "fcvm-container".into(),
    ]);

    let child = match tokio::process::Command::new(&exe)
        .args(&cmd_args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            debug!(target: "health-monitor", error = %e, "podman inspect spawn failed");
            return false;
        }
    };

    let output =
        match tokio::time::timeout(HEALTH_CHECK_EXEC_TIMEOUT, child.wait_with_output()).await {
            Ok(Ok(o)) => o,
            Ok(Err(e)) => {
                debug!(target: "health-monitor", error = %e, "podman inspect exec failed");
                return false;
            }
            Err(_) => {
                debug!(target: "health-monitor", "podman inspect exec timed out");
                return false;
            }
        };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        // Exit code 125 = container not found (expected during startup).
        // Any other failure is unexpected and worth warning about.
        let code = output.status.code().unwrap_or(-1);
        if code == 125 {
            debug!(target: "health-monitor", "waiting for container to be created");
        } else {
            warn!(target: "health-monitor", stderr = %stderr, code, "podman inspect failed");
        }
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
async fn check_podman_healthcheck(
    pid: u32,
    username: Option<&str>,
    user_spec: Option<&str>,
) -> Option<bool> {
    // Use fcvm exec to run podman inspect inside the VM
    let exe = match find_fcvm_binary() {
        Some(e) => e,
        None => return None, // Can't find fcvm binary, can't determine health
    };

    let mut cmd_args: Vec<String> = vec![
        "exec".into(),
        // Quiet: health-monitor subprocess. A benign "stream closed before exit" race
        // during teardown is downgraded to debug for quiet callers (#607); user-invoked
        // execs still surface a visible error.
        "--quiet".into(),
        "--pid".into(),
        pid.to_string(),
        "--vm".into(),
        "--".into(),
    ];
    if let Some(name) = username {
        cmd_args.extend(user_cmd_prefix(name, user_spec));
    }
    cmd_args.extend([
        "podman".into(),
        "inspect".into(),
        "--format".into(),
        "{{.State.Health.Status}}".into(),
        "fcvm-container".into(),
    ]);

    let child = match tokio::process::Command::new(&exe)
        .args(&cmd_args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            debug!(target: "health-monitor", error = %e, "podman healthcheck spawn failed");
            return Some(false);
        }
    };

    // kill_on_drop ensures the child is killed if the timeout fires
    let output = match tokio::time::timeout(HEALTH_CHECK_EXEC_TIMEOUT, child.wait_with_output())
        .await
    {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => {
            debug!(target: "health-monitor", error = %e, "podman healthcheck exec failed, will retry");
            return Some(false);
        }
        Err(_) => {
            debug!(target: "health-monitor", "podman healthcheck exec timed out, will retry");
            return Some(false);
        }
    };

    if !output.status.success() {
        // Container may not be running yet, don't assume healthy - keep checking
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        let code = output.status.code().unwrap_or(-1);
        if code == 125 {
            debug!(target: "health-monitor", "waiting for container to be created");
        } else {
            warn!(target: "health-monitor", stderr = %stderr, code, "podman healthcheck inspect failed");
        }
        return Some(false);
    }

    let status = String::from_utf8_lossy(&output.stdout).trim().to_string();
    debug!(target: "health-monitor", podman_health = %status, "podman healthcheck status");

    match status.as_str() {
        "healthy" => Some(true),
        "" => None, // No healthcheck defined - skip future checks
        "unhealthy" => Some(false),
        "starting" => {
            // Rootless podman without a systemd user session doesn't run the
            // periodic healthcheck timer. Trigger it manually.
            debug!(target: "health-monitor", "podman health is 'starting', triggering healthcheck run");
            run_podman_healthcheck(pid, username, user_spec).await;
            Some(false)
        }
        _ => Some(true), // Unknown status, assume healthy
    }
}

/// Trigger `podman healthcheck run` inside the VM.
/// Rootless podman without systemd doesn't auto-run healthchecks.
async fn run_podman_healthcheck(pid: u32, username: Option<&str>, user_spec: Option<&str>) {
    let exe = match find_fcvm_binary() {
        Some(e) => e,
        None => return,
    };

    let mut cmd_args: Vec<String> = vec![
        "exec".into(),
        // Quiet: health-monitor subprocess. A benign "stream closed before exit" race
        // during teardown is downgraded to debug for quiet callers (#607); user-invoked
        // execs still surface a visible error.
        "--quiet".into(),
        "--pid".into(),
        pid.to_string(),
        "--vm".into(),
        "--".into(),
    ];
    if let Some(name) = username {
        cmd_args.extend(user_cmd_prefix(name, user_spec));
    }
    cmd_args.extend([
        "podman".into(),
        "healthcheck".into(),
        "run".into(),
        "fcvm-container".into(),
    ]);

    let child = match tokio::process::Command::new(&exe)
        .args(&cmd_args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            debug!(target: "health-monitor", error = %e, "podman healthcheck run spawn failed");
            return;
        }
    };

    match tokio::time::timeout(HEALTH_CHECK_EXEC_TIMEOUT, child.wait_with_output()).await {
        Ok(Ok(o)) => {
            debug!(target: "health-monitor", exit = %o.status, "podman healthcheck run completed");
        }
        Ok(Err(e)) => {
            debug!(target: "health-monitor", error = %e, "podman healthcheck run exec failed");
        }
        Err(_) => {
            debug!(target: "health-monitor", "podman healthcheck run timed out");
        }
    }
}

/// Perform a single health check iteration WITHOUT persisting the result.
///
/// The monitor loop persists separately so the first Healthy can be gated on
/// the startup-snapshot pause finishing (see `spawn_health_monitor_full`).
async fn check_health_once(
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
            // Process exists - first check if container has already exited (e.g., failed to load)
            // This catches cases where the container fails early (exit 125 = image load error)
            // but the Firecracker VM is still running
            let exit_file = paths::vm_runtime_dir(vm_id).join("container-exit");
            if exit_file.exists() {
                let exit_code = std::fs::read_to_string(&exit_file)
                    .ok()
                    .and_then(|s| s.trim().parse::<i32>().ok());
                info!(target: "health-monitor", exit_code = ?exit_code, "container exited while VM still running");
                return Ok((HealthStatus::Stopped, exit_code));
            }

            // Process exists and container hasn't exited, now check application health
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
                    let container_running = check_container_running(
                        pid,
                        state.config.username.as_deref(),
                        state.config.user.as_deref(),
                    )
                    .await;
                    if container_running {
                        debug!(target: "health-monitor", "container is running");
                        *last_failure_log = None;
                        // Continue to podman healthcheck below
                        HealthStatus::Healthy
                    } else {
                        // Container not running yet - check if there's a podman healthcheck defined
                        // so we can skip future checks if there isn't one
                        // Note: We don't return Unhealthy here because check_podman_healthcheck
                        // returns Some(false) when the container doesn't exist yet (inspect fails)
                        if !*skip_podman_healthcheck
                            && check_podman_healthcheck(
                                pid,
                                state.config.username.as_deref(),
                                state.config.user.as_deref(),
                            )
                            .await
                            .is_none()
                        {
                            // No healthcheck defined - skip future checks
                            debug!(target: "health-monitor", "no podman healthcheck defined, skipping future checks");
                            *skip_podman_healthcheck = true;
                        }
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

                    // Extract Host header from the URL hostname.
                    // The URL hostname is the virtual host the server routes by.
                    // We override the connection IP with the guest IP but send
                    // the original hostname as the Host header.
                    let url_host = url.host_str();
                    let health_timeout = state.config.health_check_timeout.max(1);

                    // Rootless mode with holder_pid: use nsenter to curl guest directly
                    // This bypasses the complexity of pasta port forwarding
                    if let Some(holder_pid) = state.holder_pid {
                        // Extract guest IP without CIDR suffix
                        let guest_ip = net
                            .guest_ip
                            .as_ref()
                            .map(|ip| ip.split('/').next().unwrap_or(ip))
                            .unwrap_or("192.168.1.2");
                        let port = url.port().unwrap_or(80);
                        debug!(target: "health-monitor", holder_pid, guest_ip = %guest_ip, port, host = ?url_host, "HTTP health check via nsenter");

                        match check_http_health_nsenter(
                            holder_pid,
                            guest_ip,
                            port,
                            health_path,
                            url_host,
                            health_timeout,
                        )
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
                                    warn!(target: "health-monitor", error = %e, "HTTP health check failed (nsenter)");
                                    *last_failure_log = Some(Instant::now());
                                }
                                HealthStatus::Unhealthy
                            }
                        }
                    } else if let Some(ref ns_name) = net.namespace_name {
                        // Routed mode with named namespace: use `ip netns exec` to curl guest
                        let guest_ip = net
                            .guest_ip
                            .as_ref()
                            .map(|ip| ip.split('/').next().unwrap_or(ip))
                            .unwrap_or("10.0.2.100");
                        let port = url.port().unwrap_or(80);
                        debug!(target: "health-monitor", ns_name = ns_name, guest_ip = %guest_ip, port = port, host = ?url_host, "HTTP health check via ip netns exec");

                        match check_http_health_netns(
                            ns_name,
                            guest_ip,
                            port,
                            health_path,
                            url_host,
                            health_timeout,
                        )
                        .await
                        {
                            Ok(true) => {
                                debug!(target: "health-monitor", "health check passed (netns)");
                                *last_failure_log = None;
                                HealthStatus::Healthy
                            }
                            Ok(false) => {
                                warn!(target: "health-monitor", "health check returned false (netns)");
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
                                    warn!(target: "health-monitor", error = %e, "HTTP health check failed (netns)");
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

                        match check_http_health_bridged(
                            &effective_url,
                            veth_device,
                            url_host,
                            health_timeout,
                        )
                        .await
                        {
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
                                    warn!(target: "health-monitor", error = %e, "HTTP health check failed");
                                    *last_failure_log = Some(Instant::now());
                                }
                                HealthStatus::Unhealthy
                            }
                        }
                    }
                }
            };

            // If base health check passed, also check podman healthcheck (AND logic)
            // Skip if we already know the container has no healthcheck.
            // Also skip when health_check_url is provided — the user explicitly specified
            // their health check, and podman's internal HEALTHCHECK may not work (e.g.,
            // rootless podman in VM without systemd can't schedule healthcheck timers).
            let has_http_check = state.config.health_check_url.is_some();
            let final_status = if status == HealthStatus::Healthy
                && !*skip_podman_healthcheck
                && !has_http_check
            {
                match check_podman_healthcheck(
                    pid,
                    state.config.username.as_deref(),
                    state.config.user.as_deref(),
                )
                .await
                {
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

    Ok((health_status, exit_code))
}

/// Run a single health check iteration and persist the result (exposed for tests).
pub async fn run_health_check_once(
    vm_id: &str,
    pid: Option<u32>,
    state_dir: PathBuf,
) -> Result<HealthStatus> {
    let state_manager = StateManager::new(state_dir);
    let mut last_failure_log = None;
    let mut skip_podman_healthcheck = false;
    let (status, exit_code) = check_health_once(
        &state_manager,
        vm_id,
        pid,
        &mut last_failure_log,
        &mut skip_podman_healthcheck,
    )
    .await?;
    // Update state file atomically (lock held across read-modify-write)
    state_manager
        .update_health_status(vm_id, status, exit_code)
        .await
        .context("updating health state atomically")?;
    Ok(status)
}

/// Check if HTTP service is responding via nsenter into the network namespace (rootless mode)
///
/// For rootless VMs, we use nsenter to enter the network namespace and curl
/// the guest directly. This bypasses the complexity of pasta port forwarding.
///
/// The holder_pid is the PID of the namespace holder process (sleep infinity).
async fn check_http_health_nsenter(
    holder_pid: u32,
    guest_ip: &str,
    port: u16,
    health_path: &str,
    host_header: Option<&str>,
    timeout_secs: u64,
) -> Result<bool> {
    let url = format!("http://{}:{}{}", guest_ip, port, health_path);

    let start = Instant::now();

    // Use nsenter to enter the namespace and curl the guest directly
    // --preserve-credentials keeps UID/GID mapping
    let nsenter_args = build_nsenter_curl_args(holder_pid, &url, host_header, timeout_secs);

    let output = tokio::process::Command::new("nsenter")
        .args(&nsenter_args)
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

/// Check if HTTP service is responding via `ip netns exec` (routed mode)
///
/// For routed VMs, the guest IP is only reachable from inside the named
/// network namespace. We use `ip netns exec <name> curl ...` to reach it.
async fn check_http_health_netns(
    ns_name: &str,
    guest_ip: &str,
    port: u16,
    health_path: &str,
    host_header: Option<&str>,
    timeout_secs: u64,
) -> Result<bool> {
    let url = format!("http://{}:{}{}", guest_ip, port, health_path);
    let timeout_str = timeout_secs.to_string();
    let start = Instant::now();

    let mut args = vec![
        "ip",
        "netns",
        "exec",
        ns_name,
        "curl",
        "-s",
        "-o",
        "/dev/null",
        "-w",
        "%{http_code}",
        "-m",
        &timeout_str,
        "--noproxy",
        "*",
    ];
    let host_arg;
    if let Some(host) = host_header {
        host_arg = format!("Host: {}", host);
        args.push("-H");
        args.push(&host_arg);
    }
    args.push(&url);

    // ip netns exec requires root (or CAP_SYS_ADMIN).
    // Skip sudo when already running as root (routed mode always runs as root).
    let output = if nix::unistd::getuid().is_root() {
        tokio::process::Command::new(args[0])
            .args(&args[1..])
            .output()
            .await
            .context("failed to run ip netns exec curl")?
    } else {
        tokio::process::Command::new("sudo")
            .args(&args)
            .output()
            .await
            .context("failed to run sudo ip netns exec curl")?
    };

    let elapsed = start.elapsed();

    if output.status.success() {
        let status_code = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if status_code.starts_with('2') || status_code.starts_with('3') {
            debug!(
                target: "health-monitor",
                ns_name = ns_name,
                guest_ip = guest_ip,
                port = port,
                status = %status_code,
                elapsed_ms = elapsed.as_millis(),
                "health check succeeded (netns)"
            );
            Ok(true)
        } else {
            anyhow::bail!(
                "Health check failed with status {} via netns {} to {}:{} ({}ms)",
                status_code,
                ns_name,
                guest_ip,
                port,
                elapsed.as_millis()
            )
        }
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "Failed to connect to {}:{} via netns {}: {}",
            guest_ip,
            port,
            ns_name,
            stderr.trim()
        )
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
async fn check_http_health_bridged(
    url: &str,
    veth_device: Option<&str>,
    host_header: Option<&str>,
    timeout_secs: u64,
) -> Result<bool> {
    // Build a reqwest client, optionally bound to the veth device
    let mut builder = reqwest::Client::builder().timeout(Duration::from_secs(timeout_secs));

    if let Some(veth) = veth_device {
        builder = builder.interface(veth);
    }

    let client = builder.build().context("building reqwest client")?;

    let start = Instant::now();
    let iface_str = veth_device.unwrap_or("default");

    let mut request = client.get(url);
    if let Some(host) = host_header {
        request = request.header("Host", host);
    }

    match request.send().await {
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
                anyhow::bail!(
                    "Health check timed out after {}s via {}",
                    timeout_secs,
                    iface_str
                )
            } else if e.is_connect() {
                anyhow::bail!("Connection refused to {} via {}", url, iface_str)
            } else {
                anyhow::bail!("Failed to connect to {} via {}: {}", url, iface_str, e)
            }
        }
    }
}

/// Build the nsenter + curl argument list for rootless health checks.
///
/// Separated from check_http_health_nsenter for testability.
fn build_nsenter_curl_args(
    holder_pid: u32,
    url: &str,
    host_header: Option<&str>,
    timeout_secs: u64,
) -> Vec<String> {
    let mut curl_args = vec![
        "curl".to_string(),
        "-s".to_string(),
        "-o".to_string(),
        "/dev/null".to_string(),
        "-w".to_string(),
        "%{http_code}".to_string(),
        "--max-time".to_string(),
        timeout_secs.to_string(),
    ];
    // Add Host header if specified (needed for servers that route by Host)
    if let Some(host) = host_header {
        curl_args.push("-H".to_string());
        curl_args.push(format!("Host: {}", host));
    }
    curl_args.push(url.to_string());

    let mut nsenter_args: Vec<String> = vec![
        "-t".to_string(),
        holder_pid.to_string(),
        "-U".to_string(),
        "-n".to_string(),
        "--preserve-credentials".to_string(),
        "--".to_string(),
    ];
    nsenter_args.extend(curl_args);

    nsenter_args
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_nsenter_curl_args_without_host_header() {
        let args = build_nsenter_curl_args(12345, "http://10.0.2.100:80/health", None, 5);
        // Find --max-time and verify "5" immediately follows it
        let max_time_pos = args.iter().position(|a| a == "--max-time").unwrap();
        assert_eq!(
            args[max_time_pos + 1],
            "5",
            "\"5\" must immediately follow \"--max-time\", got {:?}",
            &args[max_time_pos..]
        );
    }

    #[test]
    fn test_nsenter_curl_args_with_host_header() {
        let args =
            build_nsenter_curl_args(12345, "http://10.0.2.100:80/health", Some("myapp.local"), 5);

        // --max-time must be immediately followed by "5"
        let max_time_pos = args.iter().position(|a| a == "--max-time").unwrap();
        assert_eq!(
            args[max_time_pos + 1],
            "5",
            "\"5\" must immediately follow \"--max-time\", but got {:?}",
            &args[max_time_pos..]
        );

        // Host header must be present
        let h_pos = args.iter().position(|a| a == "-H").unwrap();
        assert_eq!(args[h_pos + 1], "Host: myapp.local");

        // URL must be last
        assert_eq!(args.last().unwrap(), "http://10.0.2.100:80/health");
    }
}
