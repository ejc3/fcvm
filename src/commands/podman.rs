use anyhow::{Context, Result};
use std::path::PathBuf;
use tokio::signal::unix::{signal, SignalKind};
use tracing::info;

use crate::cli::{PodmanArgs, PodmanCommands, RunArgs};
use crate::firecracker::VmManager;
use crate::network::{NetworkManager, PortMapping, RootlessNetwork, PrivilegedNetwork};
use crate::storage::DiskManager;
use crate::state::{StateManager, VmState, VmStatus, generate_vm_id};
use crate::Mode;

/// Main dispatcher for podman commands
pub async fn cmd_podman(args: PodmanArgs) -> Result<()> {
    match args.cmd {
        PodmanCommands::Run(run_args) => cmd_podman_run(run_args).await,
    }
}

async fn cmd_podman_run(args: RunArgs) -> Result<()> {
    info!("Starting fcvm podman run");

    // Ensure kernel and rootfs exist (auto-setup on first run)
    let kernel_path = crate::setup::ensure_kernel().await
        .context("setting up kernel")?;
    let base_rootfs = crate::setup::ensure_rootfs().await
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
    tokio::fs::create_dir_all(&data_dir).await
        .context("creating VM data directory")?;

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
    let vm_dir = data_dir.join("disks");
    let disk_manager = DiskManager::new(vm_id.clone(), base_rootfs.clone(), vm_dir);

    let rootfs_path = disk_manager.create_cow_disk().await
        .context("creating CoW disk")?;

    // Update network configuration in the overlay to match the assigned IPs
    if let (Some(guest_ip), Some(host_ip)) = (&network_config.guest_ip, &network_config.host_ip) {
        update_rootfs_network(&rootfs_path, guest_ip, host_ip).await
            .context("updating rootfs network configuration")?;
    }

    info!(rootfs = %rootfs_path.display(), "disk prepared");

    // Start Firecracker VM (disable file logging for now to avoid permission issues)
    let mut vm_manager = VmManager::new(vm_id.clone(), socket_path.clone(), None);
    let firecracker_bin = PathBuf::from("/usr/local/bin/firecracker");

    vm_manager.start(&firecracker_bin, None).await
        .context("starting Firecracker")?;

    let client = vm_manager.client()?;

    // Configure VM via API
    info!("configuring VM via Firecracker API");

    // Boot source
    client.set_boot_source(crate::firecracker::api::BootSource {
        kernel_image_path: kernel_path.display().to_string(),
        initrd_path: None,
        boot_args: Some("console=ttyS0 reboot=k panic=1 pci=off random.trust_cpu=1".to_string()),
    }).await?;

    // Machine config
    client.set_machine_config(crate::firecracker::api::MachineConfig {
        vcpu_count: args.cpu,
        mem_size_mib: args.mem,
        smt: Some(false),
        cpu_template: None,
        track_dirty_pages: Some(true), // Enable snapshot support
    }).await?;

    // Root drive
    client.add_drive(
        "rootfs",
        crate::firecracker::api::Drive {
            drive_id: "rootfs".to_string(),
            path_on_host: rootfs_path.display().to_string(),
            is_root_device: true,
            is_read_only: false,
            partuuid: None,
            rate_limiter: None,
        },
    ).await?;

    // Network interface - required for MMDS V2 in all modes
    // For rootless: create TAP device first, then slirp4netns will use it
    // For privileged: TAP is created and added to bridge
    client.add_network_interface(
        "eth0",
        crate::firecracker::api::NetworkInterface {
            iface_id: "eth0".to_string(),
            host_dev_name: network_config.tap_device.clone(),
            guest_mac: Some(network_config.guest_mac.clone()),
            rx_rate_limiter: None,
            tx_rate_limiter: None,
        },
    ).await?;

    // MMDS configuration - V2 works in rootless mode as long as interface exists
    client.set_mmds_config(crate::firecracker::api::MmdsConfig {
        version: "V2".to_string(),
        network_interfaces: Some(vec!["eth0".to_string()]),
        ipv4_address: Some("169.254.169.254".to_string()),
    }).await?;

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
                "cmd": args.cmd,
                "volumes": args.map,
            },
            "host-time": chrono::Utc::now().timestamp().to_string(),
        }
    });

    client.put_mmds(mmds_data).await?;

    // Configure entropy device (virtio-rng) for better random number generation
    client.set_entropy_device(crate::firecracker::api::EntropyDevice {
        rate_limiter: None,
    }).await?;

    // Balloon (if specified)
    if let Some(balloon_mib) = args.balloon {
        client.set_balloon(crate::firecracker::api::Balloon {
            amount_mib: balloon_mib,
            deflate_on_oom: true,
            stats_polling_interval_s: Some(1),
        }).await?;
    }

    // Start VM
    client.put_action(crate::firecracker::api::InstanceAction::InstanceStart).await?;

    vm_state.status = VmStatus::Running;
    state_manager.save_state(&vm_state).await?;

    info!(vm_id = %vm_id, "VM started successfully");

    // Note: No need for slirp4netns - we use static IP + NAT routing
    // TAP device is already configured with IP and iptables rules in network setup

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

/// Update network configuration in the rootfs overlay
async fn update_rootfs_network(rootfs_path: &std::path::Path, guest_ip: &str, gateway_ip: &str) -> Result<()> {
    use std::process::Command;

    // Mount the rootfs
    let mount_point = std::path::PathBuf::from("/tmp/fcvm-rootfs-mount");
    tokio::fs::create_dir_all(&mount_point).await?;

    let output = Command::new("sudo")
        .args(&["mount", "-o", "loop", rootfs_path.to_str().unwrap(), mount_point.to_str().unwrap()])
        .output()?;

    if !output.status.success() {
        anyhow::bail!("failed to mount rootfs: {}", String::from_utf8_lossy(&output.stderr));
    }

    // Extract netmask from guest IP (assume /24 for simplicity)
    let netmask = "255.255.255.0";

    // Write network interfaces config with dynamic IPs
    // Add MMDS (169.254.169.254) as link-local on eth0
    // This is required for Fire cracker MMDS V2 - packets go directly to the interface
    let interfaces_config = format!(r#"auto lo
iface lo inet loopback

auto eth0
iface eth0 inet static
    address {}
    netmask {}
    gateway {}
    up ip route add 169.254.169.254/32 dev eth0
"#, guest_ip, netmask, gateway_ip);

    let interfaces_path = mount_point.join("etc/network/interfaces");
    tokio::fs::write(&interfaces_path, interfaces_config).await
        .context("writing network interfaces config")?;

    // Unmount
    let _ = Command::new("sudo")
        .args(&["umount", mount_point.to_str().unwrap()])
        .output()?;

    Ok(())
}
