use anyhow::{bail, Context, Result};
use fs2::FileExt;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::mpsc;
use tracing::{info, warn};

mod image;
mod listeners;
mod namespace;
mod snapshot;
mod types;
mod vm_config;

use types::VolumeMapping;
pub use types::{
    CacheRequest, LogLine, SnapshotCreationParams, SnapshotOutcome, VmContext, VmHandle,
};

pub(crate) use listeners::run_output_listener;
use listeners::run_status_listener;

use snapshot::{build_firecracker_config, snapshot_run_firecracker_overrides};
pub use snapshot::{check_podman_snapshot, create_snapshot_interruptible, startup_snapshot_key};
use vm_config::{cleanup_nfs_exports, run_vm_setup};

use crate::cli::{NetworkMode, PodmanArgs, PodmanCommands, RunArgs};
use crate::commands::common::{
    VSOCK_OUTPUT_PORT, VSOCK_STATUS_PORT, VSOCK_TTY_PORT, VSOCK_VOLUME_PORT_BASE,
};
use crate::network::{BridgedNetwork, NetworkManager, PortMapping, SlirpNetwork};
use crate::paths;
use crate::state::{generate_vm_id, truncate_id, validate_vm_name, StateManager, VmState};
use crate::volume::{spawn_volume_servers, VolumeConfig};
use image::{build_storage_image, get_image_identifier, validate_docker_archive};
use tokio_util::sync::CancellationToken;

/// Resolve the image delivery mode from CLI args and kernel profile.
///
/// Priority: explicit `--image-mode` > auto-detect from kernel profile.
/// Auto-detect: kernel profile name containing "btrfs" → Btrfs, otherwise Overlay.
fn resolve_image_mode(args: &RunArgs) -> crate::firecracker::ImageMode {
    use crate::firecracker::ImageMode;

    // Explicit CLI flag wins
    if let Some(ref mode) = args.image_mode {
        return match mode {
            crate::cli::ImageMode::Overlay => ImageMode::Overlay,
            crate::cli::ImageMode::Btrfs => ImageMode::Btrfs,
            crate::cli::ImageMode::Archive => ImageMode::Archive,
        };
    }

    // Auto-detect from kernel profile name
    if let Some(ref profile_name) = args.kernel_profile {
        if profile_name.contains("btrfs") {
            return ImageMode::Btrfs;
        }
    }

    // Default: overlay
    ImageMode::Overlay
}

/// Start a VM with the given args. Returns a handle to the running VM.
///
/// The VM event loop runs in a background task. The handle's `Drop` impl cancels
/// the VM, so dropping the handle triggers cleanup. Use `stop()` for explicit
/// shutdown with exit code, or `wait()` to wait for natural exit.
///
/// Note: `args.no_snapshot` is forced to `true` to skip snapshot cache lookups,
/// ensuring a fresh VM is always started.
pub async fn start_vm(mut args: RunArgs) -> Result<VmHandle> {
    // Force no-snapshot mode — start_vm always creates a fresh VM
    args.no_snapshot = true;

    let mut ctx = prepare_vm(args)
        .await?
        .ok_or_else(|| anyhow::anyhow!("unexpected snapshot cache hit with no_snapshot=true"))?;

    let vm_id = ctx.vm_id.clone();
    let name = ctx.vm_name.clone();
    let log_tx = ctx.log_tx.clone();
    let cancel = CancellationToken::new();
    let cancel_clone = cancel.clone();

    // Get actual PID from VM state (set during prepare_vm)
    let actual_pid = ctx
        .state_manager
        .load_state_by_name(&name)
        .await
        .ok()
        .and_then(|s| s.pid)
        .unwrap_or(0);

    let task = tokio::spawn(async move {
        let result = run_vm_loop(&mut ctx, cancel_clone).await;
        cleanup_vm_context(ctx).await;
        result
    });

    Ok(VmHandle {
        vm_id,
        name,
        pid: actual_pid,
        cancel,
        task: Some(task),
        log_tx,
    })
}

/// Main dispatcher for podman commands
pub async fn cmd_podman(args: PodmanArgs) -> Result<()> {
    match args.cmd {
        PodmanCommands::Run(run_args) => cmd_podman_run(run_args).await,
    }
}

