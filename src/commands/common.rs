//! Common utilities for VM lifecycle management
//!
//! This module contains shared functions used by both baseline VM creation (podman.rs)
//! and clone VM creation (snapshot.rs) to ensure consistent behavior.

use std::path::Path;

use anyhow::{Context, Result};
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use std::path::PathBuf;

use crate::{
    firecracker::VmManager,
    network::{BridgedNetwork, NetworkConfig, NetworkManager, SlirpNetwork},
    paths,
    state::{StateManager, VmState, VmStatus},
    storage::DiskManager,
};

/// Vsock base port for volume servers (used by both podman and snapshot commands)
pub const VSOCK_VOLUME_PORT_BASE: u32 = 5000;

/// Vsock port for status channel (fc-agent notifies when container starts)
pub const VSOCK_STATUS_PORT: u32 = 4999;

/// Vsock port for container output streaming (bidirectional, line-based)
pub const VSOCK_OUTPUT_PORT: u32 = 4997;

/// Vsock port for TTY container I/O (binary exec_proto)
pub const VSOCK_TTY_PORT: u32 = 4996;

/// Minimum required Firecracker version for network_overrides support
const MIN_FIRECRACKER_VERSION: (u32, u32, u32) = (1, 13, 1);

/// Find and validate Firecracker binary
///
/// Returns the path to the Firecracker binary if it exists and meets minimum version requirements.
/// Fails with a clear error if Firecracker is not found or version is too old.
///
/// Checks `FCVM_FIRECRACKER_BIN` env var first, then falls back to PATH lookup.
pub fn find_firecracker() -> Result<std::path::PathBuf> {
    let firecracker_bin = if let Ok(path) = std::env::var("FCVM_FIRECRACKER_BIN") {
        let p = std::path::PathBuf::from(&path);
        if !p.exists() {
            anyhow::bail!("FCVM_FIRECRACKER_BIN={} does not exist", path);
        }
        p
    } else {
        which::which("firecracker").context("firecracker not found in PATH")?
    };

    // Check version
    let output = std::process::Command::new(&firecracker_bin)
        .arg("--version")
        .output()
        .context("failed to run firecracker --version")?;

    let version_str = String::from_utf8_lossy(&output.stdout);
    let version = parse_firecracker_version(&version_str)?;

    if version < MIN_FIRECRACKER_VERSION {
        anyhow::bail!(
            "Firecracker version {}.{}.{} is too old. Minimum required: {}.{}.{} (for network_overrides support in snapshot cloning)",
            version.0, version.1, version.2,
            MIN_FIRECRACKER_VERSION.0, MIN_FIRECRACKER_VERSION.1, MIN_FIRECRACKER_VERSION.2
        );
    }

    debug!(
        "Found Firecracker {}.{}.{} at {:?}",
        version.0, version.1, version.2, firecracker_bin
    );

    Ok(firecracker_bin)
}

/// Parse Firecracker version from --version output
///
/// Expected format: "Firecracker v1.14.0" or similar
fn parse_firecracker_version(output: &str) -> Result<(u32, u32, u32)> {
    // Find version number pattern vX.Y.Z
    let version_re = regex::Regex::new(r"v?(\d+)\.(\d+)\.(\d+)").context("invalid regex")?;

    let caps = version_re
        .captures(output)
        .context("could not parse Firecracker version from output")?;

    let major: u32 = caps[1].parse().context("invalid major version")?;
    let minor: u32 = caps[2].parse().context("invalid minor version")?;
    let patch: u32 = caps[3].parse().context("invalid patch version")?;

    Ok((major, minor, patch))
}

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

/// Memory backend configuration for snapshot restore
pub enum MemoryBackend {
    /// Load memory directly from file (used by podman cache restore)
    File { memory_path: PathBuf },
    /// Use UFFD server for on-demand page loading (used by snapshot clones)
    Uffd { socket_path: PathBuf },
}

/// Configuration for restoring a VM from a snapshot
pub struct SnapshotRestoreConfig {
    /// VM state path (vmstate.bin)
    pub vmstate_path: PathBuf,
    /// Memory backend configuration
    pub memory_backend: MemoryBackend,
    /// Source disk for CoW copy
    pub source_disk_path: PathBuf,
    /// Original VM ID (for vsock socket path redirect)
    pub original_vm_id: String,
}

