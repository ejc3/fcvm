mod cli;

use anyhow::{Context, Result};
use clap::Parser;
use cli::{Cli, Commands, RunArgs, CloneArgs, NameArgs};
mod lib;
use lib::{Mode, network::*, storage::*, firecracker::*, readiness::*, state::*};
use std::path::PathBuf;
use tokio::signal::unix::{signal, SignalKind};
use tracing::{info, error};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive(tracing::Level::INFO.into()))
        .with_target(false)
        .init();

    let cli = Cli::parse();
    let result = match cli.cmd {
        Commands::Run(args) => cmd_run(args).await,
        Commands::Clone(args) => cmd_clone(args).await,
        Commands::Stop(args) => cmd_stop(args).await,
        Commands::Ls => cmd_ls().await,
        Commands::Inspect(args) => cmd_inspect(args).await,
        Commands::Logs(args) => cmd_logs(args).await,
        Commands::Top => cmd_top().await,
    };

    if let Err(e) = &result {
        error!("Error: {:#}", e);
        std::process::exit(1);
    }

    result
}

async fn cmd_run(args: RunArgs) -> Result<()> {
    info!("Starting fcvm run");

    // Generate VM ID
    let vm_id = generate_vm_id();
    let vm_name = args.name.clone().unwrap_or_else(|| vm_id.clone());

    // Parse port mappings
    let port_mappings: Vec<PortMapping> = args
        .publish
        .iter()
        .map(|s| PortMapping::parse(s))
        .collect::<Result<Vec<_>>>()
        .context("parsing port mappings")?;

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

    info!(mode = ?mode, vm_id = %vm_id, "detected execution mode");

    // Setup paths
    let data_dir = PathBuf::from(format!("/tmp/fcvm/{}", vm_id));
    let socket_path = data_dir.join("firecracker.sock");
    let log_path = data_dir.join("firecracker.log");

    // Create VM state
    let mut vm_state = VmState::new(vm_id.clone(), args.image.clone(), args.cpu, args.mem);
    vm_state.name = Some(vm_name.clone());
    vm_state.config.env = args.env.clone();
    vm_state.config.volumes = args.map.clone();

    // Initialize state manager
    let state_manager = StateManager::new(PathBuf::from("/tmp/fcvm/state"));
    state_manager.init().await?;

    // Setup networking
    let tap_device = format!("tap-{}", &vm_id[..8]);
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

    info!(tap = %network_config.tap_device, mac = %network_config.guest_mac, "network configured");

    // Setup storage
    let base_rootfs = PathBuf::from("/var/lib/fcvm/rootfs/base.ext4");
    let vm_dir = data_dir.join("disks");
    let disk_manager = DiskManager::new(vm_id.clone(), base_rootfs, vm_dir);

    let rootfs_path = disk_manager.create_cow_disk().await
        .context("creating CoW disk")?;

    info!(rootfs = %rootfs_path.display(), "disk prepared");

    // Start Firecracker VM
    let mut vm_manager = VmManager::new(vm_id.clone(), socket_path.clone(), Some(log_path));
    let firecracker_bin = PathBuf::from("/usr/local/bin/firecracker");

    vm_manager.start(&firecracker_bin, None).await
        .context("starting Firecracker")?;

    let client = vm_manager.client()?;

    // Configure VM via API
    info!("configuring VM via Firecracker API");

    // Boot source
    client.set_boot_source(firecracker::api::BootSource {
        kernel_image_path: "/var/lib/fcvm/kernels/vmlinux.bin".to_string(),
        initrd_path: None,
        boot_args: Some("console=ttyS0 reboot=k panic=1 pci=off".to_string()),
    }).await?;

    // Machine config
    client.set_machine_config(firecracker::api::MachineConfig {
        vcpu_count: args.cpu,
        mem_size_mib: args.mem,
        smt: Some(false),
        cpu_template: None,
        track_dirty_pages: args.save_snapshot.is_some().then(|| true),
    }).await?;

    // Root drive
    client.add_drive(
        "rootfs",
        firecracker::api::Drive {
            drive_id: "rootfs".to_string(),
            path_on_host: rootfs_path.display().to_string(),
            is_root_device: true,
            is_read_only: false,
            partuuid: None,
            rate_limiter: None,
        },
    ).await?;

    // Network interface
    client.add_network_interface(
        "eth0",
        firecracker::api::NetworkInterface {
            iface_id: "eth0".to_string(),
            host_dev_name: network_config.tap_device.clone(),
            guest_mac: Some(network_config.guest_mac.clone()),
            rx_rate_limiter: None,
            tx_rate_limiter: None,
        },
    ).await?;

    // MMDS configuration
    client.set_mmds_config(firecracker::api::MmdsConfig {
        version: "V2".to_string(),
        network_interfaces: Some(vec!["eth0".to_string()]),
        ipv4_address: Some("169.254.169.254".to_string()),
    }).await?;

    // MMDS data (container plan)
    let mmds_data = serde_json::json!({
        "image": args.image,
        "env": args.env.iter().map(|e| {
            let parts: Vec<&str> = e.splitn(2, '=').collect();
            (parts[0], parts.get(1).copied().unwrap_or(""))
        }).collect::<std::collections::HashMap<_, _>>(),
        "cmd": args.cmd,
        "volumes": args.map,
    });

    client.put_mmds(mmds_data).await?;

    // Balloon (if specified)
    if let Some(balloon_mib) = args.balloon {
        client.set_balloon(firecracker::api::Balloon {
            amount_mib: balloon_mib,
            deflate_on_oom: true,
            stats_polling_interval_s: Some(1),
        }).await?;
    }

    // Start VM
    client.put_action(firecracker::api::InstanceAction::InstanceStart).await?;

    vm_state.status = VmStatus::Running;
    state_manager.save_state(&vm_state).await?;

    info!(vm_id = %vm_id, "VM started successfully");

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

async fn cmd_clone(args: CloneArgs) -> Result<()> {
    info!("fcvm clone - not yet implemented");
    println!("Clone from snapshot: {} (snapshot: {})", args.name, args.snapshot);
    Ok(())
}

async fn cmd_stop(args: NameArgs) -> Result<()> {
    info!("fcvm stop - not yet implemented");
    println!("Stop VM: {}", args.name);
    Ok(())
}

async fn cmd_ls() -> Result<()> {
    info!("fcvm ls");
    let state_manager = StateManager::new(PathBuf::from("/tmp/fcvm/state"));
    let vms = state_manager.list_vms().await?;

    println!("{:<20} {:<10} {:<6} {:<8} {:<20}", "NAME", "STATUS", "CPU", "MEM(MB)", "CREATED");
    println!("{}", "-".repeat(80));

    for vm in vms {
        println!(
            "{:<20} {:<10} {:<6} {:<8} {:<20}",
            vm.name.unwrap_or(vm.vm_id),
            format!("{:?}", vm.status),
            vm.config.vcpu,
            vm.config.memory_mib,
            vm.created_at.format("%Y-%m-%d %H:%M:%S")
        );
    }

    Ok(())
}

async fn cmd_inspect(args: NameArgs) -> Result<()> {
    info!("fcvm inspect - not yet implemented");
    println!("Inspect VM: {}", args.name);
    Ok(())
}

async fn cmd_logs(args: NameArgs) -> Result<()> {
    info!("fcvm logs - not yet implemented");
    println!("Logs for VM: {}", args.name);
    Ok(())
}

async fn cmd_top() -> Result<()> {
    info!("fcvm top - not yet implemented");
    println!("VM resource usage - not yet implemented");
    Ok(())
}
