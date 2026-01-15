use anyhow::{bail, Context, Result};
use std::path::PathBuf;
use std::time::Duration;
use tokio::signal::unix::{signal, SignalKind};
use tracing::{debug, info, warn};

use crate::cli::{
    NetworkMode, SnapshotArgs, SnapshotCommands, SnapshotCreateArgs, SnapshotRunArgs,
    SnapshotServeArgs,
};
use crate::network::{BridgedNetwork, NetworkManager, PortMapping, SlirpNetwork};
use crate::paths;
use crate::state::{
    generate_vm_id, truncate_id, validate_vm_name, StateManager, VmState, VmStatus,
};
use crate::storage::SnapshotManager;
use crate::uffd::UffdServer;
use crate::volume::{spawn_volume_servers, VolumeConfig};

use super::common::{MemoryBackend, SnapshotRestoreConfig};

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
                    "Failed to create reflink copy. Ensure {} is a btrfs filesystem. Error: {}",
                    crate::paths::assets_dir().display(),
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
        let volume_configs: Vec<SnapshotVolumeConfig> = vm_state
            .config
            .volumes
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

        // Use original_vsock_vm_id from the VM state if available.
        // When a VM is restored from cache, its vmstate.bin references vsock paths from the
        // ORIGINAL (cached) VM. Taking a snapshot of this restored VM creates a NEW vmstate.bin,
        // but Firecracker doesn't update vsock paths - they still reference the original VM ID.
        // So we must preserve the original_vsock_vm_id through the chain:
        // Cache(vm-AAA) → Restore(vm-BBB) → Snapshot → Clone(vm-CCC)
        // The clone needs to redirect from vm-AAA's path, not vm-BBB's.
        let original_vsock_vm_id = vm_state
            .config
            .original_vsock_vm_id
            .clone()
            .unwrap_or_else(|| vm_state.vm_id.clone());

        let snapshot_config = SnapshotConfig {
            name: snapshot_name.clone(),
            vm_id: vm_state.vm_id.clone(),
            original_vsock_vm_id: Some(original_vsock_vm_id),
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

        let vm_name = vm_state
            .name
            .as_deref()
            .unwrap_or(truncate_id(&vm_state.vm_id, 8));
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
///
/// Two modes:
/// - `--pid <serve_pid>`: Clone via UFFD memory sharing (for multiple concurrent clones)
/// - `--snapshot <name>`: Clone directly from snapshot files (simpler, no serve process needed)
async fn cmd_snapshot_run(args: SnapshotRunArgs) -> Result<()> {
    // Determine mode and get snapshot name
    let (snapshot_name, serve_pid, use_uffd) = match (&args.pid, &args.snapshot) {
        (Some(pid), None) => {
            // UFFD mode: verify serve process is alive
            if !crate::utils::is_process_alive(*pid) {
                anyhow::bail!(
                    "serve process (PID {}) is not running - start with 'fcvm snapshot serve'",
                    pid
                );
            }

            // Load serve state by PID to get snapshot name
            let state_manager = StateManager::new(paths::state_dir());
            let serve_state = state_manager
                .load_state_by_pid(*pid)
                .await
                .context("loading serve process state - is serve running?")?;

            let name = serve_state
                .config
                .snapshot_name
                .ok_or_else(|| anyhow::anyhow!("serve process has no snapshot_name"))?;

            info!(
                "Cloning VM from serve PID {} (snapshot: {})",
                pid, name
            );
            (name, Some(*pid), true)
        }
        (None, Some(name)) => {
            // Direct file mode: no serve process needed
            info!("Cloning VM directly from snapshot: {}", name);
            (name.clone(), None, false)
        }
        (None, None) => {
            anyhow::bail!("Either --pid or --snapshot must be specified");
        }
        (Some(_), Some(_)) => {
            // clap's conflicts_with should prevent this, but just in case
            anyhow::bail!("Cannot specify both --pid and --snapshot");
        }
    };

    let state_manager = StateManager::new(paths::state_dir());

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
    vm_state.config.serve_pid = serve_pid; // Track which serve spawned us (None for direct mode)

    // Setup paths
    let data_dir = paths::vm_runtime_dir(&vm_id);
    tokio::fs::create_dir_all(&data_dir)
        .await
        .context("creating VM data directory")?;

    let socket_path = data_dir.join("firecracker.sock");

    // Build UFFD socket path for memory server (only for UFFD mode)
    let uffd_socket = if use_uffd {
        let pid = serve_pid.expect("serve_pid must be set for UFFD mode");
        let socket = paths::data_dir().join(format!("uffd-{}-{}.sock", snapshot_name, pid));
        info!(
            uffd_socket = %socket.display(),
            serve_pid = pid,
            "connecting to memory server"
        );
        Some(socket)
    } else {
        info!(
            memory_file = %snapshot_config.memory_path.display(),
            "loading memory directly from file"
        );
        None
    };

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

    // Build restore configuration
    // For snapshots of cache-restored VMs:
    // - original_vsock_vm_id (vm-AAA) = vsock paths in vmstate.bin (unchanged from cache)
    // - vm_id (vm-BBB) = disk paths in vmstate.bin (patched during cache restore)
    // For snapshots of fresh VMs:
    // - vm_id is used for both (no separate original_vsock_vm_id)
    let original_vm_id = snapshot_config
        .original_vsock_vm_id
        .clone()
        .unwrap_or_else(|| snapshot_config.vm_id.clone());

    // snapshot_vm_id is the VM ID where disk paths point (snapshot_config.vm_id)
    // Only set if different from original_vm_id (for cache-restored VMs)
    let snapshot_vm_id = if snapshot_config.original_vsock_vm_id.is_some() {
        // Snapshot of cache-restored VM: disk paths point to snapshot's vm_id
        Some(snapshot_config.vm_id.clone())
    } else {
        // Snapshot of fresh VM: disk and vsock both use same vm_id
        None
    };

    // Choose memory backend based on mode
    let memory_backend = if let Some(ref uffd_socket_path) = uffd_socket {
        MemoryBackend::Uffd {
            socket_path: uffd_socket_path.clone(),
        }
    } else {
        MemoryBackend::File {
            memory_path: snapshot_config.memory_path.clone(),
        }
    };

    let restore_config = SnapshotRestoreConfig {
        vmstate_path: snapshot_config.vmstate_path.clone(),
        memory_backend,
        source_disk_path: snapshot_config.disk_path.clone(),
        original_vm_id,
        snapshot_vm_id,
    };

    // Run clone setup using shared restore function
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

    if use_uffd {
        info!(
            vm_id = %vm_id,
            vm_name = %vm_name,
            "VM cloned successfully with UFFD memory sharing!"
        );
        println!(
            "✓ VM '{}' cloned from snapshot '{}' (UFFD mode)",
            vm_name, snapshot_name
        );
        println!("  Memory pages shared via UFFD server");
    } else {
        info!(
            vm_id = %vm_id,
            vm_name = %vm_name,
            "VM cloned successfully from snapshot files!"
        );
        println!(
            "✓ VM '{}' cloned from snapshot '{}' (direct mode)",
            vm_name, snapshot_name
        );
        println!("  Memory loaded from file");
    }
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
