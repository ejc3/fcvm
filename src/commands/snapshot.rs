use anyhow::{bail, Context, Result};
use std::path::PathBuf;
use std::time::Duration;
use tokio::signal::unix::{signal, SignalKind};
use tracing::{debug, info, warn};

use crate::cli::{
    NetworkMode, SnapshotArgs, SnapshotCommands, SnapshotCreateArgs, SnapshotRunArgs,
    SnapshotServeArgs,
};
use crate::firecracker::VmManager;
use crate::network::{BridgedNetwork, NetworkManager, PortMapping, SlirpNetwork};
use crate::paths;
use crate::state::{
    generate_vm_id, truncate_id, validate_vm_name, StateManager, VmState, VmStatus,
};
use crate::storage::{DiskManager, SnapshotManager};
use crate::uffd::UffdServer;
use crate::volume::{spawn_volume_servers, VolumeConfig};

/// Main dispatcher for snapshot commands
pub async fn cmd_snapshot(args: SnapshotArgs) -> Result<()> {
    match args.cmd {
        SnapshotCommands::Create(create_args) => cmd_snapshot_create(create_args).await,
        SnapshotCommands::Serve(serve_args) => cmd_snapshot_serve(serve_args).await,
        SnapshotCommands::Run(run_args) => cmd_snapshot_run(run_args).await,
        SnapshotCommands::Ls => cmd_snapshot_ls().await,
    }
}

