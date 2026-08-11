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

/// Vsock port for the restored guest's exact restore-generation completion ACK.
/// The host binds this before VMM resume and does not publish lifecycle readiness
/// until fc-agent confirms the same UUID after all restore phases have succeeded.
/// Must match `fc-agent::vsock::RESTORE_COMPLETE_PORT`.
pub const VSOCK_RESTORE_COMPLETE_PORT: u32 = 4994;

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

    // Check version. Cached per binary identity (path + mtime + size): this runs
    // on every VM launch and clone, and the answer only changes when the binary
    // does — a rebuilt or swapped firecracker is re-probed. See version_cache.
    let version_str =
        crate::version_cache::version_output(&firecracker_bin).with_context(|| {
            format!(
                "checking firecracker version at {}",
                firecracker_bin.display()
            )
        })?;
    let version = parse_firecracker_version(&version_str).with_context(|| {
        format!(
            "binary: {}, stdout: {:?}",
            firecracker_bin.display(),
            version_str.trim(),
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

/// Locate the Cloud Hypervisor binary (#632).
///
/// Resolution order:
///   1. `FCVM_CLOUD_HYPERVISOR_BIN` (explicit override)
///   2. newest content-addressed binary built by `fcvm setup --cloud-hypervisor`
///      (assets_dir/cloud-hypervisor/cloud-hypervisor-<sha>.bin) — offline, no git ls-remote
///   3. `cloud-hypervisor` on PATH
///
/// The content-addressed lookup carries the fork build (aarch64 SVE register fix,
/// CH #8268, post-v52.0) that CH snapshot/restore requires. Cold boot (P1) works with
/// any v52+; the cached build is preferred so CI and dev hosts use the pinned fork.
pub fn find_cloud_hypervisor() -> Result<std::path::PathBuf> {
    // Run `<bin> --version`; returns the trimmed version string on success.
    // Cached per binary identity (only successful probes are cached), so a
    // binary that cannot run here keeps failing and keeps falling through.
    fn version_of(bin: &std::path::Path) -> Option<String> {
        crate::version_cache::version_output(bin)
            .ok()
            .map(|s| s.trim().to_string())
    }

    // 1. Explicit override — must exist and run; no fallback (the user asked for THIS one).
    if let Ok(path) = std::env::var("FCVM_CLOUD_HYPERVISOR_BIN") {
        let p = std::path::PathBuf::from(&path);
        if !p.exists() {
            anyhow::bail!("FCVM_CLOUD_HYPERVISOR_BIN={} does not exist", path);
        }
        match version_of(&p) {
            Some(v) => {
                debug!(
                    "Found Cloud Hypervisor via FCVM_CLOUD_HYPERVISOR_BIN {:?}: {}",
                    p, v
                );
                return Ok(p);
            }
            None => anyhow::bail!("FCVM_CLOUD_HYPERVISOR_BIN={} failed `--version`", path),
        }
    }

    // 2. Content-addressed build. A cached binary that can't run here (e.g. an assets_dir
    //    shared across an incompatible arch/libc) must NOT mask a working PATH binary —
    //    fall through to PATH instead of failing.
    if let Some(cached) = crate::setup::newest_cached_cloud_hypervisor() {
        match version_of(&cached) {
            Some(v) => {
                debug!(
                    "Found Cloud Hypervisor (content-addressed build) {:?}: {}",
                    cached, v
                );
                return Ok(cached);
            }
            None => warn!(
                path = %cached.display(),
                "cached cloud-hypervisor failed `--version`; falling back to PATH"
            ),
        }
    }

    // 3. PATH.
    let bin = which::which("cloud-hypervisor").context(
        "cloud-hypervisor not found: build it with `fcvm setup --cloud-hypervisor`, \
         set FCVM_CLOUD_HYPERVISOR_BIN, or install it on PATH",
    )?;
    match version_of(&bin) {
        Some(v) => {
            debug!("Found Cloud Hypervisor on PATH {:?}: {}", bin, v);
            Ok(bin)
        }
        None => anyhow::bail!(
            "cloud-hypervisor on PATH ({}) failed `--version`",
            bin.display()
        ),
    }
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
    // Keep the owner's in-memory state identical to the persisted process identity.
    // StateManager::save_state also records this on its private clone, but snapshot
    // creation later revalidates the long-lived in-memory state against disk.
    vm_state.pid_start_time = crate::utils::process_start_time(fcvm_pid);

    // Mark VM as running and persist to disk
    vm_state.status = VmStatus::Running;
    state_manager
        .save_state(vm_state)
        .await
        .context("persisting VM state to disk")?;

    Ok(())
}

/// Atomically publish that the VM lifecycle is safe for external observers to act on.
///
/// State and PID are written earlier because setup components need them, but an observer
/// must not capture children or deliver a lifecycle signal until the owner has installed
/// every long-lived resource. The identity predicate is evaluated while the state-file
/// update lock is held, so a stale/replaced record is never marked ready.
pub async fn publish_lifecycle_ready(
    state_manager: &StateManager,
    vm_state: &mut VmState,
) -> Result<()> {
    let expected_vm_id = vm_state.vm_id.clone();
    let expected_pid = vm_state
        .pid
        .ok_or_else(|| anyhow::anyhow!("cannot publish lifecycle readiness without a PID"))?;
    let expected_start = vm_state.pid_start_time.ok_or_else(|| {
        anyhow::anyhow!("cannot publish lifecycle readiness without a process start time")
    })?;
    anyhow::ensure!(
        crate::utils::process_start_time(expected_pid) == Some(expected_start),
        "cannot publish lifecycle readiness for stale PID {} start {}",
        expected_pid,
        expected_start
    );

    let mut identity_matched = false;
    let updated = state_manager
        .update_state(&expected_vm_id, |state| {
            if state.vm_id == expected_vm_id
                && state.pid == Some(expected_pid)
                && state.pid_start_time == Some(expected_start)
            {
                state.lifecycle_ready = true;
                identity_matched = true;
            }
        })
        .await
        .context("publishing VM lifecycle readiness")?;
    anyhow::ensure!(
        updated.is_some(),
        "VM state disappeared before lifecycle readiness could be published"
    );
    anyhow::ensure!(
        identity_matched,
        "VM state identity changed before lifecycle readiness could be published"
    );

    vm_state.lifecycle_ready = true;
    info!(vm_id = %vm_state.vm_id, "VM lifecycle ready");
    Ok(())
}

const LIFECYCLE_SETTING_UP: u8 = 0;
const LIFECYCLE_PUBLISHING: u8 = 1;
const LIFECYCLE_READY: u8 = 2;
const LIFECYCLE_CANCELLED: u8 = 3;

/// Linearizes cancellation against the one transition that publishes lifecycle readiness.
///
/// Signal handlers must call [`LifecycleReadyGate::cancel`] instead of cancelling the
/// token directly. A cancellation that claims the gate while setup is still in progress
/// prevents the state write. If publication claims it first, the owning task awaits that
/// write before it can observe the cancellation and enter cleanup, so cleanup can never
/// race an in-flight readiness write.
#[derive(Clone)]
pub struct LifecycleReadyGate {
    phase: std::sync::Arc<std::sync::atomic::AtomicU8>,
    cancel: tokio_util::sync::CancellationToken,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LifecycleReadyOutcome {
    Published,
    Cancelled,
}

impl LifecycleReadyGate {
    pub fn new() -> Self {
        Self {
            phase: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(LIFECYCLE_SETTING_UP)),
            cancel: tokio_util::sync::CancellationToken::new(),
        }
    }

    pub fn cancellation_token(&self) -> tokio_util::sync::CancellationToken {
        self.cancel.clone()
    }

    /// Record cancellation before waking code that can begin cleanup.
    pub fn cancel(&self) {
        let _ = self.phase.compare_exchange(
            LIFECYCLE_SETTING_UP,
            LIFECYCLE_CANCELLED,
            std::sync::atomic::Ordering::SeqCst,
            std::sync::atomic::Ordering::SeqCst,
        );
        self.cancel.cancel();
    }

    /// Claim a terminal, synchronous publication without writing VM readiness state.
    ///
    /// Finite lifecycle commands use this immediately before their final response. A signal
    /// that claims setup first prevents the response; once publication claims the gate, a
    /// later signal cannot turn an already-linearized success into a cancellation race.
    pub fn claim_terminal_publication(&self) -> Result<LifecycleReadyOutcome> {
        match self.phase.compare_exchange(
            LIFECYCLE_SETTING_UP,
            LIFECYCLE_READY,
            std::sync::atomic::Ordering::SeqCst,
            std::sync::atomic::Ordering::SeqCst,
        ) {
            Ok(_) | Err(LIFECYCLE_READY) => Ok(LifecycleReadyOutcome::Published),
            Err(LIFECYCLE_CANCELLED) => Ok(LifecycleReadyOutcome::Cancelled),
            Err(LIFECYCLE_PUBLISHING) => {
                bail!("lifecycle readiness publication is already in progress")
            }
            Err(phase) => bail!("invalid lifecycle readiness phase {phase}"),
        }
    }

    /// Publish readiness only if cancellation did not claim the setup phase first.
    pub async fn publish(
        &self,
        state_manager: &StateManager,
        vm_state: &mut VmState,
    ) -> Result<LifecycleReadyOutcome> {
        // Test-only when armed; the normal fast path is a single environment lookup.
        // Holding here gives the lifecycle interleave regression an exact boundary
        // between a caller's cancellation precheck and this gate's linearization CAS.
        failpoint::hit_async("lifecycle.before_ready_claim").await;
        match self.phase.compare_exchange(
            LIFECYCLE_SETTING_UP,
            LIFECYCLE_PUBLISHING,
            std::sync::atomic::Ordering::SeqCst,
            std::sync::atomic::Ordering::SeqCst,
        ) {
            Ok(_) => {}
            Err(LIFECYCLE_CANCELLED) => return Ok(LifecycleReadyOutcome::Cancelled),
            Err(LIFECYCLE_READY) => return Ok(LifecycleReadyOutcome::Published),
            Err(LIFECYCLE_PUBLISHING) => {
                bail!("lifecycle readiness publication is already in progress")
            }
            Err(phase) => bail!("invalid lifecycle readiness phase {phase}"),
        }

        match publish_lifecycle_ready(state_manager, vm_state).await {
            Ok(()) => {
                self.phase
                    .store(LIFECYCLE_READY, std::sync::atomic::Ordering::SeqCst);
                Ok(LifecycleReadyOutcome::Published)
            }
            Err(error) => {
                self.phase
                    .store(LIFECYCLE_CANCELLED, std::sync::atomic::Ordering::SeqCst);
                self.cancel.cancel();
                Err(error)
            }
        }
    }
}

impl Default for LifecycleReadyGate {
    fn default() -> Self {
        Self::new()
    }
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
    pub output_listener_handle: Option<JoinHandle<()>>,
}

/// Errors observed while tearing down independent VM resources.
///
/// Cleanup must always attempt every resource even when an earlier action fails.  Normal
/// `run` keeps its existing best-effort contract, while `prepare` uses the verified wrapper
/// below and refuses to publish success when this collection is non-empty.
#[derive(Default)]
struct CleanupFailures {
    failures: Vec<String>,
}

impl CleanupFailures {
    /// Log and collect a failed teardown action. `action` is a gerund phrase naming the
    /// resource ("cleaning network"), so it reads as a sentence in both the warning and
    /// the aggregated error.
    fn record<T>(&mut self, action: &str, result: Result<T>) {
        if let Err(error) = result {
            warn!("failed while {action}: {error:#}");
            self.failures.push(format!("{action}: {error:#}"));
        }
    }

    fn into_result(self) -> Result<()> {
        if self.failures.is_empty() {
            Ok(())
        } else {
            bail!("verified VM cleanup failed: {}", self.failures.join("; "))
        }
    }
}

/// How long teardown waits for the health monitor to observe its cancellation before
/// aborting it. Cleanup must then await the abort before tearing down network resources:
/// dropping a live `JoinHandle` detaches its task, which could otherwise keep using the
/// network namespace after cleanup has begun destroying it.
const HEALTH_MONITOR_STOP_BUDGET: std::time::Duration = std::time::Duration::from_millis(100);

/// Stop and reap the exact health-monitor task before resource teardown continues.
///
/// The timeout borrows the handle. If graceful cancellation misses its budget, ownership
/// therefore remains here so the task can be aborted and awaited; moving the handle directly
/// into a `select!` arm would drop and detach it when the timeout arm wins.
async fn stop_health_monitor(mut handle: JoinHandle<()>) {
    match tokio::time::timeout(HEALTH_MONITOR_STOP_BUDGET, &mut handle).await {
        Ok(Ok(())) => debug!("health monitor stopped gracefully"),
        Ok(Err(error)) => {
            warn!(?error, "health monitor task failed while stopping");
        }
        Err(_) => {
            debug!(
                budget_ms = HEALTH_MONITOR_STOP_BUDGET.as_millis(),
                "health monitor didn't stop in time; aborting it"
            );
            handle.abort();
            match handle.await {
                Ok(()) => debug!("health monitor completed while its abort was being delivered"),
                Err(error) if error.is_cancelled() => {
                    debug!("health monitor aborted and reaped");
                }
                Err(error) => {
                    warn!(?error, "health monitor task failed while being aborted");
                }
            }
        }
    }
}

/// Total CPU charged to this process's REAPED children so far (`RUSAGE_CHILDREN`).
///
/// A child's ENTIRE lifetime CPU lands here at the moment it is reaped, not as it runs — so
/// a raw delta across teardown is the VMM's whole-life CPU (measured: 12.3 CPU-seconds for
/// a VM that lived a couple of minutes), NOT the cost of tearing it down. See
/// [`process_cpu`] for the subtraction that turns this into a reclaim figure.
fn reaped_children_cpu() -> std::time::Duration {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
    // SAFETY: getrusage fully initializes the rusage it is given; we only read it on success.
    if unsafe { libc::getrusage(libc::RUSAGE_CHILDREN, usage.as_mut_ptr()) } != 0 {
        return std::time::Duration::ZERO;
    }
    let usage = unsafe { usage.assume_init() };
    let to_duration = |t: libc::timeval| {
        std::time::Duration::new(t.tv_sec.max(0) as u64, (t.tv_usec.max(0) as u32) * 1000)
    };
    to_duration(usage.ru_utime) + to_duration(usage.ru_stime)
}

/// CPU (`utime + stime`) consumed so far by a LIVE process, from `/proc/<pid>/stat`.
///
/// Sampled just before the SIGKILL, this is what a child had already spent while doing its
/// job. Subtracting it from that child's whole-life CPU (credited to `RUSAGE_CHILDREN` when
/// it is reaped) leaves exactly the CPU it accrued after the signal — i.e. its exit path,
/// which for the VMM is dominated by `exit_mmap()` unmapping the guest's multi-GiB address
/// space. Returns zero if the process is already gone, which biases the reported reclaim
/// UP, never down: this number exists to stop teardown being called free.
///
/// `comm` (field 2) can contain spaces and parentheses, so fields are counted from after
/// the last `") "`; `utime`/`stime` are fields 14/15, i.e. indices 11/12 from there.
fn process_cpu(pid: u32) -> std::time::Duration {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{}/stat", pid)) else {
        return std::time::Duration::ZERO;
    };
    let Some(after_comm) = stat.rsplit_once(") ").map(|(_, rest)| rest) else {
        return std::time::Duration::ZERO;
    };
    let fields: Vec<&str> = after_comm.split_whitespace().collect();
    let ticks: u64 = [11usize, 12]
        .iter()
        .filter_map(|i| fields.get(*i))
        .filter_map(|f| f.parse::<u64>().ok())
        .sum();
    // SAFETY: sysconf is a pure query with no memory effects.
    let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if hz <= 0 {
        return std::time::Duration::ZERO;
    }
    std::time::Duration::from_nanos(ticks.saturating_mul(1_000_000_000 / hz as u64))
}

fn verify_process_reaped(label: &str, identity: Option<(u32, u64)>) -> Result<()> {
    let Some((pid, start_time)) = identity else {
        return Ok(());
    };
    anyhow::ensure!(
        crate::utils::process_start_time(pid) != Some(start_time),
        "{} process {} with start time {} is still running after cleanup",
        label,
        pid,
        start_time
    );
    Ok(())
}

/// Tear down every resource a VM owns. Used by both the podman and snapshot commands.
///
/// Ordered to minimize how long the caller is blocked without deferring any of the work:
///
/// 1. **Signal, block on nothing.** SIGKILL the VMM, the namespace holder and the network
///    helper, and cancel/abort the in-process tasks — no `await` in between, so every
///    process is already dying before the first wait. Previously each kill awaited its own
///    exit before the next was even signalled, which serialized ~40ms of VMM address-space
///    reclaim, the holder's exit and ~28ms of pasta teardown that could have overlapped.
/// 2. **Join the waits together**, so the total is the slowest exit rather than their sum.
/// 3. **Network cleanup**, once the processes that were using the namespace are gone.
/// 4. **On-disk reaping — synchronous, always.** State file, NFS exports, Firecracker log,
///    VM data directory.
///
/// Nothing is moved to a background task or a janitor. "Leave nothing on disk" is a
/// correctness contract exactly like "leave no orphaned VM": a deferred sweep would let a
/// crash between here and the sweep strand a state file (whose PID is then reused), a
/// reflinked rootfs, or an `/etc/exports.d` entry that makes every later `exportfs -ra`
/// fail. It measures ~10ms. It is not the problem.
///
/// Emits one `vm teardown complete` record with three DISTINCT numbers, because a fast
/// teardown and a free one are not the same thing:
/// - `caller_blocking_ms` — what the request actually pays, start to finish.
/// - `until_gone_ms` — first SIGKILL until the last resource is released. Below
///   `caller_blocking_ms` only by the pre-signal bookkeeping; that they are near-equal is
///   the point, and is what makes "nothing was deferred" checkable rather than asserted.
/// - `reclaim_cpu_ms` — CPU the VMM and holder accrued AFTER their SIGKILL, i.e. what the
///   kernel spent actually destroying them (mostly `exit_mmap()` on the guest mapping).
///   Overlapping the waits does not shrink it: the box is still burning this either way,
///   which is the whole reason it is reported separately from the wall-clock numbers.
///
/// Plus a per-phase breakdown in MICROseconds (`processes_us`/`network_us`/`disk_us`) —
/// two of those phases are sub-millisecond in bridged mode and would round to a useless 0.
pub async fn cleanup_vm(
    ctx: CleanupContext,
    vm_manager: &mut dyn Hypervisor,
    holder_child: &mut Option<tokio::process::Child>,
    network: &mut dyn NetworkManager,
    state_manager: &StateManager,
) {
    // Normal runs intentionally retain their best-effort teardown contract.  Prepare calls
    // `cleanup_vm_verified` so it can fail closed instead of reporting a durable artifact
    // while host resources remain.
    let _ = cleanup_vm_inner(ctx, vm_manager, holder_child, network, state_manager).await;
}

/// Tear down every VM resource and return an error if any cleanup action failed.
///
/// All actions are attempted before the aggregated error is returned.  This is the cleanup
/// contract used by finite lifecycle operations such as `podman prepare`, where a successful
/// response is also a claim that the disposable source VM has been fully reaped.
pub async fn cleanup_vm_verified(
    ctx: CleanupContext,
    vm_manager: &mut dyn Hypervisor,
    holder_child: &mut Option<tokio::process::Child>,
    network: &mut dyn NetworkManager,
    state_manager: &StateManager,
) -> Result<()> {
    cleanup_vm_inner(ctx, vm_manager, holder_child, network, state_manager).await
}

async fn cleanup_vm_inner(
    ctx: CleanupContext,
    vm_manager: &mut dyn Hypervisor,
    holder_child: &mut Option<tokio::process::Child>,
    network: &mut dyn NetworkManager,
    state_manager: &StateManager,
) -> Result<()> {
    let CleanupContext {
        vm_id,
        mut volume_server_handles,
        remap_refs: _,
        data_dir,
        health_cancel_token,
        health_monitor_handle,
        mut output_listener_handle,
    } = ctx;
    // Started before the first log line so `caller_blocking_ms` covers everything the
    // caller actually waits for, including our own bookkeeping.
    let cleanup_start = std::time::Instant::now();
    let cpu_before = reaped_children_cpu();
    let mut failures = CleanupFailures::default();
    info!("cleaning up resources");

    // --- Phase 1: signal everything. No `await` until all signals are out. -------------
    // Sample what the VMM and holder have already spent, so the reclaim figure below is
    // their POST-signal CPU rather than their whole-life CPU (see `process_cpu`).
    let cpu_spent_before_kill = vm_manager.pid().map(process_cpu).unwrap_or_default()
        + holder_child
            .as_ref()
            .and_then(|h| h.id())
            .map(process_cpu)
            .unwrap_or_default();
    let vm_process_identity = vm_manager
        .pid()
        .ok()
        .and_then(|pid| crate::utils::process_start_time(pid).map(|start| (pid, start)));
    let holder_process_identity = holder_child.as_ref().and_then(|holder| {
        holder
            .id()
            .and_then(|pid| crate::utils::process_start_time(pid).map(|start| (pid, start)))
    });

    let kill_start = std::time::Instant::now();
    failures.record("signalling VM process", vm_manager.start_kill());
    if let Some(ref mut holder) = holder_child {
        failures.record(
            "signalling namespace holder process",
            // `record` names the action, so no duplicate `.context` here.
            holder.start_kill().map_err(anyhow::Error::from),
        );
    }
    network.start_kill_processes();

    // In-process tasks: cancel/abort is synchronous, the join happens in phase 2.
    if let Some(ref token) = health_cancel_token {
        token.cancel();
    }
    if let Some(handle) = output_listener_handle.as_ref() {
        handle.abort();
    }
    for handle in &volume_server_handles {
        handle.abort();
    }

    // --- Phase 2: join every wait at once. ---------------------------------------------
    let vmm_reaped = vm_manager.reap();
    let holder_reaped = async {
        if let Some(ref mut holder) = holder_child {
            // Reap the zombie left by the SIGKILL above.
            holder
                .wait()
                .await
                .context("waiting for namespace holder process")?;
        }
        Ok::<(), anyhow::Error>(())
    };
    let health_stopped = async {
        // Unconditional abort-and-reap: dropping a live JoinHandle detaches its
        // task, which could keep using the network namespace after cleanup has
        // begun destroying it — on the fast path as much as the verified one.
        if let Some(handle) = health_monitor_handle {
            stop_health_monitor(handle).await;
        }
        Ok::<(), anyhow::Error>(())
    };
    let listeners_stopped = async {
        let mut errors = Vec::new();
        if let Some(handle) = output_listener_handle.take() {
            if let Err(error) = handle.await {
                if !error.is_cancelled() {
                    errors.push(format!("joining output listener: {error}"));
                }
            }
        }
        for handle in volume_server_handles.drain(..) {
            if let Err(error) = handle.await {
                if !error.is_cancelled() {
                    errors.push(format!("joining volume server: {error}"));
                }
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            bail!(errors.join("; "))
        }
    };
    let ((), holder_reaped, health_stopped, listeners_stopped) =
        tokio::join!(vmm_reaped, holder_reaped, health_stopped, listeners_stopped);
    failures.record("reaping namespace holder process", holder_reaped);
    failures.record("stopping health monitor", health_stopped);
    failures.record("stopping VM listeners", listeners_stopped);
    failures.record(
        "verifying VM process reap",
        verify_process_reaped("VM", vm_process_identity),
    );
    failures.record(
        "verifying namespace holder process reap",
        verify_process_reaped("namespace holder", holder_process_identity),
    );
    let processes_us = kill_start.elapsed().as_micros();

    // Sampled HERE, before network cleanup reaps pasta, so the delta covers exactly the two
    // processes whose pre-kill CPU we subtracted. Anything reaped later (pasta) would add
    // its whole lifetime and silently inflate the reclaim.
    let reclaim_cpu = reaped_children_cpu()
        .saturating_sub(cpu_before)
        .saturating_sub(cpu_spent_before_kill);

    // --- Phase 3: network teardown, now that nothing is using the namespace. -----------
    let network_start = std::time::Instant::now();
    failures.record("cleaning network", network.cleanup().await);
    let network_us = network_start.elapsed().as_micros();

    // --- Phase 4: on-disk reaping. Synchronous — cleanup_vm does not return until every
    // one of these is done. They are independent of each other, so they run concurrently;
    // the log copy and the directory removal are the one ordered pair (the log lives in
    // the directory), so they share a future.
    let disk_start = std::time::Instant::now();
    // Removing this VM's /etc/exports.d entry (no-op when it had none) belongs on every exit
    // path — podman run, converge teardown, restored clones. A leftover entry for a deleted
    // directory makes every later `exportfs -ra` fail until the self-heal prunes it.
    let exports_removed = super::podman::cleanup_nfs_exports(&vm_id);
    let state_deleted = state_manager.delete_state(&vm_id);
    let data_dir_removed = async {
        // Preserve the Firecracker log (for debugging snapshot restore failures) BEFORE
        // removing the directory it lives in.
        let fc_log = data_dir.join("firecracker.log");
        if fc_log.exists() {
            let dest = std::path::PathBuf::from(format!("/tmp/fcvm-firecracker-{}.log", vm_id));
            if let Err(e) = tokio::fs::copy(&fc_log, &dest).await {
                debug!(vm_id = %vm_id, error = %e, "could not save firecracker log");
            } else {
                info!(vm_id = %vm_id, log = %dest.display(), "saved firecracker log");
            }
        }

        // Includes disks, sockets, everything under the VM's runtime directory.
        match tokio::fs::remove_dir_all(&data_dir).await {
            Ok(()) => {
                info!(vm_id = %vm_id, "cleaned up VM data directory");
                Ok(())
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error)
                .with_context(|| format!("removing VM data directory {}", data_dir.display())),
        }
    };
    let (exports_removed, state_deleted, data_dir_removed) =
        tokio::join!(exports_removed, state_deleted, data_dir_removed);
    failures.record("removing NFS exports", exports_removed);
    failures.record("deleting state", state_deleted);
    failures.record("removing VM data directory", data_dir_removed);
    let disk_us = disk_start.elapsed().as_micros();

    info!(
        vm_id = %vm_id,
        caller_blocking_ms = cleanup_start.elapsed().as_millis(),
        until_gone_ms = kill_start.elapsed().as_millis(),
        reclaim_cpu_ms = reclaim_cpu.as_millis(),
        processes_us,
        network_us,
        disk_us,
        "vm teardown complete"
    );

    failures.into_result()
}

/// Memory backend configuration for snapshot restore
pub enum MemoryBackend {
    /// Load memory directly from file (used by podman cache restore).
    /// Firecracker maps the snapshot file MAP_PRIVATE, so clean pages are shared through
    /// the host page cache and guest writes CoW.
    File { memory_path: PathBuf },
    /// Use UFFD server for on-demand page loading (used by snapshot clones).
    /// MISSING faults on anonymous memory, resolved with UFFDIO_COPY: lazy, but every
    /// faulted page becomes a private per-clone copy.
    Uffd { socket_path: PathBuf },
    /// Use the UFFD server in MINOR mode: Firecracker receives a shared memfd from the
    /// server, maps it MAP_PRIVATE, and the server resolves minor faults with
    /// UFFDIO_CONTINUE. Lazy *and* shared — clean pages have exactly one physical copy
    /// across all clones, writes CoW into private memory.
    UffdMinor { socket_path: PathBuf },
}

/// Configuration for restoring a VM from a snapshot
pub struct SnapshotRestoreConfig {
    /// VM state path (vmstate.bin)
    pub vmstate_path: PathBuf,
    /// Memory backend configuration
    pub memory_backend: MemoryBackend,
    /// Source disk for CoW copy
    pub source_disk_path: PathBuf,
    /// Original VM lineage (from original cache creation), used for disk redirects.
    pub original_vm_id: String,
    /// Exact vsock base path embedded in the source VMM state. Its parent is
    /// redirected to the clone runtime directory before the VMM starts.
    pub source_vsock_socket_path: PathBuf,
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
    /// Restore generation already published by the caller's vsock listener
    /// before VMM resume. Firecracker mirrors this exact value into MMDS after
    /// resume so switching back to its normal transport cannot trigger a
    /// duplicate restore.
    pub restore_epoch: &'a str,
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

/// Refuse a restore whose metadata does not name the exact vsock path embedded
/// in Firecracker's vmstate. Redirecting a guessed/default directory would make
/// a custom `--vsock-dir` clone bind the source socket or fail with EADDRINUSE.
fn assert_vmstate_vsock_source_matches(vmstate_path: &Path, source_path: &Path) -> Result<()> {
    let bytes = std::fs::read(vmstate_path)
        .with_context(|| format!("reading vmstate vsock source: {}", vmstate_path.display()))?;
    anyhow::ensure!(
        vmstate_contains_path(&bytes, source_path),
        "refusing to restore: vmstate.bin does not contain recorded source vsock path {}; delete and recreate the snapshot",
        source_path.display()
    );
    Ok(())
}

fn add_source_vsock_redirect_dir(
    baseline_dirs: &mut Vec<PathBuf>,
    source_path: &Path,
) -> Result<()> {
    let source_dir = source_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("source vsock path has no parent"))?
        .to_path_buf();
    if !baseline_dirs.contains(&source_dir) {
        baseline_dirs.push(source_dir);
    }
    Ok(())
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

/// #608 guard for the Cloud Hypervisor restore path — the CH analogue of
/// [`assert_vmstate_rootfs_covered`].
///
/// CH has no `patch_drive`: it opens the disk paths embedded in the snapshot's
/// `config.json` directly, and the mount-namespace redirect only retargets the baseline VM
/// dirs (`vm_runtime_dir(original|snapshot)`). A WRITABLE disk path outside those dirs
/// (sibling VM, or a different `data_dir` prefix between create and restore) would be
/// opened read-write against another VM's real disk and corrupt it — the exact exposure the
/// FC path guards against. Read-only disks (external `--disk`) may legitimately point
/// elsewhere, so only writable disks are checked. Called before any holder/disk side
/// effects so a violation aborts cleanly instead of corrupting silently.
/// Pure check behind [`assert_ch_config_disks_covered`]: returns the first WRITABLE disk
/// `path` in `cfg` that is not under any `covered_dirs` entry, or `None` if all are covered.
/// Read-only disks (external `--disk`) may legitimately point anywhere and are skipped.
/// Split out (like [`vmstate_rootfs_covered`]) so it is unit-testable without `paths` init.
fn ch_config_uncovered_writable_disk<'a>(
    cfg: &'a serde_json::Value,
    covered_dirs: &[PathBuf],
) -> Option<&'a str> {
    let disks = cfg.get("disks").and_then(|v| v.as_array())?;
    for disk in disks {
        let readonly = disk
            .get("readonly")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if readonly {
            continue;
        }
        if let Some(path) = disk.get("path").and_then(|v| v.as_str()) {
            if !covered_dirs.iter().any(|d| Path::new(path).starts_with(d)) {
                return Some(path);
            }
        }
    }
    None
}

fn assert_ch_config_disks_covered(
    cfg: &serde_json::Value,
    original_vm_id: &str,
    snapshot_vm_id: Option<&str>,
) -> Result<()> {
    let mut covered_dirs = vec![paths::vm_runtime_dir(original_vm_id)];
    if let Some(s) = snapshot_vm_id {
        if s != original_vm_id {
            covered_dirs.push(paths::vm_runtime_dir(s));
        }
    }
    if let Some(path) = ch_config_uncovered_writable_disk(cfg, &covered_dirs) {
        anyhow::bail!(
            "#608 (CH): refusing to restore — snapshot config.json references writable disk \
             {} outside any baseline bind-mount {:?} (data_dir prefix {}). Cloud Hypervisor \
             opens disk paths directly, so this would open an uncovered/sibling VM's disk and \
             corrupt it.",
            path,
            covered_dirs
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>(),
            paths::data_dir().display(),
        );
    }
    Ok(())
}

/// Refuse a Cloud Hypervisor restore whose snapshot config does not name the exact
/// vsock socket recorded in fcvm's snapshot metadata.
///
/// Unlike Firecracker's binary vmstate, CH serializes the source socket directly in
/// `config.json`. The mount redirect must cover that exact source directory before CH
/// starts: accepting a guessed or stale path could make the clone bind the still-live
/// source VM's socket, or fail with `EADDRINUSE`. This is deliberately a pure check so it
/// can run before [`prepare_clone_substrate`] creates a holder, disk, or namespace.
fn assert_ch_config_vsock_source_matches(
    cfg: &serde_json::Value,
    source_path: &Path,
) -> Result<()> {
    let configured_path = cfg
        .get("vsock")
        .and_then(|vsock| vsock.get("socket"))
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "refusing to restore: Cloud Hypervisor config.json has no vsock.socket; \
                 delete and recreate the snapshot"
            )
        })?;
    anyhow::ensure!(
        Path::new(configured_path) == source_path,
        "refusing to restore: Cloud Hypervisor config.json vsock socket {} does not match \
         recorded source {}; delete and recreate the snapshot",
        configured_path,
        source_path.display()
    );
    Ok(())
}

