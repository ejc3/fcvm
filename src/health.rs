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
/// With a health check URL the task probes it over HTTP, and `check_health_once` says
/// where each network mode connects. Without one, health is the container's running
/// state and its podman healthcheck.
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

/// Whether a failed `fcvm exec ... podman inspect` means the container does
/// not exist yet, which is expected while the VM starts.
///
/// podman exits 125 for a missing container. `fcvm exec` exits 125 too when it
/// fails itself (no connection, a refused request), and that must stay a
/// warning, so the exit code alone does not decide it: podman's message does.
fn container_not_created_yet(code: i32, stderr: &str) -> bool {
    code == 125 && (stderr.contains("no such container") || stderr.contains("no such object"))
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
        if container_not_created_yet(code, &stderr) {
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
        if container_not_created_yet(code, &stderr) {
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
                    let (port, health_path) = probe_port_and_path(&url);
                    let net = &state.config.network;

                    // The URL's hostname is the virtual host the server routes by, and it
                    // only travels as the Host header. Where the probe connects depends
                    // on the network mode:
                    // - Rootless: the guest address, from inside the holder's namespaces.
                    // - Routed: the guest address, from inside the VM's named namespace.
                    // - Bridged: the VM's own veth address, from the host
                    //   (`bridged::host_reachable_ip`).
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
                        // Bridged mode: probe the VM's reachable address
                        // (`bridged::host_reachable_ip`) through its own veth. Not
                        // `guest_ip` over the host's route to it: every VM restored from
                        // one snapshot has the same one, and that route belongs to
                        // whichever of them set up last (#948).
                        let veth_device = net.host_veth.as_deref();
                        // A state that names no probe target fails the check the way an
                        // unanswered probe does: it reads unhealthy and is persisted, so
                        // a status an earlier check wrote cannot outlive it.
                        let probed = async {
                            let effective_url = bridged_probe_url(net, &url)?;
                            debug!(target: "health-monitor", original_url = %url_str, effective_url = %effective_url, veth = ?veth_device, "HTTP health check via veth");
                            check_http_health_bridged(
                                &effective_url,
                                veth_device,
                                url_host,
                                health_timeout,
                            )
                            .await
                        }
                        .await;

                        match probed {
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
        anyhow::bail!(
            "{}",
            curl_probe_failure(
                &format!("{}:{}", guest_ip, port),
                "nsenter",
                output.status.code(),
                &String::from_utf8_lossy(&output.stderr),
            )
        )
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
    let start = Instant::now();
    let args = build_netns_curl_args(ns_name, &url, host_header, timeout_secs);

    // ip netns exec requires root (or CAP_SYS_ADMIN).
    // Skip sudo when already running as root (routed mode always runs as root).
    let output = if nix::unistd::getuid().is_root() {
        tokio::process::Command::new(&args[0])
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
        anyhow::bail!(
            "{}",
            curl_probe_failure(
                &format!("{}:{}", guest_ip, port),
                &format!("netns {}", ns_name),
                output.status.code(),
                &String::from_utf8_lossy(&output.stderr),
            )
        )
    }
}

/// Check if HTTP service is responding using reqwest with optional interface binding (bridged mode)
///
/// `url` names the VM's reachable address (`bridged_probe_url`), which sits on the /30 of
/// the VM's veth. When a veth device is given the client binds to it (SO_BINDTODEVICE
/// through reqwest's `.interface()`), so the request leaves through that VM's veth whatever
/// else the host's routing table holds. A baseline depends on that: its reachable address
/// is its guest address, which the host routes to the newest VM restored from its snapshot.
async fn check_http_health_bridged(
    url: &str,
    veth_device: Option<&str>,
    host_header: Option<&str>,
    timeout_secs: u64,
) -> Result<bool> {
    // Build a reqwest client, optionally bound to the veth device
    // `no_proxy`: reqwest would otherwise take `http_proxy` from the environment and
    // send the guest's health check to it.
    let mut builder = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(timeout_secs));

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
            // reqwest's own message stops at "error sending request". What the operating
            // system said (refused, unreachable, no such device) is further down the chain.
            let cause = error_chain(&e);
            if e.is_timeout() {
                anyhow::bail!(
                    "Health check timed out after {}s via {}: {}",
                    timeout_secs,
                    iface_str,
                    cause
                )
            } else {
                anyhow::bail!(
                    "Health check request to {} via {} failed: {}",
                    url,
                    iface_str,
                    cause
                )
            }
        }
    }
}

