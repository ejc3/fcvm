use anyhow::{bail, Context, Result};
use fs2::FileExt;
use std::net::ToSocketAddrs;
use std::path::{Path, PathBuf};
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, info, warn};

use crate::cli::{NetworkMode, PodmanArgs, PodmanCommands, RunArgs};

/// Resolve a proxy URL's hostname to an IP address.
///
/// VMs using slirp4netns with --enable-ipv6 can reach both IPv4 (via 10.0.2.2 gateway)
/// and IPv6 (via fd00::2 gateway) addresses. We prefer IPv4 but fall back to IPv6.
/// Returns None only if the hostname can't be resolved at all.
fn resolve_proxy_url(url: &str) -> Option<String> {
    // Parse URL to extract scheme, host, port
    let scheme = if url.starts_with("https://") {
        "https"
    } else {
        "http"
    };
    let without_scheme = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url);

    // Parse host:port
    let (host_port, path) = without_scheme
        .find('/')
        .map(|i| (&without_scheme[..i], &without_scheme[i..]))
        .unwrap_or((without_scheme, ""));

    // Try to resolve to an IP address (prefer IPv4, fall back to IPv6)
    match host_port.to_socket_addrs() {
        Ok(addrs) => {
            let addrs: Vec<_> = addrs.collect();

            // First pass: look for IPv4
            for addr in &addrs {
                if addr.is_ipv4() {
                    debug!(
                        original = %url,
                        resolved = %addr,
                        "resolved proxy hostname to IPv4"
                    );
                    return Some(format!("{}://{}{}", scheme, addr, path));
                }
            }

            // Second pass: use IPv6 if no IPv4 available
            // With --enable-ipv6 and --outbound-addr6, VM can reach IPv6 via fd00::2 gateway
            for addr in &addrs {
                if addr.is_ipv6() {
                    info!(
                        original = %url,
                        resolved = %addr,
                        "resolved proxy hostname to IPv6 (no IPv4 available)"
                    );
                    // Format IPv6 with brackets: http://[::1]:8080
                    let ip = addr.ip();
                    let port = addr.port();
                    return Some(format!("{}://[{}]:{}{}", scheme, ip, port, path));
                }
            }

            warn!(url = %url, "proxy resolved but no addresses found");
            None
        }
        Err(e) => {
            warn!(url = %url, error = %e, "failed to resolve proxy hostname");
            None
        }
    }
}
use crate::firecracker::VmManager;
use crate::network::{BridgedNetwork, NetworkConfig, NetworkManager, PortMapping, SlirpNetwork};
use crate::paths;
use crate::state::{generate_vm_id, truncate_id, validate_vm_name, StateManager, VmState};
use crate::storage::{
    DiskManager, SnapshotConfig, SnapshotManager, SnapshotMetadata, SnapshotType,
    SnapshotVolumeConfig,
};
use crate::volume::{spawn_volume_servers, VolumeConfig};

/// Request to create a podman cache snapshot.
/// Sent from status listener to main task when fc-agent signals cache-ready.
struct CacheRequest {
    /// Image digest from fc-agent
    digest: String,
    /// Oneshot channel to signal completion back to status listener
    ack_tx: oneshot::Sender<()>,
}

/// Parameters for creating a snapshot, used by both podman run and snapshot run.
/// This allows snapshot creation from both fresh VMs (using RunArgs) and
/// restored VMs (using existing snapshot metadata).
pub struct SnapshotCreationParams {
    /// Container image name
    pub image: String,
    /// Number of vCPUs
    pub vcpu: u8,
    /// Memory in MiB
    pub memory_mib: u32,
}

impl SnapshotCreationParams {
    /// Create from RunArgs (for fresh VMs)
    pub fn from_run_args(args: &RunArgs) -> Self {
        Self {
            image: args.image.clone(),
            vcpu: args.cpu,
            memory_mib: args.mem,
        }
    }

    /// Create from SnapshotMetadata (for restored VMs)
    pub fn from_metadata(metadata: &SnapshotMetadata) -> Self {
        Self {
            image: metadata.image.clone(),
            vcpu: metadata.vcpu,
            memory_mib: metadata.memory_mib,
        }
    }
}

/// Result of a snapshot creation attempt that can be interrupted by signals.
pub enum SnapshotOutcome {
    /// Snapshot created successfully
    Created,
    /// Snapshot creation failed
    Failed(anyhow::Error),
    /// Signal received during creation (caller should break and shutdown)
    Interrupted,
}

/// Validate that a Docker archive contains manifest.json.
///
/// Docker archive format requires manifest.json to be loadable.
/// If this file is missing, the archive is corrupted and will fail to load.
fn validate_docker_archive(archive_path: &Path) -> Result<bool> {
    let tar_file = std::fs::File::open(archive_path)
        .with_context(|| format!("opening archive {} for validation", archive_path.display()))?;

    let mut archive = tar::Archive::new(tar_file);

    for entry in archive.entries().context("reading archive entries")? {
        let entry = entry.context("reading archive entry")?;
        if let Ok(path) = entry.path() {
            if path.to_str() == Some("manifest.json") {
                return Ok(true);
            }
        }
    }

    Ok(false)
}

/// Create a snapshot with signal interruption support.
///
/// This wraps `create_podman_snapshot` in a `tokio::select!` that checks for
/// SIGTERM/SIGINT, allowing graceful shutdown during snapshot creation.
///
/// Returns `SnapshotOutcome::Interrupted` if a signal is received - caller
/// should break their event loop and proceed to cleanup.
#[allow(clippy::too_many_arguments)]
pub async fn create_snapshot_interruptible(
    vm_manager: &VmManager,
    snapshot_key: &str,
    vm_id: &str,
    params: &SnapshotCreationParams,
    disk_path: &Path,
    network_config: &NetworkConfig,
    volume_configs: &[VolumeConfig],
    parent_snapshot_key: Option<&str>,
    sigterm: &mut tokio::signal::unix::Signal,
    sigint: &mut tokio::signal::unix::Signal,
) -> SnapshotOutcome {
    let snapshot_fut = create_podman_snapshot(
        vm_manager,
        snapshot_key,
        vm_id,
        params,
        disk_path,
        network_config,
        volume_configs,
        parent_snapshot_key,
    );

    tokio::select! {
        biased; // Check signals first
        _ = sigterm.recv() => {
            info!("received SIGTERM during snapshot creation, shutting down VM");
            SnapshotOutcome::Interrupted
        }
        _ = sigint.recv() => {
            info!("received SIGINT during snapshot creation, shutting down VM");
            SnapshotOutcome::Interrupted
        }
        result = snapshot_fut => {
            match result {
                Ok(()) => SnapshotOutcome::Created,
                Err(e) => SnapshotOutcome::Failed(e),
            }
        }
    }
}

// Podman cache now uses SnapshotConfig from storage module.
// Cache key becomes the snapshot name, stored in paths::snapshot_dir().

/// Parsed volume mapping from --map HOST:GUEST[:ro]
#[derive(Debug, Clone)]
struct VolumeMapping {
    host_path: PathBuf,
    guest_path: String,
    read_only: bool,
}

impl VolumeMapping {
    /// Parse a volume spec string: HOST:GUEST[:ro]
    fn parse(spec: &str) -> Result<Self> {
        let parts: Vec<&str> = spec.split(':').collect();
        if parts.len() < 2 {
            bail!("Invalid volume spec '{}': expected HOST:GUEST[:ro]", spec);
        }

        let host_path = PathBuf::from(parts[0]);
        let guest_path = parts[1].to_string();
        let read_only = parts.len() > 2 && parts[2] == "ro";

        // Validate host path exists
        if !host_path.exists() {
            bail!("Volume host path does not exist: {}", host_path.display());
        }

        // Validate guest path is absolute
        if !guest_path.starts_with('/') {
            bail!(
                "Volume guest path must be absolute: {} (from spec '{}')",
                guest_path,
                spec
            );
        }

        Ok(Self {
            host_path,
            guest_path,
            read_only,
        })
    }
}

/// Build FirecrackerConfig from run args.
/// The config is the single source of truth for both cache key and VM launch.
fn build_firecracker_config(
    args: &RunArgs,
    image_identifier: &str,
    kernel_path: &Path,
    rootfs_path: &Path,
    initrd_path: &Path,
    cmd_args: Option<Vec<String>>,
) -> crate::firecracker::FirecrackerConfig {
    use crate::firecracker::{FcNetworkMode, FirecrackerConfig};

    let network_mode = match args.network {
        crate::cli::args::NetworkMode::Bridged => FcNetworkMode::Bridged,
        crate::cli::args::NetworkMode::Rootless => FcNetworkMode::Rootless,
    };

    // Collect extra disk specifications for cache key.
    // These are block devices that must match between cache create and restore.
    let mut extra_disks: Vec<String> = Vec::new();
    extra_disks.extend(args.disk.iter().cloned());
    extra_disks.extend(args.disk_dir.iter().cloned());
    extra_disks.extend(args.nfs.iter().cloned());

    // Collect env vars for cache key (affects container behavior)
    let env_vars: Vec<String> = args.env.to_vec();

    // Collect volume mounts for cache key (affects MMDS plan)
    let volume_mounts: Vec<String> = args.map.to_vec();

    FirecrackerConfig::new(
        kernel_path.to_path_buf(),
        initrd_path.to_path_buf(),
        rootfs_path.to_path_buf(),
        image_identifier.to_string(),
        cmd_args,
        args.cpu,
        args.mem,
        network_mode,
        crate::paths::data_dir(),
        extra_disks,
        env_vars,
        volume_mounts,
        args.privileged,
        args.tty,
        args.interactive,
        args.rootfs_size.clone(),
    )
}

