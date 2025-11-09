use anyhow::{Context, Result};
use std::path::PathBuf;
use tokio::signal::unix::{signal, SignalKind};
use tracing::info;

use crate::cli::{SnapshotArgs, SnapshotCommands, SnapshotCreateArgs, SnapshotServeArgs, SnapshotRunArgs};
use crate::firecracker::VmManager;
use crate::network::{NetworkManager, PortMapping, RootlessNetwork, PrivilegedNetwork};
use crate::storage::{DiskManager, SnapshotManager};
use crate::state::{StateManager, generate_vm_id};
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

    // Load VM state
    let state_manager = StateManager::new(PathBuf::from("/tmp/fcvm/state"));
    let vm_state = state_manager.load_state(&args.name).await
        .context("loading VM state")?;

    // Connect to running VM
    let socket_path = PathBuf::from(format!("/tmp/fcvm/{}/firecracker.sock", vm_state.id));
    let vm_manager = VmManager::new(vm_state.id.clone(), socket_path, None);
    let client = vm_manager.client()?;

    // Create snapshot
    let snapshot_manager = SnapshotManager::new(PathBuf::from("/tmp/fcvm/snapshots"));
    let snapshot_config = snapshot_manager.create_snapshot(
        &snapshot_name,
        &vm_state,
        &client,
    ).await
        .context("creating snapshot")?;

    info!(
        snapshot = %snapshot_name,
        mem_size = snapshot_config.metadata.memory_mib,
        "snapshot created successfully"
    );

    println!("✓ Snapshot '{}' created from VM '{}'", snapshot_name, args.name);
    println!("  Memory: {} MB", snapshot_config.metadata.memory_mib);
    println!("  Files:");
    println!("    {}", snapshot_config.memory_path.display());
    println!("    {}", snapshot_config.disk_path.display());

    // TODO: Stop the original VM after snapshotting
    println!("\nNote: Original VM '{}' is still running. Stop it with Ctrl-C.", args.name);

    Ok(())
}

/// Serve snapshot memory (foreground)
async fn cmd_snapshot_serve(args: SnapshotServeArgs) -> Result<()> {
    info!("Starting memory server for snapshot: {}", args.snapshot_name);

    // Load snapshot configuration
    let snapshot_manager = SnapshotManager::new(PathBuf::from("/tmp/fcvm/snapshots"));
    let snapshot_config = snapshot_manager.load_snapshot(&args.snapshot_name).await
        .context("loading snapshot configuration")?;

    info!(
        snapshot = %args.snapshot_name,
        mem_file = %snapshot_config.memory_path.display(),
        mem_size_mb = snapshot_config.metadata.memory_mib,
        "loaded snapshot configuration"
    );

    // Create and start UFFD server
    let server = UffdServer::new(
        args.snapshot_name.clone(),
        &snapshot_config.memory_path,
    ).await
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
    server.run().await
        .context("running UFFD server")?;

    println!("Memory server stopped");

    Ok(())
}

/// Run clone from snapshot
async fn cmd_snapshot_run(args: SnapshotRunArgs) -> Result<()> {
    info!("Cloning VM from snapshot: {}", args.snapshot_name);

    // Load snapshot configuration
    let snapshot_manager = SnapshotManager::new(PathBuf::from("/tmp/fcvm/snapshots"));
    let snapshot_config = snapshot_manager.load_snapshot(&args.snapshot_name).await
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
    let data_dir = PathBuf::from(format!("/tmp/fcvm/{}", vm_id));
    tokio::fs::create_dir_all(&data_dir).await
        .context("creating VM data directory")?;

    let socket_path = data_dir.join("firecracker.sock");
    let log_path = data_dir.join("firecracker.log");

    // Check for running memory server
    let uffd_socket = PathBuf::from(format!("/tmp/fcvm/uffd-{}.sock", args.snapshot_name));

    if !uffd_socket.exists() {
        anyhow::bail!(
            "Memory server not running for snapshot '{}'.\\n\\n\\\
             Start it first in another terminal:\\n\\\
             fcvm snapshot serve {}",
            args.snapshot_name, args.snapshot_name
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

    let network_config = network.setup().await
        .context("setting up network")?;

    info!(
        tap = %network_config.tap_device,
        mac = %network_config.guest_mac,
        "network configured for clone"
    );

    // Setup storage - Create CoW disk from snapshot disk
    let vm_dir = data_dir.join("disks");
    let disk_manager = DiskManager::new(
        vm_id.clone(),
        snapshot_config.disk_path.clone(),
        vm_dir,
    );

    let rootfs_path = disk_manager.create_cow_disk().await
        .context("creating CoW disk from snapshot")?;

    info!(
        rootfs = %rootfs_path.display(),
        snapshot_disk = %snapshot_config.disk_path.display(),
        "CoW disk prepared from snapshot"
    );

    // Start Firecracker VM
    let mut vm_manager = VmManager::new(vm_id.clone(), socket_path.clone(), Some(log_path));
    let firecracker_bin = PathBuf::from("/usr/local/bin/firecracker");

    vm_manager.start(&firecracker_bin, None).await
        .context("starting Firecracker")?;

    let client = vm_manager.client()?;

    // Load snapshot with UFFD backend
    use crate::firecracker::api::{SnapshotLoad, MemBackend, NetworkOverride};

    info!("loading snapshot with uffd backend via memory server");
    client.load_snapshot(SnapshotLoad {
        snapshot_path: snapshot_config.memory_path.display().to_string(),
        mem_backend: MemBackend {
            backend_type: "Uffd".to_string(),
            backend_path: uffd_socket.display().to_string(),
        },
        enable_diff_snapshots: Some(false),
        resume_vm: Some(true),
        network_overrides: Some(vec![NetworkOverride {
            iface_id: "eth0".to_string(),
            host_dev_name: network_config.tap_device.clone(),
        }]),
    }).await
        .context("loading snapshot with uffd backend")?;

    info!(
        vm_id = %vm_id,
        vm_name = %vm_name,
        "VM cloned successfully with UFFD memory sharing!"
    );
    println!("✓ VM '{}' cloned from snapshot '{}'", vm_name, args.snapshot_name);
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
