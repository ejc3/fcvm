use anyhow::{bail, Context, Result};
use tracing::{debug, info, warn};

use crate::firecracker::VmManager;
use crate::network::SlirpNetwork;
use crate::state::VmState;

/// Set up rootless namespace for VM networking.
///
/// Spawns a holder process with retry logic, runs network setup via nsenter,
/// and verifies TAP device creation. Returns the holder child process and PID.
pub(super) async fn setup_rootless_namespace(
    slirp_net: &SlirpNetwork,
    network_config: &crate::network::NetworkConfig,
    vm_manager: &mut VmManager,
    vm_state: &mut VmState,
) -> Result<tokio::process::Child> {
    // Step 1: Spawn holder process (keeps namespace alive)
    // Retry for up to 5 seconds if holder dies (transient failures under load)
    let holder_cmd = slirp_net.build_holder_command();
    info!(cmd = ?holder_cmd, "spawning namespace holder for rootless networking");

    let retry_deadline = std::time::Instant::now() + crate::commands::common::HOLDER_RETRY_TIMEOUT;
    let mut attempt = 0;

    let (mut child, mut holder_pid, mut holder_stderr) = loop {
        attempt += 1;

        // Spawn holder with piped stderr to capture errors if it fails
        let mut child = tokio::process::Command::new(&holder_cmd[0])
            .args(&holder_cmd[1..])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .with_context(|| format!("failed to spawn holder: {:?}", holder_cmd))?;

        let holder_pid = child.id().context("getting holder process PID")?;
        if attempt > 1 {
            info!(
                holder_pid = holder_pid,
                attempt = attempt,
                "namespace holder started (retry)"
            );
        } else {
            info!(holder_pid = holder_pid, "namespace holder started");
        }

        // Wait for namespace to be ready by checking uid_map
        let namespace_ready = crate::utils::wait_for_namespace_ready(
            holder_pid,
            crate::commands::common::NAMESPACE_READY_TIMEOUT,
        )
        .await;

        // If namespace didn't become ready, kill holder and retry
        if !namespace_ready {
            let _ = child.kill().await;

            if std::time::Instant::now() < retry_deadline {
                warn!(
                    holder_pid = holder_pid,
                    attempt = attempt,
                    "namespace not ready, retrying holder creation..."
                );
                tokio::time::sleep(crate::commands::common::HOLDER_RETRY_INTERVAL).await;
                continue;
            } else {
                bail!(
                    "namespace not ready after {} attempts (holder PID {})",
                    attempt,
                    holder_pid
                );
            }
        }

        // Take stderr pipe - we'll use it for diagnostics if holder dies later
        let mut holder_stderr = child.stderr.take();

        match child.try_wait() {
            Ok(Some(status)) => {
                // Holder exited - capture stderr to see why
                let stderr = if let Some(ref mut pipe) = holder_stderr {
                    use tokio::io::AsyncReadExt;
                    let mut buf = String::new();
                    let _ = pipe.read_to_string(&mut buf).await;
                    buf
                } else {
                    String::new()
                };

                if std::time::Instant::now() < retry_deadline {
                    warn!(
                        holder_pid = holder_pid,
                        attempt = attempt,
                        status = %status,
                        stderr = %stderr.trim(),
                        "holder died, retrying..."
                    );
                    tokio::time::sleep(crate::commands::common::HOLDER_RETRY_INTERVAL).await;
                    continue;
                } else {
                    bail!(
                        "holder process exited immediately after {} attempts: status={}, stderr={}, cmd={:?}",
                        attempt,
                        status,
                        stderr.trim(),
                        holder_cmd
                    );
                }
            }
            Ok(None) => {
                debug!(holder_pid = holder_pid, "holder running");
            }
            Err(e) => {
                warn!(holder_pid = holder_pid, error = ?e, "failed to check holder status");
            }
        }

        // Check if holder is still alive before proceeding
        if !crate::utils::is_process_alive(holder_pid) {
            // Try to capture stderr from the dead holder process
            let holder_stderr_content = if let Some(ref mut pipe) = holder_stderr {
                use tokio::io::AsyncReadExt;
                let mut buf = String::new();
                match tokio::time::timeout(
                    std::time::Duration::from_millis(100),
                    pipe.read_to_string(&mut buf),
                )
                .await
                {
                    Ok(Ok(_)) => buf,
                    _ => String::new(),
                }
            } else {
                String::new()
            };

            let _ = child.kill().await;

            if std::time::Instant::now() < retry_deadline {
                warn!(
                    holder_pid = holder_pid,
                    attempt = attempt,
                    holder_stderr = %holder_stderr_content.trim(),
                    "holder died after initial check, retrying..."
                );
                tokio::time::sleep(crate::commands::common::HOLDER_RETRY_INTERVAL).await;
                continue;
            } else {
                let max_user_ns = std::fs::read_to_string("/proc/sys/user/max_user_namespaces")
                    .unwrap_or_else(|_| "unknown".to_string());
                bail!(
                    "holder process (PID {}) died after {} attempts. \
                     stderr='{}', max_user_namespaces={}. \
                     This may indicate resource exhaustion or namespace limit reached.",
                    holder_pid,
                    attempt,
                    holder_stderr_content.trim(),
                    max_user_ns.trim()
                );
            }
        }

        // Holder is alive - break out of retry loop
        break (child, holder_pid, holder_stderr);
    };

    // Step 2: Run setup script via nsenter (creates TAPs, iptables, etc.)
    // This is also inside retry logic - if holder dies during nsenter, retry everything
    let setup_script = slirp_net.build_setup_script();
    let mut nsenter_prefix = slirp_net.build_nsenter_prefix(holder_pid);

    // Debug: Check if holder is still alive and namespace files exist
    let proc_dir = format!("/proc/{}", holder_pid);
    let ns_user = format!("/proc/{}/ns/user", holder_pid);
    let ns_net = format!("/proc/{}/ns/net", holder_pid);
    debug!(
        holder_pid = holder_pid,
        proc_exists = std::path::Path::new(&proc_dir).exists(),
        ns_user_exists = std::path::Path::new(&ns_user).exists(),
        ns_net_exists = std::path::Path::new(&ns_net).exists(),
        "checking holder process before nsenter"
    );

    // Check for required devices before attempting network setup
    let tun_exists = std::path::Path::new("/dev/net/tun").exists();
    debug!(
        holder_pid = holder_pid,
        tun_exists = tun_exists,
        "checking /dev/net/tun availability"
    );
    if !tun_exists {
        warn!("/dev/net/tun not available - TAP device creation will fail");
    }

    info!(holder_pid = holder_pid, "running network setup via nsenter");

    // Log the setup script for debugging
    debug!(
        holder_pid = holder_pid,
        script = %setup_script.lines().filter(|l| !l.trim().is_empty() && !l.trim().starts_with('#')).collect::<Vec<_>>().join("; "),
        "network setup script"
    );

    let setup_output = tokio::process::Command::new(&nsenter_prefix[0])
        .args(&nsenter_prefix[1..])
        .arg("bash")
        .arg("-c")
        .arg(&setup_script)
        .output()
        .await
        .context("running network setup via nsenter")?;

    if !setup_output.status.success() {
        let stderr = String::from_utf8_lossy(&setup_output.stderr);
        let stdout = String::from_utf8_lossy(&setup_output.stdout);

        // Re-check state for diagnostics
        let holder_alive = std::path::Path::new(&proc_dir).exists();
        let ns_user_exists = std::path::Path::new(&ns_user).exists();
        let ns_net_exists = std::path::Path::new(&ns_net).exists();

        // If holder died during nsenter, this is a retryable error
        if !holder_alive && std::time::Instant::now() < retry_deadline {
            // Holder died during nsenter - retry the whole thing
            let holder_stderr_content = if let Some(ref mut pipe) = holder_stderr {
                use tokio::io::AsyncReadExt;
                let mut buf = String::new();
                match tokio::time::timeout(
                    std::time::Duration::from_millis(100),
                    pipe.read_to_string(&mut buf),
                )
                .await
                {
                    Ok(Ok(_)) => buf,
                    _ => String::new(),
                }
            } else {
                String::new()
            };

            let _ = child.kill().await;

            warn!(
                holder_pid = holder_pid,
                attempt = attempt,
                holder_stderr = %holder_stderr_content.trim(),
                nsenter_stderr = %stderr.trim(),
                "holder died during nsenter, retrying..."
            );

            // Jump back to the retry loop by recursing into this block
            // We need to restructure - for now just retry once more inline
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;

            // Retry: spawn new holder
            attempt += 1;
            let mut retry_child = tokio::process::Command::new(&holder_cmd[0])
                .args(&holder_cmd[1..])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .with_context(|| format!("failed to spawn holder on retry: {:?}", holder_cmd))?;

            let retry_holder_pid = retry_child.id().context("getting retry holder PID")?;
            info!(
                holder_pid = retry_holder_pid,
                attempt = attempt,
                "namespace holder started (retry after nsenter failure)"
            );

            tokio::time::sleep(std::time::Duration::from_millis(100)).await;

            if !crate::utils::is_process_alive(retry_holder_pid) {
                let _ = retry_child.kill().await;
                bail!(
                    "holder died on retry after nsenter failure (attempt {})",
                    attempt
                );
            }

            // Retry nsenter with new holder
            let retry_nsenter_prefix = slirp_net.build_nsenter_prefix(retry_holder_pid);
            let retry_output = tokio::process::Command::new(&retry_nsenter_prefix[0])
                .args(&retry_nsenter_prefix[1..])
                .arg("bash")
                .arg("-c")
                .arg(&setup_script)
                .output()
                .await
                .context("running network setup via nsenter (retry)")?;

            if !retry_output.status.success() {
                let retry_stderr = String::from_utf8_lossy(&retry_output.stderr);
                let _ = retry_child.kill().await;
                bail!(
                    "network setup failed on retry: {} (attempt {})",
                    retry_stderr.trim(),
                    attempt
                );
            }

            // Success on retry - update variables for rest of function
            child = retry_child;
            holder_pid = retry_holder_pid;
            nsenter_prefix = slirp_net.build_nsenter_prefix(holder_pid);
            info!(
                holder_pid = holder_pid,
                attempts = attempt,
                "network setup succeeded after retry"
            );
        } else {
            // If holder died, try to capture its stderr for more context
            let holder_stderr_content = if !holder_alive {
                if let Some(ref mut pipe) = holder_stderr {
                    use tokio::io::AsyncReadExt;
                    let mut buf = String::new();
                    match tokio::time::timeout(
                        std::time::Duration::from_millis(100),
                        pipe.read_to_string(&mut buf),
                    )
                    .await
                    {
                        Ok(Ok(_)) => buf,
                        _ => String::new(),
                    }
                } else {
                    String::new()
                }
            } else {
                String::new()
            };

            // Kill holder before bailing
            let _ = child.kill().await;

            // Log comprehensive error info at ERROR level (always visible)
            warn!(
                holder_pid = holder_pid,
                holder_alive = holder_alive,
                holder_stderr = %holder_stderr_content.trim(),
                tun_exists = tun_exists,
                ns_user_exists = ns_user_exists,
                ns_net_exists = ns_net_exists,
                nsenter_stderr = %stderr.trim(),
                nsenter_stdout = %stdout.trim(),
                "network setup failed - diagnostics"
            );

            if !holder_alive {
                bail!(
                    "network setup failed: holder died during nsenter after {} attempts. \
                     nsenter_stderr='{}', holder_stderr='{}', \
                     (tun={}, ns_user={}, ns_net={})",
                    attempt,
                    stderr.trim(),
                    holder_stderr_content.trim(),
                    tun_exists,
                    ns_user_exists,
                    ns_net_exists
                );
            } else {
                bail!(
                    "network setup failed: {} (tun={}, holder_alive={}, ns_user={}, ns_net={})",
                    stderr.trim(),
                    tun_exists,
                    holder_alive,
                    ns_user_exists,
                    ns_net_exists
                );
            }
        }
    }

    if attempt > 1 {
        info!(
            holder_pid = holder_pid,
            attempts = attempt,
            "namespace setup succeeded after retries"
        );
    }

    info!(holder_pid = holder_pid, "network setup complete");

    // Verify TAP device was created successfully
    let tap_device = &network_config.tap_device;
    let verify_output = tokio::process::Command::new(&nsenter_prefix[0])
        .args(&nsenter_prefix[1..])
        .arg("ip")
        .arg("link")
        .arg("show")
        .arg(tap_device)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .context("verifying TAP device")?;

    if !verify_output.success() {
        let _ = child.kill().await;
        bail!(
            "TAP device '{}' not found after network setup - setup may have failed silently",
            tap_device
        );
    }
    debug!(tap_device = %tap_device, "TAP device verified");

    // Set holder_pid so VmManager uses nsenter
    vm_manager.set_holder_pid(holder_pid);

    // Store holder_pid in state for health checks and cleanup
    vm_state.holder_pid = Some(holder_pid);

    Ok(child)
}