/// Get the image identifier for cache key computation.
///
/// For localhost/ images: returns SHA256 digest from podman (requires podman)
/// For remote images: returns the image URL/name as-is (no podman needed)
async fn get_image_identifier(image: &str) -> Result<String> {
    if image.starts_with("localhost/") {
        // Use podman to get the digest for localhost images
        let output = tokio::process::Command::new("podman")
            .args(["image", "inspect", image, "--format", "{{.Digest}}"])
            .output()
            .await
            .context("running podman inspect")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("Failed to get digest for image '{}': {}", image, stderr);
        }

        let digest = String::from_utf8_lossy(&output.stdout)
            .trim()
            .trim_start_matches("sha256:")
            .to_string();

        Ok(digest)
    } else {
        // For remote images, use the image name/URL as identifier
        Ok(image.to_string())
    }
}

/// Check if a podman snapshot exists.
/// Uses SnapshotManager to check for the snapshot with snapshot_key as name.
pub async fn check_podman_snapshot(snapshot_key: &str) -> Option<SnapshotConfig> {
    let snapshot_manager = SnapshotManager::new(paths::snapshot_dir());
    snapshot_manager.load_snapshot(snapshot_key).await.ok()
}

/// Generate the startup snapshot key from a base snapshot key.
///
/// Startup snapshots capture VM state after the container reports healthy,
/// enabling subsequent runs to skip application initialization time.
pub fn startup_snapshot_key(base_key: &str) -> String {
    format!("{}-startup", base_key)
}

/// Create a podman snapshot from a running VM.
///
/// This pauses the VM, creates a Firecracker snapshot, copies the disk,
/// saves metadata using SnapshotManager, and resumes the VM.
///
/// The snapshot is stored in snapshot_dir with snapshot_key as the name,
/// making it accessible via `fcvm snapshot run --snapshot <snapshot_key>`.
///
/// If `parent_snapshot_key` is provided, the parent's memory.bin will be copied
/// (via reflink) as a base, enabling diff snapshots for new directories.
#[allow(clippy::too_many_arguments)]
pub async fn create_podman_snapshot(
    vm_manager: &VmManager,
    snapshot_key: &str,
    vm_id: &str,
    params: &SnapshotCreationParams,
    disk_path: &Path,
    network_config: &NetworkConfig,
    volume_configs: &[VolumeConfig],
    parent_snapshot_key: Option<&str>,
) -> Result<()> {
    // Snapshots stored in snapshot_dir with snapshot_key as name
    let snapshot_dir = paths::snapshot_dir().join(snapshot_key);

    // Lock to prevent concurrent snapshot creation
    let lock_path = snapshot_dir.with_extension("lock");
    tokio::fs::create_dir_all(paths::snapshot_dir())
        .await
        .context("creating snapshot directory")?;

    let lock_file = std::fs::File::create(&lock_path).context("creating snapshot lock file")?;

    // Use try_lock in a loop so we yield to the async runtime and can be interrupted
    use fs2::FileExt;
    loop {
        match lock_file.try_lock_exclusive() {
            Ok(()) => break,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                // Lock is held by another process, yield and retry
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            Err(e) => return Err(anyhow::anyhow!("acquiring snapshot lock: {}", e)),
        }
    }

    // Double-check after lock (another process might have created it)
    if snapshot_dir.join("config.json").exists() {
        info!(snapshot_key = %snapshot_key, "Snapshot already exists (created by another process)");
        return Ok(());
    }

    info!(snapshot_key = %snapshot_key, "Creating podman snapshot");

    // Get Firecracker client
    let client = vm_manager.client().context("VM not started")?;

    // Convert VolumeConfig to SnapshotVolumeConfig for metadata
    let snapshot_volumes: Vec<SnapshotVolumeConfig> = volume_configs
        .iter()
        .map(|v| SnapshotVolumeConfig {
            host_path: v.host_path.clone(),
            guest_path: v.guest_path.to_string_lossy().to_string(),
            read_only: v.read_only,
            vsock_port: v.port,
        })
        .collect();

    // Build final paths (create_snapshot_core handles temp dir)
    let final_memory_path = snapshot_dir.join("memory.bin");
    let final_vmstate_path = snapshot_dir.join("vmstate.bin");
    let final_disk_path = snapshot_dir.join("disk.raw");

    // Build snapshot config with final paths
    let snapshot_config = SnapshotConfig {
        name: snapshot_key.to_string(),
        vm_id: vm_id.to_string(),
        original_vsock_vm_id: None, // Fresh VM, no redirect needed
        memory_path: final_memory_path,
        vmstate_path: final_vmstate_path,
        disk_path: final_disk_path,
        created_at: chrono::Utc::now(),
        snapshot_type: SnapshotType::System, // Auto-generated cache snapshot
        metadata: SnapshotMetadata {
            image: params.image.clone(),
            vcpu: params.vcpu,
            memory_mib: params.memory_mib,
            network_config: network_config.clone(),
            volumes: snapshot_volumes,
        },
    };

    // Use shared core function for snapshot creation
    // If parent key provided, resolve to directory path
    let parent_dir = parent_snapshot_key.map(|key| paths::snapshot_dir().join(key));
    super::common::create_snapshot_core(client, snapshot_config, disk_path, parent_dir.as_deref())
        .await
}

use super::common::{VSOCK_OUTPUT_PORT, VSOCK_STATUS_PORT, VSOCK_TTY_PORT, VSOCK_VOLUME_PORT_BASE};

/// Create an ext4 disk image from a directory's contents.
/// Returns the path to the created image.
async fn create_disk_from_dir(
    source_dir: &std::path::Path,
    output_path: &std::path::Path,
) -> Result<()> {
    use std::process::Stdio;

    // Calculate directory size (add 20% overhead for ext4 metadata, min 16MB)
    let dir_size = tokio::process::Command::new("du")
        .args(["-sb", source_dir.to_str().unwrap()])
        .output()
        .await
        .context("calculating directory size")?;

    let size_str = String::from_utf8_lossy(&dir_size.stdout);
    let size_bytes: u64 = size_str
        .split_whitespace()
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or(16 * 1024 * 1024);

    // Add 20% overhead, minimum 16MB
    let image_size = std::cmp::max(size_bytes * 120 / 100, 16 * 1024 * 1024);

    info!(
        "Creating disk image from {}: {} bytes -> {} bytes",
        source_dir.display(),
        size_bytes,
        image_size
    );

    // Create sparse file
    let truncate_status = tokio::process::Command::new("truncate")
        .args(["-s", &image_size.to_string(), output_path.to_str().unwrap()])
        .status()
        .await
        .context("creating sparse file")?;

    if !truncate_status.success() {
        bail!(
            "truncate failed with exit code: {:?}",
            truncate_status.code()
        );
    }

    // Format as ext4
    let mkfs = tokio::process::Command::new("mkfs.ext4")
        .args(["-q", "-F", output_path.to_str().unwrap()])
        .output()
        .await
        .context("formatting as ext4")?;

    if !mkfs.status.success() {
        bail!(
            "mkfs.ext4 failed: {}",
            String::from_utf8_lossy(&mkfs.stderr)
        );
    }

    // Mount and copy contents
    let mount_dir = format!("/tmp/fcvm-disk-dir-{}", std::process::id());
    tokio::fs::create_dir_all(&mount_dir).await?;

    let mount = tokio::process::Command::new("mount")
        .args([output_path.to_str().unwrap(), &mount_dir])
        .output()
        .await
        .context("mounting image")?;

    if !mount.status.success() {
        tokio::fs::remove_dir(&mount_dir).await.ok();
        bail!("mount failed: {}", String::from_utf8_lossy(&mount.stderr));
    }

    // Copy directory contents (use rsync for reliability)
    let copy = tokio::process::Command::new("rsync")
        .args([
            "-a",
            &format!("{}/", source_dir.display()),
            &format!("{}/", mount_dir),
        ])
        .stderr(Stdio::piped())
        .output()
        .await
        .context("copying directory contents")?;

    // Always unmount
    let umount = tokio::process::Command::new("umount")
        .arg(&mount_dir)
        .output()
        .await;

    tokio::fs::remove_dir(&mount_dir).await.ok();

    if !copy.status.success() {
        bail!("rsync failed: {}", String::from_utf8_lossy(&copy.stderr));
    }

    if let Ok(u) = umount {
        if !u.status.success() {
            warn!("umount warning: {}", String::from_utf8_lossy(&u.stderr));
        }
    }

    info!("Created disk image: {}", output_path.display());
    Ok(())
}

/// Set up NFS exports for VM.
/// Creates /etc/exports.d/fcvm-{vm_id}.exports and refreshes exportfs.
async fn setup_nfs_exports(
    vm_id: &str,
    shares: &[crate::state::types::NfsShare],
    network_config: &crate::network::NetworkConfig,
) -> Result<()> {
    use std::io::Write;

    // Ensure NFS server is running
    let status = tokio::process::Command::new("systemctl")
        .args(["is-active", "nfs-server"])
        .output()
        .await?;

    if !status.status.success() {
        info!("Starting NFS server...");
        let start = tokio::process::Command::new("systemctl")
            .args(["start", "nfs-server"])
            .status()
            .await?;
        if !start.success() {
            anyhow::bail!("Failed to start NFS server. Run: sudo apt install nfs-kernel-server");
        }
    }

    // Create exports directory if needed
    tokio::fs::create_dir_all("/etc/exports.d").await.ok();

    // Guest IP for access control (use /30 subnet for the VM)
    let guest_ip = network_config
        .guest_ip
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("No guest IP configured for NFS"))?;

    // Build exports file content
    let mut exports = String::new();
    for share in shares {
        let opts = if share.read_only {
            "ro,sync,no_subtree_check,no_root_squash"
        } else {
            "rw,sync,no_subtree_check,no_root_squash"
        };
        exports.push_str(&format!("{} {}({})\n", share.host_path, guest_ip, opts));
    }

    // Write exports file
    let exports_path = format!("/etc/exports.d/fcvm-{}.exports", vm_id);
    let mut file = std::fs::File::create(&exports_path)?;
    file.write_all(exports.as_bytes())?;

    info!("Created NFS exports: {}", exports_path);

    // Refresh exports
    let refresh = tokio::process::Command::new("exportfs")
        .arg("-ra")
        .status()
        .await?;

    if !refresh.success() {
        warn!("exportfs -ra failed, NFS mounts may not work");
    }

    Ok(())
}

