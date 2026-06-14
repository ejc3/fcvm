//! Common utilities for VM lifecycle management
//!
//! This module contains shared functions used by both baseline VM creation (podman.rs)
//! and clone VM creation (snapshot.rs) to ensure consistent behavior.

use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::sync::OnceLock;

use anyhow::{bail, Context, Result};
use nix::sys::uio::{pread, pwrite};
use nix::unistd::{lseek, Whence};
use tokio::sync::Semaphore;
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};

use std::path::PathBuf;

use crate::{
    firecracker::VmManager,
    hypervisor::{firecracker::FirecrackerBackend, Hypervisor},
    network::{BridgedNetwork, NetworkConfig, NetworkManager, PastaNetwork},
    paths,
    state::{StateManager, VmState, VmStatus},
    storage::DiskManager,
};

/// Runtime configuration from kernel profile, passed explicitly instead of env vars.
///
/// This replaces the pattern of writing config to process-global env vars (which races
/// when multiple VMs with different profiles run concurrently in an async server).
#[derive(Default, Clone, Debug)]
pub struct RuntimeConfig {
    /// Custom Firecracker binary path (from kernel profile)
    pub firecracker_bin: Option<PathBuf>,
    /// Extra Firecracker CLI arguments (e.g., "--enable-nv2")
    pub firecracker_args: Option<String>,
    /// Extra kernel boot arguments (e.g., "arm64.nv2")
    pub boot_args: Option<String>,
    /// FUSE reader thread count override
    pub fuse_readers: Option<u32>,
}

/// Vsock base port for volume servers (used by both podman and snapshot commands)
pub const VSOCK_VOLUME_PORT_BASE: u32 = 5000;

/// Vsock port for status channel (fc-agent notifies when container starts)
pub const VSOCK_STATUS_PORT: u32 = 4999;

/// Vsock port for container output streaming (bidirectional, line-based)
pub const VSOCK_OUTPUT_PORT: u32 = 4997;

/// Vsock port for TTY container I/O (binary exec_proto)
pub const VSOCK_TTY_PORT: u32 = 4996;

/// Vsock port the host serves the boot plan on, for VMMs without a metadata service
/// (Cloud Hypervisor — #632 P0.5). fc-agent fetches its plan here when the kernel
/// boot args contain `fcvm_bootplan=vsock`; Firecracker uses MMDS instead. Must match
/// `fc-agent::bootplan::BOOTPLAN_VSOCK_PORT`.
pub const VSOCK_BOOTPLAN_PORT: u32 = 4995;

/// Minimum required Firecracker version for network_overrides support
const MIN_FIRECRACKER_VERSION: (u32, u32, u32) = (1, 13, 1);

/// Timeout for namespace holder creation retries
pub const HOLDER_RETRY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Maximum wait time for namespace setup via nsenter
pub const NSENTER_MAX_WAIT: std::time::Duration = std::time::Duration::from_millis(1000);

/// Poll interval for namespace setup retries
pub const NSENTER_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(5);

/// Retry interval between holder creation attempts (only used when holder dies)
pub const HOLDER_RETRY_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

/// Set up UID/GID mappings for a process in a new user namespace.
///
/// Tries extended mappings (UIDs 0-65535) via newuidmap/newgidmap first, which
/// enables OCI runtimes (crun) to mount devpts inside containers (needed by fc-mock).
/// Falls back to single-UID mapping (like --map-root-user) when the helpers aren't
/// available or lack permissions (e.g., inside containers).
async fn setup_namespace_mappings(pid: u32) -> anyhow::Result<()> {
    use std::os::unix::fs::MetadataExt;

    let uid = nix::unistd::getuid().as_raw();
    let gid = nix::unistd::getgid().as_raw();
    let pid_s = pid.to_string();
    let uid_s = uid.to_string();
    let gid_s = gid.to_string();

    // Wait for unshare(2) to create the new user namespace.
    // The namespace exists once /proc/PID/ns/user has a different inode than ours.
    let self_ino = std::fs::metadata("/proc/self/ns/user")
        .context("reading own user namespace inode")?
        .ino();
    let ns_deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        if std::fs::metadata(format!("/proc/{pid}/ns/user"))
            .map(|m| m.ino() != self_ino)
            .unwrap_or(false)
        {
            break;
        }
        if std::time::Instant::now() >= ns_deadline {
            anyhow::bail!("timed out waiting for user namespace (PID {pid})");
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }

    // Try extended mappings (0-65535) via newuidmap/newgidmap (setuid helpers).
    // These read /etc/subuid and /etc/subgid to authorize the mapping range.
    let uid_ok = tokio::process::Command::new("newuidmap")
        .args([&pid_s, "0", &uid_s, "1", "1", "100000", "65535"])
        .output()
        .await
        .map(|o| o.status.success())
        .unwrap_or(false);
    let gid_ok = uid_ok
        && tokio::process::Command::new("newgidmap")
            .args([&pid_s, "0", &gid_s, "1", "1", "100000", "65535"])
            .output()
            .await
            .map(|o| o.status.success())
            .unwrap_or(false);

    if uid_ok && gid_ok {
        info!(pid, "extended UID/GID mappings (0-65535)");
        return Ok(());
    }

    // Fallback: write mappings directly (equivalent to --map-root-user).
    // Must deny setgroups before writing gid_map as unprivileged user.
    // Root (uid 0) can write gid_map directly and doesn't need (or can't) deny setgroups.
    if uid != 0 {
        std::fs::write(format!("/proc/{pid}/setgroups"), "deny").context("denying setgroups")?;
    }
    if !uid_ok {
        std::fs::write(format!("/proc/{pid}/uid_map"), format!("0 {uid} 1\n"))
            .context("writing uid_map")?;
    }
    std::fs::write(format!("/proc/{pid}/gid_map"), format!("0 {gid} 1\n"))
        .context("writing gid_map")?;
    info!(pid, "single UID/GID mapping (fallback)");

    Ok(())
}

/// Spawn a namespace holder process and wait for it to be ready.
///
/// Spawns `unshare --user --net -- sleep infinity`, writes UID/GID mappings
/// (extended if possible, single otherwise), and waits for nsenter to work.
///
/// Key design: gives the first holder the FULL retry deadline. Only spawns a new
/// holder if the current one dies. Under heavy load, mapping setup may be slow
/// due to CPU scheduling pressure — killing and respawning wastes time since
/// the new holder faces the same pressure.
pub async fn spawn_namespace_holder(
    holder_cmd: &[String],
) -> anyhow::Result<(tokio::process::Child, u32)> {
    let deadline = std::time::Instant::now() + HOLDER_RETRY_TIMEOUT;
    let mut attempt = 0u32;

    loop {
        attempt += 1;

        let mut cmd = tokio::process::Command::new(&holder_cmd[0]);
        cmd.args(&holder_cmd[1..])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped());

        // Kill holder if parent (fcvm) dies. Without this, holders orphan to init
        // and accumulate when fcvm is SIGKILL'd or crashes.
        unsafe {
            cmd.pre_exec(|| {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }

        let child = cmd.spawn().with_context(|| {
            format!(
                "spawning namespace holder (attempt {}): {:?}",
                attempt, holder_cmd
            )
        })?;

        let holder_pid = child.id().context("getting holder process PID")?;
        if attempt > 1 {
            info!(holder_pid, attempt, "namespace holder started (retry)");
        } else {
            info!(holder_pid, "namespace holder started");
        }

        // Write UID/GID mappings for the new user namespace.
        // Must happen before wait_for_namespace_ready which checks uid_map.
        if let Err(e) = setup_namespace_mappings(holder_pid).await {
            warn!(holder_pid, error = %e, "failed to set up namespace mappings");
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(holder_pid as i32),
                nix::sys::signal::Signal::SIGKILL,
            );
            if std::time::Instant::now() < deadline {
                tokio::time::sleep(HOLDER_RETRY_INTERVAL).await;
                continue;
            }
            return Err(e).context("setting up namespace mappings");
        }

        // Give this holder the FULL remaining time to become ready.
        // Don't kill on timeout — if it times out, the deadline is exceeded anyway.
        let result = crate::utils::wait_for_namespace_ready(holder_pid, deadline).await;

        match result {
            crate::utils::NamespaceReadyResult::Ready => {
                return Ok((child, holder_pid));
            }
            crate::utils::NamespaceReadyResult::HolderDied => {
                if std::time::Instant::now() < deadline {
                    warn!(
                        holder_pid,
                        attempt, "holder died before namespace ready, retrying..."
                    );
                    tokio::time::sleep(HOLDER_RETRY_INTERVAL).await;
                    continue;
                } else {
                    let max_user_ns = std::fs::read_to_string("/proc/sys/user/max_user_namespaces")
                        .unwrap_or_else(|_| "unknown".to_string());
                    anyhow::bail!(
                        "namespace holder died and no time remaining to retry \
                         (attempt {}, holder PID {}, max_user_namespaces={})",
                        attempt,
                        holder_pid,
                        max_user_ns.trim()
                    );
                }
            }
            crate::utils::NamespaceReadyResult::TimedOut => {
                // Holder is alive but maps not written within deadline.
                // No point killing and retrying — deadline is exceeded.
                let _ = nix::sys::signal::kill(
                    nix::unistd::Pid::from_raw(holder_pid as i32),
                    nix::sys::signal::Signal::SIGKILL,
                );
                anyhow::bail!(
                    "namespace not ready within {:?} (holder PID {} alive, \
                     uid_map not written — likely CPU scheduling pressure \
                     with many concurrent VMs). Attempt {}.",
                    HOLDER_RETRY_TIMEOUT,
                    holder_pid,
                    attempt
                );
            }
        }
    }
}

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

/// Disable swap for a process by moving it to a dedicated cgroup with
/// memory.swap.max=0.
///
/// VM anon pages are expensive to swap out and fault back in (random I/O, blocks
/// guest threads). File cache pages (e.g. memory.bin) are cheap to re-fault
/// (sequential btrfs reads with zstd decompression). With default swappiness=60,
/// the kernel prefers swapping VM anon pages over evicting file cache, which
/// causes severe I/O pressure and degraded VM performance.
///
/// Creates `/sys/fs/cgroup/fcvm.slice/fcvm-{pid}.scope` — a dedicated cgroup
/// under the root slice where the memory controller is always available. This
/// avoids the cgroup v2 "no internal processes" constraint that prevents creating
/// child cgroups under session scopes.
pub fn disable_cgroup_swap(pid: u32) {
    // Create fcvm.slice if it doesn't exist (first VM on this host)
    let slice_path = "/sys/fs/cgroup/fcvm.slice";
    if let Err(e) = std::fs::create_dir_all(slice_path) {
        warn!(pid, path = slice_path, error = %e, "failed to create fcvm.slice");
        return;
    }

    // Enable memory controller on fcvm.slice so child cgroups get memory.*
    let subtree_path = format!("{}/cgroup.subtree_control", slice_path);
    if let Err(e) = std::fs::write(&subtree_path, "+memory") {
        warn!(pid, path = %subtree_path, error = %e, "failed to enable memory controller");
        return;
    }

    // Create a scope for this specific Firecracker process
    let scope_path = format!("{}/fcvm-{}.scope", slice_path, pid);
    if let Err(e) = std::fs::create_dir_all(&scope_path) {
        warn!(pid, path = %scope_path, error = %e, "failed to create cgroup scope");
        return;
    }

    // Set memory.swap.max=0 BEFORE moving the process in
    let swap_max_path = format!("{}/memory.swap.max", scope_path);
    if let Err(e) = std::fs::write(&swap_max_path, "0") {
        warn!(pid, path = %swap_max_path, error = %e, "failed to set memory.swap.max=0");
        return;
    }

    // Move the process into the scope
    let procs_path = format!("{}/cgroup.procs", scope_path);
    match std::fs::write(&procs_path, pid.to_string()) {
        Ok(()) => {
            info!(pid, cgroup = %scope_path, "moved to dedicated cgroup with swap disabled");
        }
        Err(e) => {
            warn!(pid, path = %procs_path, error = %e, "failed to move process to cgroup");
        }
    }
}

