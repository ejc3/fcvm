use anyhow::{Context, Result};
use std::path::PathBuf;
use tokio::signal::unix::{signal, SignalKind};
use tracing::info;

use crate::cli::{
    SnapshotArgs, SnapshotCommands, SnapshotCreateArgs, SnapshotRunArgs, SnapshotServeArgs,
};
use crate::firecracker::VmManager;
use crate::network::{NetworkManager, PortMapping, PrivilegedNetwork, RootlessNetwork};
use crate::paths;
use crate::state::{generate_vm_id, StateManager};
use crate::storage::{DiskManager, SnapshotManager};
use crate::uffd::UffdServer;
use crate::Mode;

/// Main dispatcher for snapshot commands
pub async fn cmd_snapshot(args: SnapshotArgs) -> Result<()> {
    match args.cmd {
        SnapshotCommands::Create(create_args) => cmd_snapshot_create(create_args).await,
        SnapshotCommands::Serve(serve_args) => cmd_snapshot_serve(serve_args).await,
        SnapshotCommands::Run(run_args) => cmd_snapshot_run(run_args).await,
    }
}

/// Create snapshot from running VM
async fn cmd_snapshot_create(args: SnapshotCreateArgs) -> Result<()> {
    info!("Creating snapshot from VM: {}", args.name);

    let snapshot_name = args.tag.unwrap_or_else(|| args.name.clone());

    // Load VM state by name
    let state_manager = StateManager::new(paths::state_dir());
    let vm_state = state_manager
        .load_state_by_name(&args.name)
        .await
        .context("loading VM state")?;

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

    // Copy the VM's disk to snapshot directory
    info!("Copying VM disk to snapshot directory");
    let vm_disk_path = paths::vm_runtime_dir(&vm_state.vm_id).join("disks/rootfs-overlay.ext4");

    if vm_disk_path.exists() {
        tokio::fs::copy(&vm_disk_path, &disk_path)
            .await
            .context("copying VM disk to snapshot")?;
        info!(
            source = %vm_disk_path.display(),
            dest = %disk_path.display(),
            "VM disk copied to snapshot"
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
            network_config: serde_json::json!({}), // TODO: capture network config
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

    // Resume the original VM after snapshotting
    info!("Resuming original VM");
    client
        .patch_vm_state(ApiVmState {
            state: "Resumed".to_string(),
        })
        .await
        .context("resuming VM after snapshot")?;

    info!("Original VM resumed successfully");

    println!(
        "✓ Snapshot '{}' created from VM '{}'",
        snapshot_name, args.name
    );
    println!("  Memory: {} MB", snapshot_config.metadata.memory_mib);
    println!("  Files:");
    println!("    {}", snapshot_config.memory_path.display());
    println!("    {}", snapshot_config.disk_path.display());
    println!(
        "\nOriginal VM '{}' has been resumed and is still running.",
        args.name
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

    // Create and start UFFD server
    let server = UffdServer::new(args.snapshot_name.clone(), &snapshot_config.memory_path)
        .await
        .context("creating UFFD server")?;

    println!("Serving snapshot: {}", args.snapshot_name);
    println!("  Socket: {}", server.socket_path().display());
    println!("  Memory: {} MB", snapshot_config.metadata.memory_mib);
    println!("  Waiting for VMs to connect...");
    println!();
    println!("Clone VMs with: fcvm snapshot run {}", args.snapshot_name);
    println!("Press Ctrl-C to stop");
    println!();

    // Run server (blocks until all VMs disconnect or Ctrl-C)
    server.run().await.context("running UFFD server")?;

    println!("Memory server stopped");

    Ok(())
}

/// Run clone from snapshot
async fn cmd_snapshot_run(args: SnapshotRunArgs) -> Result<()> {
    info!("Cloning VM from snapshot: {}", args.snapshot_name);

    // Load snapshot configuration
    let snapshot_manager = SnapshotManager::new(paths::snapshot_dir());
    let snapshot_config = snapshot_manager
        .load_snapshot(&args.snapshot_name)
        .await
        .context("loading snapshot configuration")?;

    info!(
        snapshot = %args.snapshot_name,
        image = %snapshot_config.metadata.image,
        vcpu = snapshot_config.metadata.vcpu,
        mem_mib = snapshot_config.metadata.memory_mib,
        "loaded snapshot configuration"
    );

    // Generate VM ID and name
    let vm_id = generate_vm_id();
    let vm_name = args.name.unwrap_or_else(|| {
        // Auto-generate: snapshot-name + random suffix
        format!("{}-{}", args.snapshot_name, &vm_id[..6])
    });

    // Detect execution mode
    let mode = match args.mode.into() {
        Mode::Auto => {
            if nix::unistd::Uid::effective().is_root() {
                Mode::Privileged
            } else {
                Mode::Rootless
            }
        }
        m => m,
    };

    info!(mode = ?mode, vm_id = %vm_id, vm_name = %vm_name, "detected execution mode");

    // Setup paths
    let data_dir = paths::vm_runtime_dir(&vm_id);
    tokio::fs::create_dir_all(&data_dir)
        .await
        .context("creating VM data directory")?;

    let socket_path = data_dir.join("firecracker.sock");
    let log_path = data_dir.join("firecracker.log");

    // Check for running memory server
    let uffd_socket = paths::base_dir().join(format!("uffd-{}.sock", args.snapshot_name));

    if !uffd_socket.exists() {
        anyhow::bail!(
            "Memory server not running for snapshot '{}'.\\n\\n\\\
             Start it first in another terminal:\\n\\\
             fcvm snapshot serve {}",
            args.snapshot_name,
            args.snapshot_name
        );
    }

    info!(
        uffd_socket = %uffd_socket.display(),
        "connecting to memory server"
    );

    // Setup networking
    let tap_device = format!("tap-{}", &vm_id[..8]);
    let port_mappings: Vec<PortMapping> = args
        .publish
        .iter()
        .map(|s| PortMapping::parse(s))
        .collect::<Result<Vec<_>>>()
        .context("parsing port mappings")?;

    let mut network: Box<dyn NetworkManager> = match mode {
        Mode::Rootless => Box::new(RootlessNetwork::new(
            vm_id.clone(),
            tap_device.clone(),
            port_mappings.clone(),
        )),
        Mode::Privileged => Box::new(PrivilegedNetwork::new(
            vm_id.clone(),
            tap_device.clone(),
            "fcvmbr0".to_string(),
            format!("172.16.0.{}", 10 + (vm_id.len() % 240)),
            "172.16.0.1".to_string(),
            port_mappings.clone(),
        )),
        Mode::Auto => unreachable!(),
    };

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

    // Create symlink so Firecracker can find the disk at the original path
    // The vmstate.bin contains hardcoded disk paths from the original VM
    let original_disk_dir = paths::vm_runtime_dir(&snapshot_config.vm_id).join("disks");
    let original_disk_path = original_disk_dir.join("rootfs-overlay.ext4");

    tokio::fs::create_dir_all(&original_disk_dir)
        .await
        .context("creating original disk directory for symlink")?;

    if original_disk_path.exists()
        || tokio::fs::symlink_metadata(&original_disk_path)
            .await
            .is_ok()
    {
        tokio::fs::remove_file(&original_disk_path)
            .await
            .context("removing existing disk symlink")?;
    }

    tokio::fs::symlink(&rootfs_path, &original_disk_path)
        .await
        .context("creating disk symlink")?;

    info!(
        symlink = %original_disk_path.display(),
        target = %rootfs_path.display(),
        "created disk symlink for snapshot compatibility"
    );

    // Start Firecracker VM (disable logging for now to avoid permission issues)
    let mut vm_manager = VmManager::new(vm_id.clone(), socket_path.clone(), None);
    let firecracker_bin = PathBuf::from("/usr/local/bin/firecracker");

    vm_manager
        .start(&firecracker_bin, None)
        .await
        .context("starting Firecracker")?;

    let client = vm_manager.client()?;

    // Load snapshot with UFFD backend and network override
    use crate::firecracker::api::{MemBackend, NetworkOverride, SnapshotLoad};

    info!(
        tap_device = %network_config.tap_device,
        disk = %rootfs_path.display(),
        "loading snapshot with uffd backend and network override"
    );
    client
        .load_snapshot(SnapshotLoad {
            snapshot_path: snapshot_config.vmstate_path.display().to_string(),
            mem_backend: MemBackend {
                backend_type: "Uffd".to_string(),
                backend_path: uffd_socket.display().to_string(),
            },
            enable_diff_snapshots: Some(false),
            resume_vm: Some(true), // Resume VM after loading
            network_overrides: Some(vec![NetworkOverride {
                iface_id: "eth0".to_string(),
                host_dev_name: network_config.tap_device.clone(),
            }]),
        })
        .await
        .context("loading snapshot with uffd backend")?;

    info!(
        vm_id = %vm_id,
        vm_name = %vm_name,
        "VM cloned successfully with UFFD memory sharing!"
    );
    println!(
        "✓ VM '{}' cloned from snapshot '{}'",
        vm_name, args.snapshot_name
    );
    println!("  Memory pages shared via UFFD");
    println!("  Disk uses CoW overlay");

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
    let _ = vm_manager.kill().await;
    let _ = network.cleanup().await;

    Ok(())
}
