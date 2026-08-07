use anyhow::{bail, Context, Result};
use tracing::{debug, info, warn};

use crate::hypervisor::ProcessSpec;
use crate::network::PastaNetwork;
use crate::state::VmState;

/// Set up rootless namespace for VM networking.
///
/// Spawns a holder process with retry logic, runs network setup via nsenter,
/// and verifies TAP device creation. Records the namespace paths + holder PID into
/// `process_spec` so the VMM is launched into them, and returns the holder child.
pub(super) async fn setup_rootless_namespace(
    pasta_net: &PastaNetwork,
    network_config: &crate::network::NetworkConfig,
    process_spec: &mut ProcessSpec,
    vm_state: &mut VmState,
) -> Result<tokio::process::Child> {
    // Step 1: Spawn holder process (keeps namespace alive)
    // Uses shared function that gives the holder the full retry deadline,
    // only respawning if the holder actually dies (not on timeout).
    let holder_cmd = pasta_net.build_holder_command();
    info!(cmd = ?holder_cmd, "spawning namespace holder for rootless networking");

    let (mut child, mut holder_pid) =
        crate::commands::common::spawn_namespace_holder(&holder_cmd).await?;

    // Take stderr pipe for diagnostics if holder dies during nsenter
    let mut holder_stderr = child.stderr.take();

    // Step 2: Run setup script via nsenter (creates TAPs, iptables, etc.)
    // This is also inside retry logic - if holder dies during nsenter, retry everything
    let setup_script = pasta_net.build_setup_script();
    let nsenter_prefix = pasta_net.build_nsenter_prefix(holder_pid);

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
        script = %setup_script.summary(),
        "network setup script"
    );

    // One nsenter+bash+ip for the whole phase: the script batches every `ip`
    // command (TAP creation, link up, loopback, and the TAP existence check)
    // into a single `ip -batch` process.
    let setup_output = tokio::process::Command::new(&nsenter_prefix[0])
        .args(&nsenter_prefix[1..])
        .arg("bash")
        .arg("-c")
        .arg(setup_script.script())
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
        if !holder_alive {
            let _ = child.kill().await;

            warn!(
                holder_pid = holder_pid,
                nsenter_stderr = %stderr.trim(),
                "holder died during nsenter, spawning new holder..."
            );

            // Use spawn_namespace_holder for the retry — it handles its own
            // deadline and readiness waiting
            let (retry_child, retry_holder_pid) =
                crate::commands::common::spawn_namespace_holder(&holder_cmd).await?;

            // Retry nsenter with new holder
            let retry_nsenter_prefix = pasta_net.build_nsenter_prefix(retry_holder_pid);
            let retry_output = tokio::process::Command::new(&retry_nsenter_prefix[0])
                .args(&retry_nsenter_prefix[1..])
                .arg("bash")
                .arg("-c")
                .arg(setup_script.script())
                .output()
                .await
                .context("running network setup via nsenter (retry)")?;

            if !retry_output.status.success() {
                let retry_stderr = String::from_utf8_lossy(&retry_output.stderr);
                let _ = nix::sys::signal::kill(
                    nix::unistd::Pid::from_raw(retry_holder_pid as i32),
                    nix::sys::signal::Signal::SIGKILL,
                );
                bail!(
                    "network setup failed on retry: {}",
                    setup_script.describe_failure(&retry_stderr)
                );
            }

            // Success on retry - update variables for rest of function
            child = retry_child;
            holder_pid = retry_holder_pid;
            info!(
                holder_pid = holder_pid,
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
                    "network setup failed: holder died during nsenter. \
                     nsenter_stderr='{}', holder_stderr='{}', \
                     (tun={}, ns_user={}, ns_net={})",
                    setup_script.describe_failure(&stderr),
                    holder_stderr_content.trim(),
                    tun_exists,
                    ns_user_exists,
                    ns_net_exists
                );
            } else {
                bail!(
                    "network setup failed: {} (tun={}, holder_alive={}, ns_user={}, ns_net={})",
                    setup_script.describe_failure(&stderr),
                    tun_exists,
                    holder_alive,
                    ns_user_exists,
                    ns_net_exists
                );
            }
        }
    }

    // The TAP existence check is the last step of the batched setup script, so a
    // successful run already proves the device is there — no second nsenter+ip.
    debug!(tap_device = %network_config.tap_device, "TAP device verified");
    info!(holder_pid = holder_pid, "network setup complete");

    // Use pre_exec setns path (not nsenter) for rootless baselines.
    // nsenter enters the user namespace internally, which clears PR_SET_PDEATHSIG
    // (kernel zeros task->pdeath_signal on credential changes). The pre_exec setns
    // path sets pdeathsig AFTER entering the user namespace, so Firecracker gets
    // SIGKILL when fcvm dies.
    process_spec.user_namespace_path = Some(std::path::PathBuf::from(format!(
        "/proc/{}/ns/user",
        holder_pid
    )));
    process_spec.net_namespace_path = Some(std::path::PathBuf::from(format!(
        "/proc/{}/ns/net",
        holder_pid
    )));
    // Still track holder_pid for health checks (nsenter curl) and cleanup
    process_spec.holder_pid = Some(holder_pid);

    // Store holder_pid in state for health checks and cleanup
    vm_state.holder_pid = Some(holder_pid);

    Ok(child)
}