/// An error's message followed by the message of every cause under it.
fn error_chain(error: &dyn std::error::Error) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        let cause_text = cause.to_string();
        // Some errors already quote their cause in their own message.
        if !text.ends_with(&cause_text) {
            text.push_str(": ");
            text.push_str(&cause_text);
        }
        source = cause.source();
    }
    text
}

/// The port and the path, query included, that a health check URL asks for.
///
/// The probe is plain HTTP in every mode, so the URL's scheme only picks the port when the
/// URL names none. A fragment is not part of a request.
fn probe_port_and_path(url: &url::Url) -> (u16, &str) {
    (
        url.port_or_known_default().unwrap_or(80),
        &url[url::Position::BeforePath..url::Position::AfterQuery],
    )
}

/// The URL the bridged probe requests: the health check's port, path and query on the
/// VM's reachable address (`bridged::host_reachable_ip`).
///
/// The probe is plain HTTP in every mode and connects to an address of the VM's own, never
/// to the health check URL's host. That host is only the `Host` header, which is what
/// `--health-check` documents.
fn bridged_probe_url(net: &crate::network::NetworkConfig, url: &url::Url) -> Result<String> {
    let ip = crate::network::bridged::host_reachable_ip(net).with_context(|| {
        format!(
            "bridged VM state has no usable host veth address ({:?}), so its health probe has no target",
            net.host_ip
        )
    })?;
    let (port, path) = probe_port_and_path(url);
    Ok(format!("http://{}:{}{}", ip, port, path))
}

/// What a failed curl probe reports: what the probe command printed, and its exit status,
/// which still says something when nothing was printed.
///
/// The status belongs to the outermost command (`nsenter`, `ip netns exec` or `sudo`). It
/// is curl's when curl ran, and the wrapper's own when the wrapper could not start curl, so
/// the message does not call it curl's.
fn curl_probe_failure(target: &str, via: &str, exit_code: Option<i32>, stderr: &str) -> String {
    let status = match exit_code {
        Some(code) => format!("probe command exit {}", code),
        None => "probe command was killed by a signal".to_string(),
    };
    match stderr.trim() {
        "" => format!(
            "Health check of {} via {} failed: {}, and it printed nothing",
            target, via, status
        ),
        said => format!(
            "Health check of {} via {} failed: {}: {}",
            target, via, status, said
        ),
    }
}

/// The curl that the rootless and routed probes run inside the VM's namespace.
fn curl_probe_args(url: &str, host_header: Option<&str>, timeout_secs: u64) -> Vec<String> {
    let mut args = vec![
        "curl".to_string(),
        // `-s` alone silences curl's error message along with its progress meter, and a
        // failed probe then has nothing to report. `-S` keeps the message.
        "-sS".to_string(),
        "-o".to_string(),
        "/dev/null".to_string(),
        "-w".to_string(),
        "%{http_code}".to_string(),
        "--max-time".to_string(),
        timeout_secs.to_string(),
        // This curl inherits fcvm's environment. A guest address is never behind a proxy:
        // with `http_proxy` set curl would ask the proxy for an address that only this
        // namespace can reach, and the VM would never read healthy.
        "--noproxy".to_string(),
        "*".to_string(),
    ];
    // Servers that route by virtual host need the health check URL's hostname.
    if let Some(host) = host_header {
        args.push("-H".to_string());
        args.push(format!("Host: {}", host));
    }
    args.push(url.to_string());
    args
}

/// The routed probe's command line: `ip netns exec <namespace>` in front of the shared curl.
fn build_netns_curl_args(
    ns_name: &str,
    url: &str,
    host_header: Option<&str>,
    timeout_secs: u64,
) -> Vec<String> {
    let mut args: Vec<String> = ["ip", "netns", "exec", ns_name]
        .iter()
        .map(|arg| arg.to_string())
        .collect();
    args.extend(curl_probe_args(url, host_header, timeout_secs));
    args
}

