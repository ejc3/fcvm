use anyhow::{Context, Result};
use std::path::PathBuf;
use tokio::signal::unix::{signal, SignalKind};
use tracing::{info, warn};

use crate::cli::{
    SnapshotArgs, SnapshotCommands, SnapshotCreateArgs, SnapshotRunArgs, SnapshotServeArgs,
};
use crate::firecracker::VmManager;
use crate::network::{NetworkManager, PortMapping, RootlessNetwork};
use crate::paths;
use crate::state::{generate_vm_id, truncate_id, StateManager, VmState, VmStatus};
use crate::storage::{DiskManager, SnapshotManager};
use crate::uffd::UffdServer;

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
    let disk_path = snapshot_dir.join("disk.ext4");

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
        let vm_disk_path = paths::vm_runtime_dir(&vm_state.vm_id).join("disks/rootfs.ext4");

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
        use crate::storage::snapshot::{SnapshotConfig, SnapshotMetadata};
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
        let vm_name = vm_state.name.as_deref().unwrap_or(truncate_id(&vm_state.vm_id, 8));
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
        paths::base_dir().join(format!("uffd-{}-{}.sock", args.snapshot_name, my_pid));

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

    let state_manager = StateManager::new(paths::state_dir());
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
    let server_handle = tokio::spawn(async move { server.run().await });

    // Wait for signal or server exit
    tokio::select! {
        _ = sigterm.recv() => {
            info!("received SIGTERM");
        }
        _ = sigint.recv() => {
            info!("received SIGINT");
        }
        result = server_handle => {
            info!("server exited: {:?}", result);
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
                        warn!("kill command returned non-zero exit for PID {}: {:?}", pid, status.code());
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
        warn!("failed to remove socket file {}: {}", socket_path.display(), e);
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
async fn cmd_snapshot_run(args: SnapshotRunArgs) -> Result<()> {
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

    let socket_path = data_dir.join("firecracker.sock");

    // Check for running memory server using serve PID
    let uffd_socket = paths::base_dir().join(format!("uffd-{}-{}.sock", snapshot_name, args.pid));

    if !uffd_socket.exists() {
        anyhow::bail!(
            "Memory server socket not found for serve PID {}.\\n\\n\\\
             The serve process may have exited or not be ready yet.\\n\\\
             Expected socket: {}",
            args.pid,
            uffd_socket.display()
        );
    }

    info!(
        uffd_socket = %uffd_socket.display(),
        "connecting to memory server"
    );

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

    // Setup networking (always rootless) - reuse guest_ip from snapshot if available
    let mut net = RootlessNetwork::new(vm_id.clone(), tap_device.clone(), port_mappings.clone());
    // If snapshot has saved network config with guest_ip, use it
    if let Some(ref guest_ip) = saved_network.guest_ip {
        net = net.with_guest_ip(guest_ip.clone());
        info!(
            guest_ip = %guest_ip,
            "clone will use same network config as snapshot"
        );
    }
    let mut network: Box<dyn NetworkManager> = Box::new(net);

    let network_config = network.setup().await.context("setting up network")?;

    info!(
        tap = %network_config.tap_device,
        mac = %network_config.guest_mac,
        "network configured for clone"
    );

    // Setup storage - Create CoW disk from snapshot disk
    let vm_dir = data_dir.join("disks");
    let disk_manager = DiskManager::new(vm_id.clone(), snapshot_config.disk_path.clone(), vm_dir);

    let rootfs_path = disk_manager
        .create_cow_disk()
        .await
        .context("creating CoW disk from snapshot")?;

    info!(
        rootfs = %rootfs_path.display(),
        snapshot_disk = %snapshot_config.disk_path.display(),
        "CoW disk prepared from snapshot"
    );

    info!(vm_name = %vm_name, vm_id = %vm_id, "creating VM manager");
    let mut vm_manager = VmManager::new(vm_id.clone(), socket_path.clone(), None);

    // Set VM name for logging
    vm_manager.set_vm_name(vm_name.clone());

    // Configure namespace isolation if network provides one
    if let Some(rootless_net) = network.as_any().downcast_ref::<RootlessNetwork>() {
        if let Some(ns_id) = rootless_net.namespace_id() {
            info!(namespace = %ns_id, "configuring VM to run in network namespace");
            vm_manager.set_namespace(ns_id.to_string());
        }
    }

    let firecracker_bin = PathBuf::from("/usr/local/bin/firecracker");

    vm_manager
        .start(&firecracker_bin, None)
        .await
        .context("starting Firecracker")?;

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
        .unwrap()
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
    info!(restore_epoch = restore_epoch, "signaled fc-agent to flush ARP via MMDS");

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

    // Save VM state with complete network configuration
    super::common::save_vm_state_with_network(&state_manager, &mut vm_state, &network_config)
        .await?;

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

    // Spawn health monitor task (store handle for cancellation)
    let health_monitor_handle = crate::health::spawn_health_monitor(vm_id.clone(), vm_state.pid);

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

    // Cleanup
    info!("cleaning up resources");

    // Cancel health monitor task first
    health_monitor_handle.abort();

    // Kill VM process
    if let Err(e) = vm_manager.kill().await {
        warn!("failed to kill VM process: {}", e);
    }

    // Cleanup network
    if let Err(e) = network.cleanup().await {
        warn!("failed to cleanup network: {}", e);
    }

    // Delete state file
    if let Err(e) = state_manager.delete_state(&vm_id).await {
        warn!("failed to delete state file: {}", e);
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