/// Backend-neutral substrate for a snapshot restore: the per-clone CoW disk, the holder
/// process (rootless), and the namespace/mount isolation to apply to the VMM process.
///
/// The work that produces it — create the network namespace (bridged/routed) or holder
/// (rootless), reflink the CoW rootfs + extra disks, and compute the mount-namespace
/// redirect — is identical for Firecracker and Cloud Hypervisor. Only the subsequent
/// VMM-specific load differs (FC `LoadSnapshot`+`patch_drive` vs CH `--restore`), so both
/// restore paths call [`prepare_clone_substrate`] and then apply this to their backend.
pub struct CloneSubstrate {
    pub rootfs_path: PathBuf,
    pub holder_child: Option<tokio::process::Child>,
    /// PID to hand `network.post_start` — the rootless holder, or (None) the VMM pid the
    /// caller fills in after spawn.
    pub holder_pid_for_post_start: Option<u32>,
    /// Namespace/mount isolation to apply to the VMM spawn. `mount_redirects` is already
    /// set to `(baseline_dirs, clone_dir)` so the VMM opens the clone's CoW disk where the
    /// snapshot embedded the baseline's path.
    pub namespace: crate::utils::NamespaceParams,
}

/// Stop and reap a rootless namespace holder retained by a failed restore.
/// Dropping `tokio::process::Child` does not kill it, so every error after
/// `prepare_clone_substrate` must cross this barrier before it propagates.
async fn cleanup_failed_clone_holder(holder: &mut Option<tokio::process::Child>, stage: &str) {
    let Some(mut child) = holder.take() else {
        return;
    };
    if let Err(error) = child.start_kill() {
        warn!(error = %error, stage, "failed to signal clone namespace holder");
    }
    if let Err(error) = child.wait().await {
        warn!(error = %error, stage, "failed to reap clone namespace holder");
    }
}