/// Clean up NFS exports for VM
async fn cleanup_nfs_exports(vm_id: &str) {
    let exports_path = format!("/etc/exports.d/fcvm-{}.exports", vm_id);
    if std::path::Path::new(&exports_path).exists() {
        if let Err(e) = tokio::fs::remove_file(&exports_path).await {
            warn!("Failed to remove NFS exports file: {}", e);
        } else {
            // Refresh exports to unregister
            let _ = tokio::process::Command::new("exportfs")
                .arg("-ra")
                .status()
                .await;
            debug!("Cleaned up NFS exports: {}", exports_path);
        }
    }
}

/// Main dispatcher for podman commands
pub async fn cmd_podman(args: PodmanArgs) -> Result<()> {
    match args.cmd {
        PodmanCommands::Run(run_args) => cmd_podman_run(run_args).await,
    }
}

/// Listen for fc-agent status messages on the status vsock port.
///
/// Firecracker forwards guest vsock connections to Unix sockets with format:
/// `{uds_path}_{port}` - so we listen on vsock.sock_4999 for port 4999.
///
/// Messages:
/// - "ready\n" - Container started, create ready file for health check
/// - "exit:{code}\n" - Container exited, write exit code to file
/// - "cache-ready:{digest}\n" - Image loaded, ready for caching (sends cache-ack back)
async fn run_status_listener(
    socket_path: &str,
    runtime_dir: &std::path::Path,
    vm_id: &str,
    cache_tx: Option<mpsc::Sender<CacheRequest>>,
) -> Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::UnixListener;

    // Remove stale socket if it exists
    let _ = std::fs::remove_file(socket_path);

    let listener = UnixListener::bind(socket_path)
        .with_context(|| format!("binding status listener to {}", socket_path))?;

    info!(socket = %socket_path, "Status listener started");

    let ready_file = runtime_dir.join("container-ready");
    let exit_file = runtime_dir.join("container-exit");

    // Accept connections in a loop (we get "cache-ready" then "ready" then "exit")
    loop {
        let accept_result = tokio::time::timeout(
            std::time::Duration::from_secs(3600), // 1 hour timeout
            listener.accept(),
        )
        .await;

        let (mut stream, _) = match accept_result {
            Ok(Ok(conn)) => conn,
            Ok(Err(e)) => {
                warn!(vm_id = %vm_id, error = %e, "Error accepting status connection");
                continue;
            }
            Err(_) => {
                // Timeout - VM probably shut down without sending exit
                break;
            }
        };

        // Read the message
        let mut buf = [0u8; 128];
        let n = match stream.read(&mut buf).await {
            Ok(n) if n > 0 => n,
            _ => continue,
        };

        let msg = String::from_utf8_lossy(&buf[..n]);
        let msg = msg.trim();

        if msg == "ready" {
            // Create ready file to signal container is running
            std::fs::write(&ready_file, "ready\n")
                .with_context(|| format!("writing ready file: {:?}", ready_file))?;
            info!(vm_id = %vm_id, "Container ready notification received");
        } else if let Some(debug_msg) = msg.strip_prefix("debug:") {
            // Debug message from fc-agent (useful when serial console is broken after restore)
            info!(vm_id = %vm_id, debug = %debug_msg, "fc-agent debug message");
        } else if let Some(code_str) = msg.strip_prefix("exit:") {
            // Write exit code to file
            std::fs::write(&exit_file, format!("{}\n", code_str))
                .with_context(|| format!("writing exit file: {:?}", exit_file))?;
            info!(vm_id = %vm_id, exit_code = %code_str, "Container exit notification received");
            // Exit loop after receiving exit code
            break;
        } else if let Some(digest) = msg.strip_prefix("cache-ready:") {
            // fc-agent has loaded the image and is ready for caching
            info!(vm_id = %vm_id, digest = %digest, "Cache-ready notification received");

            if let Some(ref tx) = cache_tx {
                // Create oneshot channel for ack
                let (ack_tx, ack_rx) = oneshot::channel();

                // Send cache request to main task
                let request = CacheRequest {
                    digest: digest.to_string(),
                    ack_tx,
                };

                if tx.send(request).await.is_ok() {
                    // Wait for main task to complete cache creation
                    // No timeout - host is responsible for completing
                    if ack_rx.await.is_ok() {
                        info!(vm_id = %vm_id, "Cache created, sending ack to fc-agent");
                    } else {
                        warn!(vm_id = %vm_id, "Cache creation failed or was cancelled");
                    }
                } else {
                    warn!(vm_id = %vm_id, "Failed to send cache request to main task");
                }
            }

            // Send ack back to fc-agent (even if cache creation failed)
            if let Err(e) = stream.write_all(b"cache-ack\n").await {
                warn!(vm_id = %vm_id, error = %e, "Failed to send cache-ack to fc-agent");
            }
        } else {
            warn!(vm_id = %vm_id, msg = %msg, "Unexpected status message");
        }
    }

    // Clean up socket
    let _ = std::fs::remove_file(socket_path);

    Ok(())
}

