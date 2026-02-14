use anyhow::{bail, Context, Result};
use std::path::PathBuf;
use std::time::Duration;
use tokio::signal::unix::{signal, SignalKind};
use tracing::{debug, info, warn};

use super::podman::{
    check_podman_snapshot, create_snapshot_interruptible, startup_snapshot_key,
    SnapshotCreationParams, SnapshotOutcome,
};
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

use super::common::{
    MemoryBackend, RuntimeConfig, SnapshotRestoreConfig, VSOCK_OUTPUT_PORT, VSOCK_TTY_PORT,
};
use super::podman::run_output_listener;

/// Main dispatcher for snapshot commands
pub async fn cmd_snapshot(args: SnapshotArgs) -> Result<()> {
    match args.cmd {
        SnapshotCommands::Create(create_args) => cmd_snapshot_create(create_args).await,
        SnapshotCommands::Serve(serve_args) => cmd_snapshot_serve(serve_args).await,
        SnapshotCommands::Run(run_args) => cmd_snapshot_run(run_args).await,
        SnapshotCommands::Ls => cmd_snapshot_ls().await,
    }
}

fn snapshot_restore_runtime_config(args: &SnapshotRunArgs) -> RuntimeConfig {
    RuntimeConfig {
        firecracker_bin: args.firecracker_bin.as_ref().map(PathBuf::from),
        firecracker_args: args.firecracker_args.clone(),
        boot_args: None,
        fuse_readers: None,
    }
}