/// Create snapshot from running VM
async fn cmd_snapshot_create(args: SnapshotCreateArgs) -> Result<()> {
    // Determine which VM to snapshot
    let state_manager = StateManager::new(paths::state_dir());

    let vm_state = if let Some(name) = &args.name {
        info!("Creating snapshot from VM: {}", name);
        state_manager
            .load_state_by_name(name)
            .await
            .context("loading VM state by name")?
    } else if let Some(pid) = args.pid {
        info!("Creating snapshot from VM with PID: {}", pid);
        state_manager
            .load_state_by_pid(pid)
            .await
            .context("loading VM state by PID")?
    } else {
        anyhow::bail!("Either --name or --pid must be specified");
    };

    // Block snapshots when VM has read-write extra disks
    let rw_disks: Vec<_> = vm_state
        .config
        .extra_disks
        .iter()
        .filter(|d| !d.read_only)
        .collect();
    if !rw_disks.is_empty() {
        anyhow::bail!(
            "Cannot create snapshot: VM has {} read-write extra disk(s). \
             Use :ro suffix for disks that should be included in snapshots.",
            rw_disks.len()
        );
    }

    let snapshot_name = args.tag.unwrap_or_else(|| {
        vm_state
            .name
            .clone()
            .unwrap_or_else(|| truncate_id(&vm_state.vm_id, 8).to_string())
    });

    // Connect to running VM
    let socket_path = paths::vm_runtime_dir(&vm_state.vm_id).join("firecracker.sock");

    // Check if socket exists
    if !socket_path.exists() {
        anyhow::bail!(
            "VM socket not found - VM may not be running: {}",
            socket_path.display()
        );
    }

    // Create client directly for existing VM
    use crate::firecracker::FirecrackerClient;
    let client = FirecrackerClient::new(socket_path)?;

    // Create snapshot paths
    let snapshot_dir = paths::snapshot_dir().join(&snapshot_name);
    tokio::fs::create_dir_all(&snapshot_dir)
        .await
        .context("creating snapshot directory")?;

    let memory_path = snapshot_dir.join("memory.bin");
    let vmstate_path = snapshot_dir.join("vmstate.bin");
    let disk_path = snapshot_dir.join("disk.raw");

    // Pause VM before snapshotting (required by Firecracker)
    info!("Pausing VM before snapshot");

    use crate::firecracker::api::VmState as ApiVmState;
    client
        .patch_vm_state(ApiVmState {
            state: "Paused".to_string(),
        })
        .await
        .context("pausing VM")?;

    info!("VM paused successfully");

    let snapshot_result = async {
        // Create snapshot via Firecracker API
        info!("Creating Firecracker snapshot");
        use crate::firecracker::api::SnapshotCreate;

        client
            .create_snapshot(SnapshotCreate {
                snapshot_type: Some("Full".to_string()),
                snapshot_path: vmstate_path.display().to_string(),
                mem_file_path: memory_path.display().to_string(),
            })
            .await
            .context("creating Firecracker snapshot")?;

        // Copy the VM's disk to snapshot directory using reflink (instant CoW copy)
        // REQUIRES btrfs filesystem - no fallback to regular copy
        info!("Copying VM disk to snapshot directory");
        let vm_disk_path = paths::vm_runtime_dir(&vm_state.vm_id).join("disks/rootfs.raw");

        if vm_disk_path.exists() {
            // Use cp --reflink=always for instant CoW copy on btrfs
            let output = tokio::process::Command::new("cp")
                .arg("--reflink=always")
                .arg(&vm_disk_path)
                .arg(&disk_path)
                .output()
                .await
                .context("executing cp command")?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                anyhow::bail!(
                    "Failed to create reflink copy. Ensure /mnt/fcvm-btrfs is a btrfs filesystem. Error: {}",
                    stderr
                );
            }
            info!(
                source = %vm_disk_path.display(),
                dest = %disk_path.display(),
                "VM disk copied to snapshot using reflink"
            );
        } else {
            anyhow::bail!("VM disk not found at {}", vm_disk_path.display());
        }

        // Save snapshot metadata
        use crate::storage::snapshot::{SnapshotConfig, SnapshotMetadata, SnapshotVolumeConfig};

        // Parse volume configs from VM state (format: HOST:GUEST[:ro])
        use super::common::VSOCK_VOLUME_PORT_BASE;
        let volume_configs: Vec<SnapshotVolumeConfig> = vm_state.config.volumes
            .iter()
            .enumerate()
            .filter_map(|(idx, spec)| {
                let parts: Vec<&str> = spec.split(':').collect();
                if parts.len() >= 2 {
                    Some(SnapshotVolumeConfig {
                        host_path: PathBuf::from(parts[0]),
                        guest_path: parts[1].to_string(),
                        read_only: parts.get(2).map(|s| *s == "ro").unwrap_or(false),
                        vsock_port: VSOCK_VOLUME_PORT_BASE + idx as u32,
                    })
                } else {
                    warn!("Invalid volume spec in VM state: {}", spec);
                    None
                }
            })
            .collect();

        if !volume_configs.is_empty() {
            info!(
                num_volumes = volume_configs.len(),
                "saving {} volume config(s) to snapshot metadata",
                volume_configs.len()
            );
        }

        let snapshot_config = SnapshotConfig {
            name: snapshot_name.clone(),
            vm_id: vm_state.vm_id.clone(),
            memory_path: memory_path.clone(),
            vmstate_path: vmstate_path.clone(),
            disk_path: disk_path.clone(),
            created_at: chrono::Utc::now(),
            metadata: SnapshotMetadata {
                image: vm_state.config.image.clone(),
                vcpu: vm_state.config.vcpu,
                memory_mib: vm_state.config.memory_mib,
                network_config: vm_state.config.network.clone(),
                volumes: volume_configs,
            },
        };

        let snapshot_manager = SnapshotManager::new(paths::snapshot_dir());
        snapshot_manager
            .save_snapshot(snapshot_config.clone())
            .await
            .context("saving snapshot metadata")?;

        info!(
            snapshot = %snapshot_name,
            mem_size = snapshot_config.metadata.memory_mib,
            "snapshot created successfully"
        );

        let vm_name = vm_state.name.as_deref().unwrap_or(truncate_id(&vm_state.vm_id, 8));
        println!(
            "✓ Snapshot '{}' created from VM '{}'",
            snapshot_name, vm_name
        );
        println!("  Memory: {} MB", snapshot_config.metadata.memory_mib);
        println!("  Files:");
        println!("    {}", snapshot_config.memory_path.display());
        println!("    {}", snapshot_config.disk_path.display());
        println!(
            "\nOriginal VM '{}' has been resumed and is still running.",
            vm_name
        );

        Ok::<_, anyhow::Error>(())
    }
    .await;

    // Resume the original VM after snapshotting regardless of snapshot result
    info!("Resuming original VM");
    let resume_result = client
        .patch_vm_state(ApiVmState {
            state: "Resumed".to_string(),
        })
        .await;

    if let Err(e) = resume_result {
        let vm_name = vm_state
            .name
            .as_deref()
            .unwrap_or(truncate_id(&vm_state.vm_id, 8));
        warn!(
            error = %e,
            vm = %vm_name,
            "failed to resume VM after snapshot"
        );
        if snapshot_result.is_ok() {
            return Err(e);
        }
    } else {
        info!("Original VM resumed successfully");
    }

    snapshot_result
}