/// Prepare the [`CloneSubstrate`]: per-network-mode namespace setup + CoW disk + extra-disk
/// copy + mount-redirect. Shared by the Firecracker and Cloud Hypervisor restore paths.
///
/// Mirrors the original inline prologue of [`restore_from_snapshot`] exactly (no behavior
/// change for Firecracker); the only difference is it records the namespace isolation into a
/// [`NamespaceParams`](crate::utils::NamespaceParams) instead of mutating a `VmManager`, so a
/// Cloud Hypervisor backend can apply the same isolation to its `ProcessSpec`.
async fn prepare_clone_substrate(
    network: &mut dyn NetworkManager,
    restore_config: &SnapshotRestoreConfig,
    vm_id: &str,
    data_dir: &Path,
    vm_state: &mut VmState,
) -> Result<CloneSubstrate> {
    let vm_dir = data_dir.join("disks");
    let mut holder_child: Option<tokio::process::Child> = None;
    let mut holder_pid_for_post_start: Option<u32> = None;
    let mut namespace = crate::utils::NamespaceParams {
        vm_id: vm_id.to_string(),
        ..Default::default()
    };

    // rootfs_path is set by either the bridged, rootless, or routed branch.
    let rootfs_path: PathBuf;

    if let Some(bridged_net) = network.as_any().downcast_ref::<BridgedNetwork>() {
        if let Some(ns_id) = bridged_net.namespace_id() {
            info!(namespace = %ns_id, "configuring VM to run in network namespace");
            namespace.namespace_id = Some(ns_id.to_string());
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
            "CoW disk prepared from snapshot"
        );
    } else if let Some(pasta_net) = network.as_any().downcast_ref::<PastaNetwork>() {
        // Rootless mode: spawn holder process, then run disk creation and network setup
        // in parallel via nsenter.
        let holder_cmd = pasta_net.build_holder_command();
        info!(cmd = ?holder_cmd, "spawning namespace holder for rootless networking");
        let (mut child, holder_pid) = spawn_namespace_holder(&holder_cmd).await?;

        let setup_script = pasta_net.build_setup_script();
        let nsenter_prefix = pasta_net.build_nsenter_prefix(holder_pid);

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

        let network_task = async {
            let ns_poll_start = std::time::Instant::now();
            info!(holder_pid = holder_pid, "running network setup via nsenter");
            // One nsenter+bash+ip for the whole phase, TAP verification included
            // as the last batch step (ip -batch aborts at the first failure, so
            // reaching it proves every step above applied).
            loop {
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
                    .arg(setup_script.script())
                    .output()
                    .await
                    .context("running network setup via nsenter")?;
                if output.status.success() {
                    debug!("namespace ready after {:?}", ns_poll_start.elapsed());
                    break;
                }
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
                anyhow::bail!(
                    "network setup failed: {}",
                    setup_script.describe_failure(&stderr)
                );
            }
            Ok::<_, anyhow::Error>(())
        };

        let (disk_result, network_result) = tokio::join!(disk_task, network_task);
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

        namespace.user_namespace_path =
            Some(PathBuf::from(format!("/proc/{}/ns/user", holder_pid)));
        namespace.net_namespace_path = Some(PathBuf::from(format!("/proc/{}/ns/net", holder_pid)));
        vm_state.holder_pid = Some(holder_pid);
        holder_pid_for_post_start = Some(holder_pid);
        holder_child = Some(child);
    } else if let Some(routed_net) = network
        .as_any()
        .downcast_ref::<crate::network::RoutedNetwork>()
    {
        if let Some(ns_id) = routed_net.namespace_id() {
            info!(namespace = %ns_id, "configuring VM to run in routed network namespace");
            namespace.namespace_id = Some(ns_id.to_string());
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

    // Mount-namespace redirect: disks are under the source VM runtime dirs, but
    // vsock may be under an arbitrary dedicated `--vsock-dir`. Redirect every
    // exact source parent to the clone dir so the VMM opens only clone-local
    // disks and binds only the clone-local socket.
    let mut baseline_dirs = vec![paths::vm_runtime_dir(&restore_config.original_vm_id)];
    if let Some(ref snapshot_vm_id) = restore_config.snapshot_vm_id {
        if snapshot_vm_id != &restore_config.original_vm_id {
            baseline_dirs.push(paths::vm_runtime_dir(snapshot_vm_id));
        }
    }
    add_source_vsock_redirect_dir(&mut baseline_dirs, &restore_config.source_vsock_socket_path)?;
    info!(
        baseline_dirs = ?baseline_dirs.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
        clone_dir = %data_dir.display(),
        "enabling mount namespace for path isolation"
    );
    namespace.mount_redirects = Some((baseline_dirs, data_dir.to_path_buf()));

    // Copy extra disk images (disk-dir) from snapshot to the clone's disk directory so the
    // redirected baseline paths resolve to real files.
    let extra_disks_result: Result<()> = async {
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
        Ok(())
    }
    .await;
    if let Err(error) = extra_disks_result {
        cleanup_failed_clone_holder(&mut holder_child, "copying clone extra disks").await;
        return Err(error);
    }

    Ok(CloneSubstrate {
        rootfs_path,
        holder_child,
        holder_pid_for_post_start,
        namespace,
    })
}

/// Mint the restore-epoch value that tells a restored guest's fc-agent "you have
/// just been restored — reconnect your vsock channels".
///
/// MUST be unique per restore, never wall-clock derived. fc-agent's watcher
/// ([`fc-agent`]'s `watch_restore_epoch`) compares epochs only for (in)equality,
/// and a snapshot taken FROM a restored VM captures the watcher's last-seen
/// epoch inside guest memory. With the old second-granularity epochs
/// (`SystemTime::now().as_secs()`), restoring such a snapshot within the same
/// wall-clock second as its ancestor's restore handed the guest an IDENTICAL
/// epoch: the watcher treated the restore as already handled, never reconnected
/// exec/egress/output vsocks, and the clone never became healthy (120s health
/// timeouts across the snapshot-hit suite once the clone hot path went
/// sub-second). A fresh UUID makes every restore observably distinct.
pub(crate) fn new_restore_epoch() -> String {
    uuid::Uuid::new_v4().to_string()
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
        restore_epoch,
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
    assert_vmstate_vsock_source_matches(
        &restore_config.vmstate_path,
        &restore_config.source_vsock_socket_path,
    )?;

    // Resolve the binary before acquiring a rootless namespace holder. This is
    // pure validation and must not create a process that a later `?` can detach.
    let firecracker_bin = find_firecracker(runtime_config)?;
    let firecracker_args = runtime_config
        .firecracker_args
        .clone()
        .or_else(|| std::env::var("FCVM_FIRECRACKER_ARGS").ok());

    // Configure namespace isolation, create the CoW disk, copy extra disks, and compute the
    // mount redirect — all shared with the Cloud Hypervisor restore path.
    let mut substrate =
        prepare_clone_substrate(network, restore_config, vm_id, data_dir, vm_state).await?;
    let rootfs_path = substrate.rootfs_path;
    let holder_pid_for_post_start = substrate.holder_pid_for_post_start;

    let fc_log_path = data_dir.join("firecracker.log");
    let mut vm_manager = VmManager::new(
        vm_id.to_string(),
        socket_path.to_path_buf(),
        Some(fc_log_path),
    );
    vm_manager.set_vm_name(vm_name.to_string());
    // Apply the substrate's namespace/mount isolation to the Firecracker VmManager (the
    // #608 coverage check ran at the top of this function, before any side effects).
    let ns = &substrate.namespace;
    if let Some(id) = &ns.namespace_id {
        vm_manager.set_namespace(id.clone());
    }
    if let Some(p) = &ns.user_namespace_path {
        vm_manager.set_user_namespace_path(p.clone());
    }
    if let Some(p) = &ns.net_namespace_path {
        vm_manager.set_net_namespace_path(p.clone());
    }
    if let Some((baseline_dirs, clone_dir)) = &ns.mount_redirects {
        vm_manager.set_mount_redirects(baseline_dirs.clone(), clone_dir.clone());
    }

    if let Err(error) = vm_manager
        .start(&firecracker_bin, None, firecracker_args.as_deref())
        .await
        .context("starting Firecracker")
    {
        // `start` can fail after spawning; ask the manager to reap any partial
        // child, then stop the independently-owned namespace holder.
        let _ = vm_manager.kill().await;
        cleanup_failed_clone_holder(&mut substrate.holder_child, "starting Firecracker clone")
            .await;
        return Err(error);
    }
    let mut holder_child = substrate.holder_child.take();

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
            MemoryBackend::UffdMinor { socket_path } => {
                info!(
                    uffd_socket = %socket_path.display(),
                    "loading snapshot with UFFD MINOR backend (shared memfd + UFFDIO_CONTINUE)"
                );
                MemBackend {
                    backend_type: "UffdMinor".to_string(),
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

        // Failpoint: snapshot loaded and paused, network post-start done (pasta
        // and its published-port listeners are live), guest not yet running, PID
        // not yet published. This is the adversarial window a client can reach
        // the host listener in while nothing behind it can serve — held here so
        // a harness can drive that connect deterministically. The CH restore
        // path holds at the mirror-image point (post_start, pre resume).
        failpoint::hit_async("restore.post_network_pre_resume").await;

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

        // Mirror the exact restore-only vsock epoch into MMDS. The guest latches
        // the vsock generation that performed cookie cleanup and ignores the
        // snapshot-old MMDS value until this acknowledgement arrives. MUST be
        // after VM resume — Firecracker accepts PUT /mmds while paused but the
        // guest-visible MMDS data isn't updated until after resume.
        let mut mmds_latest = serde_json::json!({
            "host-time": chrono::Utc::now().timestamp().to_string(),
            "restore-epoch": restore_epoch
        });
        if let Some(ref ipv6) = clone_ipv6 {
            mmds_latest["clone-ipv6"] = serde_json::Value::String(ipv6.clone());
        }
        client
            .put_mmds(serde_json::json!({ "latest": mmds_latest }))
            .await
            .context("updating MMDS with restore-epoch")?;
        info!(
            restore_epoch = %restore_epoch,
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
        vm_state.config.source_vsock_socket_path =
            Some(restore_config.source_vsock_socket_path.clone());

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

        // Post-resume liveness check (non-blocking): catch a Firecracker process that
        // already died during load/patch/resume. This used to be `sleep(200ms)` +
        // try_wait (fe4376b0) to detect restores of corrupt diff snapshots, but the
        // sleep could never catch what it aimed for: a panicking guest reboots (and
        // Firecracker exits) only ~1s after resume (`panic=1` boot arg), outside any
        // 200ms window — and since #630 narrowed cached-snapshot fallback to
        // Firecracker "Load snapshot error"s, this error is a hard failure anyway
        // (corrupt diffs are caught at CREATE time by the diff-size validation).
        // Delayed guest crashes are detected event-driven by every caller's positive
        // readiness gate: the output-connect wait (with its try_wait liveness poll)
        // in cmd_snapshot_run, --exec's vsock connect probe, and
        // verify_port_forwarding. Sleeping here bought nothing but 200ms per clone.
        if let Some(status) = vm_manager.try_wait()? {
            bail!(
                "VM crashed immediately after snapshot restore (exit status: {:?}). \
                 This can happen under heavy I/O load due to memory corruption during restore.",
                status
            );
        }

        // Persist process/network identity now; cmd_snapshot_run publishes the separate
        // lifecycle-ready barrier after it has transferred every setup owner and installed
        // the path-specific supervision resources.
        save_vm_state_with_network(state_manager, vm_state, network_config).await?;

        Ok::<(), anyhow::Error>(())
    }
    .await;
    if let Err(e) = post_start {
        warn!(error = %format!("{e:#}"), "restore failed after Firecracker start; killing the process");
        if let Err(kill_err) = vm_manager.kill().await {
            warn!(error = %kill_err, "failed to kill Firecracker after restore failure");
        }
        cleanup_failed_clone_holder(&mut holder_child, "Firecracker restore failure").await;
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

/// Restore a Cloud Hypervisor VM from a snapshot (#632 P2).
///
/// The Cloud Hypervisor analogue of [`restore_from_snapshot`]. It reuses the shared clone
/// substrate (network/namespace/CoW disk/mount-redirect via [`prepare_clone_substrate`]),
/// then — instead of Firecracker's `LoadSnapshot` + `patch_drive` — launches
/// `cloud-hypervisor --restore source_url=file://{snapshot}/ch,memory_restore_mode=copy`.
/// CH reads its own `config.json`/`state.json`/memory ranges from that subdir; the disk
/// paths it embeds are redirected to the clone's CoW disk by the mount namespace (CH has no
/// `patch_drive`). The VM restores PAUSED (`resume=false`) so the network post-start runs
/// before [`Hypervisor::resume`], mirroring Firecracker.
///
/// `copy` mode is eager read-copy (simplest, proven); in-process on-demand UFFD
/// (`memory_restore_mode=ondemand`) is a follow-on (P2.5). The restored guest reconnects its
/// vsock channels and syncs its clock when the CALLER serves a restore-epoch over the
/// boot-plan vsock port (its restore-epoch watcher triggers `handle_clone_restore`).
pub async fn restore_from_snapshot_ch(
    params: RestoreParams<'_>,
    network: &mut dyn NetworkManager,
    state_manager: &StateManager,
    vm_state: &mut VmState,
) -> Result<(
    crate::hypervisor::cloud_hypervisor::CloudHypervisorBackend,
    Option<tokio::process::Child>,
)> {
    use crate::hypervisor::cloud_hypervisor::CloudHypervisorBackend;
    use crate::hypervisor::{Hypervisor, ProcessSpec};

    let RestoreParams {
        vm_id,
        vm_name,
        data_dir,
        socket_path,
        runtime_config: _, // CH binary is resolved via find_cloud_hypervisor (env/PATH)
        restore_config,
        network_config,
        restore_epoch: _,     // delivered by the caller's boot-plan vsock listener
        clone_ipv6: _,        // delivered to the guest via the boot-plan restore-epoch (caller)
        track_dirty_pages: _, // CH has no dirty-page tracking
    } = params;
    let vm_dir = data_dir.join("disks");

    // CH's own snapshot files live in the `ch/` subdir (written by create_snapshot_ch).
    let snapshot_dir = restore_config
        .snapshot_dir
        .as_ref()
        .context("Cloud Hypervisor restore requires the snapshot directory")?;
    let ch_dir = snapshot_dir.join(CH_SNAPSHOT_SUBDIR);
    if !ch_dir.join("config.json").exists() {
        bail!(
            "Cloud Hypervisor snapshot incomplete: {} has no config.json",
            ch_dir.display()
        );
    }

    // #608 guard (CH analogue of assert_vmstate_rootfs_covered): CH opens the disk paths
    // embedded in config.json directly (no patch_drive), so validate they are covered by the
    // mount redirect BEFORE prepare_clone_substrate's holder/disk side effects — an uncovered
    // writable path would be opened read-write against a sibling VM's disk and corrupt it.
    // Parsed once here and reused for the net-TAP rewrite below.
    let cfg_bytes = tokio::fs::read(ch_dir.join("config.json"))
        .await
        .context("reading CH snapshot config.json")?;
    let mut cfg: serde_json::Value =
        serde_json::from_slice(&cfg_bytes).context("parsing CH snapshot config.json")?;
    assert_ch_config_disks_covered(
        &cfg,
        &restore_config.original_vm_id,
        restore_config.snapshot_vm_id.as_deref(),
    )?;
    assert_ch_config_vsock_source_matches(&cfg, &restore_config.source_vsock_socket_path)?;

    // Shared substrate: network namespace / CoW disk / mount-redirect / extra disks.
    let mut substrate =
        prepare_clone_substrate(network, restore_config, vm_id, data_dir, vm_state).await?;

    // Cloud Hypervisor reads its restore config (disk/vsock/net) from the snapshot's `ch/`
    // dir. Disk + vsock paths are the source VM's and are redirected to the clone's by the
    // mount namespace, but a network TAP is a device NAME, not a path — the redirect can't
    // fix it, and the source's TAP doesn't exist in the clone's namespace. So copy `ch/`
    // into a clone-local dir (reflink: instant, shares blocks) and rewrite each net device
    // to the clone's TAP before restoring (the FC path does the equivalent via
    // LoadSnapshot's `network_overrides`). Copying also avoids mutating the shared snapshot,
    // which concurrent clones restore from.
    let clone_prepare: Result<(PathBuf, PathBuf)> = async {
        let clone_ch_dir = data_dir.join("ch-restore");
        let _ = tokio::fs::remove_dir_all(&clone_ch_dir).await;
        tokio::fs::create_dir_all(&clone_ch_dir)
            .await
            .context("creating CH restore directory")?;
        let mut entries = tokio::fs::read_dir(&ch_dir)
            .await
            .with_context(|| format!("reading CH snapshot dir {}", ch_dir.display()))?;
        while let Some(entry) = entries.next_entry().await? {
            let name = entry.file_name();
            // config.json is rewritten below; reflink the rest (state.json, memory ranges).
            if name == std::ffi::OsStr::new("config.json") {
                continue;
            }
            reflink_copy(&ch_dir.join(&name), &clone_ch_dir.join(&name))
                .await
                .with_context(|| format!("reflinking CH snapshot file {name:?}"))?;
        }
        // `cfg` was read, parsed, and #608-validated above (before the substrate side effects).
        if let Some(nets) = cfg.get_mut("net").and_then(|v| v.as_array_mut()) {
            for net in nets.iter_mut() {
                if let Some(obj) = net.as_object_mut() {
                    obj.insert(
                        "tap".to_string(),
                        serde_json::Value::String(network_config.tap_device.clone()),
                    );
                }
            }
        }
        tokio::fs::write(
            clone_ch_dir.join("config.json"),
            serde_json::to_vec_pretty(&cfg).context("serializing CH restore config.json")?,
        )
        .await
        .context("writing CH restore config.json")?;

        Ok((clone_ch_dir, find_cloud_hypervisor()?))
    }
    .await;
    let (clone_ch_dir, ch_bin) = match clone_prepare {
        Ok(prepared) => prepared,
        Err(error) => {
            cleanup_failed_clone_holder(
                &mut substrate.holder_child,
                "preparing Cloud Hypervisor restore",
            )
            .await;
            return Err(error);
        }
    };
    let log_path = data_dir.join("firecracker.log");
    let mut backend =
        CloudHypervisorBackend::new(vm_id.to_string(), socket_path.to_path_buf(), Some(log_path));

    // Restore paused, then resume after network post-start (mirrors the FC load/resume split).
    let restore_args = format!(
        "--restore source_url=file://{},memory_restore_mode=copy,resume=false",
        clone_ch_dir.display()
    );
    let spec = ProcessSpec {
        binary: ch_bin,
        extra_args: Some(restore_args),
        vm_name: Some(vm_name.to_string()),
        namespace_id: substrate.namespace.namespace_id.clone(),
        holder_pid: substrate.holder_pid_for_post_start,
        user_namespace_path: substrate.namespace.user_namespace_path.clone(),
        net_namespace_path: substrate.namespace.net_namespace_path.clone(),
        mount_redirects: substrate.namespace.mount_redirects.clone(),
    };
    if let Err(error) = backend
        .spawn(&spec)
        .await
        .context("launching Cloud Hypervisor --restore")
    {
        let _ = backend.kill().await;
        cleanup_failed_clone_holder(
            &mut substrate.holder_child,
            "launching Cloud Hypervisor restore",
        )
        .await;
        return Err(error);
    }

    let mut holder_child = substrate.holder_child.take();
    // Everything after spawn runs in a fallible block so a failure kills the CH process
    // (callers may fall back to a fresh boot, so a half-restored process must not leak).
    let post_start = async {
        let vm_pid = backend.pid()?;
        let post_start_pid = substrate.holder_pid_for_post_start.unwrap_or(vm_pid);
        network
            .post_start(post_start_pid)
            .await
            .context("post-start network setup")?;

        // Mirror of the Firecracker restore hold: snapshot loaded and paused,
        // pasta's published-port listeners live, guest not yet running, PID not
        // yet published. See the Firecracker path for the full rationale.
        failpoint::hit_async("restore.post_network_pre_resume").await;

        backend
            .resume()
            .await
            .context("resuming Cloud Hypervisor after restore")?;

        vm_state.pid = Some(std::process::id());
        // Future snapshots of this clone must redirect using the original vsock vm_id.
        vm_state.config.original_vsock_vm_id = Some(restore_config.original_vm_id.clone());
        vm_state.config.source_vsock_socket_path =
            Some(restore_config.source_vsock_socket_path.clone());
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

        // Liveness (non-blocking): catch a CH process that already died during
        // restore/resume. Same rationale as the Firecracker path above — delayed
        // guest crashes are detected event-driven by the caller's readiness gates,
        // so there is nothing a fixed post-resume sleep can catch that they don't.
        if let Some(status) = backend.try_wait()? {
            bail!(
                "Cloud Hypervisor crashed immediately after snapshot restore (exit status: {:?})",
                status
            );
        }

        // Lifecycle readiness is published by cmd_snapshot_run after setup ownership and
        // supervision are complete, not by this lower-level restore primitive.
        save_vm_state_with_network(state_manager, vm_state, network_config).await?;
        Ok::<(), anyhow::Error>(())
    }
    .await;
    if let Err(e) = post_start {
        warn!(error = %format!("{e:#}"), "CH restore failed after launch; killing the process");
        if let Err(kill_err) = backend.kill().await {
            warn!(error = %kill_err, "failed to kill Cloud Hypervisor after restore failure");
        }
        cleanup_failed_clone_holder(&mut holder_child, "Cloud Hypervisor restore failure").await;
        return Err(e);
    }

    Ok((backend, holder_child))
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

/// Reflink the rootfs + any extra disks into the temp snapshot dir (instant btrfs CoW).
/// Shared by the Firecracker and Cloud Hypervisor snapshot-create paths; the caller must
/// have the VM paused so the disk image matches the captured memory.
async fn reflink_disks_to_snapshot(
    disk_path: &Path,
    snapshot_config: &crate::storage::snapshot::SnapshotConfig,
    temp_snapshot_dir: &Path,
) -> Result<()> {
    reflink_copy(disk_path, &temp_snapshot_dir.join("disk.raw")).await?;
    for extra_disk in &snapshot_config.metadata.extra_disks {
        let source = paths::vm_runtime_dir(&snapshot_config.vm_id)
            .join("disks")
            .join(&extra_disk.filename);
        let dest = temp_snapshot_dir.join(&extra_disk.filename);
        reflink_copy(&source, &dest)
            .await
            .with_context(|| format!("copying extra disk {}", extra_disk.filename))?;
    }
    Ok(())
}

/// Atomically replace `final_dir` with `temp_dir`: move any existing dir aside, rename the
/// temp dir into place, then remove the old. Shared by the snapshot-create paths so a
/// finalized snapshot directory always appears atomically.
async fn atomic_replace_dir(temp_dir: &Path, final_dir: &Path) -> Result<()> {
    if final_dir.exists() {
        let old = snapshot_sibling(final_dir, "old");
        let _ = tokio::fs::remove_dir_all(&old).await;
        tokio::fs::rename(final_dir, &old)
            .await
            .context("moving old snapshot out of the way")?;
        tokio::fs::rename(temp_dir, final_dir)
            .await
            .context("renaming temp snapshot to final location")?;
        let _ = tokio::fs::remove_dir_all(&old).await;
    } else {
        tokio::fs::rename(temp_dir, final_dir)
            .await
            .context("renaming temp snapshot to final location")?;
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
) -> Result<crate::storage::SnapshotConfig> {
    let original_vsock_vm_id = vm_state
        .config
        .original_vsock_vm_id
        .clone()
        .unwrap_or_else(|| vm_state.vm_id.clone());
    let source_vsock_socket_path = vm_state
        .config
        .source_vsock_socket_path
        .as_ref()
        .ok_or_else(|| {
            anyhow::anyhow!(
                "running VM {} has no recorded VMM source vsock socket path",
                vm_state.vm_id
            )
        })?
        .clone();
    anyhow::ensure!(
        source_vsock_socket_path.is_absolute(),
        "running VM {} has non-absolute VMM source vsock socket path {}",
        vm_state.vm_id,
        source_vsock_socket_path.display()
    );
    anyhow::ensure!(
        source_vsock_socket_path.file_name() == Some(std::ffi::OsStr::new("vsock.sock"))
            && source_vsock_socket_path.parent().is_some()
            && source_vsock_socket_path.parent() != Some(Path::new("/")),
        "running VM {} has invalid VMM source vsock socket path {}; expected an absolute \
         path named vsock.sock under a dedicated directory",
        vm_state.vm_id,
        source_vsock_socket_path.display()
    );

    Ok(crate::storage::SnapshotConfig {
        name: snapshot_key.to_string(),
        vm_id: vm_state.vm_id.clone(),
        generation_id: uuid::Uuid::new_v4(),
        network_boundary_version: crate::storage::snapshot::SNAPSHOT_NETWORK_BOUNDARY_VERSION,
        original_vsock_vm_id: Some(original_vsock_vm_id),
        source_vsock_socket_path,
        parent_snapshot: None, // Set by create_snapshot_core after determining diff base
        // Set by create_podman_snapshot, the only caller whose snapshot holds the
        // content of a cache key. `snapshot create` captures a live VM, not a config.
        content_key: None,
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
    })
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
    let wait_started = std::time::Instant::now();
    let mut next_info_log = std::time::Duration::from_secs(5);
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
                let elapsed = wait_started.elapsed();
                if elapsed >= next_info_log {
                    info!(
                        lock = %lock_path.display(),
                        exclusive,
                        waited_ms = elapsed.as_millis(),
                        "still waiting for snapshot generation lock held by another command"
                    );
                    next_info_log = elapsed + std::time::Duration::from_secs(5);
                }
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

/// Locks that pin every snapshot generation read or replaced by one snapshot create.
///
/// The files stay locked until this value is dropped.  Callers acquire this before the
/// per-VM snapshot lock and keep it through the parent-memory copy/merge and the target's
/// atomic replacement.
pub struct SnapshotCreateGenerationLocks {
    _files: Vec<std::fs::File>,
}

fn snapshot_create_lock_requests(
    target_dir: &Path,
    parent_dir: Option<&Path>,
) -> Vec<(PathBuf, bool)> {
    let mut requests = vec![(target_dir.to_path_buf(), true)];
    if let Some(parent_dir) = parent_dir {
        // Re-creating the VM's current parent uses the target's exclusive lock for both
        // roles. Trying to flock the same inode through a second fd can self-deadlock.
        if parent_dir != target_dir {
            requests.push((parent_dir.to_path_buf(), false));
        }
    }
    // Two creates may have inverse target/parent relationships. A single canonical order
    // prevents each from holding one generation lock while waiting for the other.
    requests.sort_by(|(left, _), (right, _)| left.cmp(right));
    requests
}

/// Pin a snapshot create's target generation exclusively and its distinct parent generation
/// shared. Locks are always acquired in canonical path order to prevent AB/BA deadlocks.
pub async fn acquire_snapshot_create_generation_locks(
    target_dir: &Path,
    parent_dir: Option<&Path>,
) -> Result<SnapshotCreateGenerationLocks> {
    let mut files = Vec::new();
    for (snapshot_dir, exclusive) in snapshot_create_lock_requests(target_dir, parent_dir) {
        files.push(acquire_snapshot_dir_lock(&snapshot_dir, exclusive).await?);
    }
    Ok(SnapshotCreateGenerationLocks { _files: files })
}

/// Fail closed if the state selected before snapshot locking no longer identifies the same
/// live fcvm process after the locks are held.
///
/// A name can be reused by a new VM while `snapshot create` waits. Pairing the original
/// runtime paths with that replacement VM's lineage would capture or merge unrelated bytes.
pub(crate) fn validate_snapshot_vm_identity(
    expected: &crate::state::VmState,
    current: &crate::state::VmState,
) -> Result<()> {
    anyhow::ensure!(
        current.vm_id == expected.vm_id,
        "snapshot target changed while acquiring locks: expected VM {}, found {}",
        expected.vm_id,
        current.vm_id
    );

    let expected_pid = expected
        .pid
        .ok_or_else(|| anyhow::anyhow!("snapshot target {} has no process PID", expected.vm_id))?;
    let expected_start = expected.pid_start_time.ok_or_else(|| {
        anyhow::anyhow!(
            "snapshot target {} has no recorded process start time",
            expected.vm_id
        )
    })?;
    let current_pid = current.pid.ok_or_else(|| {
        anyhow::anyhow!(
            "snapshot target {} lost its process PID while acquiring locks",
            expected.vm_id
        )
    })?;
    let current_start = current.pid_start_time.ok_or_else(|| {
        anyhow::anyhow!(
            "snapshot target {} lost its process start time while acquiring locks",
            expected.vm_id
        )
    })?;

    anyhow::ensure!(
        (current_pid, current_start) == (expected_pid, expected_start),
        "snapshot target {} process identity changed while acquiring locks: \
         expected PID {} start {}, found PID {} start {}",
        expected.vm_id,
        expected_pid,
        expected_start,
        current_pid,
        current_start
    );

    let observed_start = crate::utils::process_start_time(current_pid).ok_or_else(|| {
        anyhow::anyhow!(
            "snapshot target {} process PID {} is no longer running",
            expected.vm_id,
            current_pid
        )
    })?;
    anyhow::ensure!(
        observed_start == current_start,
        "snapshot target {} process PID {} was reused: expected start {}, observed {}",
        expected.vm_id,
        current_pid,
        current_start,
        observed_start
    );

    Ok(())
}

/// Extra files to write into the snapshot directory before it is atomically finalized.
///
/// Returns (filename, contents) pairs. Invoked after the Firecracker snapshot is taken
/// (and the VM resumed) so contents reflect host-side state at snapshot time — e.g.
/// portable-volume inode tables.
pub type SnapshotExtraFiles<'a> = Option<&'a (dyn Fn() -> Vec<(String, Vec<u8>)> + Send + Sync)>;

/// What to do with the source VM after capturing its memory and disks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SnapshotSourceDisposition {
    /// The source is the caller's workload and must continue running.
    Resume,
    /// The source is disposable and remains paused until verified cleanup reaps it.
    LeavePaused,
}

/// Disk-only capture: quiesce the guest, reflink only the disk (no memory dump,
/// no vCPU pause — `fsfreeze` provides consistency), unfreeze, and finalize a
/// `DiskOnly` snapshot. Clones cold-boot from this disk. See
/// docs/disk-only-clone.html.
///
/// `snapshot_config.kind` must already be `DiskOnly`. `vsock_socket` is the VM's exact
/// recorded exec vsock (including a custom `--vsock-dir`), used to run the quiesce
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
    let marker_out =
        run_exec_in_vm_captured(vsock_socket, &marker_cmd, false, &snapshot_config.vm_id)
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
    let sync_out = run_exec_in_vm_captured(vsock_socket, &sync_cmd, false, &snapshot_config.vm_id)
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
    let freeze_out =
        match run_exec_in_vm_captured(vsock_socket, &freeze_cmd, false, &snapshot_config.vm_id)
            .await
        {
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
                let _ = run_exec_in_vm_captured(vsock_socket, &thaw, false, &snapshot_config.vm_id)
                    .await;
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
        let _ = run_exec_in_vm_captured(vsock_socket, &thaw, false, &snapshot_config.vm_id).await;
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
    let unfreeze_result =
        run_exec_in_vm_captured(vsock_socket, &unfreeze_cmd, false, &snapshot_config.vm_id).await;

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

/// Record that this VM's vsock transport was reset by a snapshot pause, by
/// bumping the persisted `vsock_epoch`.
///
/// MUST be called after the pause/save and BEFORE the resume (see
/// `StateManager::bump_vsock_epoch` for the ordering contract that makes the
/// exec-side orphan detection race-free). Best-effort: a missing state file
/// (VM mid-teardown) is expected, and a failed bump must not fail the
/// snapshot — it only degrades orphan detection for currently-blocked execs
/// back to the pre-epoch behavior.
async fn record_vsock_reset_for_snapshot(vm_id: &str, snapshot_name: &str) {
    let state_manager = crate::state::StateManager::new(paths::state_dir());
    match state_manager.bump_vsock_epoch(vm_id).await {
        Ok(Some(epoch)) => {
            info!(
                vm_id,
                snapshot = snapshot_name,
                vsock_epoch = epoch,
                "bumped vsock epoch after snapshot pause \
                 (exec sessions from before the pause are dead and will abort)"
            );
        }
        Ok(None) => {
            debug!(
                vm_id,
                snapshot = snapshot_name,
                "no state file to bump vsock epoch on (VM being torn down)"
            );
        }
        Err(e) => {
            warn!(
                vm_id,
                snapshot = snapshot_name,
                error = %e,
                "failed to bump vsock epoch after snapshot pause; exec sessions \
                 orphaned by this pause will not detect it"
            );
        }
    }
}

const FC_AGENT_PATH: &str = "/usr/local/bin/fc-agent";

/// Return the exact host-side vsock socket recorded for a running VM.
///
/// A path reconstructed from `vm_id` is incorrect for VMs started with
/// `--vsock-dir`. Missing data is therefore an error rather than a fallback to
/// the conventional runtime path: snapshot bracketing must fail closed if it
/// cannot address the guest that is about to be paused.
pub(crate) fn recorded_vsock_socket_path(vm_state: &VmState) -> Result<&Path> {
    let path = vm_state
        .config
        .vsock_socket_path
        .as_deref()
        .ok_or_else(|| {
            anyhow::anyhow!(
                "running VM {} has no recorded vsock socket path",
                vm_state.vm_id
            )
        })?;
    anyhow::ensure!(
        path.is_absolute(),
        "running VM {} has non-absolute recorded vsock socket path {}",
        vm_state.vm_id,
        path.display()
    );
    Ok(path)
}

/// Run one side of the guest network boundary around a memory snapshot.
///
/// The caller supplies the exact socket recorded in the running VM's state.
/// This is intentionally part of both snapshot APIs: deriving a conventional
/// runtime path here would silently bypass `--vsock-dir`.
async fn run_snapshot_network_command(vsock_socket: &Path, vm_id: &str, flag: &str) -> Result<()> {
    use crate::commands::exec::run_exec_in_vm_captured;

    let command = vec![FC_AGENT_PATH.to_string(), flag.to_string()];
    let output = run_exec_in_vm_captured(vsock_socket, &command, false, vm_id)
        .await
        .with_context(|| format!("running `{FC_AGENT_PATH} {flag}` in guest {vm_id}"))?;
    anyhow::ensure!(
        output.exit_code == 0,
        "`{FC_AGENT_PATH} {flag}` failed in guest {vm_id} with exit code {}: \
         stdout={:?}, stderr={:?}",
        output.exit_code,
        output.stdout,
        output.stderr
    );
    Ok(())
}

/// Preserve the primary snapshot error plus every recovery error in execution
/// order. Cleanup failures must never replace the operation that made cleanup
/// necessary, and a successful snapshot is still an error if either recovery
/// step failed.
fn combine_snapshot_boundary_results(
    operation_result: Result<()>,
    hypervisor_resume_result: Result<()>,
    guest_network_resume_result: Result<()>,
) -> Result<()> {
    let mut combined = operation_result.err();
    for recovery_result in [hypervisor_resume_result, guest_network_resume_result] {
        if let Err(recovery_error) = recovery_result {
            combined = Some(match combined {
                Some(existing) => {
                    anyhow::anyhow!("{existing:#}; additionally: {recovery_error:#}")
                }
                None => recovery_error,
            });
        }
    }
    combined.map_or(Ok(()), Err)
}

/// Create a snapshot of the running VM.
///
/// # Locking
/// Caller MUST hold the per-VM snapshot lock (via `acquire_vm_snapshot_lock`)
/// before calling this function.
///
/// # Returns
/// Ok(()) on success, Err on failure. A normal source is resumed regardless of
/// success/failure after a successful pause. A disposable source remains paused on every
/// post-pause return so it cannot mutate after its prepared startup point.
pub async fn create_snapshot_core(
    client: &crate::firecracker::FirecrackerClient,
    mut snapshot_config: crate::storage::snapshot::SnapshotConfig,
    disk_path: &Path,
    vsock_socket_path: &Path,
    parent_snapshot_dir: Option<&Path>,
    extra_files: SnapshotExtraFiles<'_>,
    source_disposition: SnapshotSourceDisposition,
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

    // Close the guest's new-flow gate immediately before pausing. The manifest
    // written by this command is captured in guest memory, creating the exact
    // socket-generation boundary that restore cleanup consumes.
    //
    // failpoint: widen the window before preparation — never sleep after the
    // gate closes and before the pause.
    failpoint::hit_async("snapshot.pre_pause").await;
    info!(snapshot = %snapshot_config.name, "preparing guest network and pausing VM for snapshot");
    if let Err(error) = run_snapshot_network_command(
        vsock_socket_path,
        &snapshot_config.vm_id,
        "--prepare-snapshot-network",
    )
    .await
    {
        // The exec transport can lose the reply after fc-agent has already
        // closed the gate and brought eth0 down.  Recovery is safe to issue
        // after every invoked prepare: the guest serializes both commands on
        // one flock, so this blocks behind an in-flight prepare and then
        // reopens the exact transaction it completed.  No hypervisor pause was
        // invoked yet.
        let operation_error =
            error.context("preparing guest network boundary for Firecracker snapshot");
        let guest_network_resume_result = run_snapshot_network_command(
            vsock_socket_path,
            &snapshot_config.vm_id,
            "--resume-snapshot-network",
        )
        .await
        .context("reopening guest network after Firecracker preparation failure");
        let _ = tokio::fs::remove_dir_all(&temp_snapshot_dir).await;
        return combine_snapshot_boundary_results(
            Err(operation_error),
            Ok(()),
            guest_network_resume_result,
        );
    }
    let pause_result = pause_client
        .patch_vm_state(ApiVmState {
            state: "Paused".to_string(),
        })
        .await
        .context("pausing VM for snapshot");

    // A pause API error is ambiguous: the VMM may have paused before its reply
    // failed. Attempt a hypervisor resume even on error, then reopen the guest
    // gate only after that attempt so the command can run if the VM recovered.
    if let Err(pause_error) = pause_result {
        record_vsock_reset_for_snapshot(&snapshot_config.vm_id, &snapshot_config.name).await;
        let hypervisor_resume_result = snapshot_client
            .patch_vm_state(ApiVmState {
                state: "Resumed".to_string(),
            })
            .await
            .context("resuming VM after Firecracker pause failure");
        if let Err(error) = &hypervisor_resume_result {
            error!(snapshot = %snapshot_config.name, error = %error,
                "CRITICAL: failed to resume VM after pause error — VM may be paused!");
        }
        let guest_network_resume_result = run_snapshot_network_command(
            vsock_socket_path,
            &snapshot_config.vm_id,
            "--resume-snapshot-network",
        )
        .await
        .context("reopening guest network after Firecracker pause failure");
        let _ = tokio::fs::remove_dir_all(&temp_snapshot_dir).await;
        return combine_snapshot_boundary_results(
            Err(pause_error),
            hypervisor_resume_result,
            guest_network_resume_result,
        );
    }

    // VM is now paused — we MUST resume it before returning, no matter what.
    let mut use_diff = has_base;
    let mut snapshot_result = snapshot_client
        .create_snapshot(SnapshotCreate {
            snapshot_type: Some(snapshot_type.to_string()),
            snapshot_path: temp_vmstate_path.display().to_string(),
            mem_file_path: temp_memory_path.display().to_string(),
        })
        .await
        .context("creating Firecracker snapshot");

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
                        // Defer the error until the shared boundary recovery below
                        // runs (vsock-reset record, disposition-aware resume, gate
                        // reopen, result combination).
                        snapshot_result = Err(e)
                            .context("Full snapshot retry failed after diff tracking failure");
                    }
                }
            }
        }
    }

    // Copy disk while VM is still paused to maintain memory/disk consistency. If we copy
    // after resume, the disk may have post-resume writes that don't match the snapshot's
    // memory state, corrupting the filesystem on restore. Reflink is O(1), so pause time is
    // unaffected.
    let disk_copy_result = if snapshot_result.is_ok() {
        info!(snapshot = %snapshot_config.name, "copying disk (VM paused)");
        reflink_disks_to_snapshot(disk_path, &snapshot_config, &temp_snapshot_dir)
            .await
            .context("copying disk during snapshot")
    } else {
        Ok(()) // Skip disk copy if snapshot failed
    };

    // The pause/save silently orphans in-flight host↔guest vsock connections
    // (exec sessions block forever with no error on either side). Record it by
    // bumping the persisted vsock epoch BEFORE resuming — see
    // StateManager::bump_vsock_epoch for why this ordering makes the exec-side
    // orphan detection race-free.
    record_vsock_reset_for_snapshot(&snapshot_config.vm_id, &snapshot_config.name).await;

    // Resume a normal source regardless of snapshot/disk copy result. A prepare source is
    // disposable and intentionally remains paused until verified cleanup reaps it.
    // Memory merge happens after resume since it operates on snapshot files, not live disk.
    // failpoint: hold after the snapshot save (VM still paused) and before resume —
    // makes "client observes a saved-but-still-paused VM" (stalled exec/curl/vsock
    // across an arbitrarily long pause) deterministic.
    failpoint::hit_async("snapshot.post_save_pre_resume").await;
    let hypervisor_resume_result = match source_disposition {
        SnapshotSourceDisposition::Resume => snapshot_client
            .patch_vm_state(ApiVmState {
                state: "Resumed".to_string(),
            })
            .await
            .context("resuming VM after snapshot"),
        // A disposable prepare source must never run again; it stays paused
        // until teardown, so there is nothing to resume.
        SnapshotSourceDisposition::LeavePaused => Ok(()),
    };

    if let Err(e) = &hypervisor_resume_result {
        // Resume failure is critical — VM may be stuck paused.
        error!(snapshot = %snapshot_config.name, error = %e,
            "CRITICAL: failed to resume VM after snapshot — VM may be paused!");
    }

    // Reopen the guest gate after the hypervisor resume attempt on every path
    // that resumes. A LeavePaused source cannot execute the reopen (its vCPUs
    // never run again), and must not: the closed gate inside the artifact IS
    // the socket-generation boundary its restored clones consume.
    let guest_network_resume_result = match source_disposition {
        SnapshotSourceDisposition::Resume => run_snapshot_network_command(
            vsock_socket_path,
            &snapshot_config.vm_id,
            "--resume-snapshot-network",
        )
        .await
        .context("reopening guest network after Firecracker snapshot"),
        SnapshotSourceDisposition::LeavePaused => Ok(()),
    };
    if let Err(e) = &guest_network_resume_result {
        error!(snapshot = %snapshot_config.name, error = %e,
            "CRITICAL: failed to reopen guest network after snapshot!");
    }

    let operation_result = snapshot_result.and(disk_copy_result);
    if let Err(error) = combine_snapshot_boundary_results(
        operation_result,
        hypervisor_resume_result,
        guest_network_resume_result,
    ) {
        let _ = tokio::fs::remove_dir_all(&temp_snapshot_dir).await;
        return Err(error);
    }

    match source_disposition {
        SnapshotSourceDisposition::Resume => {
            info!(snapshot = %snapshot_config.name, "VM and guest network resumed, processing snapshot");
        }
        SnapshotSourceDisposition::LeavePaused => {
            info!(snapshot = %snapshot_config.name, "disposable source remains paused, processing snapshot");
        }
    }

    // NOTE: Do NOT bump restore-epoch here. Snapshot create DOES sever the guest's
    // established vsock connections — Firecracker's `Vsock::prepare_save` queues a
    // VIRTIO_VSOCK_EVENT_TRANSPORT_RESET into the (paused) guest during the save,
    // and the resumed source processes it exactly like a restored clone would
    // (this is what made a transport reset useless as a restore classifier; the
    // guest now re-asks the status listener, which answers from CacheVerdict).
    // But a severed connection is all it is: bumping restore-epoch here would
    // additionally trigger handle_clone_restore() in fc-agent, which kills TCP
    // connections and remounts NFS as if the whole VM had been replaced,
    // crashing the running container. Connection-level recovery (output/egress/
    // FUSE reconnect, exec rebind via the vsock_epoch below) is sufficient and
    // already happens. restore-epoch is bumped only in the restore path
    // (snapshot.rs) where a clone genuinely needs full restore handling.
    //
    // The HOST-side `vsock_epoch` (VmState) bumped above is a different mechanism:
    // it never reaches the guest. It only tells host exec clients that byte streams
    // in flight across the pause were silently lost (the CI-observed orphan mode),
    // so their blocked reads abort instead of hanging.

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

    atomic_replace_dir(&temp_snapshot_dir, snapshot_dir).await?;

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
    vsock_socket_path: &Path,
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

    // As on Firecracker, no fallible or blocking work may separate successful
    // guest preparation from the pause that captures its manifest.
    info!(snapshot = %snapshot_config.name, "preparing guest network and pausing Cloud Hypervisor VM for snapshot");
    if let Err(error) = run_snapshot_network_command(
        vsock_socket_path,
        &snapshot_config.vm_id,
        "--prepare-snapshot-network",
    )
    .await
    {
        // A failed exec response does not prove that preparation did not run.
        // Guest-side flock serialization makes an unconditional resume the
        // exact recovery operation even if the prepare command is still
        // finishing when this second exec arrives.
        let operation_error =
            error.context("preparing guest network boundary for Cloud Hypervisor snapshot");
        let guest_network_resume_result = run_snapshot_network_command(
            vsock_socket_path,
            &snapshot_config.vm_id,
            "--resume-snapshot-network",
        )
        .await
        .context("reopening guest network after Cloud Hypervisor preparation failure");
        let _ = tokio::fs::remove_dir_all(&temp_snapshot_dir).await;
        return combine_snapshot_boundary_results(
            Err(operation_error),
            Ok(()),
            guest_network_resume_result,
        );
    }
    let pause_result = client
        .pause_vm()
        .await
        .context("pausing Cloud Hypervisor VM for snapshot");

    // A failed pause response can still mean CH reached Paused. Always attempt
    // resume after invoking pause, then reopen the guest network gate.
    if let Err(pause_error) = pause_result {
        record_vsock_reset_for_snapshot(&snapshot_config.vm_id, &snapshot_config.name).await;
        let hypervisor_resume_result = client
            .resume_vm()
            .await
            .context("resuming Cloud Hypervisor VM after pause failure");
        if let Err(error) = &hypervisor_resume_result {
            error!(snapshot = %snapshot_config.name, error = %error,
                "CRITICAL: failed to resume Cloud Hypervisor VM after pause error — VM may be paused!");
        }
        let guest_network_resume_result = run_snapshot_network_command(
            vsock_socket_path,
            &snapshot_config.vm_id,
            "--resume-snapshot-network",
        )
        .await
        .context("reopening guest network after Cloud Hypervisor pause failure");
        let _ = tokio::fs::remove_dir_all(&temp_snapshot_dir).await;
        return combine_snapshot_boundary_results(
            Err(pause_error),
            hypervisor_resume_result,
            guest_network_resume_result,
        );
    }

    // VM is paused — we MUST resume it before returning, regardless of outcome.
    // Dumping all of guest RAM can take well over the default 30s API timeout for large
    // `--mem` VMs or slow disks, so scale it like the Firecracker path: max(300s, mem_gib*10).
    let dest_url = format!("file://{}", ch_dir.display());
    let mem_gib = snapshot_config.metadata.memory_mib / 1024;
    let snapshot_timeout =
        std::time::Duration::from_secs(std::cmp::max(300, (mem_gib as u64) * 10));
    let snapshot_result = client
        .with_timeout(snapshot_timeout)
        .snapshot_vm(&dest_url)
        .await
        .context("creating Cloud Hypervisor snapshot");

    // Reflink the disk(s) WHILE PAUSED so the disk image matches the captured memory
    // (a post-resume write would desync memory/disk and corrupt the clone's filesystem).
    let disk_copy_result = if snapshot_result.is_ok() {
        info!(snapshot = %snapshot_config.name, "copying disk (CH VM paused)");
        reflink_disks_to_snapshot(disk_path, &snapshot_config, &temp_snapshot_dir)
            .await
            .context("copying disk during Cloud Hypervisor snapshot")
    } else {
        Ok(())
    };

    // Same as the Firecracker path: record the vsock reset caused by the
    // pause/save BEFORE resuming, so blocked exec sessions abort loudly.
    record_vsock_reset_for_snapshot(&snapshot_config.vm_id, &snapshot_config.name).await;

    // Resume ALWAYS, even if the snapshot or disk copy failed.
    let hypervisor_resume_result = client
        .resume_vm()
        .await
        .context("resuming Cloud Hypervisor VM after snapshot");
    if let Err(e) = &hypervisor_resume_result {
        error!(snapshot = %snapshot_config.name, error = %e,
            "CRITICAL: failed to resume Cloud Hypervisor VM after snapshot — VM may be paused!");
    }

    let guest_network_resume_result = run_snapshot_network_command(
        vsock_socket_path,
        &snapshot_config.vm_id,
        "--resume-snapshot-network",
    )
    .await
    .context("reopening guest network after Cloud Hypervisor snapshot");
    if let Err(e) = &guest_network_resume_result {
        error!(snapshot = %snapshot_config.name, error = %e,
            "CRITICAL: failed to reopen guest network after Cloud Hypervisor snapshot!");
    }

    let operation_result = snapshot_result.and(disk_copy_result);
    if let Err(error) = combine_snapshot_boundary_results(
        operation_result,
        hypervisor_resume_result,
        guest_network_resume_result,
    ) {
        let _ = tokio::fs::remove_dir_all(&temp_snapshot_dir).await;
        return Err(error);
    }

    info!(snapshot = %snapshot_config.name, "CH VM and guest network resumed, finalizing snapshot");

    // Write fcvm's metadata config.json, then atomically swap the dir into place.
    let temp_config_path = temp_snapshot_dir.join("config.json");
    let config_json =
        serde_json::to_string_pretty(&snapshot_config).context("serializing snapshot config")?;
    tokio::fs::write(&temp_config_path, &config_json)
        .await
        .context("writing snapshot config")?;

    atomic_replace_dir(&temp_snapshot_dir, snapshot_dir).await?;

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

    #[test]
    fn verified_cleanup_reports_every_failure_after_all_attempts() {
        let mut failures = CleanupFailures::default();
        failures.record(
            "signalling VMM",
            Err::<(), _>(anyhow::anyhow!("kill denied")),
        );
        failures.record(
            "cleaning network",
            Err::<(), _>(anyhow::anyhow!("netlink denied")),
        );
        failures.record("deleting state", Ok(()));

        let error = failures
            .into_result()
            .expect_err("verified cleanup must fail when any cleanup action failed");
        assert_eq!(
            format!("{error:#}"),
            "verified VM cleanup failed: signalling VMM: kill denied; cleaning network: netlink denied"
        );
    }

    #[test]
    fn snapshot_boundary_error_preserves_operation_and_both_recovery_failures() {
        let error = combine_snapshot_boundary_results(
            Err(anyhow::anyhow!("snapshot save failed")),
            Err(anyhow::anyhow!("hypervisor resume failed")),
            Err(anyhow::anyhow!("guest network reopen failed")),
        )
        .unwrap_err();

        assert_eq!(
            format!("{error:#}"),
            "snapshot save failed; additionally: hypervisor resume failed; additionally: guest network reopen failed"
        );
    }

    #[test]
    fn verified_cleanup_checks_exact_process_identity_is_gone() {
        let pid = std::process::id();
        let start = crate::utils::process_start_time(pid).unwrap();
        let error = verify_process_reaped("test", Some((pid, start))).unwrap_err();
        assert!(format!("{error:#}").contains("is still running after cleanup"));

        verify_process_reaped("test", Some((pid, start + 1)))
            .expect("a reused PID with a different start time is not the owned process");
        verify_process_reaped("test", None).expect("an unstarted process owns no resource");
    }

    #[test]
    fn snapshot_boundary_recovery_failure_fails_successful_operation() {
        let error = combine_snapshot_boundary_results(
            Ok(()),
            Ok(()),
            Err(anyhow::anyhow!("guest network reopen failed")),
        )
        .unwrap_err();

        assert_eq!(format!("{error:#}"), "guest network reopen failed");
    }

    #[test]
    fn snapshot_boundary_uses_exact_recorded_custom_vsock_path() {
        let mut state = make_vm_state("vm-custom-vsock", None);
        state.config.vsock_socket_path =
            Some(std::path::PathBuf::from("/srv/custom-vsock/vsock.sock"));

        assert_eq!(
            recorded_vsock_socket_path(&state).unwrap(),
            Path::new("/srv/custom-vsock/vsock.sock")
        );

        state.config.vsock_socket_path = None;
        let error = recorded_vsock_socket_path(&state).unwrap_err();
        assert_eq!(
            error.to_string(),
            "running VM vm-custom-vsock has no recorded vsock socket path"
        );

        state.config.vsock_socket_path = Some(std::path::PathBuf::from("relative/vsock.sock"));
        let error = recorded_vsock_socket_path(&state).unwrap_err();
        assert!(error
            .to_string()
            .contains("non-absolute recorded vsock socket path"));
    }

    #[test]
    fn restore_redirect_covers_exact_custom_vsock_source_directory() {
        let mut dirs = vec![PathBuf::from("/runtime/source-vm")];
        add_source_vsock_redirect_dir(&mut dirs, Path::new("/srv/dedicated-vsock/vsock.sock"))
            .unwrap();
        assert_eq!(
            dirs,
            vec![
                PathBuf::from("/runtime/source-vm"),
                PathBuf::from("/srv/dedicated-vsock")
            ]
        );

        // Adding the conventional path again must not create two bind mounts
        // onto the same directory.
        add_source_vsock_redirect_dir(&mut dirs, Path::new("/runtime/source-vm/vsock.sock"))
            .unwrap();
        assert_eq!(dirs.len(), 2);
    }

    #[test]
    fn restore_rejects_vsock_metadata_that_does_not_match_vmstate() {
        let temp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(temp.path(), b"prefix/srv/right/vsock.sock/suffix").unwrap();
        assert_vmstate_vsock_source_matches(temp.path(), Path::new("/srv/right/vsock.sock"))
            .unwrap();
        let error =
            assert_vmstate_vsock_source_matches(temp.path(), Path::new("/srv/wrong/vsock.sock"))
                .unwrap_err();
        assert!(error
            .to_string()
            .contains("does not contain recorded source"));
    }

    /// A health monitor that ignores cooperative cancellation must still be gone before the
    /// stop barrier returns. Before the fix, the timeout branch dropped its `JoinHandle`,
    /// detaching the task; this test then observed `dropped == false`.
    #[tokio::test(start_paused = true)]
    async fn health_monitor_timeout_aborts_and_reaps_task() {
        struct DropOracle(std::sync::Arc<std::sync::atomic::AtomicBool>);

        impl Drop for DropOracle {
            fn drop(&mut self) {
                self.0.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }

        let cancel = tokio_util::sync::CancellationToken::new();
        let ignored_cancel = cancel.clone();
        let dropped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let task_dropped = std::sync::Arc::clone(&dropped);
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let health_monitor = tokio::spawn(async move {
            let _drop_oracle = DropOracle(task_dropped);
            let _ignored_cancel = ignored_cancel;
            started_tx.send(()).unwrap();
            // Deliberately ignore graceful cancellation forever. Tokio abort must destroy
            // this future, and awaiting its JoinHandle must observe that destruction.
            std::future::pending::<()>().await;
        });
        started_rx.await.unwrap();
        cancel.cancel();

        let stopper = tokio::spawn(stop_health_monitor(health_monitor));
        tokio::task::yield_now().await;
        assert!(
            !stopper.is_finished(),
            "the graceful-stop budget must be honored"
        );

        tokio::time::advance(HEALTH_MONITOR_STOP_BUDGET - std::time::Duration::from_millis(1))
            .await;
        tokio::task::yield_now().await;
        assert!(
            !dropped.load(std::sync::atomic::Ordering::SeqCst),
            "the monitor must not be aborted before its graceful-stop budget"
        );

        tokio::time::advance(std::time::Duration::from_millis(1)).await;
        tokio::time::timeout(std::time::Duration::from_secs(1), stopper)
            .await
            .expect("health-monitor stop remained blocked after abort")
            .expect("health-monitor stopper task panicked");
        assert!(
            dropped.load(std::sync::atomic::Ordering::SeqCst),
            "the timed-out health-monitor future must be dropped before the stop barrier returns"
        );
    }

    /// Regression guard for the deaf-clone bug: a snapshot taken from a restored
    /// VM embeds the ancestor's restore epoch in guest memory, so two restores in
    /// the same wall-clock second MUST still produce different epochs — otherwise
    /// fc-agent sees an unchanged epoch, skips handle_clone_restore, and the clone
    /// never becomes healthy. Wall-clock-second epochs fail this; unique-per-call
    /// epochs pass.
    #[test]
    fn restore_epochs_differ_for_back_to_back_restores() {
        let a = new_restore_epoch();
        let b = new_restore_epoch();
        assert_ne!(
            a, b,
            "restore epochs minted back-to-back (same wall-clock second) must differ"
        );
    }

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
        state.config.vsock_socket_path =
            Some(PathBuf::from(format!("/runtime/{vm_id}/vsock.sock")));
        state.config.source_vsock_socket_path = Some(PathBuf::from(format!(
            "/runtime/{}/vsock.sock",
            original_vsock.unwrap_or(vm_id)
        )));
        state
    }

    #[test]
    fn snapshot_create_lock_requests_are_canonical_and_deduplicate_self_parent() {
        let parent = Path::new("/snapshots/a-parent");
        let target = Path::new("/snapshots/z-target");
        assert_eq!(
            snapshot_create_lock_requests(target, Some(parent)),
            vec![(parent.to_path_buf(), false), (target.to_path_buf(), true)]
        );
        assert_eq!(
            snapshot_create_lock_requests(target, Some(target)),
            vec![(target.to_path_buf(), true)]
        );
    }

    #[tokio::test]
    async fn snapshot_create_generation_locks_pin_parent_until_drop() {
        let tmp = tempfile::TempDir::new().unwrap();
        let parent = tmp.path().join("a-parent");
        let target = tmp.path().join("z-target");
        let locks = acquire_snapshot_create_generation_locks(&target, Some(&parent))
            .await
            .unwrap();

        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let contender_parent = parent.clone();
        let contender = tokio::spawn(async move {
            started_tx.send(()).unwrap();
            acquire_snapshot_dir_lock(&contender_parent, true)
                .await
                .unwrap()
        });
        started_rx.await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        assert!(
            !contender.is_finished(),
            "an exclusive parent replacement must wait for the create's shared pin"
        );

        drop(locks);
        let contender_lock = tokio::time::timeout(std::time::Duration::from_secs(1), contender)
            .await
            .expect("parent replacement did not acquire the released generation lock")
            .unwrap();
        drop(contender_lock);

        // A target that is also its own parent uses one exclusive lock rather than
        // attempting a second flock on the same inode.
        let self_parent = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            acquire_snapshot_create_generation_locks(&target, Some(&target)),
        )
        .await
        .expect("target==parent lock acquisition self-deadlocked")
        .unwrap();
        drop(self_parent);
    }

    #[test]
    fn snapshot_vm_identity_revalidation_fails_closed() {
        let pid = std::process::id();
        let start = crate::utils::process_start_time(pid).unwrap();
        let mut expected = make_vm_state("vm-original", None);
        expected.pid = Some(pid);
        expected.pid_start_time = Some(start);

        let current = expected.clone();
        validate_snapshot_vm_identity(&expected, &current).unwrap();

        let mut replacement = current.clone();
        replacement.vm_id = "vm-replacement".to_string();
        assert!(validate_snapshot_vm_identity(&expected, &replacement).is_err());

        let mut changed_process = current.clone();
        changed_process.pid_start_time = Some(start + 1);
        assert!(validate_snapshot_vm_identity(&expected, &changed_process).is_err());

        let mut missing_identity = current.clone();
        missing_identity.pid_start_time = None;
        assert!(validate_snapshot_vm_identity(&expected, &missing_identity).is_err());

        let mut reused = current.clone();
        reused.pid_start_time = Some(start + 1);
        assert!(validate_snapshot_vm_identity(&reused, &reused).is_err());
    }

    #[tokio::test]
    async fn lifecycle_ready_publication_is_atomic_and_identity_guarded() {
        let tmp = tempfile::TempDir::new().unwrap();
        let manager = crate::state::StateManager::new(tmp.path().to_path_buf());
        manager.init().await.unwrap();

        let pid = std::process::id();
        let start = crate::utils::process_start_time(pid).unwrap();
        let mut state = make_vm_state("vm-ready", None);
        state.pid = Some(pid);
        state.pid_start_time = Some(start);
        manager.save_state(&state).await.unwrap();

        publish_lifecycle_ready(&manager, &mut state).await.unwrap();
        assert!(state.lifecycle_ready);
        assert!(
            manager
                .load_state("vm-ready")
                .await
                .unwrap()
                .lifecycle_ready
        );

        manager
            .update_state("vm-ready", |persisted| persisted.lifecycle_ready = false)
            .await
            .unwrap();
        let mut stale = state.clone();
        stale.lifecycle_ready = false;
        stale.pid_start_time = Some(start + 1);
        assert!(publish_lifecycle_ready(&manager, &mut stale).await.is_err());
        assert!(
            !manager
                .load_state("vm-ready")
                .await
                .unwrap()
                .lifecycle_ready,
            "a stale process identity must not publish readiness"
        );
    }

    #[tokio::test]
    async fn cancellation_between_precheck_and_ready_claim_prevents_publication() {
        let tmp = tempfile::TempDir::new().unwrap();
        let state_dir = tmp.path().to_path_buf();
        let manager = crate::state::StateManager::new(state_dir.clone());
        manager.init().await.unwrap();

        let pid = std::process::id();
        let start = crate::utils::process_start_time(pid).unwrap();
        let mut state = make_vm_state("vm-ready-cancelled", None);
        state.pid = Some(pid);
        state.pid_start_time = Some(start);
        manager.save_state(&state).await.unwrap();

        let gate = LifecycleReadyGate::new();
        let publisher_gate = gate.clone();
        let publisher_cancel = gate.cancellation_token();
        let (checked_tx, checked_rx) = tokio::sync::oneshot::channel();
        let (continue_tx, continue_rx) = tokio::sync::oneshot::channel();
        let publisher = tokio::spawn(async move {
            // Reproduce the old call-site interleave exactly: setup observes no
            // cancellation, then the signal handler wins before the async save.
            assert!(!publisher_cancel.is_cancelled());
            checked_tx.send(()).unwrap();
            continue_rx.await.unwrap();

            let manager = crate::state::StateManager::new(state_dir);
            let outcome = publisher_gate.publish(&manager, &mut state).await.unwrap();
            (outcome, state)
        });

        checked_rx.await.unwrap();
        gate.cancel();
        continue_tx.send(()).unwrap();

        let (outcome, state) = publisher.await.unwrap();
        assert_eq!(outcome, LifecycleReadyOutcome::Cancelled);
        assert!(!state.lifecycle_ready);
        assert!(gate.cancellation_token().is_cancelled());
        assert!(
            !manager
                .load_state("vm-ready-cancelled")
                .await
                .unwrap()
                .lifecycle_ready,
            "cancellation that wins the lifecycle gate must leave persisted readiness false"
        );
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
        )
        .unwrap();
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
        )
        .unwrap();
        assert_eq!(config.vm_id, "vm-BBB");
        // Critical: original_vsock_vm_id must be vm-AAA (the ORIGINAL), not vm-BBB
        assert_eq!(config.original_vsock_vm_id, Some("vm-AAA".to_string()));
    }

    #[test]
    fn snapshot_config_preserves_vmm_source_vsock_path_not_clone_listener() {
        let mut state = make_vm_state("vm-clone", Some("vm-source"));
        state.config.vsock_socket_path = Some(PathBuf::from("/runtime/vm-clone/vsock.sock"));
        state.config.source_vsock_socket_path =
            Some(PathBuf::from("/srv/custom-source/vsock.sock"));

        let config = build_snapshot_config(
            &state,
            "grand-clone-source",
            SnapshotType::User,
            Path::new("/tmp/snap"),
            vec![],
            vec![],
        )
        .unwrap();
        assert_eq!(
            config.source_vsock_socket_path,
            PathBuf::from("/srv/custom-source/vsock.sock")
        );
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
        )
        .unwrap();
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
        )
        .unwrap();
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
        )
        .unwrap();
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
        )
        .unwrap();
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
            "generation_id": "3f53da72-8c67-4ec7-8f3d-d25367129872",
            "network_boundary_version": 1,
            "source_vsock_socket_path": "/runtime/vm-OLD/vsock.sock",
            "memory_path": "/tmp/memory.bin",
            "vmstate_path": "/tmp/vmstate.bin",
            "disk_path": "/tmp/disk.raw",
            "created_at": "2026-01-01T00:00:00Z",
            "snapshot_type": "User",
            "kind": "Full",
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
                build_snapshot_config(&state, name, SnapshotType::System, &dir, vec![], vec![])
                    .unwrap();
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

    /// #608 guard for the CH restore path: a WRITABLE disk path outside the baseline
    /// bind-mount dirs must be flagged (CH opens it directly and would corrupt a sibling
    /// disk); a read-only external disk anywhere is fine.
    #[test]
    fn test_ch_config_disk_coverage() {
        use serde_json::json;

        let cfg = json!({
            "disks": [
                { "path": "/data/vm-disks/vm-AAA/disks/rootfs.raw", "readonly": false },
                // a read-only external --disk pointing under ANOTHER vm -> must be ignored
                { "path": "/data/vm-disks/vm-ext999/disks/data.raw", "readonly": true },
            ]
        });

        // rootfs under the covered dir -> all writable disks covered.
        assert!(
            ch_config_uncovered_writable_disk(&cfg, &[PathBuf::from("/data/vm-disks/vm-AAA")])
                .is_none()
        );

        // Same id, DIFFERENT data_dir prefix -> the writable rootfs is uncovered (#638 class).
        assert_eq!(
            ch_config_uncovered_writable_disk(&cfg, &[PathBuf::from("/other/vm-disks/vm-AAA")]),
            Some("/data/vm-disks/vm-AAA/disks/rootfs.raw")
        );

        // A baseline for a sibling vm -> the writable rootfs is uncovered (the #608 corruption).
        assert_eq!(
            ch_config_uncovered_writable_disk(&cfg, &[PathBuf::from("/data/vm-disks/vm-BBB")]),
            Some("/data/vm-disks/vm-AAA/disks/rootfs.raw")
        );

        // A WRITABLE second disk outside the covered dirs is flagged.
        let cfg2 = json!({
            "disks": [
                { "path": "/data/vm-disks/vm-AAA/disks/rootfs.raw", "readonly": false },
                { "path": "/data/vm-disks/vm-CCC/disks/data.raw", "readonly": false },
            ]
        });
        assert_eq!(
            ch_config_uncovered_writable_disk(&cfg2, &[PathBuf::from("/data/vm-disks/vm-AAA")]),
            Some("/data/vm-disks/vm-CCC/disks/data.raw")
        );

        // No disks array -> nothing to open, nothing uncovered.
        assert!(ch_config_uncovered_writable_disk(&json!({}), &[PathBuf::from("/data")]).is_none());
    }

    #[test]
    fn test_ch_config_requires_exact_recorded_source_vsock_path() {
        use serde_json::json;

        let source = Path::new("/srv/source-vsock/vsock.sock");
        let matching = json!({
            "vsock": {
                "cid": 3,
                "socket": "/srv/source-vsock/vsock.sock"
            }
        });
        assert_ch_config_vsock_source_matches(&matching, source).unwrap();

        let stale = json!({
            "vsock": {
                "cid": 3,
                "socket": "/runtime/default/vsock.sock"
            }
        });
        let error = assert_ch_config_vsock_source_matches(&stale, source).unwrap_err();
        assert!(error.to_string().contains("does not match recorded source"));
        assert!(error.to_string().contains("/runtime/default/vsock.sock"));
        assert!(error.to_string().contains("/srv/source-vsock/vsock.sock"));

        for invalid in [json!({}), json!({ "vsock": null }), json!({ "vsock": {} })] {
            let error = assert_ch_config_vsock_source_matches(&invalid, source).unwrap_err();
            assert!(error.to_string().contains("has no vsock.socket"));
        }
    }
}