/// Returns the path to the Firecracker binary if it exists and meets minimum version requirements.
/// Fails with a clear error if Firecracker is not found or version is too old.
///
/// Resolution order:
/// 1. `RuntimeConfig.firecracker_bin` (from kernel profile or [firecracker] config)
/// 2. `FCVM_FIRECRACKER_BIN` env var
/// 3. PATH lookup (system firecracker)
pub fn find_firecracker(config: &RuntimeConfig) -> Result<std::path::PathBuf> {
    let firecracker_bin = if let Some(ref path) = config.firecracker_bin {
        if !path.exists() {
            anyhow::bail!(
                "Firecracker binary from profile does not exist: {}",
                path.display()
            );
        }
        path.clone()
    } else if let Ok(path) = std::env::var("FCVM_FIRECRACKER_BIN") {
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
        .with_context(|| {
            format!(
                "failed to run firecracker --version (binary: {})",
                firecracker_bin.display()
            )
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "firecracker --version failed (exit {}, binary: {}): {}",
            output.status,
            firecracker_bin.display(),
            stderr.trim()
        );
    }

    let version_str = String::from_utf8_lossy(&output.stdout);
    let version = parse_firecracker_version(&version_str).with_context(|| {
        format!(
            "binary: {}, stdout: {:?}, stderr: {:?}",
            firecracker_bin.display(),
            version_str.trim(),
            String::from_utf8_lossy(&output.stderr).trim()
        )
    })?;

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

/// Locate the Cloud Hypervisor binary (#632). Honors `FCVM_CLOUD_HYPERVISOR_BIN`, then PATH.
///
/// Note: CH snapshot/restore on aarch64 SVE hosts requires a build with the SVE register
/// fix (CH #8268, post-v52.0); cold boot (P1) works with any v52+. Version pinning lands
/// with P2.
pub fn find_cloud_hypervisor() -> Result<std::path::PathBuf> {
    let bin = if let Ok(path) = std::env::var("FCVM_CLOUD_HYPERVISOR_BIN") {
        let p = std::path::PathBuf::from(&path);
        if !p.exists() {
            anyhow::bail!("FCVM_CLOUD_HYPERVISOR_BIN={} does not exist", path);
        }
        p
    } else {
        which::which("cloud-hypervisor").context("cloud-hypervisor not found in PATH")?
    };

    let output = std::process::Command::new(&bin)
        .arg("--version")
        .output()
        .with_context(|| format!("failed to run {} --version", bin.display()))?;
    if !output.status.success() {
        anyhow::bail!(
            "cloud-hypervisor --version failed (binary: {})",
            bin.display()
        );
    }
    debug!(
        "Found Cloud Hypervisor at {:?}: {}",
        bin,
        String::from_utf8_lossy(&output.stdout).trim()
    );
    Ok(bin)
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

/// Owned resources for VM cleanup that can be moved into the cleanup call.
pub struct CleanupContext {
    pub vm_id: String,
    pub volume_server_handles: Vec<JoinHandle<()>>,
    /// RemapFs references for portable volumes (for inode table serialization).
    /// One entry per volume; `Some` for portable volumes, `None` for plain.
    /// Dropped during cleanup — the Arc prevents the RemapFs from being freed
    /// while the VolumeServer task still holds a reference.
    pub remap_refs: Vec<Option<std::sync::Arc<fuse_pipe::RemapFs<fuse_pipe::PassthroughFs>>>>,
    pub data_dir: PathBuf,
    pub health_cancel_token: Option<tokio_util::sync::CancellationToken>,
    pub health_monitor_handle: Option<JoinHandle<()>>,
    pub output_listener_handle: Option<JoinHandle<Vec<(String, String)>>>,
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
pub async fn cleanup_vm(
    ctx: CleanupContext,
    vm_manager: &mut dyn Hypervisor,
    holder_child: &mut Option<tokio::process::Child>,
    network: &mut dyn NetworkManager,
    state_manager: &StateManager,
) {
    let CleanupContext {
        vm_id,
        volume_server_handles,
        remap_refs: _,
        data_dir,
        health_cancel_token,
        health_monitor_handle,
        output_listener_handle,
    } = ctx;
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

    // Abort output listener task if still running
    if let Some(handle) = output_listener_handle {
        handle.abort();
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

    // Remove this VM's NFS exports (no-op when the VM had none). Lives here so
    // every exit path — podman run, converge teardown, restored clones — drops
    // its /etc/exports.d entry; a leftover entry for a deleted directory makes
    // every later `exportfs -ra` fail until the self-heal prunes it.
    super::podman::cleanup_nfs_exports(&vm_id).await;

    // Delete state file
    if let Err(e) = state_manager.delete_state(&vm_id).await {
        warn!("failed to delete state file: {}", e);
    }

    // Save Firecracker log before cleanup (for debugging snapshot restore failures)
    let fc_log = data_dir.join("firecracker.log");
    if fc_log.exists() {
        let dest = std::path::PathBuf::from(format!("/tmp/fcvm-firecracker-{}.log", vm_id));
        if let Err(e) = tokio::fs::copy(&fc_log, &dest).await {
            debug!(vm_id = %vm_id, error = %e, "could not save firecracker log");
        } else {
            info!(vm_id = %vm_id, log = %dest.display(), "saved firecracker log");
        }
    }

    // Cleanup VM data directory (includes disks, sockets, etc.)
    if let Err(e) = tokio::fs::remove_dir_all(&data_dir).await {
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
    /// Whether this VM uses hugepages
    pub hugepages: bool,
    /// Extra disk images to copy from snapshot directory
    pub extra_disks: Vec<crate::storage::snapshot::SnapshotExtraDisk>,
    /// Snapshot directory for extra disk source files
    pub snapshot_dir: Option<PathBuf>,
}

/// Parameters for snapshot restore, grouping the many read-only inputs.
pub struct RestoreParams<'a> {
    pub vm_id: &'a str,
    pub vm_name: &'a str,
    pub data_dir: &'a Path,
    pub socket_path: &'a Path,
    pub runtime_config: &'a RuntimeConfig,
    pub restore_config: &'a SnapshotRestoreConfig,
    pub network_config: &'a NetworkConfig,
    /// For routed mode clones: the unique per-clone IPv6 that fc-agent should
    /// configure on eth0, replacing the snapshot's shared guest IPv6.
    pub clone_ipv6: Option<String>,
    /// Enable KVM dirty page tracking (needed for subsequent diff snapshots
    /// from this VM). File-backed restore memory is mmap'd MAP_PRIVATE either
    /// way, so clones share clean pages through the host page cache in BOTH
    /// modes — sharing is bounded by the guest's write footprint, and tracking
    /// showed no material PSS erosion when measured (#632: 3x 1GiB clones
    /// ≈ 230MiB total PSS, ON vs OFF within 1MiB). Disabled for hugepage VMs
    /// (KVM would split 2MB TLB entries to 4K).
    pub track_dirty_pages: bool,
}

/// Diagnostic helper (#608): the `vm-disks/<id>` directory ids whose **rootfs** path is
/// embedded in a Firecracker `vmstate.bin`.
///
/// Firecracker serializes each drive's `path_on_host` as a plain UTF-8 string, so we can
/// read which rootfs path `LoadSnapshot` will open. Anchored to the `…/disks/rootfs.raw`
/// suffix so an external `--disk` that merely lives under a vm-disks dir is not matched.
/// Best-effort: an unreadable file or no matches yields an empty vec. Used log-only at
/// restore to detect (and make diagnosable) any case where the embedded rootfs dir is not
/// covered by `baseline_dirs` — empirically that never happens (CreateSnapshot serializes
/// the patched creator path, always in baseline_dirs), but this turns the rare observed
/// failure into evidence instead of a guess.
fn rootfs_disk_vm_ids_in_bytes(bytes: &[u8]) -> Vec<String> {
    const PREFIX: &[u8] = b"/vm-disks/";
    const SUFFIX: &[u8] = b"/disks/rootfs.raw";

    let mut ids: Vec<String> = Vec::new();
    let mut search_from = 0usize;
    while let Some(rel) = bytes[search_from..]
        .windows(PREFIX.len())
        .position(|w| w == PREFIX)
    {
        let id_start = search_from + rel + PREFIX.len();
        let mut id_end = id_start;
        while id_end < bytes.len() {
            let c = bytes[id_end];
            if c == b'/' {
                break;
            }
            if !(c.is_ascii_alphanumeric() || c == b'-') {
                id_end = id_start;
                break;
            }
            id_end += 1;
        }
        if id_end > id_start && bytes[id_end..].starts_with(SUFFIX) {
            if let Ok(id) = std::str::from_utf8(&bytes[id_start..id_end]) {
                if !ids.iter().any(|e| e == id) {
                    ids.push(id.to_string());
                }
            }
        }
        search_from = id_start.max(search_from + rel + 1);
    }
    ids
}

/// Whether `needle` (an absolute path) appears verbatim in the vmstate bytes.
fn vmstate_contains_path(bytes: &[u8], needle: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let needle = needle.as_os_str().as_bytes();
    if needle.is_empty() || needle.len() > bytes.len() {
        return false;
    }
    bytes.windows(needle.len()).any(|w| w == needle)
}

/// True if vmstate references a rootfs path under one of the baseline bind-mount dirs,
/// i.e. `<dir>/disks/rootfs.raw` for some `dir` in `covered_dirs` appears verbatim.
///
/// Presence-based on purpose (#638): we confirm the EXPECTED covered rootfs path is the
/// one vmstate stores, rather than enumerating every embedded path and rejecting unknowns
/// — the latter would false-abort a restore that legitimately attaches a read-only
/// external `--disk` pointing under another VM's `vm-disks/<id>/disks/...`. Checking the
/// exact absolute path (current `data_dir` prefix) also catches a same-id/different-prefix
/// mismatch that a vm-id reconstruction would miss.
fn vmstate_rootfs_covered(bytes: &[u8], covered_dirs: &[PathBuf]) -> bool {
    covered_dirs
        .iter()
        .any(|d| vmstate_contains_path(bytes, &d.join("disks").join("rootfs.raw")))
}

/// #608 real fix: refuse to restore if vmstate.bin does not reference a rootfs disk path
/// covered by the mount-namespace redirect (`vm_runtime_dir(original|snapshot)`).
///
/// `LoadSnapshot` reopens the rootfs path embedded in vmstate BEFORE `patch_drive`
/// retargets it; the redirect only covers the baseline VM dirs. If the embedded rootfs
/// path is not one of those (sibling VM, or a different `data_dir` prefix), restore would
/// open another VM's real disk and corrupt it (the ~0.7% #608 failure). Aborting here —
/// called at the very start of restore, before any holder/disk side effects — converts
/// that silent corruption into a clear, actionable error.
fn assert_vmstate_rootfs_covered(
    vmstate_path: &Path,
    original_vm_id: &str,
    snapshot_vm_id: Option<&str>,
) -> Result<()> {
    // Fail CLOSED: if we cannot read vmstate we cannot verify coverage, and Firecracker
    // may still open whatever path it has embedded — so abort rather than proceed blind.
    let bytes = std::fs::read(vmstate_path).with_context(|| {
        format!(
            "reading vmstate for #608 coverage check: {}",
            vmstate_path.display()
        )
    })?;
    let mut covered_dirs = vec![paths::vm_runtime_dir(original_vm_id)];
    if let Some(s) = snapshot_vm_id {
        if s != original_vm_id {
            covered_dirs.push(paths::vm_runtime_dir(s));
        }
    }
    if vmstate_rootfs_covered(&bytes, &covered_dirs) {
        // #608 diagnostics: the observed ~0.7% sibling-disk failure has never been
        // caught with its inputs visible (the metadata-divergence hypothesis was
        // empirically refuted — the embedded path's vm_id always matched). Log the
        // exact coverage inputs on every restore so the next occurrence is
        // self-diagnosing instead of a reconstruction from artifacts: which vm_ids
        // vmstate embeds, which dirs the bind-mount will cover, and the data_dir
        // prefix (a prefix change between create and restore is the leading
        // remaining hypothesis — #638-class).
        debug!(
            embedded_vm_ids = ?rootfs_disk_vm_ids_in_bytes(&bytes),
            covered_dirs = ?covered_dirs
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>(),
            data_dir = %paths::data_dir().display(),
            "#608 coverage check passed"
        );
        return Ok(());
    }
    anyhow::bail!(
        "#608: refusing to restore — vmstate.bin does not reference a rootfs disk under any \
         baseline bind-mount {:?} (it references vm-disks ids {:?}; data_dir prefix {}). \
         LoadSnapshot would open an uncovered/sibling VM's disk and corrupt it.",
        covered_dirs
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>(),
        rootfs_disk_vm_ids_in_bytes(&bytes),
        paths::data_dir().display(),
    )
}

/// Restore a VM from a snapshot.
///
/// This is the core snapshot restore logic shared by:
/// - `fcvm snapshot run` (clone with UFFD memory sharing)
/// - `fcvm podman run` with cache hit (direct file load)
///
/// Both paths use identical Firecracker setup, the only differences are:
/// - Memory backend: UFFD vs File
/// - Snapshot source: snapshots/{name} vs podman-cache/{hash}
pub async fn restore_from_snapshot(
    params: RestoreParams<'_>,
    network: &mut dyn NetworkManager,
    state_manager: &StateManager,
    vm_state: &mut VmState,
) -> Result<(FirecrackerBackend, Option<tokio::process::Child>)> {
    let RestoreParams {
        vm_id,
        vm_name,
        data_dir,
        socket_path,
        runtime_config,
        restore_config,
        network_config,
        clone_ipv6,
        track_dirty_pages,
    } = params;
    let vm_dir = data_dir.join("disks");

    // #608: abort BEFORE any side effects (holder/disk) if vmstate's rootfs path would not
    // be covered by the mount redirect — otherwise LoadSnapshot opens a sibling VM's disk.
    assert_vmstate_rootfs_covered(
        &restore_config.vmstate_path,
        &restore_config.original_vm_id,
        restore_config.snapshot_vm_id.as_deref(),
    )?;

    // Configure namespace isolation if network provides one
    let mut holder_child: Option<tokio::process::Child> = None;
    let mut holder_pid_for_post_start: Option<u32> = None;
    let fc_log_path = data_dir.join("firecracker.log");
    let mut vm_manager = VmManager::new(
        vm_id.to_string(),
        socket_path.to_path_buf(),
        Some(fc_log_path),
    );
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
    } else if let Some(pasta_net) = network.as_any().downcast_ref::<PastaNetwork>() {
        // Rootless mode: spawn holder process and set up namespace via nsenter
        // OPTIMIZATION: Parallelize disk creation with network setup

        // Step 1: Spawn holder process (keeps namespace alive)
        let holder_cmd = pasta_net.build_holder_command();
        info!(cmd = ?holder_cmd, "spawning namespace holder for rootless networking");

        let (mut child, holder_pid) = spawn_namespace_holder(&holder_cmd).await?;

        // Step 2: Run disk creation and network setup IN PARALLEL
        let setup_script = pasta_net.build_setup_script();
        let nsenter_prefix = pasta_net.build_nsenter_prefix(holder_pid);
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
            let verify_output = tokio::process::Command::new(&nsenter_prefix[0])
                .args(&nsenter_prefix[1..])
                .arg("ip")
                .arg("link")
                .arg("show")
                .arg(&tap_device)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
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
    } else if let Some(routed_net) = network
        .as_any()
        .downcast_ref::<crate::network::RoutedNetwork>()
    {
        // Routed mode: like bridged but with veth+IPv6 routing instead of iptables NAT
        if let Some(ns_id) = routed_net.namespace_id() {
            info!(namespace = %ns_id, "configuring VM to run in routed network namespace");
            vm_manager.set_namespace(ns_id.to_string());
        }

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
            "CoW disk prepared from snapshot (routed)"
        );
    } else {
        anyhow::bail!("Unknown network type");
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

    // (#608 coverage is asserted at the top of this function, before any side effects.)
    vm_manager.set_mount_redirects(baseline_dirs, data_dir.to_path_buf());

    // Copy extra disk images (disk-dir) from snapshot to clone's disk directory.
    // The mount namespace will redirect Firecracker's baseline paths to clone paths,
    // so these files need to exist at {vm_dir}/{filename}.
    if !restore_config.extra_disks.is_empty() {
        if let Some(ref snap_dir) = restore_config.snapshot_dir {
            for extra_disk in &restore_config.extra_disks {
                let source = snap_dir.join(&extra_disk.filename);
                let dest = vm_dir.join(&extra_disk.filename);
                reflink_copy(&source, &dest)
                    .await
                    .with_context(|| format!("copying extra disk {}", extra_disk.filename))?;
            }
            info!(
                num_disks = restore_config.extra_disks.len(),
                "copied {} extra disk image(s) to clone",
                restore_config.extra_disks.len()
            );
        }
    }

    let firecracker_bin = find_firecracker(runtime_config)?;
    let firecracker_args = runtime_config
        .firecracker_args
        .clone()
        .or_else(|| std::env::var("FCVM_FIRECRACKER_ARGS").ok());

    vm_manager
        .start(&firecracker_bin, None, firecracker_args.as_deref())
        .await
        .context("starting Firecracker")?;

    // Everything after start() runs inside a fallible block so a failure
    // (network post-start, snapshot load — e.g. an incompatible snapshot —
    // drive patch, resume, crash check, state save) KILLS the Firecracker
    // process before the error propagates. Callers may continue past a
    // restore failure (snapshot-cache invalidation falls back to a fresh
    // boot), so the process must not be left running; its parent-death
    // signal only fires when fcvm itself exits.
    let post_start = async {
        // For rootless mode with pasta: post_start starts pasta + bridge in the namespace
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
                track_dirty_pages: Some(track_dirty_pages),
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
            track_dirty_pages, "snapshot load completed"
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

        // FCVM_KVM_TRACE: enable KVM ftrace around VM resume for debugging snapshot restore.
        let kvm_trace = if std::env::var("FCVM_KVM_TRACE").is_ok() {
            match crate::kvm_trace::KvmTrace::start(&vm_state.vm_id) {
                Ok(t) => {
                    info!("KVM trace started for VM resume");
                    Some(t)
                }
                Err(e) => {
                    warn!("FCVM_KVM_TRACE: could not start KVM trace: {}", e);
                    None
                }
            }
        } else {
            None
        };

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

        // Signal fc-agent to flush ARP cache and reconnect output vsock via MMDS.
        // MUST be after VM resume — Firecracker accepts PUT /mmds while paused but
        // the guest-visible MMDS data isn't updated until after resume.
        let restore_epoch = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .context("system time before Unix epoch")?
            .as_secs();

        let mut mmds_latest = serde_json::json!({
            "host-time": chrono::Utc::now().timestamp().to_string(),
            "restore-epoch": restore_epoch.to_string()
        });
        if let Some(ref ipv6) = clone_ipv6 {
            mmds_latest["clone-ipv6"] = serde_json::Value::String(ipv6.clone());
        }
        client
            .put_mmds(serde_json::json!({ "latest": mmds_latest }))
            .await
            .context("updating MMDS with restore-epoch")?;
        info!(
            restore_epoch = restore_epoch,
            clone_ipv6 = ?clone_ipv6,
            "signaled fc-agent via MMDS"
        );

        // Stop KVM trace and dump results (captures resume + early VM execution)
        if let Some(trace) = kvm_trace {
            // Brief delay to capture initial KVM exits after resume
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            match trace.stop_and_dump() {
                Ok(path) => info!("KVM trace saved to {}", path),
                Err(e) => warn!("FCVM_KVM_TRACE: failed to save: {}", e),
            }
        }

        // Store fcvm process PID (not Firecracker PID)
        vm_state.pid = Some(std::process::id());

        // Track original vsock vm_id for future snapshots
        // When this VM is later snapshotted, clones need to use this original_vm_id
        // for vsock redirect because vmstate.bin stores paths from this vm
        vm_state.config.original_vsock_vm_id = Some(restore_config.original_vm_id.clone());

        // Update extra_disks in clone state with clone-local paths
        if !restore_config.extra_disks.is_empty() {
            vm_state.config.extra_disks = restore_config
                .extra_disks
                .iter()
                .map(|d| crate::state::types::ExtraDisk {
                    path: vm_dir.join(&d.filename).display().to_string(),
                    mount_path: d.mount_path.clone(),
                    read_only: d.read_only,
                })
                .collect();
        }

        // Post-resume liveness check: verify VM didn't crash immediately.
        // Under heavy I/O load, snapshot restore can corrupt guest memory (e.g., stack
        // canary in do_idle), causing an immediate kernel panic + reboot. Detecting this
        // early surfaces the error instead of silently serving a crashed VM.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        if let Some(status) = vm_manager.try_wait()? {
            bail!(
                "VM crashed immediately after snapshot restore (exit status: {:?}). \
                 This can happen under heavy I/O load due to memory corruption during restore.",
                status
            );
        }

        // Save VM state with complete network configuration
        save_vm_state_with_network(state_manager, vm_state, network_config).await?;

        Ok::<(), anyhow::Error>(())
    }
    .await;
    if let Err(e) = post_start {
        warn!(error = %format!("{e:#}"), "restore failed after Firecracker start; killing the process");
        if let Err(kill_err) = vm_manager.kill().await {
            warn!(error = %kill_err, "failed to kill Firecracker after restore failure");
        }
        return Err(e);
    }

    // Wrap the fully-restored, resumed VmManager as the Firecracker backend. The restore
    // path is Firecracker-specific (snapshot format, external UFFD, patch_drive, MMDS
    // restore-epoch) and operates on the raw VmManager above; the caller holds it through
    // the hypervisor trait.
    Ok((
        FirecrackerBackend::from_vm_manager(vm_manager),
        holder_child,
    ))
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
/// Copy a file using btrfs reflink (instant CoW copy).
pub(crate) async fn reflink_copy(source: &Path, dest: &Path) -> Result<()> {
    let source_str = source
        .to_str()
        .with_context(|| format!("non-UTF-8 path: {}", source.display()))?;
    let dest_str = dest
        .to_str()
        .with_context(|| format!("non-UTF-8 path: {}", dest.display()))?;
    let result = tokio::process::Command::new("cp")
        .args(["--reflink=always", source_str, dest_str])
        .status()
        .await
        .with_context(|| format!("reflink copy {} -> {}", source.display(), dest.display()))?;

    if !result.success() {
        anyhow::bail!(
            "Reflink copy failed ({} -> {}) - btrfs filesystem required",
            source.display(),
            dest.display()
        );
    }
    Ok(())
}

/// Limit concurrent snapshot creation to prevent dirty_ratio writeback throttling.
///
/// Each full snapshot writes the VM's entire configured memory (default 1GB) to page cache.
/// The Linux kernel throttles ALL writers when dirty pages exceed `dirty_ratio` (typically
/// 20% of RAM). On a 125GB machine with default dirty_ratio=20%, that's 25GB.
/// When 150 VMs snapshot simultaneously (CI SnapshotEnabled mode), total dirty pages
/// cause the kernel to force synchronous writeback, stalling each snapshot for 100+ seconds.
///
/// With a semaphore of 10, peak dirty pages stay low enough that snapshots complete
/// at memory speed (~1s each) without triggering kernel writeback throttling.
static SNAPSHOT_SEMAPHORE: OnceLock<Semaphore> = OnceLock::new();

fn snapshot_semaphore() -> &'static Semaphore {
    SNAPSHOT_SEMAPHORE.get_or_init(|| {
        let permits = std::env::var("FCVM_SNAPSHOT_CONCURRENCY")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(10);
        Semaphore::new(permits)
    })
}

/// Build a SnapshotConfig from VmState. Single source of truth for snapshot metadata.
///
/// Both user-triggered snapshots (`fcvm snapshot create`) and cache snapshots
/// (pre-start/startup) use this to ensure consistent metadata. The key fields
/// that must stay in sync — `original_vsock_vm_id`, network config, health check
/// URL, etc. — all come from VmState.
///
/// Callers provide volume and extra_disk configs because those are stored in
/// different formats (VolumeConfig vs SnapshotVolumeConfig, ExtraDisk vs
/// SnapshotExtraDisk) and the conversion depends on context.
pub fn build_snapshot_config(
    vm_state: &VmState,
    snapshot_key: &str,
    snapshot_type: crate::storage::SnapshotType,
    snapshot_dir: &std::path::Path,
    volumes: Vec<crate::storage::SnapshotVolumeConfig>,
    extra_disks: Vec<crate::storage::SnapshotExtraDisk>,
) -> crate::storage::SnapshotConfig {
    let original_vsock_vm_id = vm_state
        .config
        .original_vsock_vm_id
        .clone()
        .unwrap_or_else(|| vm_state.vm_id.clone());

    crate::storage::SnapshotConfig {
        name: snapshot_key.to_string(),
        vm_id: vm_state.vm_id.clone(),
        original_vsock_vm_id: Some(original_vsock_vm_id),
        parent_snapshot: None, // Set by create_snapshot_core after determining diff base
        memory_path: snapshot_dir.join("memory.bin"),
        vmstate_path: snapshot_dir.join("vmstate.bin"),
        disk_path: snapshot_dir.join("disk.raw"),
        created_at: chrono::Utc::now(),
        snapshot_type,
        // Prod builder always produces Full snapshots; the disk-only path (P2)
        // sets DiskOnly in create_disk_only_snapshot_core.
        kind: crate::storage::SnapshotKind::Full,
        metadata: crate::storage::SnapshotMetadata {
            image: vm_state.config.image.clone(),
            vcpu: vm_state.config.vcpu,
            memory_mib: vm_state.config.memory_mib,
            network_config: vm_state.config.network.clone(),
            volumes,
            health_check_url: vm_state.config.health_check_url.clone(),
            health_check_timeout: vm_state.config.health_check_timeout,
            hugepages: vm_state.config.hugepages,
            extra_disks,
            nfs_shares: vm_state.config.nfs_shares.clone(),
            username: vm_state.config.username.clone(),
            user: vm_state.config.user.clone(),
            port_mappings: vm_state.config.port_mappings.clone(),
            forward_localhost: vm_state.config.forward_localhost.clone(),
            network_mode: vm_state.config.network_mode,
            ipv6_prefix: vm_state.config.ipv6_prefix.clone(),
            tty: vm_state.config.tty,
            interactive: vm_state.config.interactive,
            kernel_profile: vm_state.config.kernel_profile.clone(),
            image_mode: vm_state.config.image_mode.clone(),
            image_disk_path: vm_state.config.image_disk_path.clone(),
            hypervisor: vm_state.config.hypervisor,
        },
    }
}

/// Convert VolumeConfig objects to SnapshotVolumeConfig for snapshot metadata.
pub fn volume_configs_to_snapshot(
    volume_configs: &[crate::volume::VolumeConfig],
) -> Vec<crate::storage::SnapshotVolumeConfig> {
    volume_configs
        .iter()
        .map(|v| crate::storage::SnapshotVolumeConfig {
            host_path: v.host_path.clone(),
            guest_path: v.guest_path.to_string_lossy().to_string(),
            read_only: v.read_only,
            vsock_port: v.port,
            portable: v.portable,
        })
        .collect()
}

/// Convert VmState extra_disks to SnapshotExtraDisk for snapshot metadata.
///
/// Only includes disks inside the VM's data directory (disk-dir disks).
/// External --disk files are at arbitrary host paths and don't need copying
/// into the snapshot — clones access them directly.
pub fn extra_disks_to_snapshot(vm_state: &VmState) -> Vec<crate::storage::SnapshotExtraDisk> {
    let vm_data_dir = paths::vm_runtime_dir(&vm_state.vm_id);
    let vm_data_prefix = vm_data_dir.to_string_lossy().to_string();
    vm_state
        .config
        .extra_disks
        .iter()
        .filter(|disk| disk.path.starts_with(&vm_data_prefix))
        .filter_map(|disk| {
            let filename = std::path::Path::new(&disk.path)
                .file_name()?
                .to_str()?
                .to_string();
            let index = filename
                .strip_prefix("disk-dir-")
                .and_then(|s| s.strip_suffix(".raw"))
                .unwrap_or("0");
            let drive_id = format!("disk{}", index);
            Some(crate::storage::SnapshotExtraDisk {
                filename,
                mount_path: disk.mount_path.clone(),
                read_only: disk.read_only,
                drive_id,
            })
        })
        .collect()
}

/// Acquire per-VM snapshot lock.
///
/// Serializes all snapshot operations on the same Firecracker VM.
/// Callers MUST hold this lock across the entire snapshot operation (including
/// reading the VM state to determine the parent snapshot key — otherwise a
/// concurrent startup snapshot can reset the KVM dirty bitmap while we hold
/// a stale parent reference, producing a corrupt merged snapshot).
///
/// disk_path is like `.../vm-disks/{vm_id}/disks/rootfs.raw` — lock is placed
/// in the vm_id directory.
pub async fn acquire_vm_snapshot_lock(disk_path: &Path) -> Result<std::fs::File> {
    let vm_dir = disk_path
        .parent()
        .and_then(|p| p.parent())
        .ok_or_else(|| anyhow::anyhow!("cannot derive VM directory from disk path"))?;
    let lock_path = vm_dir.join("snapshot.lock");
    let lock_file = std::fs::File::create(&lock_path)
        .with_context(|| format!("creating snapshot lock: {}", lock_path.display()))?;
    use fs2::FileExt;
    loop {
        match lock_file.try_lock_exclusive() {
            Ok(()) => break,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                debug!("waiting for per-VM snapshot lock");
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            Err(e) => {
                return Err(anyhow::anyhow!("acquiring per-VM snapshot lock: {}", e));
            }
        }
    }
    debug!(lock = %lock_path.display(), "acquired per-VM snapshot lock");
    Ok(lock_file)
}

/// Sibling path for a snapshot directory's auxiliary files (lock, .creating,
/// .old): APPENDS `.suffix` to the directory name. `Path::with_extension` would
/// REPLACE everything after the last dot, so a tag like "app.v1" would map onto
/// "app.creating"/"app.old"/"app.lock" — colliding with (and deleting) files of
/// an unrelated tag.
pub(crate) fn snapshot_sibling(dir: &Path, suffix: &str) -> std::path::PathBuf {
    let name = dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    dir.with_file_name(format!("{name}.{suffix}"))
}

/// Acquire the per-snapshot directory lock (`<snapshot_dir>.lock`).
///
/// Creators take it exclusively while writing or atomically replacing a snapshot
/// directory; restore paths take it shared so an in-flight restore never observes
/// the directory mid-swap (mixing one generation's disk.raw with another
/// generation's memory.bin/vmstate.bin) or mid-removal.
pub async fn acquire_snapshot_dir_lock(
    snapshot_dir: &Path,
    exclusive: bool,
) -> Result<std::fs::File> {
    let lock_path = snapshot_sibling(snapshot_dir, "lock");
    let lock_file = std::fs::File::create(&lock_path)
        .with_context(|| format!("creating snapshot lock: {}", lock_path.display()))?;
    loop {
        // Fully-qualified fs2 calls: std::fs::File now has inherent try_lock_*
        // methods with a different error type, and inherent methods win over
        // trait methods.
        let result = if exclusive {
            fs2::FileExt::try_lock_exclusive(&lock_file)
        } else {
            fs2::FileExt::try_lock_shared(&lock_file)
        };
        match result {
            Ok(()) => break,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                debug!(lock = %lock_path.display(), "waiting for per-snapshot lock");
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            Err(e) => {
                return Err(anyhow::anyhow!("acquiring per-snapshot lock: {}", e));
            }
        }
    }
    debug!(lock = %lock_path.display(), exclusive, "acquired per-snapshot lock");
    Ok(lock_file)
}

/// Extra files to write into the snapshot directory before it is atomically finalized.
///
/// Returns (filename, contents) pairs. Invoked after the Firecracker snapshot is taken
/// (and the VM resumed) so contents reflect host-side state at snapshot time — e.g.
/// portable-volume inode tables.
pub type SnapshotExtraFiles<'a> = Option<&'a (dyn Fn() -> Vec<(String, Vec<u8>)> + Send + Sync)>;

/// Disk-only capture: quiesce the guest, reflink only the disk (no memory dump,
/// no vCPU pause — `fsfreeze` provides consistency), unfreeze, and finalize a
/// `DiskOnly` snapshot. Clones cold-boot from this disk. See
/// docs/disk-only-clone.html.
///
/// `snapshot_config.kind` must already be `DiskOnly`. `vsock_socket` is the VM's
/// exec vsock (`vm_runtime_dir/<vm_id>/vsock.sock`), used to run the quiesce
/// commands in the guest.
///
/// # Locking
/// Caller holds the per-snapshot-dir + per-VM locks (same as `create_snapshot_core`).
pub async fn create_disk_only_snapshot_core(
    snapshot_config: crate::storage::snapshot::SnapshotConfig,
    disk_path: &Path,
    vsock_socket: &Path,
) -> Result<()> {
    use crate::commands::exec::run_exec_in_vm_captured;

    let snapshot_dir = snapshot_config
        .disk_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("invalid disk_path in snapshot config"))?
        .to_path_buf();
    let temp_snapshot_dir = snapshot_sibling(&snapshot_dir, "creating");

    let _ = tokio::fs::remove_dir_all(&temp_snapshot_dir).await;
    tokio::fs::create_dir_all(&temp_snapshot_dir)
        .await
        .context("creating temp snapshot directory")?;

    // The provisioned marker gates the clone's boot behavior: without it, a clone
    // of this disk would WIPE the captured container storage on boot. Refuse to
    // capture a disk that isn't marked (old fc-agent, or provisioning failed).
    let marker_cmd = vec![
        "test".to_string(),
        "-f".to_string(),
        "/var/lib/fcvm/provisioned".to_string(),
    ];
    let marker_out = run_exec_in_vm_captured(vsock_socket, &marker_cmd, false)
        .await
        .context("checking provisioned marker in guest")?;
    if marker_out.exit_code != 0 {
        let _ = tokio::fs::remove_dir_all(&temp_snapshot_dir).await;
        anyhow::bail!(
            "source VM has no provisioned marker (/var/lib/fcvm/provisioned); a clone \
             of this disk would wipe its container storage on boot. The source must be \
             running a current fc-agent that completed provisioning."
        );
    }

    // Quiesce so the reflink captures a crash-consistent filesystem. No vCPU pause.
    // `sync` flushes dirty pages, then fsfreeze blocks new writes across the reflink.
    // The podman container store is its own filesystem (btrfs loopback mounted at
    // /var/lib/containers/storage, backed by a file on the rootfs) — it must be
    // frozen FIRST, then the rootfs: freezing it flushes its data through the loop
    // device into the backing file while the rootfs is still writable. (Freezing the
    // rootfs first would deadlock that flush.) Unfreeze happens in reverse order.
    let sync_cmd = vec!["sync".to_string()];
    let sync_out = run_exec_in_vm_captured(vsock_socket, &sync_cmd, false)
        .await
        .context("guest sync before freeze")?;
    if sync_out.exit_code != 0 {
        let _ = tokio::fs::remove_dir_all(&temp_snapshot_dir).await;
        anyhow::bail!(
            "guest sync failed before disk capture (exit {}): {}",
            sync_out.exit_code,
            sync_out.stderr
        );
    }

    const STORAGE_MOUNT: &str = "/var/lib/containers/storage";
    let freeze_script = format!(
        "if findmnt -n {m} >/dev/null 2>&1; then fsfreeze --freeze {m} || exit 1; fi; \
         fsfreeze --freeze /",
        m = STORAGE_MOUNT
    );
    let freeze_cmd = vec!["sh".to_string(), "-c".to_string(), freeze_script];
    // An exec-level error (vsock reset after the request was written) can leave the
    // guest frozen with no response — always attempt a best-effort thaw of BOTH
    // mounts before bailing, never leave the source wedged.
    let freeze_out = match run_exec_in_vm_captured(vsock_socket, &freeze_cmd, false).await {
        Ok(out) => out,
        Err(e) => {
            let thaw = vec![
                "sh".to_string(),
                "-c".to_string(),
                format!(
                    "fsfreeze --unfreeze / 2>/dev/null; \
                     fsfreeze --unfreeze {m} 2>/dev/null; true",
                    m = STORAGE_MOUNT
                ),
            ];
            let _ = run_exec_in_vm_captured(vsock_socket, &thaw, false).await;
            let _ = tokio::fs::remove_dir_all(&temp_snapshot_dir).await;
            return Err(e).context("freezing guest filesystems");
        }
    };
    if freeze_out.exit_code != 0 {
        // Best-effort thaw in case the storage mount froze but the rootfs didn't.
        let thaw = vec![
            "sh".to_string(),
            "-c".to_string(),
            format!(
                "fsfreeze --unfreeze {m} 2>/dev/null; true",
                m = STORAGE_MOUNT
            ),
        ];
        let _ = run_exec_in_vm_captured(vsock_socket, &thaw, false).await;
        let _ = tokio::fs::remove_dir_all(&temp_snapshot_dir).await;
        anyhow::bail!(
            "fsfreeze failed (exit {}): {}",
            freeze_out.exit_code,
            freeze_out.stderr
        );
    }
    info!(snapshot = %snapshot_config.name, "guest frozen; reflinking disk");

    // Reflink the disk (+ any disk-dir images) while frozen.
    let copy_result: Result<()> = async {
        let temp_disk = temp_snapshot_dir.join("disk.raw");
        reflink_copy(disk_path, &temp_disk).await?;
        for extra in &snapshot_config.metadata.extra_disks {
            let source = paths::vm_runtime_dir(&snapshot_config.vm_id)
                .join("disks")
                .join(&extra.filename);
            let dest = temp_snapshot_dir.join(&extra.filename);
            reflink_copy(&source, &dest).await?;
        }
        Ok(())
    }
    .await;

    // Unfreeze ALWAYS, in reverse order (rootfs first so the loop device can write
    // again, then the storage mount). A failed unfreeze wedges the source VM — that
    // is a hard error even when the reflink succeeded, so the caller knows.
    let unfreeze_script = format!(
        "fsfreeze --unfreeze /; rc=$?; \
         if findmnt -n {m} >/dev/null 2>&1; then fsfreeze --unfreeze {m} || rc=1; fi; \
         exit $rc",
        m = STORAGE_MOUNT
    );
    let unfreeze_cmd = vec!["sh".to_string(), "-c".to_string(), unfreeze_script];
    let unfreeze_result = run_exec_in_vm_captured(vsock_socket, &unfreeze_cmd, false).await;

    if let Err(e) = copy_result {
        let _ = tokio::fs::remove_dir_all(&temp_snapshot_dir).await;
        return Err(e).context("reflinking disk for disk-only snapshot");
    }

    match unfreeze_result {
        Ok(o) if o.exit_code == 0 => info!(snapshot = %snapshot_config.name, "guest unfrozen"),
        Ok(o) => {
            let _ = tokio::fs::remove_dir_all(&temp_snapshot_dir).await;
            anyhow::bail!(
                "fsfreeze --unfreeze failed (exit {}): {} — source VM may be wedged",
                o.exit_code,
                o.stderr
            );
        }
        Err(e) => {
            let _ = tokio::fs::remove_dir_all(&temp_snapshot_dir).await;
            return Err(e).context("unfreezing guest — source VM may be wedged");
        }
    }

    // Write config.json (kind = DiskOnly; no memory.bin / vmstate.bin).
    let temp_config = temp_snapshot_dir.join("config.json");
    let config_json = serde_json::to_string_pretty(&snapshot_config)
        .context("serializing disk-only snapshot config")?;
    tokio::fs::write(&temp_config, &config_json)
        .await
        .context("writing disk-only snapshot config")?;

    // Atomic replace into the final location.
    if snapshot_dir.exists() {
        let old = snapshot_sibling(&snapshot_dir, "old");
        let _ = tokio::fs::remove_dir_all(&old).await;
        tokio::fs::rename(&snapshot_dir, &old)
            .await
            .context("moving old snapshot aside")?;
        tokio::fs::rename(&temp_snapshot_dir, &snapshot_dir)
            .await
            .context("finalizing snapshot")?;
        let _ = tokio::fs::remove_dir_all(&old).await;
    } else {
        tokio::fs::rename(&temp_snapshot_dir, &snapshot_dir)
            .await
            .context("finalizing snapshot")?;
    }

    info!(snapshot = %snapshot_config.name, "disk-only snapshot created");
    Ok(())
}

/// Create a snapshot of the running VM.
///
/// # Locking
/// Caller MUST hold the per-VM snapshot lock (via `acquire_vm_snapshot_lock`)
/// before calling this function.
///
/// # Returns
/// Ok(()) on success, Err on failure. VM is resumed regardless of success/failure.
pub async fn create_snapshot_core(
    client: &crate::firecracker::FirecrackerClient,
    mut snapshot_config: crate::storage::snapshot::SnapshotConfig,
    disk_path: &Path,
    parent_snapshot_dir: Option<&Path>,
    extra_files: SnapshotExtraFiles<'_>,
) -> Result<()> {
    use crate::firecracker::api::{SnapshotCreate, VmState as ApiVmState};

    // Acquire snapshot concurrency permit BEFORE pausing the VM.
    // This prevents dirty_ratio throttling when many VMs snapshot simultaneously.
    let _permit = snapshot_semaphore()
        .acquire()
        .await
        .map_err(|e| anyhow::anyhow!("snapshot semaphore closed: {}", e))?;

    // Derive directories from snapshot config (memory_path's parent is the snapshot dir)
    let snapshot_dir = snapshot_config
        .memory_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("invalid memory_path in snapshot config"))?;
    let temp_snapshot_dir = snapshot_sibling(snapshot_dir, "creating");

    // Determine base memory for diff snapshot support.
    //
    // Firecracker resets the dirty bitmap after each snapshot, so the diff only
    // contains pages dirtied since the LAST snapshot (not the original restore).
    // The merge base MUST be the immediate parent — skipping levels corrupts memory.
    //
    // The caller provides parent_snapshot_dir from VmState.config.snapshot_name,
    // which tracks the last snapshot created from (or restored into) this VM.
    // This is always the correct diff base.
    let (has_base, base_memory_source, diff_parent_name) =
        if let Some(parent_dir) = parent_snapshot_dir {
            let parent_memory = parent_dir.join("memory.bin");
            if parent_memory.exists() {
                let parent_name = parent_dir
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| parent_dir.display().to_string());
                info!(
                    snapshot = %snapshot_config.name,
                    parent = %parent_dir.display(),
                    "using parent memory.bin as diff base"
                );
                (true, Some(parent_memory), Some(parent_name))
            } else {
                (false, None, None)
            }
        } else {
            (false, None, None)
        };

    // Clean up stale snapshot directory (e.g., from a previous aborted attempt).
    // A directory with memory.bin but no config.json is incomplete — remove it
    // to prevent confusion and reclaim disk space.
    if snapshot_dir.exists() && !snapshot_dir.join("config.json").exists() {
        info!(snapshot = %snapshot_config.name, "cleaning up stale snapshot directory");
        let _ = tokio::fs::remove_dir_all(snapshot_dir).await;
    }

    let snapshot_type = if has_base { "Diff" } else { "Full" };

    // Check available disk space before attempting snapshot.
    // A full snapshot dumps all VM memory; a diff snapshot is smaller but still needs space.
    // Failing ENOSPC mid-snapshot corrupts the VM (Firecracker can't resume properly).
    {
        let memory_bytes = (snapshot_config.metadata.memory_mib as u64) * 1024 * 1024;
        // For full snapshots, need ~memory_mib. For diff, need ~10% as buffer.
        let required_bytes = if has_base {
            memory_bytes / 10
        } else {
            memory_bytes
        };
        // Use parent directory for statvfs if snapshot_dir was cleaned up
        let statvfs_path = if snapshot_dir.exists() {
            snapshot_dir
        } else {
            snapshot_dir.parent().unwrap_or(snapshot_dir)
        };
        if let Ok(stat) = nix::sys::statvfs::statvfs(statvfs_path) {
            let available_bytes = stat.blocks_available() * stat.fragment_size();
            if available_bytes < required_bytes {
                anyhow::bail!(
                    "Not enough disk space for {} snapshot: need {} MiB, have {} MiB free on {}. \
                     Use --mem to limit VM memory, or increase btrfs_size in rootfs-config.toml.",
                    snapshot_type.to_lowercase(),
                    required_bytes / (1024 * 1024),
                    available_bytes / (1024 * 1024),
                    snapshot_dir.display()
                );
            }
        }
    }

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

    // For diff snapshots, write to memory.diff so we can merge onto memory.bin
    // For full snapshots, write directly to memory.bin
    let temp_memory_path = if has_base {
        temp_snapshot_dir.join("memory.diff")
    } else {
        temp_snapshot_dir.join("memory.bin")
    };
    let temp_vmstate_path = temp_snapshot_dir.join("vmstate.bin");

    // Pause timeout: should be fast (just pauses vCPU threads). 30s is generous.
    // Snapshot timeout: scales with memory size for large VMs.
    // 64GB at ~500MB/s ≈ 128s for full, but with dirty page tracking overhead can be 2-3x.
    // Formula: max(300, mem_gib * 10) seconds — 300s minimum, 64GB=640s, 128GB=1280s.
    let mem_gib = snapshot_config.metadata.memory_mib / 1024;
    let snapshot_timeout_secs = std::cmp::max(300, (mem_gib as u64) * 10);
    let pause_client = client.with_timeout(std::time::Duration::from_secs(30));
    let snapshot_client =
        client.with_timeout(std::time::Duration::from_secs(snapshot_timeout_secs));

    // Pause VM before snapshotting (required by Firecracker).
    // If Pause fails/times out, the VM is NOT paused — no resume needed.
    info!(snapshot = %snapshot_config.name, "pausing VM for snapshot");
    if let Err(e) = pause_client
        .patch_vm_state(ApiVmState {
            state: "Paused".to_string(),
        })
        .await
    {
        let _ = tokio::fs::remove_dir_all(&temp_snapshot_dir).await;
        return Err(e).context("pausing VM for snapshot (VM still running, no data loss)");
    }

    // VM is now paused — we MUST resume it before returning, no matter what.
    let mut use_diff = has_base;
    let snapshot_result = snapshot_client
        .create_snapshot(SnapshotCreate {
            snapshot_type: Some(snapshot_type.to_string()),
            snapshot_path: temp_vmstate_path.display().to_string(),
            mem_file_path: temp_memory_path.display().to_string(),
        })
        .await;

    // Validate diff snapshot: detect KVM dirty page tracking failure.
    //
    // On ARM64 under load, KVM can silently lose the dirty bitmap — the diff snapshot
    // captures only device-emulation pages (~94 KB) while missing all guest OS writes
    // (~37-43 MB). Restoring such a snapshot kernel-panics with:
    //   "stack-protector: Kernel stack is corrupted in: do_idle"
    // because the vmstate has startup-time registers but memory has pre-start content.
    //
    // Detection: check the diff file's actual disk usage. A VM that ran from pre-start
    // to healthy MUST have dirtied at least 0.1% of memory (~1 MB for a 1 GB VM).
    // If the diff is smaller, retry as Full while the VM is still paused.
    if snapshot_result.is_ok() && has_base {
        use std::os::unix::fs::MetadataExt;
        if let Ok(meta) = std::fs::metadata(&temp_memory_path) {
            let diff_allocated = meta.blocks() * 512;
            let memory_bytes = (snapshot_config.metadata.memory_mib as u64) * 1024 * 1024;
            // 0.1% of VM memory — a VM that started a container must dirty at least this much.
            // Empirically, pre-start → startup diffs are 36-43 MB for a 1 GB VM (3.5-4.1%).
            let min_diff_bytes = memory_bytes / 1024;

            if diff_allocated < min_diff_bytes {
                error!(
                    snapshot = %snapshot_config.name,
                    diff_allocated_bytes = diff_allocated,
                    diff_file_size = meta.len(),
                    min_expected_bytes = min_diff_bytes,
                    memory_mib = snapshot_config.metadata.memory_mib,
                    "diff snapshot too small — KVM dirty page tracking lost guest writes. \
                     Retrying as Full snapshot (VM still paused)."
                );

                // Remove bad diff file and retry as Full
                let _ = std::fs::remove_file(&temp_memory_path);
                let full_memory_path = temp_snapshot_dir.join("memory.bin");
                match snapshot_client
                    .create_snapshot(SnapshotCreate {
                        snapshot_type: Some("Full".to_string()),
                        snapshot_path: temp_vmstate_path.display().to_string(),
                        mem_file_path: full_memory_path.display().to_string(),
                    })
                    .await
                {
                    Ok(()) => {
                        use_diff = false;
                        info!(
                            snapshot = %snapshot_config.name,
                            "Full snapshot retry succeeded"
                        );
                    }
                    Err(e) => {
                        // Full retry failed — resume VM and abort
                        let _ = snapshot_client
                            .patch_vm_state(ApiVmState {
                                state: "Resumed".to_string(),
                            })
                            .await;
                        let _ = tokio::fs::remove_dir_all(&temp_snapshot_dir).await;
                        return Err(e)
                            .context("Full snapshot retry failed after diff tracking failure");
                    }
                }
            }
        }
    }

    // Copy disk while VM is still paused to maintain memory/disk consistency.
    // If we copy after resume, the disk may have post-resume writes that don't
    // match the snapshot's memory state. This causes filesystem corruption on
    // restore (e.g., btrfs detects inconsistent transaction log and goes read-only).
    // Reflink copy is instant (O(1) metadata operation), so pause time is not affected.
    let disk_copy_result = if snapshot_result.is_ok() {
        let temp_disk_path = temp_snapshot_dir.join("disk.raw");
        info!(snapshot = %snapshot_config.name, "copying disk (VM paused)");
        let r = reflink_copy(disk_path, &temp_disk_path).await;

        // Also copy extra disks while paused
        if r.is_ok() {
            let mut extra_ok = true;
            for extra_disk in &snapshot_config.metadata.extra_disks {
                let source = paths::vm_runtime_dir(&snapshot_config.vm_id)
                    .join("disks")
                    .join(&extra_disk.filename);
                let dest = temp_snapshot_dir.join(&extra_disk.filename);
                if let Err(e) = reflink_copy(&source, &dest).await {
                    error!(error = %e, disk = %extra_disk.filename, "failed to copy extra disk");
                    extra_ok = false;
                    break;
                }
            }
            if extra_ok {
                Ok(())
            } else {
                Err(anyhow::anyhow!("extra disk copy failed"))
            }
        } else {
            r
        }
    } else {
        Ok(()) // Skip disk copy if snapshot failed
    };

    // Resume VM (ALWAYS, regardless of snapshot/disk copy result).
    // Memory merge happens after resume since it operates on snapshot files, not live disk.
    let resume_result = snapshot_client
        .patch_vm_state(ApiVmState {
            state: "Resumed".to_string(),
        })
        .await;

    if let Err(e) = &resume_result {
        // Resume failure is critical — VM may be stuck paused.
        error!(snapshot = %snapshot_config.name, error = %e,
            "CRITICAL: failed to resume VM after snapshot — VM may be paused!");
    }

    // Check results — clean up temp dir on failure
    if let Err(e) = snapshot_result {
        let _ = tokio::fs::remove_dir_all(&temp_snapshot_dir).await;
        return Err(e).context("creating Firecracker snapshot");
    }
    if let Err(e) = disk_copy_result {
        let _ = tokio::fs::remove_dir_all(&temp_snapshot_dir).await;
        return Err(e).context("copying disk during snapshot");
    }
    if let Err(e) = resume_result {
        let _ = tokio::fs::remove_dir_all(&temp_snapshot_dir).await;
        return Err(e).context("resuming VM after snapshot");
    }

    info!(snapshot = %snapshot_config.name, "VM resumed, processing snapshot");

    // NOTE: Do NOT bump restore-epoch here. Snapshot create (pause → dump → resume)
    // does NOT reset vsock connections — empirically verified with scratch VMs.
    // VIRTIO_VSOCK_EVENT_TRANSPORT_RESET only occurs on snapshot RESTORE (loading
    // a new VM from snapshot files), not on create. Bumping restore-epoch here
    // would trigger handle_clone_restore() in fc-agent, which kills TCP connections
    // and reconnects FUSE/output vsock unnecessarily, crashing the running container.
    // restore-epoch is bumped in the restore path (snapshot.rs) where it's needed.

    if use_diff {
        // Diff snapshot: copy base to temp, merge diff onto it, then atomic rename
        // At this point:
        //   - temp_memory_path = memory.diff (Firecracker wrote the sparse diff here)
        //   - base_memory_source = parent or existing snapshot's memory.bin (never in snapshot_dir
        //     without config.json — stale dirs were cleaned up above)
        let base_source = base_memory_source
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("has_base=true but no base_memory_source"))?;
        let diff_file_path = temp_memory_path.clone(); // memory.diff
        let final_memory_path = temp_snapshot_dir.join("memory.bin");

        info!(
            snapshot = %snapshot_config.name,
            base = %base_source.display(),
            diff = %diff_file_path.display(),
            "merging diff snapshot onto base copy"
        );

        // Copy base memory to temp dir as memory.bin (will merge diff into this copy)
        tokio::fs::copy(base_source, &final_memory_path)
            .await
            .context("copying base memory to temp for merge")?;

        // Run merge in blocking task since it's CPU/IO bound
        // Merge from memory.diff onto memory.bin
        let merge_target = final_memory_path.clone();
        let merge_source = diff_file_path.clone();
        let bytes_merged =
            tokio::task::spawn_blocking(move || merge_diff_snapshot(&merge_target, &merge_source))
                .await
                .context("diff merge task panicked")?
                .context("merging diff snapshot")?;

        info!(
            snapshot = %snapshot_config.name,
            bytes_merged = bytes_merged,
            "diff merge complete, building atomic update"
        );
    }

    // Write caller-provided extra files (e.g. portable-volume inode tables) into the
    // temp directory BEFORE the atomic rename. A finalized snapshot (config.json present)
    // must never be missing these files — clones would silently restore without them —
    // so a write failure here fails the snapshot instead of being logged and ignored.
    if let Some(extra_files) = extra_files {
        for (filename, contents) in extra_files() {
            let path = temp_snapshot_dir.join(&filename);
            if let Err(e) = tokio::fs::write(&path, &contents).await {
                let _ = tokio::fs::remove_dir_all(&temp_snapshot_dir).await;
                return Err(e).with_context(|| format!("writing snapshot extra file {}", filename));
            }
        }
    }

    // Record parent snapshot for the diff chain.
    // Full snapshots: parent = None (self-contained).
    // Diff snapshots: parent = name of snapshot whose memory.bin was the merge base.
    snapshot_config.parent_snapshot = if use_diff { diff_parent_name } else { None };

    // Write config.json to temp directory
    let temp_config_path = temp_snapshot_dir.join("config.json");
    let config_json =
        serde_json::to_string_pretty(&snapshot_config).context("serializing snapshot config")?;
    tokio::fs::write(&temp_config_path, &config_json)
        .await
        .context("writing snapshot config")?;

    // Atomic replace: rename old out of the way, then rename new into place.
    // Handles both cases: snapshot_dir exists (re-creating) or doesn't (first creation).
    if snapshot_dir.exists() {
        let old_snapshot_dir = snapshot_sibling(snapshot_dir, "old");
        let _ = tokio::fs::remove_dir_all(&old_snapshot_dir).await;
        tokio::fs::rename(snapshot_dir, &old_snapshot_dir)
            .await
            .context("moving old snapshot out of the way")?;
        tokio::fs::rename(&temp_snapshot_dir, snapshot_dir)
            .await
            .context("renaming temp snapshot to final location")?;
        let _ = tokio::fs::remove_dir_all(&old_snapshot_dir).await;
    } else {
        tokio::fs::rename(&temp_snapshot_dir, snapshot_dir)
            .await
            .context("renaming temp snapshot to final location")?;
    }

    let actual_type = if use_diff { "Diff" } else { "Full" };
    info!(
        snapshot = %snapshot_config.name,
        snapshot_type = actual_type,
        disk = %snapshot_config.disk_path.display(),
        "snapshot created successfully"
    );

    Ok(())
}