/// Restore a VM from a snapshot
///
/// This is the core snapshot restore logic shared by:
/// - `fcvm snapshot run` (clone with UFFD memory sharing)
/// - `fcvm podman run` with cache hit (direct file load)
///
/// Both paths use identical Firecracker setup, the only differences are:
/// - Memory backend: UFFD vs File
/// - Snapshot source: snapshots/{name} vs podman-cache/{hash}
#[allow(clippy::too_many_arguments)]
pub async fn restore_from_snapshot(
    vm_id: &str,
    vm_name: &str,
    data_dir: &Path,
    socket_path: &Path,
    restore_config: &SnapshotRestoreConfig,
    network_config: &NetworkConfig,
    network: &mut dyn NetworkManager,
    state_manager: &StateManager,
    vm_state: &mut VmState,
) -> Result<(VmManager, Option<tokio::process::Child>)> {
    let vm_dir = data_dir.join("disks");

    // Configure namespace isolation if network provides one
    let mut holder_child: Option<tokio::process::Child> = None;
    let mut holder_pid_for_post_start: Option<u32> = None;
    let mut vm_manager = VmManager::new(vm_id.to_string(), socket_path.to_path_buf(), None);
    vm_manager.set_vm_name(vm_name.to_string());

    // rootfs_path is set by either the bridged or rootless branch
    let rootfs_path: PathBuf;

    if let Some(bridged_net) = network.as_any().downcast_ref::<BridgedNetwork>() {
        if let Some(ns_id) = bridged_net.namespace_id() {
            info!(namespace = %ns_id, "configuring VM to run in network namespace");
            vm_manager.set_namespace(ns_id.to_string());
        }

        // For bridged mode, create disk
        let disk_manager = DiskManager::new(
            vm_id.to_string(),
            restore_config.source_disk_path.clone(),
            vm_dir.clone(),
        );

        rootfs_path = disk_manager
            .create_cow_disk()
            .await
            .context("creating CoW disk from snapshot")?;

        info!(
            rootfs = %rootfs_path.display(),
            source_disk = %restore_config.source_disk_path.display(),
            "CoW disk prepared from snapshot"
        );
    } else if let Some(slirp_net) = network.as_any().downcast_ref::<SlirpNetwork>() {
        // Rootless mode: spawn holder process and set up namespace via nsenter
        // OPTIMIZATION: Parallelize disk creation with network setup

        // Step 1: Spawn holder process (keeps namespace alive)
        let holder_cmd = slirp_net.build_holder_command();
        info!(cmd = ?holder_cmd, "spawning namespace holder for rootless networking");

        let retry_deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut attempt = 0u32;

        let (mut child, holder_pid) = loop {
            attempt += 1;

            let mut child = tokio::process::Command::new(&holder_cmd[0])
                .args(&holder_cmd[1..])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .context("spawning namespace holder process")?;

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
                std::time::Duration::from_millis(500),
            )
            .await;

            if namespace_ready {
                break (child, holder_pid);
            }

            // Namespace not ready, kill holder and retry
            let _ = child.kill().await;
            if std::time::Instant::now() < retry_deadline {
                warn!(
                    holder_pid = holder_pid,
                    attempt = attempt,
                    "namespace not ready, retrying holder creation..."
                );
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            } else {
                anyhow::bail!(
                    "namespace not ready after {} attempts (holder PID {})",
                    attempt,
                    holder_pid
                );
            }
        };

        // Step 2: Run disk creation and network setup IN PARALLEL
        let setup_script = slirp_net.build_setup_script();
        let nsenter_prefix = slirp_net.build_nsenter_prefix(holder_pid);
        let tap_device = network_config.tap_device.clone();

        // Disk creation task
        let source_disk = restore_config.source_disk_path.clone();
        let disk_task = async {
            let disk_manager =
                DiskManager::new(vm_id.to_string(), source_disk.clone(), vm_dir.clone());

            let rootfs_path = disk_manager
                .create_cow_disk()
                .await
                .context("creating CoW disk from snapshot")?;

            info!(
                rootfs = %rootfs_path.display(),
                source_disk = %source_disk.display(),
                "CoW disk prepared from snapshot"
            );

            Ok::<_, anyhow::Error>(rootfs_path)
        };

        // Network setup task
        let network_task = async {
            const MAX_NS_WAIT: std::time::Duration = std::time::Duration::from_millis(1000);
            const NS_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(5);
            let ns_poll_start = std::time::Instant::now();

            info!(holder_pid = holder_pid, "running network setup via nsenter");
            loop {
                // Verify holder is still alive before attempting nsenter
                if !crate::utils::is_process_alive(holder_pid) {
                    anyhow::bail!(
                        "holder process (PID {}) died before network setup could run",
                        holder_pid
                    );
                }

                let output = tokio::process::Command::new(&nsenter_prefix[0])
                    .args(&nsenter_prefix[1..])
                    .arg("bash")
                    .arg("-c")
                    .arg(&setup_script)
                    .output()
                    .await
                    .context("running network setup via nsenter")?;

                if output.status.success() {
                    debug!("namespace ready after {:?}", ns_poll_start.elapsed());
                    break;
                }

                // Check if it's a namespace-not-ready error (retry) vs permanent error (fail)
                let stderr = String::from_utf8_lossy(&output.stderr);
                if stderr.contains("Invalid argument") || stderr.contains("No such process") {
                    if ns_poll_start.elapsed() > MAX_NS_WAIT {
                        anyhow::bail!(
                            "namespace not ready after {:?}: {}",
                            ns_poll_start.elapsed(),
                            stderr
                        );
                    }
                    tokio::time::sleep(NS_POLL_INTERVAL).await;
                    continue;
                }

                // Permanent error
                anyhow::bail!("network setup failed: {}", stderr);
            }

            // Verify TAP device was created successfully
            let verify_cmd = format!("ip link show {} >/dev/null 2>&1", tap_device);
            let verify_output = tokio::process::Command::new(&nsenter_prefix[0])
                .args(&nsenter_prefix[1..])
                .arg("bash")
                .arg("-c")
                .arg(&verify_cmd)
                .status()
                .await
                .context("verifying TAP device")?;

            if !verify_output.success() {
                anyhow::bail!(
                    "TAP device '{}' not found after network setup - setup may have failed silently",
                    tap_device
                );
            }
            debug!(tap_device = %tap_device, "TAP device verified");

            Ok::<_, anyhow::Error>(())
        };

        // Run both tasks in parallel
        let (disk_result, network_result) = tokio::join!(disk_task, network_task);

        // Handle errors - kill holder child if either fails
        if let Err(e) = &disk_result {
            let _ = child.kill().await;
            return Err(anyhow::anyhow!("disk creation failed: {}", e));
        }
        if let Err(e) = &network_result {
            let _ = child.kill().await;
            return Err(anyhow::anyhow!("network setup failed: {}", e));
        }

        rootfs_path = disk_result?;
        network_result?;

        info!(
            holder_pid = holder_pid,
            "parallel disk + network setup complete"
        );

        // Step 3: Set namespace paths for pre_exec setns
        vm_manager.set_user_namespace_path(PathBuf::from(format!("/proc/{}/ns/user", holder_pid)));
        vm_manager.set_net_namespace_path(PathBuf::from(format!("/proc/{}/ns/net", holder_pid)));

        // Store holder_pid in state for health checks
        vm_state.holder_pid = Some(holder_pid);
        holder_pid_for_post_start = Some(holder_pid);

        holder_child = Some(child);
    } else {
        // Unknown network type - should not happen
        anyhow::bail!("Unknown network type - must be either BridgedNetwork or SlirpNetwork");
    }

    // Configure mount namespace isolation for vsock redirect
    let baseline_dir = paths::vm_runtime_dir(&restore_config.original_vm_id);
    info!(
        baseline_dir = %baseline_dir.display(),
        clone_dir = %data_dir.display(),
        "enabling mount namespace for vsock socket isolation"
    );
    vm_manager.set_vsock_redirect(baseline_dir, data_dir.to_path_buf());

    let firecracker_bin = find_firecracker()?;

    vm_manager
        .start(&firecracker_bin, None)
        .await
        .context("starting Firecracker")?;

    // For rootless mode with slirp4netns: post_start starts slirp4netns in the namespace
    let vm_pid = vm_manager.pid()?;
    let post_start_pid = holder_pid_for_post_start.unwrap_or(vm_pid);
    network
        .post_start(post_start_pid)
        .await
        .context("post-start network setup")?;

    let client = vm_manager.client()?;

    // Load snapshot with configured memory backend and network override
    use crate::firecracker::api::{
        DrivePatch, MemBackend, NetworkOverride, SnapshotLoad, VmState as ApiVmState,
    };

    let mem_backend = match &restore_config.memory_backend {
        MemoryBackend::File { memory_path } => {
            info!(
                memory = %memory_path.display(),
                "loading snapshot with File backend"
            );
            MemBackend {
                backend_type: "File".to_string(),
                backend_path: memory_path.display().to_string(),
            }
        }
        MemoryBackend::Uffd { socket_path } => {
            info!(
                uffd_socket = %socket_path.display(),
                "loading snapshot with UFFD backend"
            );
            MemBackend {
                backend_type: "Uffd".to_string(),
                backend_path: socket_path.display().to_string(),
            }
        }
    };

    // Timing instrumentation: measure snapshot load operation
    let load_start = std::time::Instant::now();
    client
        .load_snapshot(SnapshotLoad {
            snapshot_path: restore_config.vmstate_path.display().to_string(),
            mem_backend,
            enable_diff_snapshots: Some(false),
            resume_vm: Some(false), // Update devices before resume
            network_overrides: Some(vec![NetworkOverride {
                iface_id: "eth0".to_string(),
                host_dev_name: network_config.tap_device.clone(),
            }]),
        })
        .await
        .context("loading snapshot")?;
    let load_duration = load_start.elapsed();
    info!(
        duration_ms = load_duration.as_millis(),
        "snapshot load completed"
    );

    // Timing instrumentation: measure disk patch operation
    let patch_start = std::time::Instant::now();
    client
        .patch_drive(
            "rootfs",
            DrivePatch {
                drive_id: "rootfs".to_string(),
                path_on_host: Some(rootfs_path.display().to_string()),
                rate_limiter: None,
            },
        )
        .await
        .context("retargeting rootfs drive")?;
    let patch_duration = patch_start.elapsed();
    info!(
        duration_ms = patch_duration.as_millis(),
        "disk patch completed"
    );

    // Signal fc-agent to flush ARP cache via MMDS restore-epoch update
    let restore_epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .context("system time before Unix epoch")?
        .as_secs();

    client
        .put_mmds(serde_json::json!({
            "latest": {
                "host-time": chrono::Utc::now().timestamp().to_string(),
                "restore-epoch": restore_epoch.to_string()
            }
        }))
        .await
        .context("updating MMDS with restore-epoch")?;
    info!(
        restore_epoch = restore_epoch,
        "signaled fc-agent to flush ARP via MMDS"
    );

    // Timing instrumentation: measure VM resume operation
    let resume_start = std::time::Instant::now();
    client
        .patch_vm_state(ApiVmState {
            state: "Resumed".to_string(),
        })
        .await
        .context("resuming VM after snapshot load")?;
    let resume_duration = resume_start.elapsed();
    info!(
        duration_ms = resume_duration.as_millis(),
        total_snapshot_ms = (load_duration + patch_duration + resume_duration).as_millis(),
        "VM resume completed"
    );

    // Store fcvm process PID (not Firecracker PID)
    vm_state.pid = Some(std::process::id());

    // Track original vsock vm_id for future snapshots
    // When this VM is later snapshotted, clones need to use this original_vm_id
    // for vsock redirect because vmstate.bin stores paths from this vm
    vm_state.config.original_vsock_vm_id = Some(restore_config.original_vm_id.clone());

    // Save VM state with complete network configuration
    save_vm_state_with_network(state_manager, vm_state, network_config).await?;

    Ok((vm_manager, holder_child))
}
