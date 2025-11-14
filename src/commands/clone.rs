use anyhow::{Context, Result};
use std::path::PathBuf;
use tokio::signal::unix::{signal, SignalKind};
use tracing::info;

use crate::cli::CloneArgs;
use crate::firecracker::{VmManager, api::{SnapshotLoad, MemBackend, NetworkOverride}};
use crate::network::{NetworkManager, PortMapping, RootlessNetwork, PrivilegedNetwork};
use crate::paths;
use crate::state::{StateManager, VmState, VmStatus, generate_vm_id};
use crate::storage::{DiskManager, SnapshotManager};
use crate::Mode;


pub async fn cmd_clone(args: CloneArgs) -> Result<()> {
    info!("Starting fcvm clone with uffd-based memory sharing");

    // Load snapshot configuration
    let snapshot_manager = SnapshotManager::new(paths::snapshot_dir());
    let snapshot_config = snapshot_manager.load_snapshot(&args.snapshot).await
        .context("loading snapshot configuration")?;

    info!(
        snapshot = %args.snapshot,
        image = %snapshot_config.metadata.image,
        vcpu = snapshot_config.metadata.vcpu,
        mem_mib = snapshot_config.metadata.memory_mib,
        "loaded snapshot configuration"
    );

    // Generate new VM ID
    let vm_id = generate_vm_id();
    let vm_name = args.name.clone();

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
    tokio::fs::create_dir_all(&data_dir).await
        .context("creating VM data directory")?;

    let socket_path = data_dir.join("firecracker.sock");
    let log_path = data_dir.join("firecracker.log");

    // Check for running memory server
    let uffd_socket = paths::base_dir().join(format!("uffd-{}.sock", args.snapshot));

    if !uffd_socket.exists() {
        anyhow::bail!(
            "Memory server not running for snapshot '{}'.\n\n\
             Start it first in another terminal:\n\
             fcvm memory-server {}",
            args.snapshot, args.snapshot
        );
    }

    info!(
        uffd_socket = %uffd_socket.display(),
        "connecting to memory server"
    );

    // Create VM state
    let mut vm_state = VmState::new(
        vm_id.clone(),
        snapshot_config.metadata.image.clone(),
        snapshot_config.metadata.vcpu,
        snapshot_config.metadata.memory_mib,
    );
    vm_state.name = Some(vm_name.clone());

    // Initialize state manager
    let state_manager = StateManager::new(paths::state_dir());
    state_manager.init().await?;

    // Setup networking (similar to run)
    let tap_device = format!("tap-{}", &vm_id[..8]);
    let port_mappings: Vec<PortMapping> = Vec::new(); // Clone doesn't add new port mappings by default

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

    // Load snapshot with UFFD backend for TRUE MEMORY SHARING!
    // backend_path points to the Unix socket where memory-server is listening
    info!("loading snapshot with uffd backend via memory server");
    client.load_snapshot(SnapshotLoad {
        snapshot_path: snapshot_config.memory_path.display().to_string(),
        mem_backend: MemBackend {
            backend_type: "Uffd".to_string(),
            backend_path: uffd_socket.display().to_string(),
        },
        enable_diff_snapshots: Some(false),
        resume_vm: Some(true), // Resume immediately with network override
        // Use network_overrides to set TAP device on load (modern Firecracker API)
        network_overrides: Some(vec![NetworkOverride {
            iface_id: "eth0".to_string(),
            host_dev_name: network_config.tap_device.clone(),
        }]),
    }).await
        .context("loading snapshot with uffd backend")?;

    vm_state.status = VmStatus::Running;
    state_manager.save_state(&vm_state).await?;

    info!(
        vm_id = %vm_id,
        vm_name = %vm_name,
        "VM cloned successfully with UFFD memory sharing!"
    );
    println!("✓ VM '{}' cloned from snapshot '{}'", vm_name, args.snapshot);
    println!("  Memory pages shared via UFFD - true copy-on-write at page level!");
    println!("  Disk uses CoW overlay - writes isolated per VM");

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
    let _ = state_manager.delete_state(&vm_id).await;

    Ok(())
}