/// Serve snapshot memory (foreground)
async fn cmd_snapshot_serve(args: SnapshotServeArgs) -> Result<()> {
    info!(
        "Starting memory server for snapshot: {}",
        args.snapshot_name
    );

    // Load snapshot configuration
    let snapshot_manager = SnapshotManager::new(paths::snapshot_dir());
    let snapshot_config = snapshot_manager
        .load_snapshot(&args.snapshot_name)
        .await
        .context("loading snapshot configuration")?;

    info!(
        snapshot = %args.snapshot_name,
        mem_file = %snapshot_config.memory_path.display(),
        mem_size_mb = snapshot_config.metadata.memory_mib,
        "loaded snapshot configuration"
    );

    // Generate unique socket name with PID to allow multiple serves per snapshot
    let my_pid = std::process::id();
    let socket_path =
        paths::data_dir().join(format!("uffd-{}-{}.sock", args.snapshot_name, my_pid));

    // Create UFFD server with custom socket path
    let server = UffdServer::new_with_path(
        args.snapshot_name.clone(),
        &snapshot_config.memory_path,
        &socket_path,
    )
    .await
    .context("creating UFFD server")?;

    // Save serve state for tracking
    let serve_id = generate_vm_id();
    let mut serve_state = VmState::new(serve_id.clone(), "".to_string(), 0, 0);
    serve_state.pid = Some(my_pid);
    serve_state.config.snapshot_name = Some(args.snapshot_name.clone());
    serve_state.config.process_type = Some(crate::state::ProcessType::Serve);
    serve_state.status = VmStatus::Running;

    let state_manager = std::sync::Arc::new(StateManager::new(paths::state_dir()));
    state_manager.init().await?;
    state_manager
        .save_state(&serve_state)
        .await
        .context("saving serve state")?;

    info!(
        serve_id = %serve_id,
        pid = my_pid,
        "serve state saved"
    );

    println!("Serving snapshot: {}", args.snapshot_name);
    println!("  Serve PID: {}", my_pid);
    println!("  Socket: {}", socket_path.display());
    println!("  Memory: {} MB", snapshot_config.metadata.memory_mib);
    println!("  Waiting for VMs to connect...");
    println!();
    println!("Clone VMs with: fcvm snapshot run --pid {}", my_pid);
    println!("Press Ctrl-C to stop");
    println!();

    // Setup signal handlers
    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;

    // Run server in background task
    let mut server_handle = tokio::spawn(async move { server.run().await });

    // Clone state_manager for signal handler use
    let state_manager_for_signal = state_manager.clone();

    // Wait for signal or server exit
    // First Ctrl-C warns about clones, second one shuts down
    let mut shutdown_requested = false;
    let mut confirm_deadline: Option<tokio::time::Instant> = None;
    loop {
        let timeout = if let Some(deadline) = confirm_deadline {
            tokio::time::sleep_until(deadline)
        } else {
            // Far future - effectively disabled
            tokio::time::sleep(std::time::Duration::from_secs(86400))
        };

        tokio::select! {
            biased;

            _ = sigterm.recv() => {
                info!("received SIGTERM");
                break;
            }
            _ = sigint.recv() => {
                info!("received SIGINT");
                if shutdown_requested {
                    // Second Ctrl-C - force shutdown
                    info!("received second SIGINT, forcing shutdown");
                    println!("\nForcing shutdown...");
                    break;
                }

                // First Ctrl-C - check for running clones
                let all_vms: Vec<crate::state::VmState> = state_manager_for_signal.list_vms().await?;
                let running_clones: Vec<crate::state::VmState> = all_vms
                    .into_iter()
                    .filter(|vm| vm.config.serve_pid == Some(my_pid))
                    .filter(|vm| vm.pid.map(crate::utils::is_process_alive).unwrap_or(false))
                    .collect();

                if running_clones.is_empty() {
                    println!("\nNo running clones, shutting down...");
                    break;
                } else {
                    println!("\n⚠️  {} clone(s) still running!", running_clones.len());
                    for clone in &running_clones {
                        if let Some(pid) = clone.pid {
                            let name = clone.name.as_deref().unwrap_or(&clone.vm_id);
                            println!("   - {} (PID {})", name, pid);
                        }
                    }
                    println!("\nPress Ctrl-C again within 3 seconds to kill clones and shut down...");
                    shutdown_requested = true;
                    confirm_deadline = Some(tokio::time::Instant::now() + std::time::Duration::from_secs(3));
                }
            }
            _ = timeout, if shutdown_requested => {
                println!("Timeout expired, continuing to serve...");
                shutdown_requested = false;
                confirm_deadline = None;
            }
            result = &mut server_handle => {
                info!("server exited: {:?}", result);
                break;
            }
        }
    }

    println!("\nShutting down memory server...");

    // Cleanup: Kill all clones that connected to THIS serve
    info!("cleaning up clones connected to serve PID {}", my_pid);
    let all_vms = state_manager.list_vms().await?;
    let my_clones: Vec<_> = all_vms
        .into_iter()
        .filter(|vm| vm.config.serve_pid == Some(my_pid))
        .collect();

    if !my_clones.is_empty() {
        println!("Killing {} clone(s)...", my_clones.len());
        for clone in my_clones {
            if let Some(pid) = clone.pid {
                info!(
                    "killing clone {} (PID {})",
                    truncate_id(&clone.vm_id, 8),
                    pid
                );
                // Kill clone process
                use std::process::Command;
                match Command::new("kill")
                    .arg("-TERM")
                    .arg(pid.to_string())
                    .status()
                {
                    Ok(status) if status.success() => {
                        info!("successfully killed clone PID {}", pid);
                    }
                    Ok(status) => {
                        warn!(
                            "kill command returned non-zero exit for PID {}: {:?}",
                            pid,
                            status.code()
                        );
                    }
                    Err(e) => {
                        warn!("failed to kill clone PID {}: {}", pid, e);
                    }
                }
            }
        }
    }

    // Clean up socket file
    if let Err(e) = std::fs::remove_file(&socket_path) {
        warn!(
            "failed to remove socket file {}: {}",
            socket_path.display(),
            e
        );
    } else {
        info!("removed socket file: {}", socket_path.display());
    }

    // Delete serve state
    if let Err(e) = state_manager.delete_state(&serve_id).await {
        warn!("failed to delete serve state {}: {}", serve_id, e);
    } else {
        info!("deleted serve state");
    }

    // Delete snapshot directory (memory.bin, disk.raw, vmstate.bin, config.json)
    let snapshot_dir = paths::snapshot_dir().join(&args.snapshot_name);
    if snapshot_dir.exists() {
        println!("Cleaning up snapshot directory...");
        if let Err(e) = std::fs::remove_dir_all(&snapshot_dir) {
            warn!(
                "failed to remove snapshot directory {}: {}",
                snapshot_dir.display(),
                e
            );
        } else {
            info!("removed snapshot directory: {}", snapshot_dir.display());
        }
    }

    println!("Memory server stopped");

    Ok(())
}

