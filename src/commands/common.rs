//! Common utilities for VM lifecycle management
//!
//! This module contains shared functions used by both baseline VM creation (podman.rs)
//! and clone VM creation (snapshot.rs) to ensure consistent behavior.

use std::os::unix::io::AsRawFd;
use std::path::Path;

use anyhow::{Context, Result};
use nix::sys::uio::{pread, pwrite};
use nix::unistd::{lseek, Whence};
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

/// Timeout for namespace holder creation retries
pub const HOLDER_RETRY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Timeout for waiting for namespace to be ready
pub const NAMESPACE_READY_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);

/// Maximum wait time for namespace setup via nsenter
pub const NSENTER_MAX_WAIT: std::time::Duration = std::time::Duration::from_millis(1000);

/// Poll interval for namespace setup retries
pub const NSENTER_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(5);

/// Retry interval between holder creation attempts
pub const HOLDER_RETRY_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

/// Merge a diff snapshot onto a base memory file.
///
/// Diff snapshots are sparse files where:
/// - Holes = unchanged memory (skip)
/// - Data blocks = dirty pages (copy to base at same offset)
///
/// Uses SEEK_DATA/SEEK_HOLE to efficiently find data blocks without reading the entire file.
///
/// # Arguments
/// * `base_path` - Path to the full memory snapshot (will be modified in place)
/// * `diff_path` - Path to the diff snapshot (sparse file)
///
/// # Returns
/// Number of bytes copied from diff to base
pub fn merge_diff_snapshot(base_path: &Path, diff_path: &Path) -> Result<u64> {
    use std::fs::OpenOptions;

    let diff_file = std::fs::File::open(diff_path)
        .with_context(|| format!("opening diff snapshot: {}", diff_path.display()))?;
    let base_file = OpenOptions::new()
        .write(true)
        .open(base_path)
        .with_context(|| format!("opening base snapshot for writing: {}", base_path.display()))?;

    let diff_fd = diff_file.as_raw_fd();
    let file_size = diff_file
        .metadata()
        .context("getting diff file metadata")?
        .len() as i64;

    let mut offset: i64 = 0;
    let mut total_bytes_copied: u64 = 0;
    let mut data_regions = 0u32;

    // 1MB buffer for copying data blocks
    const BUFFER_SIZE: usize = 1024 * 1024;
    let mut buffer = vec![0u8; BUFFER_SIZE];

    loop {
        // Find next data block (skip holes)
        let data_start = match lseek(diff_fd, offset, Whence::SeekData) {
            Ok(pos) => pos,
            Err(nix::errno::Errno::ENXIO) => {
                // ENXIO means no more data after this offset - we're done
                break;
            }
            Err(e) => {
                return Err(anyhow::anyhow!(
                    "SEEK_DATA failed at offset {}: {}",
                    offset,
                    e
                ));
            }
        };

        // Find end of this data block (start of next hole)
        let data_end = match lseek(diff_fd, data_start, Whence::SeekHole) {
            Ok(pos) => pos,
            Err(_) => file_size, // Data extends to EOF
        };

        let block_size = (data_end - data_start) as usize;
        data_regions += 1;
        debug!(
            data_start = data_start,
            data_end = data_end,
            block_size = block_size,
            "merging diff data region"
        );

        // Copy data block from diff to base at same offset
        // Use pread/pwrite for atomic position+read/write without affecting file cursor
        let mut file_offset = data_start;
        let mut remaining = block_size;
        while remaining > 0 {
            let to_read = remaining.min(buffer.len());
            let bytes_read = pread(&diff_file, &mut buffer[..to_read], file_offset)
                .with_context(|| format!("reading from diff at offset {}", file_offset))?;

            if bytes_read == 0 {
                // EOF before expected - shouldn't happen with SEEK_DATA/SEEK_HOLE
                anyhow::bail!(
                    "unexpected EOF in diff snapshot at offset {} (expected {} more bytes)",
                    file_offset,
                    remaining
                );
            }

            let mut write_offset = 0;
            while write_offset < bytes_read {
                let bytes_written = pwrite(
                    &base_file,
                    &buffer[write_offset..bytes_read],
                    file_offset + write_offset as i64,
                )
                .with_context(|| {
                    format!(
                        "writing to base at offset {}",
                        file_offset + write_offset as i64
                    )
                })?;
                write_offset += bytes_written;
            }

            file_offset += bytes_read as i64;
            remaining -= bytes_read;
            total_bytes_copied += bytes_read as u64;
        }

        offset = data_end;
    }

    // Ensure all data is flushed to disk
    base_file.sync_all().context("syncing base snapshot")?;

    info!(
        total_bytes = total_bytes_copied,
        data_regions = data_regions,
        diff_size = file_size,
        "merged diff snapshot onto base"
    );

    Ok(total_bytes_copied)
}

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
    /// Original VM ID for vsock socket path redirect (from original cache creation)
    pub original_vm_id: String,
    /// Snapshot VM ID for disk path redirect (the VM that was snapshotted)
    /// This is needed because disk paths are patched during cache restore,
    /// so vmstate.bin has a different VM ID for disk than for vsock.
    pub snapshot_vm_id: Option<String>,
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

        let retry_deadline = std::time::Instant::now() + HOLDER_RETRY_TIMEOUT;
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
            let namespace_ready =
                crate::utils::wait_for_namespace_ready(holder_pid, NAMESPACE_READY_TIMEOUT).await;

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
                tokio::time::sleep(HOLDER_RETRY_INTERVAL).await;
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
                    if ns_poll_start.elapsed() > NSENTER_MAX_WAIT {
                        anyhow::bail!(
                            "namespace not ready after {:?}: {}",
                            ns_poll_start.elapsed(),
                            stderr
                        );
                    }
                    tokio::time::sleep(NSENTER_POLL_INTERVAL).await;
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

    // Configure mount namespace isolation for path redirects
    // We need to redirect BOTH:
    // 1. original_vm_id - for vsock paths in vmstate.bin (original cache VM)
    // 2. snapshot_vm_id - for disk paths in vmstate.bin (snapshotted VM, if different)
    let mut baseline_dirs = vec![paths::vm_runtime_dir(&restore_config.original_vm_id)];
    if let Some(ref snapshot_vm_id) = restore_config.snapshot_vm_id {
        if snapshot_vm_id != &restore_config.original_vm_id {
            baseline_dirs.push(paths::vm_runtime_dir(snapshot_vm_id));
        }
    }
    info!(
        baseline_dirs = ?baseline_dirs.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
        clone_dir = %data_dir.display(),
        "enabling mount namespace for path isolation"
    );
    vm_manager.set_mount_redirects(baseline_dirs, data_dir.to_path_buf());

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
            enable_diff_snapshots: Some(true),
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

