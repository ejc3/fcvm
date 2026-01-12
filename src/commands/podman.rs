use anyhow::{bail, Context, Result};
use fs2::FileExt;
use std::path::{Path, PathBuf};
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, info, warn};

use crate::cli::{NetworkMode, PodmanArgs, PodmanCommands, RunArgs};
use crate::firecracker::VmManager;
use crate::network::{BridgedNetwork, NetworkConfig, NetworkManager, PortMapping, SlirpNetwork};
use crate::paths;
use crate::state::{generate_vm_id, truncate_id, validate_vm_name, StateManager, VmState};
use crate::storage::DiskManager;
use crate::volume::{spawn_volume_servers, VolumeConfig};

/// Request to create a podman cache snapshot.
/// Sent from status listener to main task when fc-agent signals cache-ready.
struct CacheRequest {
    /// Image digest from fc-agent
    digest: String,
    /// Oneshot channel to signal completion back to status listener
    ack_tx: oneshot::Sender<()>,
}

/// Metadata stored in podman cache config.json
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct PodmanCacheConfig {
    /// Cache key (12-char hex)
    cache_key: String,
    /// Image name
    image: String,
    /// Image digest
    image_digest: String,
    /// vCPU count
    vcpu: u8,
    /// Memory MiB
    memory_mib: u32,
    /// Network config for restore
    network_config: NetworkConfig,
    /// Original VM ID (for vsock socket path redirect)
    original_vm_id: String,
    /// Creation timestamp
    created_at: chrono::DateTime<chrono::Utc>,
}

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
) -> crate::firecracker::FirecrackerConfig {
    use crate::firecracker::{FirecrackerConfig, FcNetworkMode};

    let network_mode = match args.network {
        crate::cli::args::NetworkMode::Bridged => FcNetworkMode::Bridged,
        crate::cli::args::NetworkMode::Rootless => FcNetworkMode::Rootless,
    };

    FirecrackerConfig::new(
        kernel_path.to_path_buf(),
        initrd_path.to_path_buf(),
        rootfs_path.to_path_buf(),
        image_identifier.to_string(),
        args.cpu,
        args.mem,
        network_mode,
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

/// Check if a podman cache exists for the given cache key.
fn check_podman_cache(cache_key: &str) -> Option<PathBuf> {
    let cache_dir = paths::podman_cache_dir().join(cache_key);
    let config_path = cache_dir.join("config.json");

    if config_path.exists() {
        Some(cache_dir)
    } else {
        None
    }
}

/// Create a podman cache snapshot from a running VM.
///
/// This pauses the VM, creates a Firecracker snapshot, copies the disk,
/// saves metadata, and resumes the VM.
async fn create_podman_cache(
    vm_manager: &VmManager,
    cache_key: &str,
    vm_id: &str,
    args: &RunArgs,
    image_digest: &str,
    disk_path: &Path,
    network_config: &NetworkConfig,
) -> Result<()> {
    let cache_dir = paths::podman_cache_dir().join(cache_key);

    // Lock to prevent concurrent cache creation
    let lock_path = cache_dir.with_extension("lock");
    tokio::fs::create_dir_all(paths::podman_cache_dir())
        .await
        .context("creating podman-cache directory")?;

    let lock_file = std::fs::File::create(&lock_path).context("creating cache lock file")?;
    lock_file.lock_exclusive().context("acquiring cache lock")?;

    // Double-check after lock (another process might have created it)
    if cache_dir.join("config.json").exists() {
        info!(cache_key = %cache_key, "Cache already exists (created by another process)");
        return Ok(());
    }

    info!(cache_key = %cache_key, "Creating podman cache snapshot");

    // Create cache directory
    tokio::fs::create_dir_all(&cache_dir)
        .await
        .context("creating cache directory")?;

    // Get Firecracker client
    let client = vm_manager.client().context("VM not started")?;

    // Pause VM before snapshotting (required by Firecracker)
    use crate::firecracker::api::{SnapshotCreate, VmState as ApiVmState};

    client
        .patch_vm_state(ApiVmState {
            state: "Paused".to_string(),
        })
        .await
        .context("pausing VM for snapshot")?;

    info!(cache_key = %cache_key, "VM paused for cache snapshot");

    // Create snapshot files
    let memory_path = cache_dir.join("memory.bin");
    let vmstate_path = cache_dir.join("vmstate.bin");

    let snapshot_result = client
        .create_snapshot(SnapshotCreate {
            snapshot_type: Some("Full".to_string()),
            snapshot_path: vmstate_path.display().to_string(),
            mem_file_path: memory_path.display().to_string(),
        })
        .await;

    // Resume VM regardless of snapshot result
    let resume_result = client
        .patch_vm_state(ApiVmState {
            state: "Resumed".to_string(),
        })
        .await;

    if let Err(e) = &resume_result {
        warn!(cache_key = %cache_key, error = %e, "Failed to resume VM after snapshot");
    }

    // Check if snapshot succeeded
    snapshot_result.context("creating Firecracker snapshot")?;
    resume_result.context("resuming VM after snapshot")?;

    // Copy disk using btrfs reflink
    let cache_disk_path = cache_dir.join("disk.raw");
    let reflink_result = tokio::process::Command::new("cp")
        .args([
            "--reflink=always",
            disk_path.to_str().unwrap(),
            cache_disk_path.to_str().unwrap(),
        ])
        .status()
        .await
        .context("copying disk with reflink")?;

    if !reflink_result.success() {
        bail!("Reflink copy failed - btrfs filesystem required for podman cache");
    }

    // Save cache metadata
    let config = PodmanCacheConfig {
        cache_key: cache_key.to_string(),
        image: args.image.clone(),
        image_digest: image_digest.to_string(),
        vcpu: args.cpu,
        memory_mib: args.mem,
        network_config: network_config.clone(),
        original_vm_id: vm_id.to_string(),
        created_at: chrono::Utc::now(),
    };

    let config_json = serde_json::to_string_pretty(&config)?;
    tokio::fs::write(cache_dir.join("config.json"), config_json)
        .await
        .context("writing cache config")?;

    info!(
        cache_key = %cache_key,
        memory = %memory_path.display(),
        disk = %cache_disk_path.display(),
        "Podman cache created successfully"
    );

    Ok(())
}

/// Restore a VM from podman cache using direct File-based memory loading.
///
/// This is similar to snapshot run but uses File backend instead of UFFD.
/// The memory is loaded directly from the cache's memory.bin file.
async fn restore_from_podman_cache(
    args: &RunArgs,
    cache_dir: &Path,
    cache_key: &str,
) -> Result<()> {
    // Load cache configuration
    let config_path = cache_dir.join("config.json");
    let config_json = tokio::fs::read_to_string(&config_path)
        .await
        .context("reading cache config")?;
    let cache_config: PodmanCacheConfig =
        serde_json::from_str(&config_json).context("parsing cache config")?;

    info!(
        cache_key = %cache_key,
        image = %cache_config.image,
        "Restoring from podman cache"
    );

    // Generate VM ID and validate name
    let vm_id = generate_vm_id();
    let vm_name = args.name.clone();
    validate_vm_name(&vm_name).context("invalid VM name")?;

    // Setup paths
    let data_dir = paths::vm_runtime_dir(&vm_id);
    tokio::fs::create_dir_all(&data_dir)
        .await
        .context("creating VM data directory")?;

    let socket_path = data_dir.join("firecracker.sock");
    let vm_dir = data_dir.join("disks");
    tokio::fs::create_dir_all(&vm_dir)
        .await
        .context("creating VM disks directory")?;

    // Create CoW disk from cache
    let disk_manager = DiskManager::new(vm_id.clone(), cache_dir.join("disk.raw"), vm_dir.clone());
    let rootfs_path = disk_manager
        .create_cow_disk()
        .await
        .context("creating CoW disk from cache")?;

    info!(
        rootfs = %rootfs_path.display(),
        cache_disk = %cache_dir.join("disk.raw").display(),
        "CoW disk prepared from cache"
    );

    // Create VM state
    let mut vm_state = VmState::new(
        vm_id.clone(),
        cache_config.image.clone(),
        cache_config.vcpu,
        cache_config.memory_mib,
    );
    vm_state.name = Some(vm_name.clone());
    vm_state.config.volumes = args.map.clone();
    vm_state.config.health_check_url = args.health_check.clone();

    // Initialize state manager
    let state_manager = StateManager::new(paths::state_dir());
    state_manager.init().await?;

    // Parse port mappings for new VM
    let port_mappings: Vec<PortMapping> = args
        .publish
        .iter()
        .map(|s| PortMapping::parse(s))
        .collect::<Result<Vec<_>>>()
        .context("parsing port mappings")?;

    // Setup networking based on mode
    // For cache restore, we need to use the original guest IP from the cached snapshot
    // because the VM's IP is baked into the kernel command line and disk
    let tap_device = format!("tap-{}", truncate_id(&vm_id, 8));
    let mut network: Box<dyn NetworkManager> = match args.network {
        NetworkMode::Bridged => {
            if !nix::unistd::geteuid().is_root() {
                bail!(
                    "Bridged networking requires root. Either:\n  \
                     - Run with sudo: sudo fcvm podman run ...\n  \
                     - Use rootless mode: fcvm podman run --network rootless ..."
                );
            }
            // Use clone approach: NAT to the original guest IP from the cached snapshot
            let original_guest_ip = cache_config
                .network_config
                .guest_ip
                .clone()
                .ok_or_else(|| anyhow::anyhow!("cached config missing guest_ip for bridged mode"))?;
            Box::new(
                BridgedNetwork::new(vm_id.clone(), tap_device.clone(), port_mappings.clone())
                    .with_guest_ip(original_guest_ip),
            )
        }
        NetworkMode::Rootless => {
            // Allocate loopback IP
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

    // Use network-provided health check URL if user didn't specify one
    if vm_state.config.health_check_url.is_none() {
        vm_state.config.health_check_url = network_config.health_check_url.clone();
    }
    if let Some(port) = network_config.health_check_port {
        vm_state.config.network.health_check_port = Some(port);
    }

    info!(
        tap = %network_config.tap_device,
        mac = %network_config.guest_mac,
        "Network configured for cache restore"
    );

    // Setup vsock socket path
    let vsock_socket_path = if let Some(ref vsock_dir) = args.vsock_dir {
        let vsock_dir = PathBuf::from(vsock_dir);
        tokio::fs::create_dir_all(&vsock_dir)
            .await
            .with_context(|| format!("creating vsock dir: {:?}", vsock_dir))?;
        vsock_dir.join("vsock.sock")
    } else {
        data_dir.join("vsock.sock")
    };

    // Parse volume mappings
    let volume_mappings: Vec<VolumeMapping> = args
        .map
        .iter()
        .map(|s| VolumeMapping::parse(s))
        .collect::<Result<Vec<_>>>()
        .context("parsing volume mappings")?;

    // Spawn VolumeServers for volumes
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

    // Start status listener BEFORE restoring VM so we're ready when fc-agent connects
    let status_socket_path = format!("{}_{}", vsock_socket_path.display(), VSOCK_STATUS_PORT);
    let status_handle = {
        let runtime_dir = data_dir.clone();
        let socket_path = status_socket_path.clone();
        let vm_id_clone = vm_id.clone();
        tokio::spawn(async move {
            // No cache_tx for restored VMs - they don't need to create cache
            if let Err(e) =
                run_status_listener(&socket_path, &runtime_dir, &vm_id_clone, None).await
            {
                tracing::warn!("Status listener error: {}", e);
            }
        })
    };

    // Setup TTY/output listeners BEFORE VM restore so we're ready to accept connections
    let tty_mode = args.tty;
    let interactive = args.interactive;
    let tty_socket_path = format!("{}_{}", vsock_socket_path.display(), VSOCK_TTY_PORT);
    let output_socket_path = format!("{}_{}", vsock_socket_path.display(), VSOCK_OUTPUT_PORT);

    let tty_handle = if tty_mode {
        let socket_path = tty_socket_path.clone();
        Some(std::thread::spawn(move || {
            super::tty::run_tty_session(&socket_path, true, interactive)
        }))
    } else {
        None
    };

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

    // Build restore configuration
    let restore_config = super::common::SnapshotRestoreConfig {
        vmstate_path: cache_dir.join("vmstate.bin"),
        memory_backend: super::common::MemoryBackend::File {
            memory_path: cache_dir.join("memory.bin"),
        },
        source_disk_path: cache_dir.join("disk.raw"),
        original_vm_id: cache_config.original_vm_id.clone(),
    };

    // Run cache restore using shared snapshot restore function
    let setup_result = super::common::restore_from_snapshot(
        &vm_id,
        &vm_name,
        &data_dir,
        &socket_path,
        &restore_config,
        &network_config,
        network.as_mut(),
        &state_manager,
        &mut vm_state,
    )
    .await;

    // If setup failed, cleanup all resources before propagating error
    if let Err(e) = setup_result {
        warn!("Cache restore setup failed, cleaning up resources");

        for handle in volume_server_handles {
            handle.abort();
        }
        status_handle.abort();

        if let Err(cleanup_err) = network.cleanup().await {
            warn!(
                "Failed to cleanup network after setup error: {}",
                cleanup_err
            );
        }
        return Err(e);
    }

    let (mut vm_manager, mut holder_child) = setup_result.unwrap();

    info!(
        vm_id = %vm_id,
        cache_key = %cache_key,
        "VM restored from cache successfully"
    );

    // Create cancellation token for graceful health monitor shutdown
    let health_cancel_token = tokio_util::sync::CancellationToken::new();

    // Spawn health monitor
    let health_monitor_handle = crate::health::spawn_health_monitor_with_cancel(
        vm_id.clone(),
        vm_state.pid,
        paths::state_dir(),
        Some(health_cancel_token.clone()),
    );

    // Setup signal handlers
    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;

    // Wait for signal or VM exit
    let container_exit_code: Option<i32>;
    tokio::select! {
        _ = sigterm.recv() => {
            info!("received SIGTERM, shutting down VM");
            container_exit_code = None;
        }
        _ = sigint.recv() => {
            info!("received SIGINT, shutting down VM");
            container_exit_code = None;
        }
        status = vm_manager.wait() => {
            info!(status = ?status, "VM exited");
            if let Some(handle) = tty_handle {
                container_exit_code = handle.join().ok().and_then(|r| r.ok());
            } else {
                let exit_file = data_dir.join("container-exit");
                container_exit_code = std::fs::read_to_string(&exit_file)
                    .ok()
                    .and_then(|s| s.trim().parse::<i32>().ok());
            }
        }
    }

    // Cancel status listener
    status_handle.abort();

    // Cleanup resources
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
    tokio::process::Command::new("truncate")
        .args(["-s", &image_size.to_string(), output_path.to_str().unwrap()])
        .status()
        .await
        .context("creating sparse file")?;

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
async fn run_output_listener(
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

    // Check for podman cache (unless --no-cache is set)
    // Keep fc_config and cache_key available for later cache creation on miss
    let (fc_config, cache_key): (Option<crate::firecracker::FirecrackerConfig>, Option<String>) = if !args.no_cache {
        // Get image identifier for cache key computation
        let image_identifier = get_image_identifier(&args.image).await?;
        let config = build_firecracker_config(&args, &image_identifier, &kernel_path, &base_rootfs, &initrd_path);
        let key = config.cache_key();

        // Check if cache exists
        if let Some(cache_dir) = check_podman_cache(&key) {
            info!(
                cache_key = %key,
                image = %args.image,
                "Cache hit! Restoring from snapshot"
            );
            return restore_from_podman_cache(&args, &cache_dir, &key).await;
        }

        info!(
            cache_key = %key,
            image = %args.image,
            "Cache miss, will create cache after image load"
        );
        (Some(config), Some(key))
    } else {
        info!("Cache disabled via --no-cache flag");
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
    let mut volume_mappings: Vec<VolumeMapping> = args
        .map
        .iter()
        .map(|s| VolumeMapping::parse(s))
        .collect::<Result<Vec<_>>>()
        .context("parsing volume mappings")?;

    // For localhost/ images, export as OCI archive for direct podman run
    // Uses content-addressable cache to avoid re-exporting the same image
    let image_archive_name = if args.image.starts_with("localhost/") {
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
        // Use OCI archive format (single tar file) for faster FUSE transfer
        let archive_path = cache_dir.with_extension("oci.tar");
        if !archive_path.exists() {
            info!(image = %args.image, digest = %digest, "Exporting localhost image as OCI archive");

            let output = tokio::process::Command::new("podman")
                .args([
                    "save",
                    "--format",
                    "oci-archive",
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

            info!(path = %archive_path.display(), "Image exported as OCI archive");
        } else {
            info!(image = %args.image, digest = %digest, "Using cached OCI archive");
        }

        // Lock released when lock_file is dropped
        drop(lock_file);

        // Add the image-cache directory as a read-only volume mount
        // Guest will access the archive at /tmp/fcvm-image/{digest}.oci.tar
        volume_mappings.push(VolumeMapping {
            host_path: image_cache_dir.clone(),
            guest_path: "/tmp/fcvm-image".to_string(),
            read_only: true,
        });

        // Return the archive filename (relative to mount point)
        Some(format!("{}.oci.tar", digest))
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

    // Parse optional container command - either from trailing args or --cmd flag
    let cmd_args = if !args.command_args.is_empty() {
        // Trailing args take precedence (e.g., "alpine:latest sh -c 'echo hello'")
        Some(args.command_args.clone())
    } else if let Some(cmd) = &args.cmd {
        // Fall back to --cmd flag with shell parsing
        Some(shell_words::split(cmd).with_context(|| format!("parsing --cmd argument: {}", cmd))?)
    } else {
        None
    };

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

    // Use network-provided health check URL if user didn't specify one
    // Each network type (bridged/rootless) generates its own appropriate URL
    if vm_state.config.health_check_url.is_none() {
        vm_state.config.health_check_url = network_config.health_check_url.clone();
    }
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

    // Create cache channel for cache-ready notifications (unless --no-cache is set)
    let (cache_tx, mut cache_rx): (
        Option<mpsc::Sender<CacheRequest>>,
        Option<mpsc::Receiver<CacheRequest>>,
    ) = if !args.no_cache {
        let (tx, rx) = mpsc::channel(1);
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
        image_archive_name.as_deref(),
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

    // Spawn health monitor task with cancellation support
    let health_monitor_handle = crate::health::spawn_health_monitor_with_cancel(
        vm_id.clone(),
        vm_state.pid,
        paths::state_dir(),
        Some(health_cancel_token.clone()),
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
                if let Some(ref key) = cache_key {
                    info!(cache_key = %key, digest = %cache_request.digest, "Creating cache snapshot");

                    let create_result = create_podman_cache(
                        &vm_manager,
                        key,
                        &vm_id,
                        &args,
                        &cache_request.digest,
                        &disk_path,
                        &network_config,
                    ).await;

                    match create_result {
                        Ok(()) => {
                            info!(cache_key = %key, "Cache created successfully");
                        }
                        Err(e) => {
                            warn!(cache_key = %key, error = %e, "Failed to create cache");
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
    image_archive_name: Option<&str>,
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

        let retry_deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut attempt = 0;
        #[allow(unused_assignments)]
        let mut _last_error: Option<String> = None;

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
                std::time::Duration::from_millis(500),
            )
            .await;

            // If namespace didn't become ready, kill holder and retry
            if !namespace_ready {
                let _ = child.kill().await;
                _last_error = Some("namespace not ready after 500ms".to_string());

                if std::time::Instant::now() < retry_deadline {
                    warn!(
                        holder_pid = holder_pid,
                        attempt = attempt,
                        "namespace not ready, retrying holder creation..."
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
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

                    _last_error = Some(format!(
                        "holder exited immediately: status={}, stderr='{}'",
                        status,
                        stderr.trim()
                    ));

                    if std::time::Instant::now() < retry_deadline {
                        warn!(
                            holder_pid = holder_pid,
                            attempt = attempt,
                            status = %status,
                            stderr = %stderr.trim(),
                            "holder died, retrying..."
                        );
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
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

                _last_error = Some(format!(
                    "holder died after 100ms: stderr='{}'",
                    holder_stderr_content.trim()
                ));

                if std::time::Instant::now() < retry_deadline {
                    warn!(
                        holder_pid = holder_pid,
                        attempt = attempt,
                        holder_stderr = %holder_stderr_content.trim(),
                        "holder died after initial check, retrying..."
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
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
            crate::firecracker::FirecrackerConfig::new(
                kernel_path.to_path_buf(),
                initrd_path.to_path_buf(),
                rootfs_path.to_path_buf(),
                args.image.clone(),
                args.cpu,
                args.mem,
                network_mode,
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
        runtime_boot_args.push_str(&format!(
            "ip={}::{}:255.255.255.252::eth0:off{}",
            guest_ip_clean, host_ip_clean, dns_suffix
        ));
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

    // Apply FirecrackerConfig to client (boot_source, machine_config, rootfs drive)
    // This ensures the same config used for cache key is used for launch
    launch_config.apply(&client, &runtime_boot_args).await?;

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
                "image_archive": image_archive_name.map(|name| format!("/tmp/fcvm-image/{}", name)),
                "privileged": args.privileged,
                "interactive": args.interactive,
                "tty": args.tty,
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
