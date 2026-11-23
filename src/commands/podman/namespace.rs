use anyhow::{bail, Result};
use tracing::{debug, info, warn};

use crate::hypervisor::ProcessSpec;
use crate::network::PastaNetwork;
use crate::state::VmState;

/// Set up rootless namespace for VM networking.
///
/// Spawns the namespace holder and has it build the VM's links
/// (`PastaNetwork::build_namespace`). Records the namespace paths + holder PID into
/// `process_spec` so the VMM is launched into them, and returns the holder child.
pub(crate) async fn setup_rootless_namespace(
    pasta_net: &PastaNetwork,
    network_config: &crate::network::NetworkConfig,
    process_spec: &mut ProcessSpec,
    vm_state: &mut VmState,
) -> Result<tokio::process::Child> {
    info!("spawning namespace holder for rootless networking");
    let (mut child, mut holder_pid) = crate::commands::common::spawn_namespace_holder().await?;

    // The holder makes the TAP through /dev/net/tun.
    let tun_exists = std::path::Path::new("/dev/net/tun").exists();
    if !tun_exists {
        warn!("/dev/net/tun not available - TAP device creation will fail");
    }

    info!(holder_pid = holder_pid, "building the namespace's links");
    if let Err(error) = pasta_net.build_namespace(holder_pid).await {
        // A holder that died under the build is replaced once; any other failure is the
        // build's own and a second holder would fail the same way.
        let died = match child.try_wait() {
            Ok(status) => status,
            Err(wait_error) => {
                warn!(holder_pid, error = %wait_error, "could not check the namespace holder");
                None
            }
        };
        let Some(status) = died else {
            let _ = child.kill().await;
            crate::companion::forget(holder_pid);
            crate::network::netns_agent::forget(holder_pid);
            bail!("network setup failed: {error:#} (tun={tun_exists})");
        };
        let report =
            crate::companion::describe_registered("namespace holder", Some(holder_pid), Ok(status))
                .await;
        crate::network::netns_agent::forget(holder_pid);
        warn!(
            holder_pid = holder_pid,
            error = %format!("{error:#}"),
            report = %report,
            "holder died during network setup, spawning new holder..."
        );
        let (mut retry_child, retry_holder_pid) =
            crate::commands::common::spawn_namespace_holder().await?;
        if let Err(retry_error) = pasta_net.build_namespace(retry_holder_pid).await {
            let _ = retry_child.kill().await;
            crate::companion::forget(retry_holder_pid);
            crate::network::netns_agent::forget(retry_holder_pid);
            bail!(
                "network setup failed on retry: {retry_error:#} (the first holder: {report}; \
                 tun={tun_exists})"
            );
        }
        child = retry_child;
        holder_pid = retry_holder_pid;
        info!(
            holder_pid = holder_pid,
            "network setup succeeded after retry"
        );
    }

    debug!(tap_device = %network_config.tap_device, "TAP device built");
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
    // Still track holder_pid for health checks and cleanup
    process_spec.holder_pid = Some(holder_pid);

    // Store holder_pid in state for health checks and cleanup
    vm_state.holder_pid = Some(holder_pid);

    Ok(child)
}