pub async fn prepare_vm(mut args: RunArgs) -> Result<Option<VmContext>> {
    info!("Starting fcvm podman run");

    // Validate VM name before any setup work
    validate_vm_name(&args.name).context("invalid VM name")?;

    // Validate hugepages memory alignment (2MB pages require even MiB)
    if args.hugepages && args.mem % 2 != 0 {
        bail!(
            "--mem {} is not divisible by 2: hugepages requires 2MB-aligned memory size",
            args.mem
        );
    }

    // Disallow --setup when running as root
    // Root users should run `fcvm setup` explicitly
    if args.setup && nix::unistd::geteuid().is_root() {
        bail!("--setup is not allowed when running as root. Run 'fcvm setup' first.");
    }

    // Validate --user format and resolve username from /etc/passwd (matching podman behavior).
    // Accepts "uid:gid" (numeric) - username is looked up from host passwd, just like
    // podman --userns=keep-id resolves the username from the container's passwd.
    if let Some(ref user) = args.user {
        let parts: Vec<&str> = user.split(':').collect();
        if parts.len() != 2 {
            bail!("invalid --user format '{}': expected 'uid:gid'", user);
        }
        let uid: u32 = parts[0]
            .parse()
            .map_err(|_| anyhow::anyhow!("invalid --user uid '{}': must be numeric", parts[0]))?;
        parts[1]
            .parse::<u32>()
            .map_err(|_| anyhow::anyhow!("invalid --user gid '{}': must be numeric", parts[1]))?;

        // Resolve username from host /etc/passwd (like podman does with keep-id)
        if !args.env.iter().any(|e| e.starts_with("USER=")) {
            let username = resolve_username(uid);
            args.env.push(format!("USER={}", username));
        }
    }

    // Resolve 0 → host values for cpu and mem
    if args.cpu == 0 {
        // Firecracker allows max 32 vCPUs (and must be 1 or even)
        let host_cpus = std::thread::available_parallelism()
            .map(|n| n.get().min(32) as u8)
            .unwrap_or(2);
        args.cpu = host_cpus;
        info!("Using host CPUs (capped at 32): {}", args.cpu);
    }
    info!("VM memory: {} MiB", args.mem);

    // Build RuntimeConfig from kernel profile (replaces env var config passing)
    let mut runtime_config = super::common::RuntimeConfig::default();
    if let Some(ref profile_name) = args.kernel_profile {
        let profile = crate::setup::get_kernel_profile(profile_name)?.ok_or_else(|| {
            anyhow::anyhow!(
                "kernel profile '{}' not found for {} in config",
                profile_name,
                std::env::consts::ARCH
            )
        })?;

        info!(profile = %profile_name, "using kernel profile");

        let fc_path = crate::setup::get_firecracker_for_profile(&profile, profile_name).await?;
        info!(firecracker_bin = %fc_path.display(), "from profile");
        runtime_config.firecracker_bin = Some(fc_path);
        if let Some(ref fc_args) = profile.firecracker_args {
            info!(firecracker_args = %fc_args, "from profile");
            runtime_config.firecracker_args = Some(fc_args.clone());
        }
        if let Some(ref boot_args) = profile.boot_args {
            info!(boot_args = %boot_args, "from profile");
            runtime_config.boot_args = Some(boot_args.clone());
        }
        if let Some(readers) = profile.fuse_readers {
            info!(fuse_readers = %readers, "from profile");
            runtime_config.fuse_readers = Some(readers);
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
        let resolved_mode = resolve_image_mode(&args);
        let config = build_firecracker_config(
            &args,
            &image_identifier,
            &kernel_path,
            &base_rootfs,
            &initrd_path,
            cmd_args.clone(),
            resolved_mode,
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
            let (firecracker_bin, firecracker_args) =
                snapshot_run_firecracker_overrides(&runtime_config);
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
                cpu: Some(args.cpu),
                mem: Some(args.mem),
                firecracker_bin,
                firecracker_args,
                hugepages: Some(args.hugepages),
            };
            super::snapshot::cmd_snapshot_run(snapshot_args).await?;
            return Ok(None);
        }

        // Check for pre-start snapshot (container loaded but not initialized)
        if check_podman_snapshot(&key).await.is_some() {
            info!(
                snapshot_key = %key,
                image = %args.image,
                "Pre-start snapshot hit! Restoring from cached snapshot"
            );
            let (firecracker_bin, firecracker_args) =
                snapshot_run_firecracker_overrides(&runtime_config);
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
                // Create startup snapshot if this config has a health check URL
                startup_snapshot_base_key: args.health_check.as_ref().map(|_| key.clone()),
                cpu: Some(args.cpu),
                mem: Some(args.mem),
                firecracker_bin,
                firecracker_args,
                hugepages: Some(args.hugepages),
            };
            super::snapshot::cmd_snapshot_run(snapshot_args).await?;
            return Ok(None);
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
        let needs_export = if !archive_path.exists() {
            true
        } else if !validate_docker_archive(&archive_path)? {
            warn!(path = %archive_path.display(), "Cached archive is invalid, re-exporting");
            let _ = tokio::fs::remove_file(&archive_path).await;
            true
        } else {
            info!(image = %args.image, digest = %digest, "Using cached Docker archive");
            false
        };

        if needs_export {
            info!(image = %args.image, digest = %digest, "Exporting localhost image as Docker archive");

            // Save to a temp file in the same directory, then rename.
            // This avoids corrupt archives from interrupted exports (atomic rename).
            // NOTE: Can't use with_extension("docker.tar.tmp") here because archive_path
            // ends in .docker.tar — with_extension replaces after the last dot, producing
            // .docker.docker.tar.tmp (double .docker). Use format! to just append .tmp.
            let tmp_path = PathBuf::from(format!("{}.tmp", archive_path.display()));

            // Remove stale tmp file if it exists (podman save won't overwrite)
            let _ = tokio::fs::remove_file(&tmp_path).await;

            let output = tokio::process::Command::new("podman")
                .args([
                    "save",
                    "--format",
                    "docker-archive",
                    "-o",
                    tmp_path.to_str().unwrap(),
                    &args.image,
                ])
                .output()
                .await
                .context("running podman save")?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                let _ = tokio::fs::remove_file(&tmp_path).await;
                drop(lock_file);
                bail!(
                    "Failed to export image '{}' with podman save: {}",
                    args.image,
                    stderr
                );
            }

            // Validate the archive contains manifest.json (required for docker-archive format)
            if !validate_docker_archive(&tmp_path)? {
                let _ = tokio::fs::remove_file(&tmp_path).await;
                drop(lock_file);
                bail!(
                    "podman save produced invalid archive (missing manifest.json) for image '{}'",
                    args.image
                );
            }

            // Atomic rename within the same filesystem
            tokio::fs::rename(&tmp_path, &archive_path)
                .await
                .context("renaming exported archive to final path")?;

            info!(path = %archive_path.display(), "Image exported as Docker archive");
        }

        let resolved_image_mode = resolve_image_mode(&args);
        info!(image = %args.image, digest = %digest, mode = %resolved_image_mode, "Image delivery mode");

        let disk_path = match resolved_image_mode {
            crate::firecracker::ImageMode::Overlay => {
                // Pre-built overlay storage: ext4 image with podman storage.
                // Guest mounts this as additionalImageStore — no podman load needed.
                // Format version for overlay image cache. Bump when the build process
                // changes in a way that invalidates previously-cached images.
                // v2: host-side cleanup of podman state files before ext4 packaging
                const OVERLAY_CACHE_VERSION: u32 = 2;
                let storage_img_path = PathBuf::from(format!(
                    "{}.storage-v{}.img",
                    cache_dir.display(),
                    OVERLAY_CACHE_VERSION
                ));
                if !storage_img_path.exists() {
                    info!(image = %args.image, digest = %digest, "Building overlay storage image");
                    build_storage_image(&archive_path, &storage_img_path).await?;
                } else {
                    info!(image = %args.image, digest = %digest, "Using cached overlay storage image");
                }
                storage_img_path
            }
            crate::firecracker::ImageMode::Btrfs => {
                // Pre-built btrfs storage: btrfs image with real subvolumes.
                // Guest reflink-copies it and mounts as graphroot — no podman load needed.
                //
                // For --user mode: build as uid 1000 (rootless podman), creating storage
                // with correct ownership — matching a physical host. Separate cache path
                // because rootless builds have different UID ownership than root builds.
                let build_uid = args
                    .user
                    .as_ref()
                    .and_then(|user_spec| user_spec.split(':').next()?.parse::<u32>().ok());
                // Cache key includes UID and rootfs_size — different sizes need different images
                let btrfs_img_path = match build_uid {
                    Some(uid) => PathBuf::from(format!(
                        "{}.btrfs-uid{}-{}.img",
                        cache_dir.display(),
                        uid,
                        args.rootfs_size
                    )),
                    None => PathBuf::from(format!(
                        "{}.btrfs-{}.img",
                        cache_dir.display(),
                        args.rootfs_size
                    )),
                };
                if !btrfs_img_path.exists() {
                    info!(image = %args.image, digest = %digest, uid = ?build_uid, "Building btrfs storage image");
                    image::build_btrfs_storage_image(
                        &archive_path,
                        &btrfs_img_path,
                        build_uid,
                        &args.rootfs_size,
                    )
                    .await?;
                } else {
                    info!(image = %args.image, digest = %digest, "Using cached btrfs storage image");
                }
                btrfs_img_path
            }
            crate::firecracker::ImageMode::Archive => {
                // Docker archive: attach as raw block device.
                // fc-agent reads docker-archive:/dev/vdX via podman load at boot.
                archive_path
            }
        };

        // Lock released when lock_file is dropped
        drop(lock_file);

        Some(disk_path)
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

    // For btrfs mode, reflink copy the image from cache to per-VM directory.
    // Each VM needs its own read-write copy because:
    // 1. Btrfs images are read-write (podman creates subvolumes in graphroot)
    // 2. Snapshot restore needs a pristine copy (not modified by previous VM runs)
    // 3. Mount namespace redirects vm_runtime_dir paths for clones
    let image_disk_path = if let Some(cache_path) = image_disk_path {
        let resolved_mode = resolve_image_mode(&args);
        if resolved_mode == crate::firecracker::ImageMode::Btrfs {
            let disks_dir = data_dir.join("disks");
            tokio::fs::create_dir_all(&disks_dir)
                .await
                .context("creating disks directory for btrfs image")?;
            let per_vm_path = disks_dir.join("image.btrfs");
            crate::commands::common::reflink_copy(&cache_path, &per_vm_path)
                .await
                .context("reflink copying btrfs image to per-VM directory")?;
            info!(
                cache = %cache_path.display(),
                per_vm = %per_vm_path.display(),
                "reflink copied btrfs image to per-VM directory"
            );
            Some(per_vm_path)
        } else {
            Some(cache_path)
        }
    } else {
        None
    };

    let socket_path = data_dir.join("firecracker.sock");

    // Create VM state
    // Note: env vars are NOT stored in state (they may contain secrets and state is world-readable)
    // Instead, env is passed directly to MMDS at VM start time
    let mut vm_state = VmState::new(vm_id.clone(), args.image.clone(), args.cpu, args.mem);
    vm_state.name = Some(vm_name.clone());
    vm_state.config.volumes = args.map.clone();
    vm_state.config.health_check_url = args.health_check.clone();
    vm_state.config.hugepages = args.hugepages;
    vm_state.config.portable_volumes = args.portable_volumes;
    vm_state.config.port_mappings = port_mappings.clone();
    vm_state.config.user = args.user.clone();
    // Store the username for health checks (runuser -u <username>).
    // USER env var was resolved from host /etc/passwd above (or explicitly passed).
    if args.user.is_some() {
        vm_state.config.username = args
            .env
            .iter()
            .find_map(|s| s.strip_prefix("USER="))
            .map(|s| s.to_string());
    }
    vm_state.config.labels = args
        .label
        .iter()
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();

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
            portable: args.portable_volumes,
        })
        .collect();

    let volume_server_handles = spawn_volume_servers(&volume_configs, &vsock_socket_path)
        .await
        .context("spawning VolumeServers")?;

    // Create snapshot channel for snapshot-ready notifications
    // Skip snapshot creation when:
    // - --no-snapshot flag or FCVM_NO_SNAPSHOT env var is set
    // Note: FUSE volumes survive snapshot/restore — fc-agent remounts them on clone restore
    let skip_snapshot_creation = no_snapshot;
    let (cache_tx, cache_rx): (
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
    // - Not skipping snapshots (no --no-snapshot)
    // - Have a snapshot key
    // - Have a health_check URL configured (HTTP health check, not just container-ready)
    let (startup_tx, startup_rx): (
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

    // Broadcast channel for container output (used by VmHandle::subscribe_logs())
    let (log_tx, _) = tokio::sync::broadcast::channel::<LogLine>(1000);

    // For non-TTY mode, use async output listener
    let output_reconnect = Arc::new(tokio::sync::Notify::new());
    let output_handle = if !tty_mode {
        let socket_path = output_socket_path.clone();
        let vm_id_clone = vm_id.clone();
        let log_tx_clone = Some(log_tx.clone());
        let reconnect = output_reconnect.clone();
        Some(tokio::spawn(async move {
            match run_output_listener(&socket_path, &vm_id_clone, log_tx_clone, reconnect).await {
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
        &runtime_config,
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

        // Abort output listener task if still running
        if let Some(handle) = output_handle {
            handle.abort();
        }

        // Cleanup network
        if let Err(cleanup_err) = network.cleanup().await {
            warn!(
                "failed to cleanup network after setup error: {}",
                cleanup_err
            );
        }
        return Err(e);
    }

    let (vm_manager, holder_child) = setup_result.unwrap();

    info!(vm_id = %vm_id, "VM started successfully");

    // Create cancellation token for graceful health monitor shutdown
    let health_cancel_token = CancellationToken::new();

    // Spawn health monitor task with startup snapshot trigger support
    let health_monitor_handle = crate::health::spawn_health_monitor_full(
        vm_id.clone(),
        vm_state.pid,
        paths::state_dir(),
        Some(health_cancel_token.clone()),
        startup_tx,
    );

    let disk_path = data_dir.join("disks/rootfs.raw");

    // Build image extra disk entries for snapshot metadata.
    // For btrfs mode, the per-VM image copy needs to be saved with snapshots
    // so clones get their own isolated copy.
    let image_extra_disks = if image_disk_path.as_ref().is_some_and(|p| {
        p.file_name()
            .is_some_and(|f| f.to_string_lossy().ends_with(".btrfs"))
    }) {
        let disk_idx = args.disk.len() + args.disk_dir.len();
        vec![crate::storage::SnapshotExtraDisk {
            filename: "image.btrfs".to_string(),
            mount_path: String::new(),
            read_only: false,
            drive_id: format!("disk{}", disk_idx),
        }]
    } else {
        vec![]
    };

    Ok(Some(VmContext {
        vm_id,
        vm_name,
        data_dir,
        vm_manager,
        holder_child,
        volume_server_handles,
        network,
        network_config,
        state_manager,
        health_cancel_token,
        health_monitor_handle,
        status_handle,
        tty_handle,
        output_handle,
        cache_rx,
        startup_rx,
        snapshot_key,
        volume_configs,
        args,
        disk_path,
        log_tx,
        output_reconnect,
        image_extra_disks,
    }))
}

/// Resolve a UID to a username via the host's /etc/passwd.
/// Falls back to "u{uid}" if the UID isn't found (matching podman's numeric fallback).
fn resolve_username(uid: u32) -> String {
    // Use nix::unistd::User which reads /etc/passwd
    match nix::unistd::User::from_uid(nix::unistd::Uid::from_raw(uid)) {
        Ok(Some(user)) => user.name,
        _ => format!("u{}", uid),
    }
}

/// Event loop: waits for VM exit, cancellation, or snapshot requests.
/// Returns the container exit code (None if cancelled/signalled).
pub async fn run_vm_loop(ctx: &mut VmContext, cancel: CancellationToken) -> Result<Option<i32>> {
    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                info!("cancellation requested, shutting down VM");
                return Ok(None);
            }
            status = ctx.vm_manager.wait() => {
                info!(status = ?status, "VM exited");
                if let Some(handle) = ctx.tty_handle.take() {
                    let exit_code = handle.join().ok().and_then(|r| r.ok());
                    info!(container_exit_code = ?exit_code, "TTY container exit code");
                    return Ok(exit_code);
                } else {
                    let exit_file = ctx.data_dir.join("container-exit");
                    let exit_code = std::fs::read_to_string(&exit_file)
                        .ok()
                        .and_then(|s| s.trim().parse::<i32>().ok());
                    info!(container_exit_code = ?exit_code, "container exit code");
                    return Ok(exit_code);
                }
            }
            // Handle cache creation requests from fc-agent
            Some(cache_request) = async {
                match ctx.cache_rx.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                if let Some(ref key) = ctx.snapshot_key {
                    info!(snapshot_key = %key, digest = %cache_request.digest, "Creating pre-start snapshot");

                    let mut params = SnapshotCreationParams::from_run_args(&ctx.args);
                    params.extra_disks = ctx.image_extra_disks.clone();
                    match create_snapshot_interruptible(
                        &ctx.vm_manager, key, &ctx.vm_id, &params, &ctx.disk_path,
                        &ctx.network_config, &ctx.volume_configs,
                        None, // Pre-start is the first snapshot, no parent
                        &cancel,
                    ).await {
                        SnapshotOutcome::Interrupted => {
                            return Ok(None);
                        }
                        SnapshotOutcome::Created => {
                            info!(snapshot_key = %key, "Pre-start snapshot created successfully");
                            // Signal output listener to re-accept (vsock reset during snapshot)
                            ctx.output_reconnect.notify_one();
                        }
                        SnapshotOutcome::Failed(e) => {
                            warn!(snapshot_key = %key, error = %e, "Failed to create pre-start snapshot");
                            // Signal even on failure — vsock was still reset during the attempt
                            ctx.output_reconnect.notify_one();
                        }
                    }
                    // Send ack back regardless of success (fc-agent should continue)
                    let _ = cache_request.ack_tx.send(());
                } else {
                    // Should not happen if channel exists, but send ack anyway
                    let _ = cache_request.ack_tx.send(());
                }
                // Continue waiting for VM exit or cancellation
            }
            // Handle startup snapshot creation when health becomes healthy
            Ok(()) = async {
                match ctx.startup_rx.as_mut() {
                    Some(rx) => rx.await,
                    None => std::future::pending().await,
                }
            } => {
                // Oneshot channel - prevent further attempts
                ctx.startup_rx = None;

                if let Some(ref key) = ctx.snapshot_key {
                    let startup_key = startup_snapshot_key(key);

                    // Skip if startup snapshot already exists
                    if check_podman_snapshot(&startup_key).await.is_some() {
                        info!(snapshot_key = %startup_key, "Startup snapshot already exists, skipping");
                    } else {
                        info!(snapshot_key = %startup_key, "Creating startup snapshot (VM healthy)");

                        let mut params = SnapshotCreationParams::from_run_args(&ctx.args);
                        params.extra_disks = ctx.image_extra_disks.clone();
                        match create_snapshot_interruptible(
                            &ctx.vm_manager, &startup_key, &ctx.vm_id, &params, &ctx.disk_path,
                            &ctx.network_config, &ctx.volume_configs,
                            Some(key.as_str()), // Parent is pre-start snapshot
                            &cancel,
                        ).await {
                            SnapshotOutcome::Interrupted => {
                                return Ok(None);
                            }
                            SnapshotOutcome::Created => {
                                info!(snapshot_key = %startup_key, "Startup snapshot created successfully");
                                // Signal output listener to re-accept (vsock reset during snapshot)
                                ctx.output_reconnect.notify_one();
                            }
                            SnapshotOutcome::Failed(e) => {
                                warn!(snapshot_key = %startup_key, error = %e, "Failed to create startup snapshot");
                                // Signal even on failure — vsock was still reset during the attempt
                                ctx.output_reconnect.notify_one();
                            }
                        }
                    }
                }
                // Continue waiting for VM exit or cancellation
            }
        }
    }
}

/// Clean up all resources associated with a VM.
pub async fn cleanup_vm_context(mut ctx: VmContext) {
    // Cancel status listener (podman-specific)
    ctx.status_handle.abort();

    // Cleanup NFS exports
    cleanup_nfs_exports(&ctx.vm_id).await;

    // Cleanup common resources
    super::common::cleanup_vm(
        &ctx.vm_id,
        &mut ctx.vm_manager,
        &mut ctx.holder_child,
        ctx.volume_server_handles,
        ctx.network.as_mut(),
        &ctx.state_manager,
        &ctx.data_dir,
        Some(ctx.health_cancel_token),
        Some(ctx.health_monitor_handle),
        ctx.output_handle,
    )
    .await;
}

/// CLI entrypoint for `fcvm podman run`. Thin wrapper around prepare_vm/run_vm_loop/cleanup.
async fn cmd_podman_run(args: RunArgs) -> Result<()> {
    let Some(mut ctx) = prepare_vm(args).await? else {
        return Ok(()); // Snapshot cache hit, already handled
    };

    // Setup signal handlers → cancellation token
    let cancel = CancellationToken::new();
    let cancel_clone = cancel.clone();
    tokio::spawn(async move {
        let mut sigterm = signal(SignalKind::terminate()).expect("SIGTERM handler");
        let mut sigint = signal(SignalKind::interrupt()).expect("SIGINT handler");
        tokio::select! {
            _ = sigterm.recv() => { info!("received SIGTERM, shutting down VM"); }
            _ = sigint.recv() => { info!("received SIGINT, shutting down VM"); }
        }
        cancel_clone.cancel();
    });

    let exit_code = run_vm_loop(&mut ctx, cancel).await?;
    cleanup_vm_context(ctx).await;

    // Return error if container exited with non-zero exit code
    if let Some(code) = exit_code {
        if code != 0 {
            bail!("container exited with code {}", code);
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::args::{ImageMode as CliImageMode, NetworkMode};
    use crate::firecracker::ImageMode;

    fn test_args() -> RunArgs {
        RunArgs {
            name: "test".to_string(),
            cpu: 2,
            mem: 2048,
            rootfs_size: "10G".to_string(),
            map: vec![],
            disk: vec![],
            disk_dir: vec![],
            nfs: vec![],
            env: vec![],
            cmd: None,
            publish: vec![],
            balloon: None,
            network: NetworkMode::Rootless,
            health_check: None,
            privileged: false,
            interactive: false,
            tty: false,
            strace_agent: false,
            setup: false,
            kernel: None,
            kernel_profile: None,
            vsock_dir: None,
            no_snapshot: true,
            user: None,
            forward_localhost: vec![],
            hugepages: false,
            portable_volumes: false,
            image_mode: None,
            label: vec![],
            image: "alpine:latest".to_string(),
            command_args: vec![],
        }
    }

    #[test]
    fn test_resolve_image_mode_default_is_overlay() {
        let args = test_args();

        assert_eq!(resolve_image_mode(&args), ImageMode::Overlay);
    }

    #[test]
    fn test_resolve_image_mode_btrfs_profile_auto_detects() {
        let mut args = test_args();
        args.kernel_profile = Some("btrfs".to_string());

        assert_eq!(resolve_image_mode(&args), ImageMode::Btrfs);
    }

    #[test]
    fn test_resolve_image_mode_btrfs_in_profile_name() {
        let mut args = test_args();
        args.kernel_profile = Some("nested-btrfs-test".to_string());

        assert_eq!(resolve_image_mode(&args), ImageMode::Btrfs);
    }

    #[test]
    fn test_resolve_image_mode_non_btrfs_profile_is_overlay() {
        let mut args = test_args();
        args.kernel_profile = Some("nested".to_string());

        assert_eq!(resolve_image_mode(&args), ImageMode::Overlay);
    }

    #[test]
    fn test_resolve_image_mode_explicit_overrides_profile() {
        let mut args = test_args();
        args.kernel_profile = Some("btrfs".to_string());
        args.image_mode = Some(CliImageMode::Archive);

        assert_eq!(resolve_image_mode(&args), ImageMode::Archive);
    }

    #[test]
    fn test_resolve_image_mode_explicit_overlay() {
        let mut args = test_args();
        args.image_mode = Some(CliImageMode::Overlay);

        assert_eq!(resolve_image_mode(&args), ImageMode::Overlay);
    }

    #[test]
    fn test_resolve_image_mode_explicit_btrfs_no_profile() {
        let mut args = test_args();
        args.image_mode = Some(CliImageMode::Btrfs);

        assert_eq!(resolve_image_mode(&args), ImageMode::Btrfs);
    }
}