/// Core snapshot creation logic with automatic diff snapshot support.
///
/// This handles the common operations for both user snapshots (`fcvm snapshot create`)
/// and system snapshots (podman cache). The caller is responsible for:
/// - Getting the Firecracker client
/// - Building the SnapshotConfig with correct metadata
/// - Lock handling (if needed)
///
/// **Diff Snapshot Behavior:**
/// - If no base exists and no parent provided: Full snapshot
/// - If no base exists but parent provided: Copy parent's memory.bin (reflink), then Diff
/// - If base exists: Diff snapshot, merge onto existing base
/// - Result is always a complete memory.bin
///
/// # Arguments
/// * `client` - Firecracker API client for the running VM
/// * `snapshot_config` - Pre-built config with FINAL paths (after atomic rename)
/// * `disk_path` - Source disk to copy to snapshot
/// * `parent_snapshot_dir` - Optional parent snapshot to copy memory.bin from (enables diff for new dirs)
///
/// # Returns
/// Ok(()) on success, Err on failure. VM is resumed regardless of success/failure.
pub async fn create_snapshot_core(
    client: &crate::firecracker::FirecrackerClient,
    snapshot_config: crate::storage::snapshot::SnapshotConfig,
    disk_path: &Path,
    parent_snapshot_dir: Option<&Path>,
) -> Result<()> {
    use crate::firecracker::api::{SnapshotCreate, VmState as ApiVmState};

    // Derive directories from snapshot config (memory_path's parent is the snapshot dir)
    let snapshot_dir = snapshot_config
        .memory_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("invalid memory_path in snapshot config"))?;
    let temp_snapshot_dir = snapshot_dir.with_extension("creating");

    // Check if base snapshot exists (for diff support)
    let base_memory_path = snapshot_dir.join("memory.bin");
    let mut has_base = base_memory_path.exists();

    // If no base but parent provided, copy parent's memory.bin as base (reflink = instant)
    if !has_base {
        if let Some(parent_dir) = parent_snapshot_dir {
            let parent_memory = parent_dir.join("memory.bin");
            if parent_memory.exists() {
                info!(
                    snapshot = %snapshot_config.name,
                    parent = %parent_dir.display(),
                    "copying parent memory.bin as base (reflink)"
                );
                // Create snapshot dir if needed
                tokio::fs::create_dir_all(snapshot_dir)
                    .await
                    .context("creating snapshot directory")?;
                // Reflink copy parent's memory.bin
                let reflink_result = tokio::process::Command::new("cp")
                    .args([
                        "--reflink=always",
                        parent_memory.to_str().unwrap(),
                        base_memory_path.to_str().unwrap(),
                    ])
                    .status()
                    .await
                    .context("copying parent memory.bin")?;
                if !reflink_result.success() {
                    anyhow::bail!("Failed to reflink copy parent memory.bin");
                }
                has_base = true;
            }
        }
    }

    let snapshot_type = if has_base { "Diff" } else { "Full" };

    info!(
        snapshot = %snapshot_config.name,
        snapshot_type = snapshot_type,
        has_base = has_base,
        "creating {} snapshot",
        snapshot_type.to_lowercase()
    );

    // Clean up any leftover temp directory from previous failed attempt
    let _ = tokio::fs::remove_dir_all(&temp_snapshot_dir).await;
    tokio::fs::create_dir_all(&temp_snapshot_dir)
        .await
        .context("creating temp snapshot directory")?;

    let temp_memory_path = temp_snapshot_dir.join("memory.bin");
    let temp_vmstate_path = temp_snapshot_dir.join("vmstate.bin");

    // Pause VM before snapshotting (required by Firecracker)
    info!(snapshot = %snapshot_config.name, "pausing VM for snapshot");
    client
        .patch_vm_state(ApiVmState {
            state: "Paused".to_string(),
        })
        .await
        .context("pausing VM for snapshot")?;

    // Create Firecracker snapshot (Full or Diff based on whether base exists)
    let snapshot_result = client
        .create_snapshot(SnapshotCreate {
            snapshot_type: Some(snapshot_type.to_string()),
            snapshot_path: temp_vmstate_path.display().to_string(),
            mem_file_path: temp_memory_path.display().to_string(),
        })
        .await;

    // Resume VM immediately (always, regardless of snapshot result)
    // This minimizes pause time - diff merge happens after resume
    let resume_result = client
        .patch_vm_state(ApiVmState {
            state: "Resumed".to_string(),
        })
        .await;

    if let Err(e) = &resume_result {
        warn!(snapshot = %snapshot_config.name, error = %e, "failed to resume VM after snapshot");
    }

    // Check if snapshot succeeded - clean up temp dir on failure
    if let Err(e) = snapshot_result {
        let _ = tokio::fs::remove_dir_all(&temp_snapshot_dir).await;
        return Err(e).context("creating Firecracker snapshot");
    }
    if let Err(e) = resume_result {
        let _ = tokio::fs::remove_dir_all(&temp_snapshot_dir).await;
        return Err(e).context("resuming VM after snapshot");
    }

    info!(snapshot = %snapshot_config.name, "VM resumed, processing snapshot");

    if has_base {
        // Diff snapshot: copy base to temp, merge diff, then atomic rename
        info!(
            snapshot = %snapshot_config.name,
            base = %base_memory_path.display(),
            diff = %temp_memory_path.display(),
            "merging diff snapshot onto base copy"
        );

        // Copy base memory to temp dir (will merge diff into this copy)
        let temp_base_memory = temp_snapshot_dir.join("memory.bin");
        tokio::fs::copy(&base_memory_path, &temp_base_memory)
            .await
            .context("copying base memory to temp for merge")?;

        // Run merge in blocking task since it's CPU/IO bound
        let merged_base_path = temp_base_memory.clone();
        let diff_path = temp_memory_path.clone();
        let bytes_merged =
            tokio::task::spawn_blocking(move || merge_diff_snapshot(&merged_base_path, &diff_path))
                .await
                .context("diff merge task panicked")?
                .context("merging diff snapshot")?;

        info!(
            snapshot = %snapshot_config.name,
            bytes_merged = bytes_merged,
            "diff merge complete, building atomic update"
        );

        // Copy disk using btrfs reflink to temp dir
        let temp_disk_path = temp_snapshot_dir.join("disk.raw");
        let reflink_result = tokio::process::Command::new("cp")
            .args([
                "--reflink=always",
                disk_path.to_str().unwrap(),
                temp_disk_path.to_str().unwrap(),
            ])
            .status()
            .await
            .context("copying disk with reflink")?;

        if !reflink_result.success() {
            let _ = tokio::fs::remove_dir_all(&temp_snapshot_dir).await;
            anyhow::bail!(
                "Reflink copy failed - btrfs filesystem required. Ensure {} is on btrfs.",
                paths::assets_dir().display()
            );
        }

        // Write config.json to temp directory
        let temp_config_path = temp_snapshot_dir.join("config.json");
        let config_json = serde_json::to_string_pretty(&snapshot_config)
            .context("serializing snapshot config")?;
        tokio::fs::write(&temp_config_path, &config_json)
            .await
            .context("writing snapshot config")?;

        // Atomic replace: remove old snapshot dir, rename temp to final
        // This ensures all files (memory, vmstate, disk, config) are updated atomically
        tokio::fs::remove_dir_all(snapshot_dir)
            .await
            .context("removing old snapshot directory")?;
        tokio::fs::rename(&temp_snapshot_dir, snapshot_dir)
            .await
            .context("renaming temp snapshot to final location")?;

        info!(
            snapshot = %snapshot_config.name,
            disk = %snapshot_config.disk_path.display(),
            "diff snapshot merged successfully"
        );
    } else {
        // Full snapshot: atomic rename to final location
        info!(snapshot = %snapshot_config.name, "copying disk");

        // Copy disk using btrfs reflink (instant CoW copy)
        let temp_disk_path = temp_snapshot_dir.join("disk.raw");
        let reflink_result = tokio::process::Command::new("cp")
            .args([
                "--reflink=always",
                disk_path.to_str().unwrap(),
                temp_disk_path.to_str().unwrap(),
            ])
            .status()
            .await
            .context("copying disk with reflink")?;

        if !reflink_result.success() {
            let _ = tokio::fs::remove_dir_all(&temp_snapshot_dir).await;
            anyhow::bail!(
                "Reflink copy failed - btrfs filesystem required. Ensure {} is on btrfs.",
                paths::assets_dir().display()
            );
        }

        // Write config.json to temp directory
        let config_path = temp_snapshot_dir.join("config.json");
        let config_json = serde_json::to_string_pretty(&snapshot_config)
            .context("serializing snapshot config")?;
        tokio::fs::write(&config_path, &config_json)
            .await
            .context("writing snapshot config")?;

        // Atomic rename from temp to final location
        // If final exists (e.g., from previous snapshot with same name), remove it first
        if snapshot_dir.exists() {
            tokio::fs::remove_dir_all(snapshot_dir)
                .await
                .context("removing existing snapshot directory")?;
        }
        tokio::fs::rename(&temp_snapshot_dir, snapshot_dir)
            .await
            .context("renaming temp snapshot to final location")?;

        info!(
            snapshot = %snapshot_config.name,
            disk = %snapshot_config.disk_path.display(),
            "full snapshot created successfully"
        );
    }

    Ok(())
}
