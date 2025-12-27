use anyhow::{bail, Context, Result};
use fs2::FileExt;
use std::path::PathBuf;
use tokio::signal::unix::{signal, SignalKind};
use tracing::{debug, info, warn};

use crate::cli::{NetworkMode, PodmanArgs, PodmanCommands, RunArgs};
use crate::firecracker::VmManager;
use crate::network::{BridgedNetwork, NetworkManager, PortMapping, SlirpNetwork};
use crate::paths;
use crate::state::{generate_vm_id, truncate_id, validate_vm_name, StateManager, VmState};
use crate::storage::DiskManager;
use crate::volume::{spawn_volume_servers, VolumeConfig};

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

use super::common::{VSOCK_OUTPUT_PORT, VSOCK_STATUS_PORT, VSOCK_VOLUME_PORT_BASE};

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
async fn run_status_listener(
    socket_path: &str,
    runtime_dir: &std::path::Path,
    vm_id: &str,
) -> Result<()> {
    use tokio::io::AsyncReadExt;
    use tokio::net::UnixListener;

    // Remove stale socket if it exists
    let _ = std::fs::remove_file(socket_path);

    let listener = UnixListener::bind(socket_path)
        .with_context(|| format!("binding status listener to {}", socket_path))?;

    // Make socket accessible by Firecracker running in user namespace (UID 100000)
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(socket_path, std::fs::Permissions::from_mode(0o777))
        .with_context(|| format!("chmod status socket {}", socket_path))?;

    info!(socket = %socket_path, "Status listener started");

    let ready_file = runtime_dir.join("container-ready");
    let exit_file = runtime_dir.join("container-exit");

    // Accept connections in a loop (we get "ready" then "exit")
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
        let mut buf = [0u8; 64];
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
        } else if let Some(code_str) = msg.strip_prefix("exit:") {
            // Write exit code to file
            std::fs::write(&exit_file, format!("{}\n", code_str))
                .with_context(|| format!("writing exit file: {:?}", exit_file))?;
            info!(vm_id = %vm_id, exit_code = %code_str, "Container exit notification received");
            // Exit loop after receiving exit code
            break;
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
/// Returns collected output lines as Vec<(stream, line)>.
async fn run_output_listener(socket_path: &str, vm_id: &str) -> Result<Vec<(String, String)>> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixListener;

    // Remove stale socket if it exists
    let _ = std::fs::remove_file(socket_path);

    let listener = UnixListener::bind(socket_path)
        .with_context(|| format!("binding output listener to {}", socket_path))?;

    // Make socket accessible by Firecracker
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(socket_path, std::fs::Permissions::from_mode(0o777))
        .with_context(|| format!("chmod output socket {}", socket_path))?;

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

    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut line_buf = String::new();

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
                    // Print to host's stderr with prefix (using tracing)
                    eprintln!("[ctr:{}] {}", stream, content);
                    output_lines.push((stream.to_string(), content.to_string()));

                    // Send ack back (bidirectional)
                    let _ = writer.write_all(b"ack\n").await;
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

    // Get kernel, rootfs, and initrd paths
    // With --kernel: use custom kernel (for inception with KVM-enabled kernel)
    // With --setup: create if missing; without: fail if missing
    let kernel_path = if let Some(custom_kernel) = &args.kernel {
        let path = PathBuf::from(custom_kernel);
        if !path.exists() {
            bail!("Custom kernel not found: {}", path.display());
        }
        info!(kernel = %path.display(), "using custom kernel");
        path
    } else {
        crate::setup::ensure_kernel(args.setup)
            .await
            .context("setting up kernel")?
    };
    let base_rootfs = crate::setup::ensure_rootfs(args.setup)
        .await
        .context("setting up rootfs")?;
    let initrd_path = crate::setup::ensure_fc_agent_initrd(args.setup)
        .await
        .context("setting up fc-agent initrd")?;

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

    // For localhost/ images, use content-addressable cache for skopeo export
    // This avoids lock contention when multiple VMs export the same image
    let _image_export_dir = if args.image.starts_with("localhost/") {
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
        let manifest_path = cache_dir.join("manifest.json");
        if !manifest_path.exists() {
            info!(image = %args.image, digest = %digest, "Exporting localhost image with skopeo");

            // Create cache dir
            tokio::fs::create_dir_all(&cache_dir)
                .await
                .context("creating image cache directory")?;

            let output = tokio::process::Command::new("skopeo")
                .arg("copy")
                .arg(format!("containers-storage:{}", args.image))
                .arg(format!("dir:{}", cache_dir.display()))
                .output()
                .await
                .context("running skopeo copy")?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                // Clean up partial export
                let _ = tokio::fs::remove_dir_all(&cache_dir).await;
                drop(lock_file); // Release lock before bailing
                bail!(
                    "Failed to export image '{}' with skopeo: {}",
                    args.image,
                    stderr
                );
            }

            info!(dir = %cache_dir.display(), "Image exported to OCI directory");
        } else {
            info!(image = %args.image, digest = %digest, "Using cached image export");
        }

        // Lock released when lock_file is dropped
        drop(lock_file);

        // Add the cached image directory as a read-only volume mount
        volume_mappings.push(VolumeMapping {
            host_path: cache_dir.clone(),
            guest_path: "/tmp/fcvm-image".to_string(),
            read_only: true,
        });

        Some(cache_dir)
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

    // Parse optional container command using shell-like semantics
    let cmd_args = if let Some(cmd) = &args.cmd {
        Some(shell_words::split(cmd).with_context(|| format!("parsing --cmd argument: {}", cmd))?)
    } else {
        None
    };

    // Setup paths
    let data_dir = paths::vm_runtime_dir(&vm_id);
    tokio::fs::create_dir_all(&data_dir)
        .await
        .context("creating VM data directory")?;

    // For rootless mode, make directory world-writable so processes inside the user
    // namespace can create sockets. User namespace UID 0 maps to subordinate UID
    // (typically 100000+), which doesn't match the directory owner (UID 1000).
    if matches!(args.network, NetworkMode::Rootless) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&data_dir, std::fs::Permissions::from_mode(0o777))
            .context("setting directory permissions for rootless mode")?;
    }

    let socket_path = data_dir.join("firecracker.sock");

    // Create VM state
    let mut vm_state = VmState::new(vm_id.clone(), args.image.clone(), args.cpu, args.mem);
    vm_state.name = Some(vm_name.clone());
    vm_state.config.env = args.env.clone();
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
    let vsock_socket_path = data_dir.join("vsock.sock");

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

    // Start status channel listener for fc-agent notifications
    // - "ready" on port 4999 -> creates container-ready file for health check
    // - "exit:{code}" on port 4999 -> creates container-exit file with exit code
    let status_socket_path = format!("{}_{}", vsock_socket_path.display(), VSOCK_STATUS_PORT);
    let status_handle = {
        let runtime_dir = data_dir.clone();
        let socket_path = status_socket_path.clone();
        let vm_id_clone = vm_id.clone();
        tokio::spawn(async move {
            if let Err(e) = run_status_listener(&socket_path, &runtime_dir, &vm_id_clone).await {
                tracing::warn!("Status listener error: {}", e);
            }
        })
    };

    // Start bidirectional output listener for container stdout/stderr
    // Port 4997 receives JSON lines: {"stream":"stdout|stderr","line":"..."}
    let output_socket_path = format!("{}_{}", vsock_socket_path.display(), VSOCK_OUTPUT_PORT);
    let _output_handle = {
        let socket_path = output_socket_path.clone();
        let vm_id_clone = vm_id.clone();
        tokio::spawn(async move {
            match run_output_listener(&socket_path, &vm_id_clone).await {
                Ok(lines) => lines,
                Err(e) => {
                    tracing::warn!("Output listener error: {}", e);
                    Vec::new()
                }
            }
        })
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

    // Wait for signal or VM exit
    let container_exit_code: Option<i32>;
    tokio::select! {
        _ = sigterm.recv() => {
            info!("received SIGTERM, shutting down VM");
            container_exit_code = None; // Signal-based shutdown, not container exit
        }
        _ = sigint.recv() => {
            info!("received SIGINT, shutting down VM");
            container_exit_code = None; // Signal-based shutdown, not container exit
        }
        status = vm_manager.wait() => {
            info!(status = ?status, "VM exited");
            // Read container exit code from file written by status listener
            let exit_file = data_dir.join("container-exit");
            container_exit_code = std::fs::read_to_string(&exit_file)
                .ok()
                .and_then(|s| s.trim().parse::<i32>().ok());
            info!(container_exit_code = ?container_exit_code, "container exit code");
        }
    }

    // Cancel status listener (podman-specific)
    status_handle.abort();

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
) -> Result<(VmManager, Option<tokio::process::Child>)> {
    // Setup storage - just need CoW copy (fc-agent is injected via initrd at boot)
    let vm_dir = data_dir.join("disks");
    let disk_manager =
        DiskManager::new(vm_id.to_string(), base_rootfs.to_path_buf(), vm_dir.clone());

    let rootfs_path = disk_manager
        .create_cow_disk()
        .await
        .context("creating CoW disk")?;

    // For rootless mode, make disk directory and file world-accessible
    // Firecracker runs as UID 100000+ inside namespace, can't access UID 1000 files
    if network.as_any().downcast_ref::<SlirpNetwork>().is_some() {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&vm_dir, std::fs::Permissions::from_mode(0o777))
            .context("setting disk directory permissions for rootless mode")?;
        std::fs::set_permissions(&rootfs_path, std::fs::Permissions::from_mode(0o666))
            .context("setting disk file permissions for rootless mode")?;
    }

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
        // Retry for up to 2 seconds if holder dies (transient failures under load)
        let holder_cmd = slirp_net.build_holder_command();
        info!(cmd = ?holder_cmd, "spawning namespace holder for rootless networking");

        let retry_deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
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

            // Give holder a moment to potentially fail, then check status
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;

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
                    debug!(holder_pid = holder_pid, "holder still running after 50ms");
                }
                Err(e) => {
                    warn!(holder_pid = holder_pid, error = ?e, "failed to check holder status");
                }
            }

            // Additional delay for namespace setup
            // The --map-root-user option invokes setuid helpers asynchronously
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;

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

    // Boot source with network configuration via kernel cmdline
    // The rootfs is a raw disk with partitions, root=/dev/vda1 specifies partition 1
    // Format: ip=<client-ip>:<server-ip>:<gw-ip>:<netmask>:<hostname>:<device>:<autoconf>:<dns0>
    // Example: ip=172.16.0.2::172.16.0.1:255.255.255.252::eth0:off:172.16.0.1
    let mut boot_args = if let (Some(guest_ip), Some(host_ip)) =
        (&network_config.guest_ip, &network_config.host_ip)
    {
        // Extract just the IP without CIDR notation if present
        let guest_ip_clean = guest_ip.split('/').next().unwrap_or(guest_ip);
        let host_ip_clean = host_ip.split('/').next().unwrap_or(host_ip);

        // Always pass DNS in boot args - gateway IP where dnsmasq listens
        // This avoids relying on NAT to reach external DNS (8.8.8.8)
        let dns_suffix = network_config
            .dns_server
            .as_ref()
            .map(|dns| format!(":{}", dns))
            .unwrap_or_default();

        // Format: ip=<client>:<server>:<gw>:<netmask>:<hostname>:<device>:<autoconf>[:<dns0>]
        // root=/dev/vda - the disk IS the ext4 filesystem (no partition table)
        format!(
            "console=ttyS0 reboot=k panic=1 pci=off random.trust_cpu=1 systemd.log_color=no root=/dev/vda rw ip={}::{}:255.255.255.252::eth0:off{}",
            guest_ip_clean, host_ip_clean, dns_suffix
        )
    } else {
        // No network config - used for basic boot (e.g., during setup)
        "console=ttyS0 reboot=k panic=1 pci=off random.trust_cpu=1 systemd.log_color=no root=/dev/vda rw".to_string()
    };

    // Enable fc-agent strace debugging if requested
    if args.strace_agent {
        boot_args.push_str(" fc_agent_strace=1");
        info!("fc-agent strace debugging enabled - output will be in /tmp/fc-agent.strace");
    }

    // Nested virtualization boot parameters for ARM64.
    // When HAS_EL2 is enabled, the guest kernel sees EL2 as available.
    // These parameters help ensure proper initialization:
    //
    // 1. id_aa64mmfr1.vh=0 - Override VHE detection to prevent VHE mode usage
    //    See: https://lore.kernel.org/linux-arm-kernel/20201228104958.1848833-13-maz@kernel.org/
    //
    // 2. kvm-arm.mode=nvhe - Force guest KVM to use nVHE mode
    //    This is the proper mode for L1 guests running nested VMs
    //
    // 3. numa=off - Disable NUMA to avoid percpu allocation issues
    //    The percpu allocator can fail with "cpu has no node" errors in nested contexts
    boot_args.push_str(" id_aa64mmfr1.vh=0 kvm-arm.mode=nvhe numa=off");

    client
        .set_boot_source(crate::firecracker::api::BootSource {
            kernel_image_path: kernel_path.display().to_string(),
            initrd_path: Some(initrd_path.display().to_string()),
            boot_args: Some(boot_args),
        })
        .await?;

    // Machine config
    client
        .set_machine_config(crate::firecracker::api::MachineConfig {
            vcpu_count: args.cpu,
            mem_size_mib: args.mem,
            smt: Some(false),
            cpu_template: None,
            track_dirty_pages: Some(true), // Enable snapshot support
        })
        .await?;

    // Root drive
    client
        .add_drive(
            "rootfs",
            crate::firecracker::api::Drive {
                drive_id: "rootfs".to_string(),
                path_on_host: rootfs_path.display().to_string(),
                is_root_device: true,
                is_read_only: false,
                partuuid: None,
                rate_limiter: None,
            },
        )
        .await?;

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
                "image_dir": if args.image.starts_with("localhost/") { Some("/tmp/fcvm-image") } else { None },
                "privileged": args.privileged,
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
