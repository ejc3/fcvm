//! Common utilities for VM lifecycle management
//!
//! This module contains shared functions used by both baseline VM creation (podman.rs)
//! and clone VM creation (snapshot.rs) to ensure consistent behavior.

use std::path::Path;

use anyhow::{Context, Result};
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use crate::{
    firecracker::VmManager,
    network::{NetworkConfig, NetworkManager},
    state::{StateManager, VmState, VmStatus},
};

/// Vsock base port for volume servers (used by both podman and snapshot commands)
pub const VSOCK_VOLUME_PORT_BASE: u32 = 5000;

/// Vsock port for status channel (fc-agent notifies when container starts)
pub const VSOCK_STATUS_PORT: u32 = 4999;

/// Save VM state with complete network configuration
///
/// This function ensures both baseline and clone VMs save identical network data,
/// preventing issues where certain fields (like host_veth) might be missing.
///
/// # Arguments
/// * `state_manager` - State manager for persisting VM state to disk
/// * `vm_state` - Mutable VM state to update
/// * `network_config` - Complete network configuration to save
pub async fn save_vm_state_with_network(
    state_manager: &StateManager,
    vm_state: &mut VmState,
    network_config: &NetworkConfig,
) -> Result<()> {
    // Assign network config directly (typed struct, no serialization needed)
    vm_state.config.network = network_config.clone();

    // Capture fcvm PID (current process, not Firecracker child)
    let fcvm_pid = std::process::id();
    info!("Saving fcvm PID: {}", fcvm_pid);
    vm_state.pid = Some(fcvm_pid);

    // Mark VM as running and persist to disk
    vm_state.status = VmStatus::Running;
    state_manager
        .save_state(vm_state)
        .await
        .context("persisting VM state to disk")?;

    Ok(())
}

/// Cleanup resources for a VM (used by both podman and snapshot commands)
///
/// This function handles the complete cleanup sequence:
/// 1. Cancel health monitor gracefully
/// 2. Abort volume server tasks
/// 3. Kill VM process
/// 4. Kill holder process (rootless mode)
/// 5. Cleanup network resources
/// 6. Delete state file
/// 7. Remove data directory
#[allow(clippy::too_many_arguments)]
pub async fn cleanup_vm(
    vm_id: &str,
    vm_manager: &mut VmManager,
    holder_child: &mut Option<tokio::process::Child>,
    volume_server_handles: Vec<JoinHandle<()>>,
    network: &mut dyn NetworkManager,
    state_manager: &StateManager,
    data_dir: &Path,
    health_cancel_token: Option<tokio_util::sync::CancellationToken>,
    health_monitor_handle: Option<JoinHandle<()>>,
) {
    info!("cleaning up resources");

    // Signal health monitor to stop gracefully, then wait briefly for it
    if let (Some(token), Some(handle)) = (health_cancel_token, health_monitor_handle) {
        token.cancel();
        tokio::select! {
            _ = handle => {
                debug!("health monitor stopped gracefully");
            }
            _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {
                debug!("health monitor didn't stop in time, continuing cleanup");
            }
        }
    }

    // Cancel VolumeServer tasks
    for handle in volume_server_handles {
        handle.abort();
    }

    // Kill VM process
    if let Err(e) = vm_manager.kill().await {
        warn!("failed to kill VM process: {}", e);
    }

    // Kill holder process (rootless mode only)
    if let Some(ref mut holder) = holder_child {
        info!("killing namespace holder process");
        if let Err(e) = holder.kill().await {
            warn!("failed to kill holder process: {}", e);
        }
        let _ = holder.wait().await; // Clean up zombie
    }

    // Cleanup network
    if let Err(e) = network.cleanup().await {
        warn!("failed to cleanup network: {}", e);
    }

    // Delete state file
    if let Err(e) = state_manager.delete_state(vm_id).await {
        warn!("failed to delete state file: {}", e);
    }

    // Cleanup VM data directory (includes disks, sockets, etc.)
    if let Err(e) = tokio::fs::remove_dir_all(data_dir).await {
        warn!(vm_id = %vm_id, error = %e, "failed to cleanup VM data directory");
    } else {
        info!(vm_id = %vm_id, "cleaned up VM data directory");
    }
}