/// Subdirectory inside a snapshot directory holding Cloud Hypervisor's own snapshot files
/// (its `config.json`, `state.json`, and memory ranges). Kept under a subdir so CH's
/// `config.json` does not collide with fcvm's top-level `config.json` (the `SnapshotConfig`
/// metadata). The CH restore path passes `file://{snapshot_dir}/{CH_SNAPSHOT_SUBDIR}` as the
/// `--restore source_url`.
pub const CH_SNAPSHOT_SUBDIR: &str = "ch";

/// Cloud Hypervisor snapshot create. Full snapshots only — CH has no diff/dirty-page
/// tracking in fcvm yet (#632 P2). Mirrors [`create_snapshot_core`]'s scaffolding (temp
/// dir, paused-disk reflink for memory/disk consistency, atomic rename) but drives CH's
/// `vm.pause` → `vm.snapshot` → `vm.resume` instead of Firecracker's snapshot API.
///
/// The VM is resumed regardless of success or failure, mirroring the Firecracker path.
pub async fn create_snapshot_ch(
    client: &crate::hypervisor::cloud_hypervisor::api::ChClient,
    mut snapshot_config: crate::storage::snapshot::SnapshotConfig,
    disk_path: &Path,
) -> Result<()> {
    // CH snapshots are always self-contained (no diff base / parent chain).
    snapshot_config.parent_snapshot = None;

    // Acquire the same concurrency permit the Firecracker path uses, to bound simultaneous
    // memory dumps (dirty_ratio throttling) across backends.
    let _permit = snapshot_semaphore()
        .acquire()
        .await
        .map_err(|e| anyhow::anyhow!("snapshot semaphore closed: {}", e))?;

    // Owned so no borrow of `snapshot_config` is held across the later config write.
    let snapshot_dir = snapshot_config
        .memory_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("invalid memory_path in snapshot config"))?
        .to_path_buf();
    let snapshot_dir = snapshot_dir.as_path();
    let temp_snapshot_dir = snapshot_sibling(snapshot_dir, "creating");

    // Clean up a stale (incomplete) snapshot dir — one with no config.json.
    if snapshot_dir.exists() && !snapshot_dir.join("config.json").exists() {
        info!(snapshot = %snapshot_config.name, "cleaning up stale snapshot directory");
        let _ = tokio::fs::remove_dir_all(snapshot_dir).await;
    }

    // Disk-space guard: a full memory dump needs ~memory_mib. Failing ENOSPC mid-dump
    // would leave the VM in a bad state.
    {
        let required_bytes = (snapshot_config.metadata.memory_mib as u64) * 1024 * 1024;
        let statvfs_path = if snapshot_dir.exists() {
            snapshot_dir
        } else {
            snapshot_dir.parent().unwrap_or(snapshot_dir)
        };
        if let Ok(stat) = nix::sys::statvfs::statvfs(statvfs_path) {
            let available_bytes = stat.blocks_available() * stat.fragment_size();
            if available_bytes < required_bytes {
                anyhow::bail!(
                    "Not enough disk space for Cloud Hypervisor snapshot: need {} MiB, \
                     have {} MiB free on {}.",
                    required_bytes / (1024 * 1024),
                    available_bytes / (1024 * 1024),
                    snapshot_dir.display()
                );
            }
        }
    }

    // Fresh temp dir + the CH sub-dir CH dumps its files into.
    let _ = tokio::fs::remove_dir_all(&temp_snapshot_dir).await;
    let ch_dir = temp_snapshot_dir.join(CH_SNAPSHOT_SUBDIR);
    tokio::fs::create_dir_all(&ch_dir)
        .await
        .context("creating temp CH snapshot directory")?;

    info!(snapshot = %snapshot_config.name, "pausing Cloud Hypervisor VM for snapshot");
    // Pause first (CH requires a paused VM to snapshot). If pause fails the VM is NOT
    // paused, so there is nothing to resume.
    if let Err(e) = client.pause_vm().await {
        let _ = tokio::fs::remove_dir_all(&temp_snapshot_dir).await;
        return Err(e).context("pausing Cloud Hypervisor VM for snapshot (VM still running)");
    }

    // VM is paused — we MUST resume it before returning, regardless of outcome.
    let dest_url = format!("file://{}", ch_dir.display());
    let snapshot_result = client.snapshot_vm(&dest_url).await;

    // Reflink the disk(s) WHILE PAUSED so the disk image matches the captured memory
    // (a post-resume write would desync memory/disk and corrupt the clone's filesystem).
    let disk_copy_result = if snapshot_result.is_ok() {
        let temp_disk_path = temp_snapshot_dir.join("disk.raw");
        info!(snapshot = %snapshot_config.name, "copying disk (CH VM paused)");
        let mut r = reflink_copy(disk_path, &temp_disk_path).await;
        if r.is_ok() {
            for extra_disk in &snapshot_config.metadata.extra_disks {
                let source = paths::vm_runtime_dir(&snapshot_config.vm_id)
                    .join("disks")
                    .join(&extra_disk.filename);
                let dest = temp_snapshot_dir.join(&extra_disk.filename);
                if let Err(e) = reflink_copy(&source, &dest).await {
                    r = Err(e).context("copying CH extra disk");
                    break;
                }
            }
        }
        r
    } else {
        Ok(())
    };

    // Resume ALWAYS, even if the snapshot or disk copy failed.
    let resume_result = client.resume_vm().await;
    if let Err(e) = &resume_result {
        error!(snapshot = %snapshot_config.name, error = %e,
            "CRITICAL: failed to resume Cloud Hypervisor VM after snapshot — VM may be paused!");
    }

    if let Err(e) = snapshot_result {
        let _ = tokio::fs::remove_dir_all(&temp_snapshot_dir).await;
        return Err(e).context("creating Cloud Hypervisor snapshot");
    }
    if let Err(e) = disk_copy_result {
        let _ = tokio::fs::remove_dir_all(&temp_snapshot_dir).await;
        return Err(e).context("copying disk during Cloud Hypervisor snapshot");
    }
    if let Err(e) = resume_result {
        let _ = tokio::fs::remove_dir_all(&temp_snapshot_dir).await;
        return Err(e).context("resuming Cloud Hypervisor VM after snapshot");
    }

    info!(snapshot = %snapshot_config.name, "CH VM resumed, finalizing snapshot");

    // Write fcvm's metadata config.json, then atomically swap the dir into place.
    let temp_config_path = temp_snapshot_dir.join("config.json");
    let config_json =
        serde_json::to_string_pretty(&snapshot_config).context("serializing snapshot config")?;
    tokio::fs::write(&temp_config_path, &config_json)
        .await
        .context("writing snapshot config")?;

    if snapshot_dir.exists() {
        let old_snapshot_dir = snapshot_sibling(snapshot_dir, "old");
        let _ = tokio::fs::remove_dir_all(&old_snapshot_dir).await;
        tokio::fs::rename(snapshot_dir, &old_snapshot_dir)
            .await
            .context("moving old snapshot out of the way")?;
        tokio::fs::rename(&temp_snapshot_dir, snapshot_dir)
            .await
            .context("renaming temp snapshot to final location")?;
        let _ = tokio::fs::remove_dir_all(&old_snapshot_dir).await;
    } else {
        tokio::fs::rename(&temp_snapshot_dir, snapshot_dir)
            .await
            .context("renaming temp snapshot to final location")?;
    }

    info!(
        snapshot = %snapshot_config.name,
        disk = %snapshot_config.disk_path.display(),
        "Cloud Hypervisor snapshot created successfully"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::VmState;
    use crate::storage::SnapshotType;
    use std::path::Path;

    /// `with_extension` would map "app.v1" onto "app.creating", colliding with an
    /// unrelated tag's files — snapshot_sibling must APPEND instead.
    #[test]
    fn snapshot_sibling_appends_suffix_even_with_dotted_tags() {
        assert_eq!(
            snapshot_sibling(Path::new("/snaps/app"), "creating"),
            Path::new("/snaps/app.creating")
        );
        assert_eq!(
            snapshot_sibling(Path::new("/snaps/app.v1"), "creating"),
            Path::new("/snaps/app.v1.creating")
        );
        assert_eq!(
            snapshot_sibling(Path::new("/snaps/app.v1"), "old"),
            Path::new("/snaps/app.v1.old")
        );
        // Distinct dotted tags must never collide on their auxiliary files.
        assert_ne!(
            snapshot_sibling(Path::new("/snaps/app.v1"), "lock"),
            snapshot_sibling(Path::new("/snaps/app.v2"), "lock")
        );
    }

    fn make_vm_state(vm_id: &str, original_vsock: Option<&str>) -> VmState {
        let mut state = VmState::new(vm_id.to_string(), "nginx:alpine".to_string(), 2, 1024);
        state.config.network = NetworkConfig::default();
        state.config.health_check_url = Some("http://localhost/".to_string());
        state.config.hugepages = false;
        state.config.username = Some("testuser".to_string());
        state.config.user = Some("1000:1000".to_string());
        state.config.original_vsock_vm_id = original_vsock.map(|s| s.to_string());
        state
    }

    #[test]
    fn test_build_snapshot_config_fresh_vm() {
        // Fresh VM: no original_vsock_vm_id → falls back to vm_id
        let state = make_vm_state("vm-AAA", None);
        let config = build_snapshot_config(
            &state,
            "test-key",
            SnapshotType::System,
            Path::new("/tmp/snap"),
            vec![],
            vec![],
        );
        assert_eq!(config.vm_id, "vm-AAA");
        assert_eq!(config.original_vsock_vm_id, Some("vm-AAA".to_string()));
        assert_eq!(config.metadata.image, "nginx:alpine");
        assert_eq!(config.metadata.memory_mib, 1024);
        assert_eq!(
            config.metadata.health_check_url,
            Some("http://localhost/".to_string())
        );
    }

    #[test]
    fn test_build_snapshot_config_cache_restored_vm() {
        // Cache-restored VM: original_vsock_vm_id set → preserved in config
        let state = make_vm_state("vm-BBB", Some("vm-AAA"));
        let config = build_snapshot_config(
            &state,
            "test-startup",
            SnapshotType::System,
            Path::new("/tmp/snap"),
            vec![],
            vec![],
        );
        assert_eq!(config.vm_id, "vm-BBB");
        // Critical: original_vsock_vm_id must be vm-AAA (the ORIGINAL), not vm-BBB
        assert_eq!(config.original_vsock_vm_id, Some("vm-AAA".to_string()));
    }

    #[test]
    fn test_build_snapshot_config_user_snapshot() {
        let state = make_vm_state("vm-CCC", Some("vm-AAA"));
        let config = build_snapshot_config(
            &state,
            "my-snapshot",
            SnapshotType::User,
            Path::new("/tmp/snap"),
            vec![],
            vec![],
        );
        assert_eq!(config.vm_id, "vm-CCC");
        assert_eq!(config.original_vsock_vm_id, Some("vm-AAA".to_string()));
        assert!(matches!(config.snapshot_type, SnapshotType::User));
    }

    #[test]
    fn test_build_snapshot_config_paths() {
        let state = make_vm_state("vm-AAA", None);
        let config = build_snapshot_config(
            &state,
            "key",
            SnapshotType::System,
            Path::new("/mnt/snap/key"),
            vec![],
            vec![],
        );
        assert_eq!(config.memory_path, Path::new("/mnt/snap/key/memory.bin"));
        assert_eq!(config.vmstate_path, Path::new("/mnt/snap/key/vmstate.bin"));
        assert_eq!(config.disk_path, Path::new("/mnt/snap/key/disk.raw"));
    }

    #[test]
    fn test_extra_disks_to_snapshot_filters_external() {
        // Only disks inside vm_runtime_dir should be included
        let mut state = make_vm_state("vm-test123", None);
        state.config.extra_disks = vec![
            // Disk inside data dir → should be included
            crate::state::types::ExtraDisk {
                path: format!(
                    "{}/disk-dir-0.raw",
                    paths::vm_runtime_dir("vm-test123").display()
                ),
                mount_path: "/data".to_string(),
                read_only: false,
            },
            // External disk → should be excluded
            crate::state::types::ExtraDisk {
                path: "/external/disk.raw".to_string(),
                mount_path: "/ext".to_string(),
                read_only: true,
            },
        ];
        let result = extra_disks_to_snapshot(&state);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].filename, "disk-dir-0.raw");
        assert_eq!(result[0].mount_path, "/data");
        assert_eq!(result[0].drive_id, "disk0");
    }

    #[test]
    fn test_extra_disks_to_snapshot_empty() {
        let state = make_vm_state("vm-empty", None);
        let result = extra_disks_to_snapshot(&state);
        assert!(result.is_empty());
    }

    #[test]
    fn test_volume_configs_to_snapshot() {
        let configs = vec![crate::volume::VolumeConfig {
            host_path: std::path::PathBuf::from("/host/data"),
            guest_path: std::path::PathBuf::from("/guest/data"),
            read_only: true,
            port: 5000,
            portable: false,
        }];
        let result = volume_configs_to_snapshot(&configs);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].host_path, Path::new("/host/data"));
        assert_eq!(result[0].guest_path, "/guest/data");
        assert!(result[0].read_only);
        assert_eq!(result[0].vsock_port, 5000);
        assert!(!result[0].portable);
    }

    #[test]
    fn test_merge_diff_snapshot_applies_dirty_pages() {
        use std::io::Write;
        use tempfile::NamedTempFile;

        // Create a base memory file (64 KB)
        let mut base = NamedTempFile::new().unwrap();
        let base_data = vec![0xAAu8; 65536];
        base.write_all(&base_data).unwrap();
        base.flush().unwrap();

        // Create a diff file: sparse, with data only at offset 4096 and 8192
        let diff = NamedTempFile::new().unwrap();
        let diff_path = diff.path().to_path_buf();
        {
            use std::os::unix::io::AsRawFd;
            let fd = diff.as_file().as_raw_fd();

            // Set file size to match base (sparse)
            nix::unistd::ftruncate(unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) }, 65536)
                .unwrap();

            // Write dirty page at offset 4096 (page 1)
            let dirty_page = vec![0xBBu8; 4096];
            nix::sys::uio::pwrite(diff.as_file(), &dirty_page, 4096).unwrap();

            // Write dirty page at offset 8192 (page 2)
            let dirty_page2 = vec![0xCCu8; 4096];
            nix::sys::uio::pwrite(diff.as_file(), &dirty_page2, 8192).unwrap();
        }

        // Merge diff onto base
        let bytes = merge_diff_snapshot(base.path(), &diff_path).unwrap();
        assert_eq!(bytes, 8192, "should merge exactly 2 pages (8192 bytes)");

        // Verify: base[0..4096] = 0xAA (unchanged)
        //         base[4096..8192] = 0xBB (from diff)
        //         base[8192..12288] = 0xCC (from diff)
        //         base[12288..] = 0xAA (unchanged)
        let result = std::fs::read(base.path()).unwrap();
        assert_eq!(result.len(), 65536);
        assert!(
            result[..4096].iter().all(|&b| b == 0xAA),
            "page 0 should be unchanged"
        );
        assert!(
            result[4096..8192].iter().all(|&b| b == 0xBB),
            "page 1 should be 0xBB from diff"
        );
        assert!(
            result[8192..12288].iter().all(|&b| b == 0xCC),
            "page 2 should be 0xCC from diff"
        );
        assert!(
            result[12288..].iter().all(|&b| b == 0xAA),
            "remaining pages should be unchanged"
        );
    }

    #[test]
    fn test_merge_diff_snapshot_empty_diff_returns_zero() {
        use std::io::Write;
        use tempfile::NamedTempFile;

        // Create base and empty sparse diff
        let mut base = NamedTempFile::new().unwrap();
        base.write_all(&vec![0xAAu8; 4096]).unwrap();
        base.flush().unwrap();

        let diff = NamedTempFile::new().unwrap();
        let diff_path = diff.path().to_path_buf();
        {
            use std::os::unix::io::AsRawFd;
            let fd = diff.as_file().as_raw_fd();
            nix::unistd::ftruncate(unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) }, 4096)
                .unwrap();
        }

        // Empty diff → zero bytes merged
        let bytes = merge_diff_snapshot(base.path(), &diff_path).unwrap();
        assert_eq!(bytes, 0, "empty diff should merge zero bytes");

        // Base should be unchanged
        let result = std::fs::read(base.path()).unwrap();
        assert!(result.iter().all(|&b| b == 0xAA));
    }

    #[test]
    fn test_parent_snapshot_set_on_full() {
        // Full snapshots should have parent_snapshot = None
        let state = make_vm_state("vm-AAA", None);
        let config = build_snapshot_config(
            &state,
            "my-snap",
            SnapshotType::User,
            Path::new("/tmp/snap"),
            vec![],
            vec![],
        );
        assert!(config.parent_snapshot.is_none());
    }

    #[test]
    fn test_parent_snapshot_serialization_roundtrip() {
        // parent_snapshot should survive JSON serialization
        let state = make_vm_state("vm-AAA", None);
        let mut config = build_snapshot_config(
            &state,
            "startup",
            SnapshotType::System,
            Path::new("/tmp/snap"),
            vec![],
            vec![],
        );
        config.parent_snapshot = Some("pre-start-abc123".to_string());

        let json = serde_json::to_string(&config).unwrap();
        let deserialized: crate::storage::SnapshotConfig = serde_json::from_str(&json).unwrap();

        assert_eq!(
            deserialized.parent_snapshot,
            Some("pre-start-abc123".to_string())
        );
    }

    #[test]
    fn test_parent_snapshot_backward_compatible() {
        // Old config.json without parent_snapshot should deserialize with None
        let json = r#"{
            "name": "old-snap",
            "vm_id": "vm-OLD",
            "memory_path": "/tmp/memory.bin",
            "vmstate_path": "/tmp/vmstate.bin",
            "disk_path": "/tmp/disk.raw",
            "created_at": "2026-01-01T00:00:00Z",
            "metadata": {
                "image": "nginx:alpine",
                "vcpu": 2,
                "memory_mib": 1024,
                "network_config": {
                    "tap_device": "tap0",
                    "guest_mac": "02:00:00:00:00:00",
                    "guest_ip": "10.0.2.100/24",
                    "host_ip": "10.0.2.2"
                },
                "volumes": [],
                "health_check_url": null,
                "health_check_timeout": 5,
                "hugepages": false,
                "extra_disks": [],
                "port_mappings": [],
                "network_mode": "rootless",
                "tty": false,
                "interactive": false
            }
        }"#;

        let config: crate::storage::SnapshotConfig = serde_json::from_str(json).unwrap();
        assert!(
            config.parent_snapshot.is_none(),
            "missing field should default to None"
        );
    }

    #[test]
    fn test_parent_chain_walkable() {
        // Simulate a chain: pre-start (Full) → startup (Diff) → user-snap (Diff)
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let snap_root = tmp.path();

        // Helper to write a snapshot config to disk
        let write_config = |name: &str, parent: Option<&str>, vm_id: &str| {
            let dir = snap_root.join(name);
            std::fs::create_dir_all(&dir).unwrap();
            let state = make_vm_state(vm_id, None);
            let mut config =
                build_snapshot_config(&state, name, SnapshotType::System, &dir, vec![], vec![]);
            config.parent_snapshot = parent.map(|s| s.to_string());
            let json = serde_json::to_string_pretty(&config).unwrap();
            std::fs::write(dir.join("config.json"), &json).unwrap();
        };

        write_config("pre-start-abc", None, "vm-AAA");
        write_config("startup-def", Some("pre-start-abc"), "vm-AAA");
        write_config("my-snap", Some("startup-def"), "vm-BBB");

        // Walk the chain from user_snap back to root
        let mut chain = vec![];
        let mut current_name = Some("my-snap".to_string());
        while let Some(name) = current_name {
            let dir = snap_root.join(&name);
            let config_json = std::fs::read_to_string(dir.join("config.json")).unwrap();
            let config: crate::storage::SnapshotConfig =
                serde_json::from_str(&config_json).unwrap();
            chain.push(config.name.clone());
            current_name = config.parent_snapshot.clone();
        }

        assert_eq!(chain, vec!["my-snap", "startup-def", "pre-start-abc"]);
    }

    /// #608 diagnostic scanner: recover embedded rootfs vm-disks ids from a binary blob,
    /// de-duplicated, ignoring binary noise, non-canonical paths, and — crucially — a
    /// non-rootfs external disk that merely lives under a vm-disks dir.
    #[test]
    fn test_rootfs_disk_vm_ids_in_vmstate() {
        let mut buf: Vec<u8> = vec![0x00, 0xFF, 0x07];
        buf.extend_from_slice(b"/mnt/fcvm-btrfs/vm-disks/vm-aaa111/disks/rootfs.raw");
        buf.push(0x13);
        // a second, distinct rootfs (different vm)
        buf.extend_from_slice(b"/mnt/fcvm-btrfs/vm-disks/vm-bbb222/disks/rootfs.raw");
        // duplicate of the first -> deduped
        buf.extend_from_slice(b"/mnt/fcvm-btrfs/vm-disks/vm-aaa111/disks/rootfs.raw");
        // an external (non-rootfs) disk under a vm-disks dir -> must be ignored
        buf.extend_from_slice(b"/mnt/fcvm-btrfs/vm-disks/vm-ext999/disks/data.raw");
        buf.extend_from_slice(b"\x00garbage");

        let mut ids = rootfs_disk_vm_ids_in_bytes(&buf);
        ids.sort();
        assert_eq!(ids, vec!["vm-aaa111".to_string(), "vm-bbb222".to_string()]);

        assert!(rootfs_disk_vm_ids_in_bytes(&[]).is_empty());
    }

    #[test]
    fn test_vmstate_rootfs_coverage() {
        // A snapshot whose rootfs lives under vm-aaa111, plus a read-only external --disk
        // that points at ANOTHER vm's rootfs.raw (vm-ext999) — the latter must NOT cause a
        // false "covered" nor (in the assert) a false abort.
        let vmstate = b"\x00pre\x00/data/vm-disks/vm-aaa111/disks/rootfs.raw\x00\
                        /data/vm-disks/vm-ext999/disks/rootfs.raw\x00tail"
            .as_slice();

        // The baseline bind-mount covers vm-aaa111 -> covered.
        assert!(vmstate_rootfs_covered(
            vmstate,
            &[PathBuf::from("/data/vm-disks/vm-aaa111")]
        ));
        // Same id, DIFFERENT data_dir prefix -> NOT covered (a vm-id reconstruction would
        // have wrongly said covered). This is the #638 regression class.
        assert!(!vmstate_rootfs_covered(
            vmstate,
            &[PathBuf::from("/other/vm-disks/vm-aaa111")]
        ));
        // A baseline for a vm whose rootfs is NOT the one stored -> NOT covered.
        assert!(!vmstate_rootfs_covered(
            vmstate,
            &[PathBuf::from("/data/vm-disks/vm-zzz000")]
        ));

        // vmstate_contains_path: exact match required, empty/oversized are false.
        assert!(vmstate_contains_path(
            vmstate,
            Path::new("/data/vm-disks/vm-ext999/disks/rootfs.raw")
        ));
        assert!(!vmstate_contains_path(vmstate, Path::new("")));
        assert!(!vmstate_contains_path(
            b"x".as_slice(),
            Path::new("/long/path")
        ));
    }
}