/// Create snapshot from running VM
async fn cmd_snapshot_create(args: SnapshotCreateArgs) -> Result<()> {
    use super::common::VSOCK_VOLUME_PORT_BASE;
    use crate::storage::snapshot::{
        SnapshotConfig, SnapshotMetadata, SnapshotType, SnapshotVolumeConfig,
    };

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

    // Check VM disk exists
    let vm_disk_path = paths::vm_runtime_dir(&vm_state.vm_id).join("disks/rootfs.raw");
    if !vm_disk_path.exists() {
        anyhow::bail!("VM disk not found at {}", vm_disk_path.display());
    }

    // Create client directly for existing VM
    use crate::firecracker::FirecrackerClient;
    let client = FirecrackerClient::new(socket_path)?;

    // Build final snapshot paths (used in config.json)
    let snapshot_dir = paths::snapshot_dir().join(&snapshot_name);
    let final_memory_path = snapshot_dir.join("memory.bin");
    let final_vmstate_path = snapshot_dir.join("vmstate.bin");
    let final_disk_path = snapshot_dir.join("disk.raw");

    // Parse volume configs from VM state (format: HOST:GUEST[:ro])
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

    // Build snapshot config with FINAL paths (create_snapshot_core handles temp dir)
    let snapshot_config = SnapshotConfig {
        name: snapshot_name.clone(),
        vm_id: vm_state.vm_id.clone(),
        original_vsock_vm_id: Some(original_vsock_vm_id),
        memory_path: final_memory_path,
        vmstate_path: final_vmstate_path,
        disk_path: final_disk_path,
        created_at: chrono::Utc::now(),
        snapshot_type: SnapshotType::User, // Explicit user-created snapshot
        metadata: SnapshotMetadata {
            image: vm_state.config.image.clone(),
            vcpu: vm_state.config.vcpu,
            memory_mib: vm_state.config.memory_mib,
            network_config: vm_state.config.network.clone(),
            volumes: volume_configs,
            health_check_url: vm_state.config.health_check_url.clone(),
            hugepages: vm_state.config.hugepages,
        },
    };

    // Use shared core function for snapshot creation
    // If the VM was restored from a snapshot, use that as parent for diff support
    let parent_dir = vm_state
        .config
        .snapshot_name
        .as_ref()
        .map(|name| paths::snapshot_dir().join(name));
    super::common::create_snapshot_core(
        &client,
        snapshot_config.clone(),
        &vm_disk_path,
        parent_dir.as_deref(),
    )
    .await?;

    // Print user-friendly output
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

    Ok(())
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

    // Run server in background task with cancellation token
    let server_cancel = tokio_util::sync::CancellationToken::new();
    let server_cancel_clone = server_cancel.clone();
    let mut server_handle = tokio::spawn(async move { server.run(server_cancel_clone).await });

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
                server_cancel.cancel();
                break;
            }
            _ = sigint.recv() => {
                info!("received SIGINT");
                if shutdown_requested {
                    // Second Ctrl-C - force shutdown
                    info!("received second SIGINT, forcing shutdown");
                    println!("\nForcing shutdown...");
                    server_cancel.cancel();
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
                    server_cancel.cancel();
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

    println!("Memory server stopped");

    Ok(())
}

/// Run clone from snapshot
///
/// Two modes:
/// - `--pid <serve_pid>`: Clone via UFFD memory sharing (for multiple concurrent clones)
/// - `--snapshot <name>`: Clone directly from snapshot files (simpler, no serve process needed)
///
/// This is public so podman.rs can call it directly for cache hits.
pub async fn cmd_snapshot_run(args: SnapshotRunArgs) -> Result<()> {
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

            info!("Cloning VM from serve PID {} (snapshot: {})", pid, name);
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
    let runtime_config = snapshot_restore_runtime_config(&args);
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
        args.cpu.unwrap_or(snapshot_config.metadata.vcpu),
        args.mem.unwrap_or(snapshot_config.metadata.memory_mib),
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

    // Setup TTY/output socket paths
    let tty_mode = args.tty;
    let interactive = args.interactive;
    let tty_socket_path = format!("{}_{}", clone_vsock_base.display(), VSOCK_TTY_PORT);
    let output_socket_path = format!("{}_{}", clone_vsock_base.display(), VSOCK_OUTPUT_PORT);

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
    let output_handle = if !tty_mode {
        let socket_path = output_socket_path.clone();
        let vm_id_clone = vm_id.clone();
        Some(tokio::spawn(async move {
            match run_output_listener(
                &socket_path,
                &vm_id_clone,
                None,
                std::sync::Arc::new(tokio::sync::Notify::new()),
            )
            .await
            {
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

            // With bridge mode, guest IP is always 10.0.2.100 on slirp network
            // Each clone runs in its own namespace, so no IP conflict
            let net = SlirpNetwork::new(vm_id.clone(), tap_device.clone(), port_mappings.clone())
                .with_loopback_ip(loopback_ip);
            Box::new(net)
        }
    };

    let network_config = network.setup().await.context("setting up network")?;

    // Health check URL comes from snapshot metadata — it's a property of the VM image.
    // The cache key includes health_check_url, so each config gets its own snapshot.
    vm_state.config.health_check_url = snapshot_config.metadata.health_check_url.clone();
    vm_state.config.hugepages = args.hugepages.unwrap_or(snapshot_config.metadata.hugepages);

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
    // Hugepages require UFFD restore (Firecracker rejects File backend for hugepage snapshots).
    // When restoring from cache (no explicit serve process), start an implicit in-process
    // UFFD server as a background tokio task.
    let hugepages = args.hugepages.unwrap_or(snapshot_config.metadata.hugepages);
    let implicit_uffd_cancel = tokio_util::sync::CancellationToken::new();

    let memory_backend = if let Some(ref uffd_socket_path) = uffd_socket {
        // Explicit UFFD mode (--pid): connect to existing serve process
        MemoryBackend::Uffd {
            socket_path: uffd_socket_path.clone(),
        }
    } else if hugepages {
        // Implicit UFFD mode: hugepages require UFFD, start in-process server
        let implicit_socket_path = data_dir.join("uffd.sock");
        info!(
            target: "uffd",
            socket = %implicit_socket_path.display(),
            "starting implicit UFFD server for hugepage snapshot restore"
        );

        let server = UffdServer::new_with_path(
            format!("implicit-{}", truncate_id(&vm_id, 8)),
            &snapshot_config.memory_path,
            &implicit_socket_path,
        )
        .await
        .context("creating implicit UFFD server for hugepages")?;

        let cancel = implicit_uffd_cancel.clone();
        tokio::spawn(async move {
            if let Err(e) = server.run(cancel).await {
                tracing::error!(target: "uffd", error = ?e, "implicit UFFD server error");
            }
        });

        // Give the server a moment to bind the socket
        tokio::time::sleep(Duration::from_millis(50)).await;

        MemoryBackend::Uffd {
            socket_path: implicit_socket_path,
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
        &runtime_config,
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

        // Stop implicit UFFD server if running
        implicit_uffd_cancel.cancel();

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

        // Cleanup data directory
        if data_dir.exists() {
            if let Err(cleanup_err) = tokio::fs::remove_dir_all(&data_dir).await {
                warn!(
                    "failed to cleanup data_dir after setup error: {}",
                    cleanup_err
                );
            }
        }

        // Cleanup state file
        if let Err(cleanup_err) = state_manager.delete_state(&vm_id).await {
            warn!("failed to cleanup state after setup error: {}", cleanup_err);
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
            output_handle, // abort output listener task
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

    // Create startup snapshot channel if:
    // - startup_snapshot_base_key is set (passed from podman run on cache hit)
    // - snapshot has a health check URL (needed to know when VM is fully initialized)
    let (startup_tx, mut startup_rx): (
        Option<tokio::sync::oneshot::Sender<()>>,
        Option<tokio::sync::oneshot::Receiver<()>>,
    ) = if args.startup_snapshot_base_key.is_some()
        && snapshot_config.metadata.health_check_url.is_some()
    {
        let (tx, rx) = tokio::sync::oneshot::channel();
        (Some(tx), Some(rx))
    } else {
        (None, None)
    };

    // Spawn health monitor task with startup snapshot trigger support
    let health_monitor_handle = crate::health::spawn_health_monitor_full(
        vm_id.clone(),
        vm_state.pid,
        paths::state_dir(),
        Some(health_cancel_token.clone()),
        startup_tx,
    );

    // Setup signal handlers with cancellation token
    let cancel = tokio_util::sync::CancellationToken::new();
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

    // Track container exit code (from TTY mode)
    let container_exit_code: Option<i32>;

    // Get disk path for startup snapshot creation
    let disk_path = data_dir.join("disks/rootfs.raw");

    // Wait for cancellation, VM exit, or startup snapshot trigger
    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                container_exit_code = None;
                break;
            }
            status = vm_manager.wait() => {
                info!(status = ?status, "VM exited");
                // If in TTY mode, get exit code from TTY handle
                if let Some(handle) = tty_handle {
                    container_exit_code = handle.join().ok().and_then(|r| r.ok());
                    info!(container_exit_code = ?container_exit_code, "TTY container exit code");
                } else {
                    container_exit_code = None;
                }
                break;
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

                if let Some(ref base_key) = args.startup_snapshot_base_key {
                    let startup_key = startup_snapshot_key(base_key);

                    // Skip if startup snapshot already exists
                    if check_podman_snapshot(&startup_key).await.is_some() {
                        info!(snapshot_key = %startup_key, "Startup snapshot already exists, skipping");
                    } else {
                        info!(snapshot_key = %startup_key, "Creating startup snapshot (VM healthy)");

                        let params = SnapshotCreationParams::from_metadata(&snapshot_config.metadata);
                        match create_snapshot_interruptible(
                            &vm_manager, &startup_key, &vm_id, &params, &disk_path,
                            &network_config, &volume_configs,
                            Some(base_key.as_str()), // Parent is pre-start snapshot
                            &cancel,
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

    // Stop implicit UFFD server if running
    implicit_uffd_cancel.cancel();

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
        output_handle, // abort output listener task
    )
    .await;

    // Return error if container exited with non-zero code
    if let Some(code) = container_exit_code {
        if code != 0 {
            std::process::exit(code);
        }
    }

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_snapshot_restore_runtime_config_preserves_firecracker_overrides() {
        let args = SnapshotRunArgs {
            pid: None,
            snapshot: Some("snap".to_string()),
            name: Some("clone".to_string()),
            publish: vec![],
            network: NetworkMode::Rootless,
            exec: None,
            tty: false,
            interactive: false,
            startup_snapshot_base_key: None,
            cpu: None,
            mem: None,
            firecracker_bin: Some("/opt/firecracker-profile".to_string()),
            firecracker_args: Some("--enable-nv2".to_string()),
            hugepages: None,
        };

        let runtime = snapshot_restore_runtime_config(&args);
        assert_eq!(
            runtime.firecracker_bin,
            Some(PathBuf::from("/opt/firecracker-profile"))
        );
        assert_eq!(runtime.firecracker_args, Some("--enable-nv2".to_string()));
    }
}
