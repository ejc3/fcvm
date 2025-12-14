use anyhow::{bail, Context, Result};
use std::path::PathBuf;
use tokio::signal::unix::{signal, SignalKind};
use tracing::{debug, info, warn};

use crate::cli::{NetworkMode, PodmanArgs, PodmanCommands, RunArgs};
use crate::firecracker::VmManager;
use crate::network::{BridgedNetwork, NetworkManager, PortMapping, SlirpNetwork};
use crate::paths;
use crate::state::{generate_vm_id, truncate_id, validate_vm_name, StateManager, VmState};
use crate::storage::DiskManager;
use crate::volume::{VolumeConfig, VolumeServer};

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

/// Vsock base port for volume servers
const VSOCK_VOLUME_PORT_BASE: u32 = 5000;

/// Vsock port for status channel (fc-agent notifies when container starts)
const VSOCK_STATUS_PORT: u32 = 4999;

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

async fn cmd_podman_run(args: RunArgs) -> Result<()> {
    info!("Starting fcvm podman run");

    // Validate VM name before any setup work
    validate_vm_name(&args.name).context("invalid VM name")?;

    // Ensure kernel and rootfs exist (auto-setup on first run)
    let kernel_path = crate::setup::ensure_kernel()
        .await
        .context("setting up kernel")?;
    let base_rootfs = crate::setup::ensure_rootfs()
        .await
        .context("setting up rootfs")?;

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

    // For localhost/ images, use skopeo to copy image to a directory
    // The guest will use skopeo to import it into local storage
    let _image_export_dir = if args.image.starts_with("localhost/") {
        let image_dir = paths::vm_runtime_dir(&vm_id).join("image-export");
        tokio::fs::create_dir_all(&image_dir)
            .await
            .context("creating image export directory")?;

        info!(image = %args.image, "Exporting localhost image with skopeo");

        let output = tokio::process::Command::new("skopeo")
            .arg("copy")
            .arg(format!("containers-storage:{}", args.image))
            .arg(format!("dir:{}", image_dir.display()))
            .output()
            .await
            .context("running skopeo copy")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!(
                "Failed to export image '{}' with skopeo: {}",
                args.image,
                stderr
            );
        }

        info!(dir = %image_dir.display(), "Image exported to OCI directory");

        // Add the image directory as a read-only volume mount
        volume_mappings.push(VolumeMapping {
            host_path: image_dir.clone(),
            guest_path: "/tmp/fcvm-image".to_string(),
            read_only: true,
        });

        Some(image_dir)
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

            // Auto-generate health check URL using loopback IP
            // Port 8080 on host forwards to port 80 in guest (unprivileged, fully rootless)
            if vm_state.config.health_check_url.is_none() {
                vm_state.config.health_check_url = Some(format!("http://{}:8080/", loopback_ip));
            }

            Box::new(
                SlirpNetwork::new(vm_id.clone(), tap_device.clone(), port_mappings.clone())
                    .with_loopback_ip(loopback_ip),
            )
        }
    };

    let network_config = network.setup().await.context("setting up network")?;

    // For bridged mode, auto-generate health check URL using guest IP
    // This ensures HTTP health checks work (not just container-ready file)
    if matches!(args.network, NetworkMode::Bridged) && vm_state.config.health_check_url.is_none() {
        if let Some(ref guest_ip) = network_config.guest_ip {
            vm_state.config.health_check_url = Some(format!("http://{}:80/", guest_ip));
            // Store the health_check_port for health monitor to use with interface binding
            vm_state.config.network.health_check_port = Some(80);
        }
    }

    info!(tap = %network_config.tap_device, mac = %network_config.guest_mac, "network configured");

    // Generate vsock socket base path for volume servers
    // Firecracker binds to vsock.sock, VolumeServers listen on vsock.sock_{port}
    let vsock_socket_path = data_dir.join("vsock.sock");

    // Start VolumeServers BEFORE the VM so the sockets are ready when guest boots
    // Each VolumeServer listens on vsock.sock_{port} (e.g., vsock.sock_5000)
    // Firecracker binds to vsock.sock and routes guest connections to the per-port sockets
    let mut volume_server_handles = Vec::new();
    for (idx, vol) in volume_mappings.iter().enumerate() {
        let port = VSOCK_VOLUME_PORT_BASE + idx as u32;
        let config = VolumeConfig {
            host_path: vol.host_path.clone(),
            guest_path: vol.guest_path.clone().into(),
            read_only: vol.read_only,
            port,
        };

        let server = VolumeServer::new(config)
            .with_context(|| format!("creating VolumeServer for {}", vol.host_path.display()))?;

        let vsock_path = vsock_socket_path.clone();
        let handle = tokio::spawn(async move {
            if let Err(e) = server.serve_vsock(&vsock_path).await {
                tracing::error!("VolumeServer error for port {}: {}", port, e);
            }
        });

        info!(
            port = port,
            host_path = %vol.host_path.display(),
            guest_path = %vol.guest_path,
            read_only = vol.read_only,
            "Started VolumeServer"
        );

        volume_server_handles.push(handle);
    }

    // Give VolumeServers time to bind to their sockets
    if !volume_mappings.is_empty() {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

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

    // Run the main VM setup in a helper to ensure cleanup on error
    let setup_result = run_vm_setup(
        &args,
        &vm_id,
        &data_dir,
        &base_rootfs,
        &socket_path,
        &kernel_path,
        &network_config,
        network.as_mut(),
        cmd_args,
        &state_manager,
        &mut vm_state,
        &volume_mappings,
        &vsock_socket_path,
    )
    .await;

    // If setup failed, cleanup network before propagating error
    if let Err(e) = setup_result {
        warn!("VM setup failed, cleaning up network resources");
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

    // Spawn health monitor task (store handle for cancellation)
    let health_monitor_handle = crate::health::spawn_health_monitor(vm_id.clone(), vm_state.pid);

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

    // Cleanup
    info!("cleaning up resources");

    // Cancel health monitor task first
    health_monitor_handle.abort();

    // Cancel status listener
    status_handle.abort();

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
    if let Err(e) = state_manager.delete_state(&vm_id).await {
        warn!("failed to delete state file: {}", e);
    }

    // Cleanup vsock socket
    let _ = std::fs::remove_file(&vsock_socket_path);

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
    network_config: &crate::network::NetworkConfig,
    network: &mut dyn NetworkManager,
    cmd_args: Option<Vec<String>>,
    state_manager: &StateManager,
    vm_state: &mut VmState,
    volume_mappings: &[VolumeMapping],
    vsock_socket_path: &std::path::Path,
) -> Result<(VmManager, Option<tokio::process::Child>)> {
    // Setup storage
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

    info!(rootfs = %rootfs_path.display(), "disk prepared");

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
        let holder_cmd = slirp_net.build_holder_command();
        info!(cmd = ?holder_cmd, "spawning namespace holder for rootless networking");

        // Spawn holder with piped stderr to capture errors if it fails
        let mut child = tokio::process::Command::new(&holder_cmd[0])
            .args(&holder_cmd[1..])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .with_context(|| format!("failed to spawn holder: {:?}", holder_cmd))?;

        let holder_pid = child.id().context("getting holder process PID")?;
        info!(holder_pid = holder_pid, "namespace holder started");

        // Give holder a moment to potentially fail, then check status
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        match child.try_wait() {
            Ok(Some(status)) => {
                // Holder exited - capture stderr to see why
                let stderr = if let Some(mut stderr_pipe) = child.stderr.take() {
                    use tokio::io::AsyncReadExt;
                    let mut buf = String::new();
                    let _ = stderr_pipe.read_to_string(&mut buf).await;
                    buf
                } else {
                    String::new()
                };
                bail!(
                    "holder process exited immediately: status={}, stderr={}, cmd={:?}",
                    status,
                    stderr.trim(),
                    holder_cmd
                );
            }
            Ok(None) => {
                debug!(holder_pid = holder_pid, "holder still running after 50ms");
                // Holder is running - drop the stderr pipe so it doesn't block
                drop(child.stderr.take());
            }
            Err(e) => {
                warn!(holder_pid = holder_pid, error = ?e, "failed to check holder status");
            }
        }

        // Additional delay for namespace setup (already waited 50ms above)
        // The --map-auto option invokes setuid helpers asynchronously
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Step 2: Run setup script via nsenter (creates TAPs, iptables, etc.)
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
            // Kill holder before bailing
            let _ = child.kill().await;
            // Re-check state for diagnostics
            let holder_alive = std::path::Path::new(&proc_dir).exists();
            let ns_user_exists = std::path::Path::new(&ns_user).exists();
            let ns_net_exists = std::path::Path::new(&ns_net).exists();

            // Log comprehensive error info at ERROR level (always visible)
            warn!(
                holder_pid = holder_pid,
                holder_alive = holder_alive,
                tun_exists = tun_exists,
                ns_user_exists = ns_user_exists,
                ns_net_exists = ns_net_exists,
                stderr = %stderr.trim(),
                stdout = %stdout.trim(),
                "network setup failed - diagnostics"
            );

            bail!(
                "network setup failed: {} (tun={}, holder_alive={}, ns_user={}, ns_net={})",
                stderr.trim(),
                tun_exists,
                holder_alive,
                ns_user_exists,
                ns_net_exists
            );
        }

        info!(holder_pid = holder_pid, "network setup complete");

        // Step 3: Set holder_pid so VmManager uses nsenter
        vm_manager.set_holder_pid(holder_pid);

        // Store holder_pid in state for health checks and cleanup
        vm_state.holder_pid = Some(holder_pid);

        holder_child = Some(child);
    } else {
        holder_child = None;
    }

    let firecracker_bin = PathBuf::from("/usr/local/bin/firecracker");

    vm_manager
        .start(&firecracker_bin, None)
        .await
        .context("starting Firecracker")?;

    let vm_pid = vm_manager.pid()?;
    let client = vm_manager.client()?;

    // Configure VM via API
    info!("configuring VM via Firecracker API");

    // Boot source with network configuration via kernel cmdline
    // Format: ip=<client-ip>:<server-ip>:<gw-ip>:<netmask>:<hostname>:<device>:<autoconf>
    // Example: ip=172.16.0.2::172.16.0.1:255.255.255.252::eth0:off
    let boot_args = if let (Some(guest_ip), Some(host_ip)) =
        (&network_config.guest_ip, &network_config.host_ip)
    {
        // Extract just the IP without CIDR notation if present
        let guest_ip_clean = guest_ip.split('/').next().unwrap_or(guest_ip);
        let host_ip_clean = host_ip.split('/').next().unwrap_or(host_ip);

        format!(
            "console=ttyS0 reboot=k panic=1 pci=off random.trust_cpu=1 systemd.log_color=no ip={}::{}:255.255.255.252::eth0:off",
            guest_ip_clean, host_ip_clean
        )
    } else {
        "console=ttyS0 reboot=k panic=1 pci=off random.trust_cpu=1 systemd.log_color=no".to_string()
    };

    client
        .set_boot_source(crate::firecracker::api::BootSource {
            kernel_image_path: kernel_path.display().to_string(),
            initrd_path: None,
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