/// Run clone from snapshot
async fn cmd_snapshot_run(args: SnapshotRunArgs) -> Result<()> {
    // Verify the serve process is actually alive before attempting any work
    // This prevents wasted setup if the serve process died between state file creation and now
    if !crate::utils::is_process_alive(args.pid) {
        anyhow::bail!(
            "serve process (PID {}) is not running - start with 'fcvm snapshot serve'",
            args.pid
        );
    }

    // Load serve state by PID to get snapshot name
    let state_manager = StateManager::new(paths::state_dir());
    let serve_state = state_manager
        .load_state_by_pid(args.pid)
        .await
        .context("loading serve process state - is serve running?")?;

    // Get snapshot name from serve state
    let snapshot_name = serve_state
        .config
        .snapshot_name
        .ok_or_else(|| anyhow::anyhow!("serve process has no snapshot_name"))?;

    info!(
        "Cloning VM from serve PID {} (snapshot: {})",
        args.pid, snapshot_name
    );

    // Load snapshot configuration
    let snapshot_manager = SnapshotManager::new(paths::snapshot_dir());
    let snapshot_config = snapshot_manager
        .load_snapshot(&snapshot_name)
        .await
        .context("loading snapshot configuration")?;

    info!(
        snapshot = %snapshot_name,
        image = %snapshot_config.metadata.image,
        vcpu = snapshot_config.metadata.vcpu,
        mem_mib = snapshot_config.metadata.memory_mib,
        "loaded snapshot configuration"
    );

    // Generate VM ID and name
    let vm_id = generate_vm_id();
    let vm_name = args.name.unwrap_or_else(|| {
        // Auto-generate: snapshot-name + random suffix
        format!("{}-{}", snapshot_name, &vm_id[..6])
    });

    // Validate VM name (whether user-provided or auto-generated)
    validate_vm_name(&vm_name).context("invalid VM name")?;

    state_manager.init().await?;

    let mut vm_state = VmState::new(
        vm_id.clone(),
        snapshot_config.metadata.image.clone(),
        snapshot_config.metadata.vcpu,
        snapshot_config.metadata.memory_mib,
    );
    vm_state.name = Some(vm_name.clone());

    // Save snapshot tracking info in clone state
    vm_state.config.snapshot_name = Some(snapshot_name.clone());
    vm_state.config.process_type = Some(crate::state::ProcessType::Clone);
    vm_state.config.serve_pid = Some(args.pid); // Track which serve spawned us!

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

    // Build UFFD socket path for memory server
    // Note: We already verified the serve process is alive above.
    // We do NOT check socket existence here (TOCTOU race) - let the actual
    // connection attempt fail with a meaningful error instead.
    let uffd_socket = paths::data_dir().join(format!("uffd-{}-{}.sock", snapshot_name, args.pid));

    info!(
        uffd_socket = %uffd_socket.display(),
        serve_pid = args.pid,
        "connecting to memory server"
    );

    // Setup VolumeServers for clones if snapshot has volumes
    //
    // Mount namespace isolation for vsock:
    // - Firecracker's vmstate.bin stores the baseline's vsock uds_path
    // - Multiple clones from the same snapshot would all try to bind() to the same path
    // - This causes "Address in use" errors for all but the first clone
    //
    // Solution: Each clone's Firecracker runs in a mount namespace where the baseline's
    // runtime directory is bind-mounted over the clone's runtime directory.
    // - Firecracker thinks it's binding to /baseline_dir/vsock.sock
    // - But the bind mount redirects this to /clone_dir/vsock.sock
    // - Each clone has its own mount namespace, so each creates unique socket files
    // - VolumeServers listen on the clone's actual socket paths
    // Clone's vsock socket base path
    // With mount namespace isolation, Firecracker will create sockets here
    // (it thinks it's writing to baseline's path but bind mount redirects to clone's)
    let clone_vsock_base = data_dir.join("vsock.sock");

    // Build VolumeConfigs from snapshot metadata and spawn VolumeServers
    let volume_configs: Vec<VolumeConfig> = snapshot_config
        .metadata
        .volumes
        .iter()
        .map(|vol| VolumeConfig {
            host_path: vol.host_path.clone(),
            guest_path: vol.guest_path.clone().into(),
            read_only: vol.read_only,
            port: vol.vsock_port,
        })
        .collect();

    let volume_server_handles = spawn_volume_servers(&volume_configs, &clone_vsock_base)
        .await
        .context("spawning VolumeServers for clone")?;

    // Setup networking - use saved network config from snapshot
    let tap_device = format!("tap-{}", truncate_id(&vm_id, 8));
    let port_mappings: Vec<PortMapping> = args
        .publish
        .iter()
        .map(|s| PortMapping::parse(s))
        .collect::<Result<Vec<_>>>()
        .context("parsing port mappings")?;

    // Extract guest_ip from snapshot metadata for network config reuse
    let saved_network = &snapshot_config.metadata.network_config;

    // Bridged mode requires root for iptables and network namespace setup
    if matches!(args.network, NetworkMode::Bridged) && !nix::unistd::geteuid().is_root() {
        bail!(
            "Bridged networking requires root. Either:\n  \
             - Run with sudo: sudo fcvm snapshot run ...\n  \
             - Use rootless mode: fcvm snapshot run --network rootless ..."
        );
    }
    // Rootless with sudo is pointless - bridged would be faster
    if matches!(args.network, NetworkMode::Rootless) && nix::unistd::geteuid().is_root() {
        warn!(
            "Running rootless mode as root is unnecessary. \
             Consider using --network bridged for better performance."
        );
    }

    // Setup networking based on mode - reuse guest_ip from snapshot if available
    let mut network: Box<dyn NetworkManager> = match args.network {
        NetworkMode::Bridged => {
            let mut net =
                BridgedNetwork::new(vm_id.clone(), tap_device.clone(), port_mappings.clone());
            // If snapshot has saved network config with guest_ip, use it
            if let Some(ref guest_ip) = saved_network.guest_ip {
                net = net.with_guest_ip(guest_ip.clone());
                info!(
                    guest_ip = %guest_ip,
                    "clone will use same network config as snapshot"
                );
            }
            Box::new(net)
        }
        NetworkMode::Rootless => {
            // For rootless mode, allocate loopback IP atomically with state persistence
            // This prevents race conditions when starting multiple clones concurrently
            let loopback_ip = state_manager
                .allocate_loopback_ip(&mut vm_state)
                .await
                .context("allocating loopback IP")?;

            let mut net =
                SlirpNetwork::new(vm_id.clone(), tap_device.clone(), port_mappings.clone())
                    .with_loopback_ip(loopback_ip);
            // If snapshot has saved network config with guest_ip, use it
            // This is critical: clones restore with the baseline's IP configuration
            if let Some(ref guest_ip) = saved_network.guest_ip {
                net = net.with_guest_ip(guest_ip.clone());
                info!(
                    guest_ip = %guest_ip,
                    "clone will use same network config as snapshot"
                );
            }
            Box::new(net)
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

    info!(
        tap = %network_config.tap_device,
        mac = %network_config.guest_mac,
        "network configured for clone"
    );

    // Run clone setup in a helper to ensure cleanup on error
    let setup_result = run_clone_setup(
        &vm_id,
        &vm_name,
        &data_dir,
        &socket_path,
        &uffd_socket,
        &snapshot_config,
        &network_config,
        network.as_mut(),
        &state_manager,
        &mut vm_state,
    )
    .await;

    // If setup failed, cleanup all resources before propagating error
    if let Err(e) = setup_result {
        warn!("Clone setup failed, cleaning up resources");

        // Abort VolumeServer tasks
        for handle in volume_server_handles {
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

    let (mut vm_manager, mut holder_child) = setup_result.unwrap();

    info!(
        vm_id = %vm_id,
        vm_name = %vm_name,
        "VM cloned successfully with UFFD memory sharing!"
    );
    println!(
        "✓ VM '{}' cloned from snapshot '{}'",
        vm_name, snapshot_name
    );
    println!("  Memory pages shared via UFFD");
    println!("  Disk uses CoW overlay");

    // Handle --exec: run command in container then cleanup and exit
    if let Some(exec_cmd) = &args.exec {
        info!("executing command in clone: {}", exec_cmd);

        // Parse command using shell_words (same as --cmd in podman run)
        let cmd_args: Vec<String> = shell_words::split(exec_cmd)
            .with_context(|| format!("parsing --exec argument: {}", exec_cmd))?;

        // Wait for vsock socket to be ready (poll instead of blind sleep)
        let vsock_socket = data_dir.join("vsock.sock");
        let poll_start = std::time::Instant::now();
        const MAX_VSOCK_WAIT: Duration = Duration::from_millis(5000);
        const VSOCK_POLL_INTERVAL: Duration = Duration::from_millis(10);

        loop {
            if poll_start.elapsed() > MAX_VSOCK_WAIT {
                bail!("vsock socket not ready after {:?}", poll_start.elapsed());
            }

            // Check if socket exists and is connectable
            if vsock_socket.exists() {
                if let Ok(_stream) = std::os::unix::net::UnixStream::connect(&vsock_socket) {
                    debug!("vsock socket ready after {:?}", poll_start.elapsed());
                    break;
                }
            }

            tokio::time::sleep(VSOCK_POLL_INTERVAL).await;
        }
        let exit_code = crate::commands::exec::run_exec_in_vm(
            &vsock_socket,
            &cmd_args,
            true, // in_container
        )
        .await?;

        // Cleanup resources (exec path has no health monitor)
        info!("exec completed with exit code {}, cleaning up", exit_code);

        super::common::cleanup_vm(
            &vm_id,
            &mut vm_manager,
            &mut holder_child,
            volume_server_handles,
            network.as_mut(),
            &state_manager,
            &data_dir,
            None, // no health monitor in exec path
            None,
        )
        .await;

        // Return error if exec failed
        if exit_code != 0 {
            bail!("exec command exited with code {}", exit_code);
        }

        return Ok(());
    }

    // Create cancellation token for graceful health monitor shutdown
    let health_cancel_token = tokio_util::sync::CancellationToken::new();

    // Spawn health monitor task with cancellation support
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
    tokio::select! {
        _ = sigterm.recv() => {
            info!("received SIGTERM, shutting down VM");
        }
        _ = sigint.recv() => {
            info!("received SIGINT, shutting down VM");
        }
        status = vm_manager.wait() => {
            info!(status = ?status, "VM exited");
        }
    }

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

    Ok(())
}

/// List running snapshot servers
async fn cmd_snapshot_ls() -> Result<()> {
    let state_manager = StateManager::new(paths::state_dir());
    let all_vms = state_manager.list_vms().await?;

    // Filter to serve processes only
    let serves: Vec<_> = all_vms
        .iter()
        .filter(|vm| vm.config.process_type == Some(crate::state::ProcessType::Serve))
        .collect();

    if serves.is_empty() {
        println!("No snapshot servers running");
        return Ok(());
    }

    // Print header
    println!(
        "{:<12} {:<10} {:<12} {:<20} {:<8}",
        "SERVE_ID", "PID", "HEALTH", "SNAPSHOT", "CLONES"
    );

    // Print each serve with clone count
    for serve in serves {
        let serve_pid = serve.pid.unwrap_or(0);

        // Count clones connected to this serve
        let clone_count = all_vms
            .iter()
            .filter(|vm| vm.config.serve_pid == Some(serve_pid))
            .count();

        println!(
            "{:<12} {:<10} {:<12} {:<20} {:<8}",
            truncate_id(&serve.vm_id, 8),
            serve_pid,
            format!("{:?}", serve.health_status),
            serve.config.snapshot_name.as_deref().unwrap_or("-"),
            clone_count,
        );
    }

    Ok(())
}

/// Helper function that runs clone setup and returns VmManager on success.
/// This allows the caller to cleanup network resources on error.
/// For rootless mode, also returns the holder process that keeps the namespace alive.
#[allow(clippy::too_many_arguments)]
async fn run_clone_setup(
    vm_id: &str,
    vm_name: &str,
    data_dir: &std::path::Path,
    socket_path: &std::path::Path,
    uffd_socket: &std::path::Path,
    snapshot_config: &crate::storage::snapshot::SnapshotConfig,
    network_config: &crate::network::NetworkConfig,
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
    let rootfs_path: std::path::PathBuf;

    if let Some(bridged_net) = network.as_any().downcast_ref::<BridgedNetwork>() {
        if let Some(ns_id) = bridged_net.namespace_id() {
            info!(namespace = %ns_id, "configuring VM to run in network namespace");
            vm_manager.set_namespace(ns_id.to_string());
        }

        // For bridged mode, create disk sequentially (no parallelization benefit)
        let disk_manager = DiskManager::new(
            vm_id.to_string(),
            snapshot_config.disk_path.clone(),
            vm_dir.clone(),
        );

        rootfs_path = disk_manager
            .create_cow_disk()
            .await
            .context("creating CoW disk from snapshot")?;

        info!(
            rootfs = %rootfs_path.display(),
            snapshot_disk = %snapshot_config.disk_path.display(),
            "CoW disk prepared from snapshot"
        );
    } else if let Some(slirp_net) = network.as_any().downcast_ref::<SlirpNetwork>() {
        // Rootless mode: spawn holder process and set up namespace via nsenter
        // OPTIMIZATION: Parallelize disk creation with network setup

        // Step 1: Spawn holder process (keeps namespace alive)
        // Retry for up to 2 seconds if namespace doesn't become ready (race condition)
        let holder_cmd = slirp_net.build_holder_command();
        info!(cmd = ?holder_cmd, "spawning namespace holder for rootless networking");

        let retry_deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
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

            // Wait for namespace to be ready by testing nsenter directly
            // The holder runs: unshare -> write uid_map/gid_map -> exec sleep -> sleep syscall
            // setns() fails with EINVAL until this sequence completes, so we just retry.
            let ready_deadline_inner =
                std::time::Instant::now() + std::time::Duration::from_millis(500);
            let mut namespace_ready = false;
            loop {
                let probe = tokio::process::Command::new("nsenter")
                    .args([
                        "-t",
                        &holder_pid.to_string(),
                        "-U",
                        "-n",
                        "--preserve-credentials",
                        "--",
                        "true",
                    ])
                    .output()
                    .await;
                match probe {
                    Ok(output) if output.status.success() => {
                        debug!(
                            holder_pid = holder_pid,
                            "namespace ready (nsenter probe succeeded)"
                        );
                        namespace_ready = true;
                        break;
                    }
                    Ok(output) => {
                        let stderr = String::from_utf8_lossy(&output.stderr);
                        if !stderr.contains("Invalid argument") {
                            warn!(holder_pid = holder_pid, stderr = %stderr.trim(), "nsenter probe failed with unexpected error");
                            break;
                        }
                    }
                    Err(e) => {
                        warn!(holder_pid = holder_pid, error = %e, "nsenter probe spawn failed");
                        break;
                    }
                }
                if std::time::Instant::now() >= ready_deadline_inner {
                    warn!(holder_pid = holder_pid, "namespace not ready after 500ms");
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }

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
                bail!(
                    "namespace not ready after {} attempts (holder PID {})",
                    attempt,
                    holder_pid
                );
            }
        };

        // Step 2: Run disk creation and network setup IN PARALLEL
        // This saves ~16ms by overlapping these independent operations
        let setup_script = slirp_net.build_setup_script();
        let nsenter_prefix = slirp_net.build_nsenter_prefix(holder_pid);
        let tap_device = network_config.tap_device.clone();

        // Disk creation task
        let disk_task = async {
            let disk_manager = DiskManager::new(
                vm_id.to_string(),
                snapshot_config.disk_path.clone(),
                vm_dir.clone(),
            );

            let rootfs_path = disk_manager
                .create_cow_disk()
                .await
                .context("creating CoW disk from snapshot")?;

            // For rootless mode, make disk directory and file world-accessible
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&vm_dir, std::fs::Permissions::from_mode(0o777))
                .context("setting disk directory permissions for rootless mode")?;
            std::fs::set_permissions(&rootfs_path, std::fs::Permissions::from_mode(0o666))
                .context("setting disk file permissions for rootless mode")?;

            info!(
                rootfs = %rootfs_path.display(),
                snapshot_disk = %snapshot_config.disk_path.display(),
                "CoW disk prepared from snapshot"
            );

            Ok::<_, anyhow::Error>(rootfs_path)
        };

        // Network setup task
        let network_task = async {
            const MAX_NS_WAIT: Duration = Duration::from_millis(1000);
            const NS_POLL_INTERVAL: Duration = Duration::from_millis(5);
            let ns_poll_start = std::time::Instant::now();

            info!(holder_pid = holder_pid, "running network setup via nsenter");
            loop {
                // Verify holder is still alive before attempting nsenter
                if !crate::utils::is_process_alive(holder_pid) {
                    bail!(
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
                        bail!(
                            "namespace not ready after {:?}: {}",
                            ns_poll_start.elapsed(),
                            stderr
                        );
                    }
                    tokio::time::sleep(NS_POLL_INTERVAL).await;
                    continue;
                }

                // Permanent error
                bail!("network setup failed: {}", stderr);
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
                bail!(
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

        // Step 3: Set namespace paths for pre_exec setns (NOT nsenter wrapper)
        // For clones, we need to enter namespaces in pre_exec because:
        // - pre_exec runs BEFORE nsenter would enter the namespace
        // - We need CAP_SYS_ADMIN (from user namespace) for mount operations
        // - Entering user namespace first gives us CAP_SYS_ADMIN for unshare(CLONE_NEWNS)
        vm_manager.set_user_namespace_path(std::path::PathBuf::from(format!(
            "/proc/{}/ns/user",
            holder_pid
        )));
        vm_manager.set_net_namespace_path(std::path::PathBuf::from(format!(
            "/proc/{}/ns/net",
            holder_pid
        )));

        // Store holder_pid in state for health checks
        vm_state.holder_pid = Some(holder_pid);
        holder_pid_for_post_start = Some(holder_pid);

        holder_child = Some(child);
    } else {
        // Unknown network type - should not happen
        bail!("Unknown network type - must be either BridgedNetwork or SlirpNetwork");
    }

    // Configure mount namespace isolation for vsock redirect
    // This is ALWAYS needed for clones because vmstate.bin stores the baseline's vsock uds_path,
    // and Firecracker cannot override it during snapshot restore. Without this isolation:
    // - The baseline VM is using /baseline_dir/vsock.sock
    // - All clones would try to bind() to the same path, causing "Address in use" errors
    //
    // Solution: Run each clone in a mount namespace where baseline_dir is bind-mounted
    // over clone_dir. When Firecracker does bind("/baseline_dir/vsock.sock"),
    // it actually binds to "/clone_dir/vsock.sock" due to the bind mount.
    let baseline_dir = paths::vm_runtime_dir(&snapshot_config.vm_id);
    info!(
        baseline_dir = %baseline_dir.display(),
        clone_dir = %data_dir.display(),
        "enabling mount namespace for vsock socket isolation"
    );
    vm_manager.set_vsock_redirect(baseline_dir, data_dir.to_path_buf());

    let firecracker_bin = super::common::find_firecracker()?;

    vm_manager
        .start(&firecracker_bin, None)
        .await
        .context("starting Firecracker")?;

    // For rootless mode with slirp4netns: post_start starts slirp4netns in the namespace
    // For bridged mode: post_start is a no-op (TAP already created)
    let vm_pid = vm_manager.pid()?;
    let post_start_pid = holder_pid_for_post_start.unwrap_or(vm_pid);
    network
        .post_start(post_start_pid)
        .await
        .context("post-start network setup")?;

    let client = vm_manager.client()?;

    // Load snapshot with UFFD backend and network override
    use crate::firecracker::api::{
        DrivePatch, MemBackend, NetworkOverride, SnapshotLoad, VmState as ApiVmState,
    };

    info!(
        tap_device = %network_config.tap_device,
        disk = %rootfs_path.display(),
        "loading snapshot with uffd backend and network override"
    );

    // Timing instrumentation: measure snapshot load operation
    let load_start = std::time::Instant::now();
    client
        .load_snapshot(SnapshotLoad {
            snapshot_path: snapshot_config.vmstate_path.display().to_string(),
            mem_backend: MemBackend {
                backend_type: "Uffd".to_string(),
                backend_path: uffd_socket.display().to_string(),
            },
            enable_diff_snapshots: Some(false),
            resume_vm: Some(false), // Update devices before resume
            network_overrides: Some(vec![NetworkOverride {
                iface_id: "eth0".to_string(),
                host_dev_name: network_config.tap_device.clone(),
            }]),
        })
        .await
        .context("loading snapshot with uffd backend")?;
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
        .context("retargeting rootfs drive for clone")?;
    let patch_duration = patch_start.elapsed();
    info!(
        duration_ms = patch_duration.as_millis(),
        "disk patch completed"
    );

    // Signal fc-agent to flush ARP cache via MMDS restore-epoch update
    // fc-agent watches for this field change and immediately flushes stale ARP entries
    //
    // IMPORTANT: After snapshot load:
    // - MMDS CONFIG is preserved from the snapshot (version, network interfaces, IP)
    // - MMDS DATA is NOT persisted (empty data store) - we need to populate it
    // - /mmds/config endpoint is PRE-BOOT ONLY - cannot be called after snapshot load
    // - /mmds endpoint (PUT/PATCH) is allowed both pre-boot and post-boot
    //
    // So we just call put_mmds() - the config is already there from the snapshot.
    let restore_epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .context("system time before Unix epoch")?
        .as_secs();

    // Put the restore-epoch data directly (MMDS config is preserved from snapshot)
    // fc-agent doesn't need container-plan after restore since container is already running
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

    // Save VM state with complete network configuration
    super::common::save_vm_state_with_network(state_manager, vm_state, network_config).await?;

    Ok((vm_manager, holder_child))
}