/// Bidirectional I/O listener for container stdin/stdout/stderr.
///
/// Listens on port 4997 for raw output from fc-agent.
/// Protocol (all lines are newline-terminated):
///   Guest → Host: "stdout:content" or "stderr:content"
///   Host → Guest: "stdin:content" (written to container stdin)
///
/// If `interactive` is true, forwards host stdin to container.
///
/// Returns collected output lines as Vec<(stream, line)>.
pub(crate) async fn run_output_listener(
    socket_path: &str,
    vm_id: &str,
    interactive: bool,
) -> Result<Vec<(String, String)>> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixListener;

    // Remove stale socket if it exists
    let _ = std::fs::remove_file(socket_path);

    let listener = UnixListener::bind(socket_path)
        .with_context(|| format!("binding output listener to {}", socket_path))?;

    info!(socket = %socket_path, "Output listener started");

    let mut output_lines: Vec<(String, String)> = Vec::new();

    // Accept connection from fc-agent
    let accept_result = tokio::time::timeout(
        std::time::Duration::from_secs(120), // Wait up to 2 min for connection
        listener.accept(),
    )
    .await;

    let (stream, _) = match accept_result {
        Ok(Ok(conn)) => conn,
        Ok(Err(e)) => {
            warn!(vm_id = %vm_id, error = %e, "Error accepting output connection");
            let _ = std::fs::remove_file(socket_path);
            return Ok(output_lines);
        }
        Err(_) => {
            // Timeout - container probably didn't produce output
            debug!(vm_id = %vm_id, "Output listener timeout, no connection");
            let _ = std::fs::remove_file(socket_path);
            return Ok(output_lines);
        }
    };

    debug!(vm_id = %vm_id, "Output connection established");

    let (reader, writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let writer = std::sync::Arc::new(tokio::sync::Mutex::new(writer));
    let mut line_buf = String::new();

    // Spawn stdin forwarder if interactive mode
    let stdin_task = if interactive {
        let writer = writer.clone();
        Some(tokio::spawn(async move {
            let stdin = tokio::io::stdin();
            let mut stdin = BufReader::new(stdin);
            let mut line = String::new();
            loop {
                line.clear();
                match stdin.read_line(&mut line).await {
                    Ok(0) => break, // EOF
                    Ok(_) => {
                        // Forward to container: "stdin:content\n"
                        let msg = format!("stdin:{}", line.trim_end());
                        let mut w = writer.lock().await;
                        if w.write_all(msg.as_bytes()).await.is_err() {
                            break;
                        }
                        if w.write_all(b"\n").await.is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        }))
    } else {
        None
    };

    // Read lines until connection closes
    loop {
        line_buf.clear();
        match tokio::time::timeout(
            std::time::Duration::from_secs(300), // 5 min read timeout
            reader.read_line(&mut line_buf),
        )
        .await
        {
            Ok(Ok(0)) => {
                // EOF - connection closed
                debug!(vm_id = %vm_id, "Output connection closed");
                break;
            }
            Ok(Ok(_)) => {
                // Parse raw line format: stream:content
                let line = line_buf.trim_end();
                if let Some((stream, content)) = line.split_once(':') {
                    // Print container output directly (stdout to stdout, stderr to stderr)
                    // No prefix - clean output for scripting
                    if stream == "stdout" {
                        println!("{}", content);
                    } else {
                        eprintln!("{}", content);
                    }
                    output_lines.push((stream.to_string(), content.to_string()));

                    // Send ack back (bidirectional)
                    let mut w = writer.lock().await;
                    let _ = w.write_all(b"ack\n").await;
                }
            }
            Ok(Err(e)) => {
                warn!(vm_id = %vm_id, error = %e, "Error reading output");
                break;
            }
            Err(_) => {
                // Read timeout
                debug!(vm_id = %vm_id, "Output read timeout");
                break;
            }
        }
    }

    // Abort stdin task if it's still running
    if let Some(task) = stdin_task {
        task.abort();
    }

    // Clean up
    let _ = std::fs::remove_file(socket_path);

    info!(vm_id = %vm_id, lines = output_lines.len(), "Output listener finished");
    Ok(output_lines)
}

async fn cmd_podman_run(args: RunArgs) -> Result<()> {
    info!("Starting fcvm podman run");

    // Validate VM name before any setup work
    validate_vm_name(&args.name).context("invalid VM name")?;

    // Disallow --setup when running as root
    // Root users should run `fcvm setup` explicitly
    if args.setup && nix::unistd::geteuid().is_root() {
        bail!("--setup is not allowed when running as root. Run 'fcvm setup' first.");
    }

    // Apply kernel profile runtime config (firecracker_args, boot_args, etc.)
    // This is done regardless of whether --kernel is also specified
    if let Some(ref profile_name) = args.kernel_profile {
        let profile = crate::setup::get_kernel_profile(profile_name)?.ok_or_else(|| {
            anyhow::anyhow!(
                "kernel profile '{}' not found for {} in config",
                profile_name,
                std::env::consts::ARCH
            )
        })?;

        info!(profile = %profile_name, "using kernel profile");

        // Apply runtime config from profile
        // Get firecracker path (custom from profile or system fallback)
        let fc_path = crate::setup::get_firecracker_for_profile(&profile, profile_name).await?;
        info!(firecracker_bin = %fc_path.display(), "from profile");
        std::env::set_var("FCVM_FIRECRACKER_BIN", fc_path.to_string_lossy().as_ref());
        if let Some(ref fc_args) = profile.firecracker_args {
            info!(firecracker_args = %fc_args, "from profile");
            std::env::set_var("FCVM_FIRECRACKER_ARGS", fc_args);
        }
        if let Some(ref boot_args) = profile.boot_args {
            info!(boot_args = %boot_args, "from profile");
            std::env::set_var("FCVM_BOOT_ARGS", boot_args);
        }
        if let Some(readers) = profile.fuse_readers {
            info!(fuse_readers = %readers, "from profile");
            std::env::set_var("FCVM_FUSE_READERS", readers.to_string());
        }
    }

    // Get kernel path
    // Priority: --kernel (explicit) > --kernel-profile (computed) > default
    let kernel_path = if let Some(custom_kernel) = &args.kernel {
        // Explicit kernel path - use directly
        let path = PathBuf::from(custom_kernel);
        if !path.exists() {
            bail!("Custom kernel not found: {}", path.display());
        }
        info!(kernel = %path.display(), "using custom kernel");
        path
    } else if let Some(ref profile_name) = args.kernel_profile {
        // Compute kernel path from profile
        let kernel = crate::setup::get_kernel_path(Some(profile_name))?;
        if !kernel.exists() {
            bail!(
                "Profile '{}' kernel not found at {}.\nRun: fcvm setup --kernel-profile {}",
                profile_name,
                kernel.display(),
                profile_name
            );
        }
        kernel
    } else {
        // Default kernel (downloads if --setup is set)
        crate::setup::ensure_kernel(None, args.setup, false)
            .await
            .context("setting up kernel")?
    };

    let base_rootfs = crate::setup::ensure_rootfs(args.setup)
        .await
        .context("setting up rootfs")?;
    let initrd_path = crate::setup::ensure_fc_agent_initrd(args.setup)
        .await
        .context("setting up fc-agent initrd")?;

    // Parse optional container command EARLY - it's part of cache key
    // Either from trailing args or --cmd flag
    let cmd_args = if !args.command_args.is_empty() {
        // Trailing args take precedence (e.g., "alpine:latest sh -c 'echo hello'")
        Some(args.command_args.clone())
    } else if let Some(cmd) = &args.cmd {
        // Fall back to --cmd flag with shell parsing
        Some(shell_words::split(cmd).with_context(|| format!("parsing --cmd argument: {}", cmd))?)
    } else {
        None
    };

    // Check for snapshot cache (unless --no-snapshot is set or FCVM_NO_SNAPSHOT env var)
    // Keep fc_config and snapshot_key available for later snapshot creation on miss
    let no_snapshot = args.no_snapshot || std::env::var("FCVM_NO_SNAPSHOT").is_ok();
    let (fc_config, snapshot_key): (
        Option<crate::firecracker::FirecrackerConfig>,
        Option<String>,
    ) = if !no_snapshot {
        // Get image identifier for cache key computation
        let image_identifier = get_image_identifier(&args.image).await?;
        let config = build_firecracker_config(
            &args,
            &image_identifier,
            &kernel_path,
            &base_rootfs,
            &initrd_path,
            cmd_args.clone(),
        );
        let key = config.snapshot_key();

        // Check if cached snapshot exists - prefer startup snapshot over pre-start snapshot
        let startup_key = startup_snapshot_key(&key);

        // Check for startup snapshot first (fully initialized application)
        if check_podman_snapshot(&startup_key).await.is_some() {
            info!(
                snapshot_key = %startup_key,
                image = %args.image,
                "Startup snapshot hit! Restoring from fully-initialized snapshot"
            );
            // Call snapshot run directly with startup snapshot
            // No need to create startup snapshot again since we're restoring from one
            let snapshot_args = crate::cli::SnapshotRunArgs {
                pid: None,
                snapshot: Some(startup_key.clone()),
                name: Some(args.name.clone()),
                publish: args.publish.clone(),
                network: args.network,
                exec: None,
                tty: args.tty,
                interactive: args.interactive,
                startup_snapshot_base_key: None, // Already using startup snapshot
                health_check_for_startup: None,
                health_check: args.health_check.clone(),
                cpu: Some(args.cpu),
                mem: Some(args.mem),
            };
            return super::snapshot::cmd_snapshot_run(snapshot_args).await;
        }

        // Check for pre-start snapshot (container loaded but not initialized)
        if check_podman_snapshot(&key).await.is_some() {
            info!(
                snapshot_key = %key,
                image = %args.image,
                "Pre-start snapshot hit! Restoring from cached snapshot"
            );
            // Call snapshot run with startup snapshot creation enabled
            // (if health_check_url is set)
            let snapshot_args = crate::cli::SnapshotRunArgs {
                pid: None,
                snapshot: Some(key.clone()),
                name: Some(args.name.clone()),
                publish: args.publish.clone(),
                network: args.network,
                exec: None,
                tty: args.tty,
                interactive: args.interactive,
                // Pass startup snapshot context if health check URL is set
                startup_snapshot_base_key: args.health_check.as_ref().map(|_| key.clone()),
                health_check_for_startup: args.health_check.clone(),
                health_check: args.health_check.clone(),
                cpu: Some(args.cpu),
                mem: Some(args.mem),
            };
            return super::snapshot::cmd_snapshot_run(snapshot_args).await;
        }

        info!(
            snapshot_key = %key,
            image = %args.image,
            "Snapshot miss, will create snapshot after image load"
        );
        (Some(config), Some(key))
    } else {
        if std::env::var("FCVM_NO_SNAPSHOT").is_ok() {
            info!("Snapshot disabled via FCVM_NO_SNAPSHOT environment variable");
        } else {
            info!("Snapshot disabled via --no-snapshot flag");
        }
        (None, None)
    };

    // Generate VM ID
    let vm_id = generate_vm_id();
    let vm_name = args.name.clone();

    // Parse port mappings
    let port_mappings: Vec<PortMapping> = args
        .publish
        .iter()
        .map(|s| PortMapping::parse(s))
        .collect::<Result<Vec<_>>>()
        .context("parsing port mappings")?;

    // Parse volume mappings (HOST:GUEST[:ro])
    let volume_mappings: Vec<VolumeMapping> = args
        .map
        .iter()
        .map(|s| VolumeMapping::parse(s))
        .collect::<Result<Vec<_>>>()
        .context("parsing volume mappings")?;

    // For localhost/ images, export as OCI archive for direct podman run
    // Uses content-addressable cache to avoid re-exporting the same image
    let image_disk_path = if args.image.starts_with("localhost/") {
        // Get image digest for content-addressable storage
        let inspect_output = tokio::process::Command::new("podman")
            .args(["image", "inspect", &args.image, "--format", "{{.Digest}}"])
            .output()
            .await
            .context("inspecting image digest")?;

        if !inspect_output.status.success() {
            let stderr = String::from_utf8_lossy(&inspect_output.stderr);
            bail!(
                "Failed to get digest for image '{}': {}",
                args.image,
                stderr
            );
        }

        let digest = String::from_utf8_lossy(&inspect_output.stdout)
            .trim()
            // Strip "sha256:" prefix for use in filenames (colons invalid in paths)
            .trim_start_matches("sha256:")
            .to_string();

        // Use content-addressable cache: /mnt/fcvm-btrfs/image-cache/{digest}/
        let image_cache_dir = paths::image_cache_dir();
        tokio::fs::create_dir_all(&image_cache_dir)
            .await
            .context("creating image-cache directory")?;

        let cache_dir = image_cache_dir.join(&digest);

        // Lock per-digest to prevent concurrent exports of the same image
        let lock_path = image_cache_dir.join(format!("{}.lock", &digest));
        let lock_file =
            std::fs::File::create(&lock_path).context("creating image cache lock file")?;
        lock_file
            .lock_exclusive()
            .context("acquiring image cache lock")?;

        // Check if already cached (inside lock to prevent race)
        // Use Docker archive format (preserves HEALTHCHECK, single tar file) for FUSE transfer
        let archive_path = cache_dir.with_extension("docker.tar");
        if !archive_path.exists() {
            info!(image = %args.image, digest = %digest, "Exporting localhost image as Docker archive");

            let output = tokio::process::Command::new("podman")
                .args([
                    "save",
                    "--format",
                    "docker-archive",
                    "-o",
                    archive_path.to_str().unwrap(),
                    &args.image,
                ])
                .output()
                .await
                .context("running podman save")?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                // Clean up partial export
                let _ = tokio::fs::remove_file(&archive_path).await;
                drop(lock_file); // Release lock before bailing
                bail!(
                    "Failed to export image '{}' with podman save: {}",
                    args.image,
                    stderr
                );
            }

            // Validate the archive contains manifest.json (required for docker-archive format)
            // This catches corrupted exports early, before they get cached and cause repeated failures
            if !validate_docker_archive(&archive_path)? {
                let _ = tokio::fs::remove_file(&archive_path).await;
                drop(lock_file);
                bail!(
                    "podman save produced invalid archive (missing manifest.json) for image '{}'",
                    args.image
                );
            }

            info!(path = %archive_path.display(), "Image exported as Docker archive");
        } else {
            // Validate cached archive in case it was corrupted
            if !validate_docker_archive(&archive_path)? {
                warn!(path = %archive_path.display(), "Cached archive is invalid, re-exporting");
                let _ = tokio::fs::remove_file(&archive_path).await;
                // Re-export
                let output = tokio::process::Command::new("podman")
                    .args([
                        "save",
                        "--format",
                        "docker-archive",
                        "-o",
                        archive_path.to_str().unwrap(),
                        &args.image,
                    ])
                    .output()
                    .await
                    .context("running podman save for re-export")?;

                if !output.status.success() {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    let _ = tokio::fs::remove_file(&archive_path).await;
                    drop(lock_file);
                    bail!(
                        "Failed to re-export image '{}' with podman save: {}",
                        args.image,
                        stderr
                    );
                }

                // Validate the re-exported archive
                if !validate_docker_archive(&archive_path)? {
                    let _ = tokio::fs::remove_file(&archive_path).await;
                    drop(lock_file);
                    bail!(
                        "podman save produced invalid archive (missing manifest.json) for image '{}' on re-export",
                        args.image
                    );
                }

                info!(path = %archive_path.display(), "Image re-exported as Docker archive");
            } else {
                info!(image = %args.image, digest = %digest, "Using cached Docker archive");
            }
        }

        // Lock released when lock_file is dropped
        drop(lock_file);

        // Attach the tar directly as a Firecracker block device (read-only).
        // fc-agent reads docker-archive:/dev/vdX — no FUSE, no ext4, no mount.
        Some(archive_path)
    } else {
        None
    };

    if !volume_mappings.is_empty() {
        info!(
            "Volumes to mount: {}",
            volume_mappings
                .iter()
                .map(|v| format!(
                    "{}:{}{}",
                    v.host_path.display(),
                    v.guest_path,
                    if v.read_only { ":ro" } else { "" }
                ))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    // Setup paths
    let data_dir = paths::vm_runtime_dir(&vm_id);
    tokio::fs::create_dir_all(&data_dir)
        .await
        .context("creating VM data directory")?;

    let socket_path = data_dir.join("firecracker.sock");

    // Create VM state
    // Note: env vars are NOT stored in state (they may contain secrets and state is world-readable)
    // Instead, env is passed directly to MMDS at VM start time
    let mut vm_state = VmState::new(vm_id.clone(), args.image.clone(), args.cpu, args.mem);
    vm_state.name = Some(vm_name.clone());
    vm_state.config.volumes = args.map.clone();
    vm_state.config.health_check_url = args.health_check.clone();

    // Initialize state manager
    let state_manager = StateManager::new(paths::state_dir());
    state_manager.init().await?;

    // Setup networking based on mode
    // Bridged mode requires root for iptables and network namespace setup
    if matches!(args.network, NetworkMode::Bridged) && !nix::unistd::geteuid().is_root() {
        bail!(
            "Bridged networking requires root. Either:\n  \
             - Run with sudo: sudo fcvm podman run ...\n  \
             - Use rootless mode: fcvm podman run --network rootless ..."
        );
    }
    // Rootless with sudo is pointless - bridged would be faster
    if matches!(args.network, NetworkMode::Rootless) && nix::unistd::geteuid().is_root() {
        warn!(
            "Running rootless mode as root is unnecessary. \
             Consider using --network bridged for better performance."
        );
    }

    let tap_device = format!("tap-{}", truncate_id(&vm_id, 8));
    let mut network: Box<dyn NetworkManager> = match args.network {
        NetworkMode::Bridged => Box::new(BridgedNetwork::new(
            vm_id.clone(),
            tap_device.clone(),
            port_mappings.clone(),
        )),
        NetworkMode::Rootless => {
            // For rootless mode, allocate loopback IP atomically with state persistence
            // This prevents race conditions when starting multiple VMs concurrently
            let loopback_ip = state_manager
                .allocate_loopback_ip(&mut vm_state)
                .await
                .context("allocating loopback IP")?;

            Box::new(
                SlirpNetwork::new(vm_id.clone(), tap_device.clone(), port_mappings.clone())
                    .with_loopback_ip(loopback_ip),
            )
        }
    };

    let network_config = network.setup().await.context("setting up network")?;

    // Don't auto-assign health check URL from network config.
    // HTTP health checks require an HTTP server - use container-ready file by default.
    // User can explicitly set --health-check if they want HTTP checks.
    if let Some(port) = network_config.health_check_port {
        vm_state.config.network.health_check_port = Some(port);
    }

    info!(tap = %network_config.tap_device, mac = %network_config.guest_mac, "network configured");

    // Generate vsock socket base path for volume servers
    // Firecracker binds to vsock.sock, VolumeServers listen on vsock.sock_{port}
    // Use custom vsock_dir if provided (for predictable socket paths)
    let vsock_socket_path = if let Some(ref vsock_dir) = args.vsock_dir {
        let vsock_dir = std::path::PathBuf::from(vsock_dir);
        tokio::fs::create_dir_all(&vsock_dir)
            .await
            .with_context(|| format!("creating vsock dir: {:?}", vsock_dir))?;
        vsock_dir.join("vsock.sock")
    } else {
        data_dir.join("vsock.sock")
    };

    // Build VolumeConfigs and spawn VolumeServers BEFORE the VM starts
    // Each VolumeServer listens on vsock.sock_{port} (e.g., vsock.sock_5000)
    // Firecracker binds to vsock.sock and routes guest connections to the per-port sockets
    let volume_configs: Vec<VolumeConfig> = volume_mappings
        .iter()
        .enumerate()
        .map(|(idx, vol)| VolumeConfig {
            host_path: vol.host_path.clone(),
            guest_path: vol.guest_path.clone().into(),
            read_only: vol.read_only,
            port: VSOCK_VOLUME_PORT_BASE + idx as u32,
        })
        .collect();

    let volume_server_handles = spawn_volume_servers(&volume_configs, &vsock_socket_path)
        .await
        .context("spawning VolumeServers")?;

    // Create snapshot channel for snapshot-ready notifications
    // Skip snapshot creation when:
    // - --no-snapshot flag or FCVM_NO_SNAPSHOT env var is set
    // - Volumes are specified (FUSE-over-vsock breaks during snapshot pause)
    let skip_snapshot_creation = no_snapshot || !args.map.is_empty();
    if !args.map.is_empty() && !no_snapshot {
        info!(
            "Skipping snapshot creation: volumes specified (FUSE doesn't survive snapshot pause)"
        );
    }
    let (cache_tx, mut cache_rx): (
        Option<mpsc::Sender<CacheRequest>>,
        Option<mpsc::Receiver<CacheRequest>>,
    ) = if !skip_snapshot_creation {
        let (tx, rx) = mpsc::channel(1);
        (Some(tx), Some(rx))
    } else {
        (None, None)
    };

    // Create startup snapshot channel for health-triggered snapshot creation
    // Only create startup snapshots if:
    // - Not skipping snapshots (no --no-snapshot, no volumes)
    // - Have a snapshot key
    // - Have a health_check URL configured (HTTP health check, not just container-ready)
    let (startup_tx, mut startup_rx): (
        Option<tokio::sync::oneshot::Sender<()>>,
        Option<tokio::sync::oneshot::Receiver<()>>,
    ) = if !skip_snapshot_creation && snapshot_key.is_some() && args.health_check.is_some() {
        let (tx, rx) = tokio::sync::oneshot::channel();
        (Some(tx), Some(rx))
    } else {
        (None, None)
    };

    // Start status channel listener for fc-agent notifications
    // - "ready" on port 4999 -> creates container-ready file for health check
    // - "exit:{code}" on port 4999 -> creates container-exit file with exit code
    // - "cache-ready:{digest}" on port 4999 -> trigger cache creation
    let status_socket_path = format!("{}_{}", vsock_socket_path.display(), VSOCK_STATUS_PORT);
    let status_handle = {
        let runtime_dir = data_dir.clone();
        let socket_path = status_socket_path.clone();
        let vm_id_clone = vm_id.clone();
        tokio::spawn(async move {
            if let Err(e) =
                run_status_listener(&socket_path, &runtime_dir, &vm_id_clone, cache_tx).await
            {
                tracing::warn!("Status listener error: {}", e);
            }
        })
    };

    // Start I/O listener for container stdin/stdout/stderr
    // TTY mode: use binary exec_proto on port 4996 (blocking, raw terminal)
    // Non-TTY mode: use line-based protocol on port 4997 (async)
    let tty_mode = args.tty;
    let interactive = args.interactive;
    let tty_socket_path = format!("{}_{}", vsock_socket_path.display(), VSOCK_TTY_PORT);
    let output_socket_path = format!("{}_{}", vsock_socket_path.display(), VSOCK_OUTPUT_PORT);

    // For TTY mode, we spawn a blocking thread that handles the TTY I/O
    // This must be set up BEFORE VM starts so we're ready to accept connection
    let tty_handle = if tty_mode {
        let socket_path = tty_socket_path.clone();
        Some(std::thread::spawn(move || {
            super::tty::run_tty_session(&socket_path, true, interactive)
        }))
    } else {
        None
    };

    // For non-TTY mode, use async output listener
    let _output_handle = if !tty_mode {
        let socket_path = output_socket_path.clone();
        let vm_id_clone = vm_id.clone();
        Some(tokio::spawn(async move {
            match run_output_listener(&socket_path, &vm_id_clone, interactive).await {
                Ok(lines) => lines,
                Err(e) => {
                    tracing::warn!("Output listener error: {}", e);
                    Vec::new()
                }
            }
        }))
    } else {
        None
    };

    // Run the main VM setup in a helper to ensure cleanup on error
    let setup_result = run_vm_setup(
        &args,
        &vm_id,
        &data_dir,
        &base_rootfs,
        &socket_path,
        &kernel_path,
        &initrd_path,
        &network_config,
        network.as_mut(),
        cmd_args,
        &state_manager,
        &mut vm_state,
        &volume_mappings,
        &vsock_socket_path,
        image_disk_path.as_deref(),
        fc_config,
    )
    .await;

    // If setup failed, cleanup all resources before propagating error
    if let Err(e) = setup_result {
        warn!("VM setup failed, cleaning up resources");

        // Abort VolumeServer tasks
        for handle in volume_server_handles {
            handle.abort();
        }

        // Abort status listener
        status_handle.abort();

        // Cleanup network
        if let Err(cleanup_err) = network.cleanup().await {
            warn!(
                "failed to cleanup network after setup error: {}",
                cleanup_err
            );
        }
        return Err(e);
    }

    let (mut vm_manager, mut holder_child) = setup_result.unwrap();

    info!(vm_id = %vm_id, "VM started successfully");

    // Create cancellation token for graceful health monitor shutdown
    let health_cancel_token = tokio_util::sync::CancellationToken::new();

    // Spawn health monitor task with startup snapshot trigger support
    // Pass startup_tx to signal when health first becomes Healthy
    let health_monitor_handle = crate::health::spawn_health_monitor_full(
        vm_id.clone(),
        vm_state.pid,
        paths::state_dir(),
        Some(health_cancel_token.clone()),
        startup_tx,
    );

    // Note: For rootless mode, slirp4netns wraps Firecracker and configures TAP automatically
    // For bridged mode, TAP is configured via NAT routing during network setup

    // Setup signal handlers
    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;

    // Wait for signal, VM exit, or cache requests
    // For TTY mode, we get exit code from the TTY listener thread
    // For non-TTY mode, we read it from the file written by status listener
    let container_exit_code: Option<i32>;
    let disk_path = data_dir.join("disks/rootfs.raw");

    loop {
        tokio::select! {
            _ = sigterm.recv() => {
                info!("received SIGTERM, shutting down VM");
                container_exit_code = None;
                break;
            }
            _ = sigint.recv() => {
                info!("received SIGINT, shutting down VM");
                container_exit_code = None;
                break;
            }
            status = vm_manager.wait() => {
                info!(status = ?status, "VM exited");
                if let Some(handle) = tty_handle {
                    container_exit_code = handle.join().ok().and_then(|r| r.ok());
                    info!(container_exit_code = ?container_exit_code, "TTY container exit code");
                } else {
                    let exit_file = data_dir.join("container-exit");
                    container_exit_code = std::fs::read_to_string(&exit_file)
                        .ok()
                        .and_then(|s| s.trim().parse::<i32>().ok());
                    info!(container_exit_code = ?container_exit_code, "container exit code");
                }
                break;
            }
            // Handle cache creation requests from fc-agent
            Some(cache_request) = async {
                match cache_rx.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                if let Some(ref key) = snapshot_key {
                    info!(snapshot_key = %key, digest = %cache_request.digest, "Creating pre-start snapshot");

                    let params = SnapshotCreationParams::from_run_args(&args);
                    match create_snapshot_interruptible(
                        &vm_manager, key, &vm_id, &params, &disk_path,
                        &network_config, &volume_configs,
                        None, // Pre-start is the first snapshot, no parent
                        &mut sigterm, &mut sigint,
                    ).await {
                        SnapshotOutcome::Interrupted => {
                            container_exit_code = None;
                            break;
                        }
                        SnapshotOutcome::Created => {
                            info!(snapshot_key = %key, "Pre-start snapshot created successfully");
                        }
                        SnapshotOutcome::Failed(e) => {
                            warn!(snapshot_key = %key, error = %e, "Failed to create pre-start snapshot");
                        }
                    }
                    // Send ack back regardless of success (fc-agent should continue)
                    let _ = cache_request.ack_tx.send(());
                } else {
                    // Should not happen if channel exists, but send ack anyway
                    let _ = cache_request.ack_tx.send(());
                }
                // Continue waiting for VM exit or signals
            }
            // Handle startup snapshot creation when health becomes healthy
            Ok(()) = async {
                match startup_rx.as_mut() {
                    Some(rx) => rx.await,
                    None => std::future::pending().await,
                }
            } => {
                // Oneshot channel - prevent further attempts
                startup_rx = None;

                if let Some(ref key) = snapshot_key {
                    let startup_key = startup_snapshot_key(key);

                    // Skip if startup snapshot already exists
                    if check_podman_snapshot(&startup_key).await.is_some() {
                        info!(snapshot_key = %startup_key, "Startup snapshot already exists, skipping");
                    } else {
                        info!(snapshot_key = %startup_key, "Creating startup snapshot (VM healthy)");

                        let params = SnapshotCreationParams::from_run_args(&args);
                        match create_snapshot_interruptible(
                            &vm_manager, &startup_key, &vm_id, &params, &disk_path,
                            &network_config, &volume_configs,
                            Some(key.as_str()), // Parent is pre-start snapshot
                            &mut sigterm, &mut sigint,
                        ).await {
                            SnapshotOutcome::Interrupted => {
                                container_exit_code = None;
                                break;
                            }
                            SnapshotOutcome::Created => {
                                info!(snapshot_key = %startup_key, "Startup snapshot created successfully");
                            }
                            SnapshotOutcome::Failed(e) => {
                                warn!(snapshot_key = %startup_key, error = %e, "Failed to create startup snapshot");
                            }
                        }
                    }
                }
                // Continue waiting for VM exit or signals
            }
        }
    }

    // Cancel status listener (podman-specific)
    status_handle.abort();

    // Cleanup NFS exports
    cleanup_nfs_exports(&vm_id).await;

    // Cleanup common resources
    super::common::cleanup_vm(
        &vm_id,
        &mut vm_manager,
        &mut holder_child,
        volume_server_handles,
        network.as_mut(),
        &state_manager,
        &data_dir,
        Some(health_cancel_token),
        Some(health_monitor_handle),
    )
    .await;

    // Return error if container exited with non-zero exit code
    if let Some(code) = container_exit_code {
        if code != 0 {
            bail!("container exited with code {}", code);
        }
    }

    Ok(())
}

/// Helper function that runs VM setup and returns VmManager on success.
/// This allows the caller to cleanup network resources on error.
/// For rootless mode, also returns the holder process that keeps the namespace alive.
#[allow(clippy::too_many_arguments)]
async fn run_vm_setup(
    args: &RunArgs,
    vm_id: &str,
    data_dir: &std::path::Path,
    base_rootfs: &std::path::Path,
    socket_path: &std::path::Path,
    kernel_path: &std::path::Path,
    initrd_path: &std::path::Path,
    network_config: &crate::network::NetworkConfig,
    network: &mut dyn NetworkManager,
    cmd_args: Option<Vec<String>>,
    state_manager: &StateManager,
    vm_state: &mut VmState,
    volume_mappings: &[VolumeMapping],
    vsock_socket_path: &std::path::Path,
    image_disk_path: Option<&std::path::Path>,
    fc_config: Option<crate::firecracker::FirecrackerConfig>,
) -> Result<(VmManager, Option<tokio::process::Child>)> {
    // Setup storage - just need CoW copy (fc-agent is injected via initrd at boot)
    let vm_dir = data_dir.join("disks");
    let disk_manager =
        DiskManager::new(vm_id.to_string(), base_rootfs.to_path_buf(), vm_dir.clone());

    let rootfs_path = disk_manager
        .create_cow_disk()
        .await
        .context("creating CoW disk")?;

    // Estimate space needed for container image extraction inside VM.
    // The archive is loaded via podman load which extracts layers onto the rootfs.
    let image_overhead = if let Some(disk_path) = image_disk_path {
        match tokio::fs::metadata(disk_path).await {
            Ok(meta) => meta.len(),
            Err(_) => 0,
        }
    } else {
        0
    };

    // Ensure minimum free space (from --rootfs-size) plus room for the container image
    crate::storage::disk::ensure_free_space(&rootfs_path, &args.rootfs_size, image_overhead)
        .await
        .context("ensuring rootfs free space")?;

    info!(rootfs = %rootfs_path.display(), "disk prepared (fc-agent baked into Layer 2)");

    let vm_name = args.name.clone();
    info!(vm_name = %vm_name, vm_id = %vm_id, "creating VM manager");
    let mut vm_manager = VmManager::new(vm_id.to_string(), socket_path.to_path_buf(), None);

    // Set VM name for logging
    vm_manager.set_vm_name(vm_name);

    // Configure namespace isolation based on network type
    let holder_child: Option<tokio::process::Child>;

    if let Some(bridged_net) = network.as_any().downcast_ref::<BridgedNetwork>() {
        // Bridged mode: use pre-created network namespace
        holder_child = None;
        if let Some(ns_id) = bridged_net.namespace_id() {
            info!(namespace = %ns_id, "configuring VM to run in network namespace");
            vm_manager.set_namespace(ns_id.to_string());
        }
    } else if let Some(slirp_net) = network.as_any().downcast_ref::<SlirpNetwork>() {
        // Rootless mode: spawn holder process and set up namespace via nsenter
        // This is fully rootless - no sudo required!

        // Step 1: Spawn holder process (keeps namespace alive)
        // Retry for up to 5 seconds if holder dies (transient failures under load)
        let holder_cmd = slirp_net.build_holder_command();
        info!(cmd = ?holder_cmd, "spawning namespace holder for rootless networking");

        let retry_deadline = std::time::Instant::now() + super::common::HOLDER_RETRY_TIMEOUT;
        let mut attempt = 0;

        let (mut child, holder_pid, mut holder_stderr) = loop {
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
                super::common::NAMESPACE_READY_TIMEOUT,
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
                    tokio::time::sleep(super::common::HOLDER_RETRY_INTERVAL).await;
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
                        tokio::time::sleep(super::common::HOLDER_RETRY_INTERVAL).await;
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
                    tokio::time::sleep(super::common::HOLDER_RETRY_INTERVAL).await;
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
        let nsenter_prefix = slirp_net.build_nsenter_prefix(holder_pid);

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
                    .with_context(|| {
                        format!("failed to spawn holder on retry: {:?}", holder_cmd)
                    })?;

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
                // Note: holder_pid is shadowed in the outer scope, but we continue with retry_holder_pid
                info!(
                    holder_pid = retry_holder_pid,
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
            let _ = child.kill().await;
            bail!(
                "TAP device '{}' not found after network setup - setup may have failed silently",
                tap_device
            );
        }
        debug!(tap_device = %tap_device, "TAP device verified");

        // Step 3: Set holder_pid so VmManager uses nsenter
        vm_manager.set_holder_pid(holder_pid);

        // Store holder_pid in state for health checks and cleanup
        vm_state.holder_pid = Some(holder_pid);

        holder_child = Some(child);
    } else {
        holder_child = None;
    }

    let firecracker_bin = super::common::find_firecracker()?;

    vm_manager
        .start(&firecracker_bin, None)
        .await
        .context("starting Firecracker")?;

    let vm_pid = vm_manager.pid()?;
    let client = vm_manager.client()?;

    // Configure VM via API
    info!("configuring VM via Firecracker API");

    // Build FirecrackerConfig for launch (single source of truth for VM config)
    // Use fc_config from cache check if available, otherwise build fresh.
    // IMPORTANT: fc_config uses content-addressed base_rootfs path for cache key,
    // but launch must use per-instance CoW copy path (rootfs_path).
    let launch_config = fc_config
        .map(|config| config.with_rootfs_path(rootfs_path.to_path_buf()))
        .unwrap_or_else(|| {
            use crate::firecracker::FcNetworkMode;
            let network_mode = match args.network {
                crate::cli::args::NetworkMode::Bridged => FcNetworkMode::Bridged,
                crate::cli::args::NetworkMode::Rootless => FcNetworkMode::Rootless,
            };
            // Collect extra disk specifications
            let mut extra_disks: Vec<String> = Vec::new();
            extra_disks.extend(args.disk.iter().cloned());
            extra_disks.extend(args.disk_dir.iter().cloned());
            extra_disks.extend(args.nfs.iter().cloned());
            // Collect env vars and volume mounts for cache key
            let env_vars: Vec<String> = args.env.to_vec();
            let volume_mounts: Vec<String> = args.map.to_vec();

            crate::firecracker::FirecrackerConfig::new(
                kernel_path.to_path_buf(),
                initrd_path.to_path_buf(),
                rootfs_path.to_path_buf(),
                args.image.clone(),
                cmd_args.clone(),
                args.cpu,
                args.mem,
                network_mode,
                crate::paths::data_dir(),
                extra_disks,
                env_vars,
                volume_mounts,
                args.privileged,
                args.tty,
                args.interactive,
                args.rootfs_size.clone(),
            )
        });

    // Build runtime boot args (per-instance values NOT in cache key)
    // These are added to the static boot_args from FirecrackerConfig
    let mut runtime_boot_args = String::new();

    // Network configuration via kernel cmdline
    // Format: ip=<client-ip>:<server-ip>:<gw-ip>:<netmask>:<hostname>:<device>:<autoconf>:<dns0>
    if let (Some(guest_ip), Some(host_ip)) = (&network_config.guest_ip, &network_config.host_ip) {
        let guest_ip_clean = guest_ip.split('/').next().unwrap_or(guest_ip);
        let host_ip_clean = host_ip.split('/').next().unwrap_or(host_ip);
        let dns_suffix = network_config
            .dns_server
            .as_ref()
            .map(|dns| format!(":{}", dns))
            .unwrap_or_default();
        // Use /24 netmask for slirp4netns (10.0.2.0/24) or bridged (172.30.x.0/24)
        runtime_boot_args.push_str(&format!(
            "ip={}::{}:255.255.255.0::eth0:off{}",
            guest_ip_clean, host_ip_clean, dns_suffix
        ));
    }

    // IPv6 configuration via kernel cmdline (for rootless networking)
    // Format: ipv6=<client>|<gateway> - parsed by fc-agent to configure eth0
    // Uses | as delimiter since : is part of IPv6 addresses
    if let (Some(guest_ipv6), Some(host_ipv6)) =
        (&network_config.guest_ipv6, &network_config.host_ipv6)
    {
        if !runtime_boot_args.is_empty() {
            runtime_boot_args.push(' ');
        }
        runtime_boot_args.push_str(&format!("ipv6={}|{}", guest_ipv6, host_ipv6));
    }

    // Pass host DNS servers to guest for direct resolution (bypasses slirp's DNS proxy)
    // This is needed on IPv6-only hosts where slirp's 10.0.2.3 can't forward to IPv6 nameservers
    if let Ok(dns_servers) = crate::network::get_host_dns_servers() {
        if !runtime_boot_args.is_empty() {
            runtime_boot_args.push(' ');
        }
        // Use | delimiter since : is part of IPv6 addresses
        runtime_boot_args.push_str(&format!("fcvm_dns={}", dns_servers.join("|")));

        // Pass search domains for short hostname resolution (only when DNS servers are available)
        if let Ok(content) = std::fs::read_to_string("/run/systemd/resolve/resolv.conf")
            .or_else(|_| std::fs::read_to_string("/etc/resolv.conf"))
        {
            let search: Vec<&str> = content
                .lines()
                .filter_map(|l| l.trim().strip_prefix("search "))
                .next()
                .map(|s| s.split_whitespace().collect())
                .unwrap_or_default();
            if !search.is_empty() {
                if !runtime_boot_args.is_empty() {
                    runtime_boot_args.push(' ');
                }
                runtime_boot_args.push_str(&format!("fcvm_dns_search={}", search.join("|")));
            }
        }
    }

    // Enable fc-agent strace debugging if requested
    if args.strace_agent {
        if !runtime_boot_args.is_empty() {
            runtime_boot_args.push(' ');
        }
        runtime_boot_args.push_str("fc_agent_strace=1");
        info!("fc-agent strace debugging enabled - output will be in /tmp/fc-agent.strace");
    }

    // Additional boot args from environment (caller controls)
    if let Ok(extra) = std::env::var("FCVM_BOOT_ARGS") {
        if !runtime_boot_args.is_empty() {
            runtime_boot_args.push(' ');
        }
        runtime_boot_args.push_str(&extra);
    }

    // Pass FUSE reader count to fc-agent via kernel command line.
    if let Ok(readers) = std::env::var("FCVM_FUSE_READERS") {
        if !runtime_boot_args.is_empty() {
            runtime_boot_args.push(' ');
        }
        runtime_boot_args.push_str(&format!("fuse_readers={}", readers));
    }

    // Pass FUSE trace rate to fc-agent via kernel command line.
    if let Ok(rate) = std::env::var("FCVM_FUSE_TRACE_RATE") {
        if !runtime_boot_args.is_empty() {
            runtime_boot_args.push(' ');
        }
        runtime_boot_args.push_str(&format!("fuse_trace_rate={}", rate));
    }

    // Pass FUSE max_write to fc-agent via kernel command line.
    if let Ok(max_write) = std::env::var("FCVM_FUSE_MAX_WRITE") {
        if !runtime_boot_args.is_empty() {
            runtime_boot_args.push(' ');
        }
        runtime_boot_args.push_str(&format!("fuse_max_write={}", max_write));
    }

    // Pass FUSE writeback cache disable flag to fc-agent via kernel command line.
    if std::env::var("FCVM_NO_WRITEBACK_CACHE").is_ok() {
        if !runtime_boot_args.is_empty() {
            runtime_boot_args.push(' ');
        }
        runtime_boot_args.push_str("no_writeback_cache=1");
    }

    // Apply FirecrackerConfig to client (boot_source, machine_config, rootfs drive)
    // This ensures the same config used for cache key is used for launch
    launch_config.apply(client, &runtime_boot_args).await?;

    // Extra disks (appear as /dev/vdb, /dev/vdc, etc.)
    // Parse format: HOST_PATH:GUEST_MOUNT[:ro]
    let mut extra_disks = Vec::new();
    for (i, disk_spec) in args.disk.iter().enumerate() {
        // Check for :ro suffix
        let (spec_without_ro, read_only) = if disk_spec.ends_with(":ro") {
            (&disk_spec[..disk_spec.len() - 3], true)
        } else {
            (disk_spec.as_str(), false)
        };

        // Split HOST_PATH:GUEST_MOUNT
        let parts: Vec<&str> = spec_without_ro.splitn(2, ':').collect();
        if parts.len() != 2 {
            anyhow::bail!(
                "Invalid disk spec '{}'. Expected format: HOST_PATH:GUEST_MOUNT[:ro]",
                disk_spec
            );
        }
        let path_str = parts[0];
        let mount_path = parts[1].to_string();

        // Validate mount path is absolute
        if !mount_path.starts_with('/') {
            anyhow::bail!(
                "Disk mount path must be absolute: {} (got '{}')",
                disk_spec,
                mount_path
            );
        }

        let drive_id = format!("disk{}", i);
        let disk_path = std::path::Path::new(path_str);
        if !disk_path.exists() {
            anyhow::bail!("Disk not found: {}", disk_path.display());
        }
        let abs_path = disk_path.canonicalize().context(format!(
            "Failed to resolve disk path: {}",
            disk_path.display()
        ))?;

        extra_disks.push(crate::state::types::ExtraDisk {
            path: abs_path.display().to_string(),
            mount_path: mount_path.clone(),
            read_only,
        });

        info!(
            "Adding extra disk: {} -> /dev/vd{} -> {} ({})",
            abs_path.display(),
            (b'b' + i as u8) as char,
            mount_path,
            if read_only { "ro" } else { "rw" }
        );
        client
            .add_drive(
                &drive_id,
                crate::firecracker::api::Drive {
                    drive_id: drive_id.clone(),
                    path_on_host: abs_path.display().to_string(),
                    is_root_device: false,
                    is_read_only: read_only,
                    partuuid: None,
                    rate_limiter: None,
                },
            )
            .await?;
    }

    // Process --disk-dir: create disk images from directories
    // Images are stored in VM's data directory (cleaned up on exit)
    let disk_offset = args.disk.len();
    for (i, dir_spec) in args.disk_dir.iter().enumerate() {
        // Check for :ro suffix
        let (spec_without_ro, read_only) = if dir_spec.ends_with(":ro") {
            (&dir_spec[..dir_spec.len() - 3], true)
        } else {
            (dir_spec.as_str(), false)
        };

        // Split HOST_DIR:GUEST_MOUNT
        let parts: Vec<&str> = spec_without_ro.splitn(2, ':').collect();
        if parts.len() != 2 {
            anyhow::bail!(
                "Invalid disk-dir spec '{}'. Expected format: HOST_DIR:GUEST_MOUNT[:ro]",
                dir_spec
            );
        }
        let source_dir = std::path::Path::new(parts[0]);
        let mount_path = parts[1].to_string();

        // Validate source directory exists
        if !source_dir.is_dir() {
            anyhow::bail!(
                "Source directory does not exist or is not a directory: {}",
                source_dir.display()
            );
        }

        // Validate mount path is absolute
        if !mount_path.starts_with('/') {
            anyhow::bail!(
                "Disk mount path must be absolute: {} (got '{}')",
                dir_spec,
                mount_path
            );
        }

        // Create disk image in VM's data directory
        let disk_idx = disk_offset + i;
        let image_path = data_dir
            .join("disks")
            .join(format!("disk-dir-{}.raw", disk_idx));
        create_disk_from_dir(source_dir, &image_path).await?;

        let drive_id = format!("disk{}", disk_idx);

        extra_disks.push(crate::state::types::ExtraDisk {
            path: image_path.display().to_string(),
            mount_path: mount_path.clone(),
            read_only,
        });

        info!(
            "Adding disk from dir: {} -> {} -> /dev/vd{} -> {} ({})",
            source_dir.display(),
            image_path.display(),
            (b'b' + disk_idx as u8) as char,
            mount_path,
            if read_only { "ro" } else { "rw" }
        );
        client
            .add_drive(
                &drive_id,
                crate::firecracker::api::Drive {
                    drive_id: drive_id.clone(),
                    path_on_host: image_path.display().to_string(),
                    is_root_device: false,
                    is_read_only: read_only,
                    partuuid: None,
                    rate_limiter: None,
                },
            )
            .await?;
    }
    // Attach image archive as a raw read-only block device.
    // fc-agent reads docker-archive:/dev/vdX directly — no FUSE, no mount.
    let image_device = if let Some(disk_path) = image_disk_path {
        let disk_idx = args.disk.len() + args.disk_dir.len();
        let drive_id = format!("disk{}", disk_idx);
        let device = format!("/dev/vd{}", (b'b' + disk_idx as u8) as char);

        info!(
            "Attaching image archive as block device: {} -> {}",
            disk_path.display(),
            device,
        );
        client
            .add_drive(
                &drive_id,
                crate::firecracker::api::Drive {
                    drive_id: drive_id.clone(),
                    path_on_host: disk_path.display().to_string(),
                    is_root_device: false,
                    is_read_only: true,
                    partuuid: None,
                    rate_limiter: None,
                },
            )
            .await?;
        Some(device)
    } else {
        None
    };

    vm_state.config.extra_disks = extra_disks;

    // Process --nfs: export directories via NFS for guest to mount
    let mut nfs_shares = Vec::new();
    for nfs_spec in args.nfs.iter() {
        // Check for :ro suffix
        let (spec_without_ro, read_only) = if nfs_spec.ends_with(":ro") {
            (&nfs_spec[..nfs_spec.len() - 3], true)
        } else {
            (nfs_spec.as_str(), false)
        };

        // Split HOST_DIR:GUEST_MOUNT
        let parts: Vec<&str> = spec_without_ro.splitn(2, ':').collect();
        if parts.len() != 2 {
            anyhow::bail!(
                "Invalid NFS spec '{}'. Expected format: HOST_DIR:GUEST_MOUNT[:ro]",
                nfs_spec
            );
        }
        let host_dir = std::path::Path::new(parts[0]);
        let mount_path = parts[1].to_string();

        // Validate host directory exists
        if !host_dir.is_dir() {
            anyhow::bail!(
                "NFS source directory does not exist or is not a directory: {}",
                host_dir.display()
            );
        }

        // Validate mount path is absolute
        if !mount_path.starts_with('/') {
            anyhow::bail!(
                "NFS mount path must be absolute: {} (got '{}')",
                nfs_spec,
                mount_path
            );
        }

        let abs_path = host_dir.canonicalize().context(format!(
            "Failed to resolve NFS path: {}",
            host_dir.display()
        ))?;

        nfs_shares.push(crate::state::types::NfsShare {
            host_path: abs_path.display().to_string(),
            mount_path: mount_path.clone(),
            read_only,
        });

        info!(
            "NFS share: {} -> {} ({})",
            abs_path.display(),
            mount_path,
            if read_only { "ro" } else { "rw" }
        );
    }

    // Set up NFS exports if we have any shares
    if !nfs_shares.is_empty() {
        setup_nfs_exports(vm_id, &nfs_shares, network_config).await?;
    }
    vm_state.config.nfs_shares = nfs_shares;

    // For rootless mode with slirp4netns: post_start starts slirp4netns in the namespace
    // For bridged mode: post_start is a no-op (TAP already created by BridgedNetwork)
    // Use holder_pid for rootless (slirp4netns attaches to holder's namespace)
    let post_start_pid = vm_state.holder_pid.unwrap_or(vm_pid);
    network
        .post_start(post_start_pid)
        .await
        .context("post-start network setup")?;

    // Network interface - required for MMDS V2 in all modes
    // For rootless: slirp4netns already created TAP, Firecracker attaches to it
    // For bridged: TAP is created by BridgedNetwork and added to bridge
    client
        .add_network_interface(
            "eth0",
            crate::firecracker::api::NetworkInterface {
                iface_id: "eth0".to_string(),
                host_dev_name: network_config.tap_device.clone(),
                guest_mac: Some(network_config.guest_mac.clone()),
                rx_rate_limiter: None,
                tx_rate_limiter: None,
            },
        )
        .await?;

    // MMDS configuration - V2 works in rootless mode as long as interface exists
    client
        .set_mmds_config(crate::firecracker::api::MmdsConfig {
            version: "V2".to_string(),
            network_interfaces: Some(vec!["eth0".to_string()]),
            ipv4_address: Some("169.254.169.254".to_string()),
        })
        .await?;

    // Always configure vsock device for status channel (and optionally volumes)
    info!(
        "Configuring vsock device at {:?} (status + {} volume(s))",
        vsock_socket_path,
        volume_mappings.len()
    );
    client
        .set_vsock(crate::firecracker::api::Vsock {
            guest_cid: 3, // Guest CID (host is always 2)
            uds_path: vsock_socket_path.display().to_string(),
        })
        .await?;

    // Build volume mount info for MMDS
    // Format: { guest_path, vsock_port, read_only }
    let volume_mounts: Vec<serde_json::Value> = volume_mappings
        .iter()
        .enumerate()
        .map(|(idx, v)| {
            serde_json::json!({
                "guest_path": v.guest_path,
                "vsock_port": VSOCK_VOLUME_PORT_BASE + idx as u32,
                "read_only": v.read_only,
            })
        })
        .collect();

    // Build extra disk info for MMDS
    // Format: { device, mount_path, read_only }
    // Disks are added as /dev/vdb, /dev/vdc, etc.
    let extra_disk_mounts: Vec<serde_json::Value> = vm_state
        .config
        .extra_disks
        .iter()
        .enumerate()
        .map(|(idx, disk)| {
            serde_json::json!({
                "device": format!("/dev/vd{}", (b'b' + idx as u8) as char),
                "mount_path": &disk.mount_path,
                "read_only": disk.read_only,
            })
        })
        .collect();

    // NFS mounts for guest
    // Format: { host_ip, host_path, mount_path, read_only }
    let nfs_mounts: Vec<serde_json::Value> = vm_state
        .config
        .nfs_shares
        .iter()
        .map(|share| {
            serde_json::json!({
                "host_ip": network_config.host_ip.as_ref().unwrap_or(&"".to_string()),
                "host_path": &share.host_path,
                "mount_path": &share.mount_path,
                "read_only": share.read_only,
            })
        })
        .collect();

    // MMDS data (container plan) - nested under "latest" for V2 compatibility
    // Include host timestamp so guest can set clock immediately (avoiding slow NTP sync)
    // Format without subsecond precision for Alpine `date` compatibility
    let mmds_data = serde_json::json!({
        "latest": {
            "container-plan": {
                "image": args.image,
                "env": args.env.iter().map(|e| {
                    let parts: Vec<&str> = e.splitn(2, '=').collect();
                    (parts[0], parts.get(1).copied().unwrap_or(""))
                }).collect::<std::collections::HashMap<_, _>>(),
                "cmd": cmd_args,
                "volumes": volume_mounts,
                "extra_disks": extra_disk_mounts,
                "nfs_mounts": nfs_mounts,
                "image_archive": image_device.clone(),
                "privileged": args.privileged,
                "interactive": args.interactive,
                "tty": args.tty,
                // Use network-provided proxy, or fall back to environment variables.
                // Resolve hostname to IPv4 since slirp VMs can only reach IPv4 addresses.
                "http_proxy": network_config.http_proxy.clone()
                    .or_else(|| std::env::var("http_proxy").ok())
                    .or_else(|| std::env::var("HTTP_PROXY").ok())
                    .and_then(|url| resolve_proxy_url(&url)),
                "https_proxy": network_config.http_proxy.clone()
                    .or_else(|| std::env::var("https_proxy").ok())
                    .or_else(|| std::env::var("HTTPS_PROXY").ok())
                    .or_else(|| std::env::var("http_proxy").ok())
                    .or_else(|| std::env::var("HTTP_PROXY").ok())
                    .and_then(|url| resolve_proxy_url(&url)),
                "no_proxy": std::env::var("no_proxy")
                    .or_else(|_| std::env::var("NO_PROXY"))
                    .ok(),
            },
            "host-time": chrono::Utc::now().timestamp().to_string(),
        }
    });

    client.put_mmds(mmds_data).await?;

    // Configure entropy device (virtio-rng) for better random number generation
    client
        .set_entropy_device(crate::firecracker::api::EntropyDevice { rate_limiter: None })
        .await?;

    // Balloon (if specified)
    if let Some(balloon_mib) = args.balloon {
        client
            .set_balloon(crate::firecracker::api::Balloon {
                amount_mib: balloon_mib,
                deflate_on_oom: true,
                stats_polling_interval_s: Some(1),
            })
            .await?;
    }

    // Start VM
    client
        .put_action(crate::firecracker::api::InstanceAction::InstanceStart)
        .await?;

    // Save VM state with complete network configuration
    super::common::save_vm_state_with_network(state_manager, vm_state, network_config).await?;

    Ok((vm_manager, holder_child))
}