/// The rootless probe's arguments to `nsenter`: the holder's user and network namespaces in
/// front of the shared curl. `--preserve-credentials` keeps the UID/GID mapping.
fn build_nsenter_curl_args(
    holder_pid: u32,
    url: &str,
    host_header: Option<&str>,
    timeout_secs: u64,
) -> Vec<String> {
    let mut args = vec![
        "-t".to_string(),
        holder_pid.to_string(),
        "-U".to_string(),
        "-n".to_string(),
        "--preserve-credentials".to_string(),
        "--".to_string(),
    ];
    args.extend(curl_probe_args(url, host_header, timeout_secs));
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    /// podman exits 125 when the container is missing, and `fcvm exec` exits
    /// 125 when fcvm itself fails. Only the first is an expected startup state;
    /// the second must stay loud, or a VM whose exec can never work looks like
    /// one that is still starting.
    #[test]
    fn only_podmans_missing_container_counts_as_not_created_yet() {
        for stderr in [
            "Error: no such container fcvm-container",
            "Error: no such object: \"fcvm-container\"",
        ] {
            assert!(container_not_created_yet(125, stderr), "{stderr}");
        }
        for stderr in [
            "ERROR fcvm::commands::exec: Error: fc-agent rejected the exec request: this VM's fc-agent speaks an older exec protocol",
            "ERROR fcvm::commands::exec: Error: exec request was never acknowledged after 3 attempts",
            "ERROR fcvm::commands::exec: Error: connecting to exec socket: No such file or directory (os error 2)",
            "",
        ] {
            assert!(!container_not_created_yet(125, stderr), "{stderr}");
        }
        assert!(!container_not_created_yet(
            1,
            "Error: no such container fcvm-container"
        ));
    }

    /// The curl that the rootless and routed probes run.
    ///
    /// - `-sS` is quiet and still prints curl's error, which is what a failed probe reports.
    ///   A bare `-s` silences the error along with the progress meter.
    /// - `--max-time` carries the health check timeout.
    /// - `--noproxy '*'`: fcvm's environment reaches this curl, and a host that needs
    ///   `http_proxy` to pull images has it set. Without the flag curl asks that proxy for
    ///   the guest's address from inside the VM's namespace, and a `--health-check` VM never
    ///   reads healthy. Measured on one command: never healthy in 240 s with `http_proxy`
    ///   exported, healthy in 4 s once the guest address was in `no_proxy`.
    /// - The `Host` header is sent only when there is one to send, and the URL comes last.
    #[test]
    fn curl_probe_args_are_quiet_bounded_and_ask_no_proxy() {
        assert_eq!(
            curl_probe_args("http://10.0.2.100:80/health", Some("myapp.local"), 7),
            [
                "curl",
                "-sS",
                "-o",
                "/dev/null",
                "-w",
                "%{http_code}",
                "--max-time",
                "7",
                "--noproxy",
                "*",
                "-H",
                "Host: myapp.local",
                "http://10.0.2.100:80/health",
            ]
        );
        assert_eq!(
            curl_probe_args("http://10.0.2.100:80/health", None, 5),
            [
                "curl",
                "-sS",
                "-o",
                "/dev/null",
                "-w",
                "%{http_code}",
                "--max-time",
                "5",
                "--noproxy",
                "*",
                "http://10.0.2.100:80/health",
            ]
        );
    }

    /// The rootless probe runs the shared curl in the holder's user and network namespaces.
    #[test]
    fn nsenter_probe_wraps_the_shared_curl() {
        let args =
            build_nsenter_curl_args(12345, "http://10.0.2.100:80/health", Some("myapp.local"), 5);
        let (wrapper, curl) = args.split_at(6);
        assert_eq!(
            wrapper,
            ["-t", "12345", "-U", "-n", "--preserve-credentials", "--"]
        );
        assert_eq!(
            curl,
            curl_probe_args("http://10.0.2.100:80/health", Some("myapp.local"), 5)
        );
    }

    /// The routed probe runs the shared curl behind `ip netns exec <namespace>`.
    #[test]
    fn netns_probe_wraps_the_shared_curl() {
        let args = build_netns_curl_args("fcvm-vm-abc12", "http://10.0.2.100:80/health", None, 7);
        let (wrapper, curl) = args.split_at(4);
        assert_eq!(wrapper, ["ip", "netns", "exec", "fcvm-vm-abc12"]);
        assert_eq!(
            curl,
            curl_probe_args("http://10.0.2.100:80/health", None, 7)
        );
    }

    /// A failed bridged probe reports the error the operating system gave. A probe with no
    /// route to its target, or whose veth is gone, must not read as a guest that refused
    /// (#948). Binding to a device that does not exist is a connect error that is not a
    /// refusal, and needs no privilege to provoke.
    #[tokio::test]
    async fn bridged_probe_reports_the_connect_error_it_got() {
        let error =
            check_http_health_bridged("http://127.0.0.1:9/", Some("fcvm-no-such0"), None, 2)
                .await
                .expect_err("a probe bound to a missing device cannot succeed");
        let text = format!("{error:#}");
        assert!(
            !text.contains("Connection refused"),
            "nothing refused this connection: {text}"
        );
        assert!(
            text.contains("No such device"),
            "the operating system's error is missing: {text}"
        );
    }

    /// A failed curl probe reports what the probe command printed and its exit status, so
    /// the message never ends at a colon. The status is the outermost command's
    /// (`curl_probe_failure`): the message does not call it curl's, and a wrapper that could
    /// not start curl reads the same way.
    #[test]
    fn a_failed_curl_probe_reports_the_commands_output_and_exit_status() {
        let said = curl_probe_failure(
            "10.0.2.100:80",
            "nsenter",
            Some(7),
            "curl: (7) Failed to connect to 10.0.2.100 port 80 after 0 ms: Couldn't connect to server\n",
        );
        assert_eq!(
            said,
            "Health check of 10.0.2.100:80 via nsenter failed: probe command exit 7: curl: (7) \
             Failed to connect to 10.0.2.100 port 80 after 0 ms: Couldn't connect to server"
        );

        let wrapper = curl_probe_failure(
            "10.0.2.100:80",
            "nsenter",
            Some(1),
            "nsenter: cannot open /proc/12345/ns/user: No such file or directory\n",
        );
        assert_eq!(
            wrapper,
            "Health check of 10.0.2.100:80 via nsenter failed: probe command exit 1: nsenter: \
             cannot open /proc/12345/ns/user: No such file or directory"
        );

        let silent = curl_probe_failure("10.0.2.100:80", "netns fcvm-vm-abc12", Some(28), " \n");
        assert_eq!(
            silent,
            "Health check of 10.0.2.100:80 via netns fcvm-vm-abc12 failed: probe command exit \
             28, and it printed nothing"
        );

        let killed = curl_probe_failure("10.0.2.100:80", "nsenter", None, "");
        assert!(killed.contains("killed by a signal"), "{killed}");
    }

    /// Where the bridged probe connects. The guest address is no VM's own once two VMs are
    /// restored from one snapshot (#948), and the URL's host is only ever a `Host` header.
    #[test]
    fn bridged_probe_url_names_the_vms_own_address() {
        let net = |host_ip: Option<&str>| crate::network::NetworkConfig {
            guest_ip: Some("172.30.135.98".to_string()),
            host_ip: host_ip.map(str::to_string),
            ..Default::default()
        };
        let url = |text: &str| url::Url::parse(text).unwrap();

        // Two restored VMs with one guest address get two different probe targets.
        assert_eq!(
            bridged_probe_url(
                &net(Some("10.73.23.93")),
                &url("http://localhost:8080/ready")
            )
            .unwrap(),
            "http://10.73.23.94:8080/ready"
        );
        assert_eq!(
            bridged_probe_url(
                &net(Some("10.147.11.45")),
                &url("http://localhost:8080/ready")
            )
            .unwrap(),
            "http://10.147.11.46:8080/ready"
        );
        // A named host does not move the connection, and the port defaults to 80.
        assert_eq!(
            bridged_probe_url(
                &net(Some("172.30.0.5")),
                &url("http://myapp.example.com/status")
            )
            .unwrap(),
            "http://172.30.0.6:80/status"
        );
        // No fallback to the shared guest address when the state cannot say.
        let error = bridged_probe_url(&net(None), &url("http://localhost/")).unwrap_err();
        assert!(
            error.to_string().contains("no usable host veth address"),
            "{error}"
        );
    }

    /// The probe asks for what the health check URL asks for: its port, or the scheme's
    /// default when it names none, and its path with the query. A fragment never reaches a
    /// server.
    #[test]
    fn bridged_probe_url_keeps_the_query_and_the_schemes_default_port() {
        let net = crate::network::NetworkConfig {
            host_ip: Some("10.73.23.93".to_string()),
            ..Default::default()
        };
        let probed: Vec<String> = [
            "http://localhost:8080/ready?full=1&source=fcvm",
            "https://myapp.example.com/status",
            "http://localhost/health?x=1#top",
        ]
        .iter()
        .map(|text| bridged_probe_url(&net, &url::Url::parse(text).unwrap()).unwrap())
        .collect();
        assert_eq!(
            probed,
            [
                "http://10.73.23.94:8080/ready?full=1&source=fcvm",
                "http://10.73.23.94:443/status",
                "http://10.73.23.94:80/health?x=1",
            ]
        );
    }

    const PROXY_PROBE_CHILD: &str = "FCVM_TEST_HEALTH_PROXY_PROBE_CHILD";

    /// The bridged probe is a reqwest client, and reqwest takes `http_proxy` from the
    /// environment unless told not to. A guest address is never behind a proxy, so a
    /// host that exports one for image pulls would send every health check to it.
    ///
    /// The variable is set on a re-executed child, per `crate::test_env`. The parent
    /// plays a guest that answers 200 and a proxy that answers 502, and counts who
    /// was asked.
    #[test]
    fn bridged_probe_never_asks_a_proxy_for_the_guest() {
        use std::io::{Read, Write};
        if let Some(target) = std::env::var_os(PROXY_PROBE_CHILD) {
            let target = target.to_string_lossy().to_string();
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("child runtime");
            let healthy = runtime.block_on(check_http_health_bridged(&target, None, None, 5));
            assert!(
                matches!(healthy, Ok(true)),
                "probe of {target} returned {healthy:?}"
            );
            return;
        }

        let guest = std::net::TcpListener::bind("127.0.0.1:0").expect("guest listener");
        let proxy = std::net::TcpListener::bind("127.0.0.1:0").expect("proxy listener");
        guest.set_nonblocking(true).expect("nonblocking guest");
        proxy.set_nonblocking(true).expect("nonblocking proxy");
        let proxy_url = format!("http://{}", proxy.local_addr().unwrap());
        let mut child = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .args([
                "--exact",
                "health::tests::bridged_probe_never_asks_a_proxy_for_the_guest",
                "--nocapture",
            ])
            .env(
                PROXY_PROBE_CHILD,
                format!("http://{}/", guest.local_addr().unwrap()),
            )
            .env("http_proxy", &proxy_url)
            .env("HTTP_PROXY", &proxy_url)
            .env_remove("no_proxy")
            .env_remove("NO_PROXY")
            .spawn()
            .expect("spawning the probe child");

        let answer = |mut stream: std::net::TcpStream, status: &str| {
            stream.set_nonblocking(false).expect("blocking stream");
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .expect("read timeout");
            let mut request = [0u8; 1024];
            let _ = stream.read(&mut request);
            let _ = write!(
                stream,
                "HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
        };
        let (mut guest_hits, mut proxy_hits) = (0, 0);
        let deadline = Instant::now() + Duration::from_secs(30);
        let status = loop {
            if let Ok((stream, _)) = guest.accept() {
                guest_hits += 1;
                answer(stream, "200 OK");
            }
            if let Ok((stream, _)) = proxy.accept() {
                proxy_hits += 1;
                answer(stream, "502 Bad Gateway");
            }
            if let Some(status) = child.try_wait().expect("polling the probe child") {
                break status;
            }
            assert!(Instant::now() < deadline, "the probe child never exited");
            std::thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(
            proxy_hits, 0,
            "the health probe asked http_proxy for the guest ({guest_hits} direct requests)"
        );
        assert!(status.success(), "the probe child failed: {status}");
        assert_eq!(guest_hits, 1);
    }
}
