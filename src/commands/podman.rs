use anyhow::{Context, Result};
use std::path::PathBuf;
use tokio::signal::unix::{signal, SignalKind};
use tracing::{info, warn};

use crate::cli::{NetworkMode, PodmanArgs, PodmanCommands, RunArgs};
use crate::firecracker::VmManager;
use crate::network::{BridgedNetwork, NetworkManager, PortMapping, SlirpNetwork};
use crate::paths;
use crate::state::{generate_vm_id, truncate_id, validate_vm_name, StateManager, VmState};
use crate::storage::DiskManager;

/// Main dispatcher for podman commands
pub async fn cmd_podman(args: PodmanArgs) -> Result<()> {
    match args.cmd {
        PodmanCommands::Run(run_args) => cmd_podman_run(run_args).await,
    }
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

    let socket_path = data_dir.join("firecracker.sock");

    // Create VM state
    let mut vm_state = VmState::new(vm_id.clone(), args.image.clone(), args.cpu, args.mem);
    vm_state.name = Some(vm_name.clone());
    vm_state.config.env = args.env.clone();
    vm_state.config.volumes = args.map.clone();

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

            Box::new(
                SlirpNetwork::new(vm_id.clone(), tap_device.clone(), port_mappings.clone())
                    .with_loopback_ip(loopback_ip),
            )
        }
    };

    let network_config = network.setup().await.context("setting up network")?;

    info!(tap = %network_config.tap_device, mac = %network_config.guest_mac, "network configured");

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
    )
    .await;

    // If setup failed, cleanup network before propagating error
    if let Err(e) = setup_result {
        warn!("VM setup failed, cleaning up network resources");
        if let Err(cleanup_err) = network.cleanup().await {
            warn!("failed to cleanup network after setup error: {}", cleanup_err);
        }
        return Err(e);
    }

    let mut vm_manager = setup_result.unwrap();

    info!(vm_id = %vm_id, "VM started successfully");

    // Spawn health monitor task (store handle for cancellation)
    let health_monitor_handle = crate::health::spawn_health_monitor(vm_id.clone(), vm_state.pid);

    // Note: For rootless mode, slirp4netns wraps Firecracker and configures TAP automatically
    // For bridged mode, TAP is configured via NAT routing during network setup

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

/// Helper function that runs VM setup and returns VmManager on success.
/// This allows the caller to cleanup network resources on error.
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
) -> Result<VmManager> {
    // Setup storage
    let vm_dir = data_dir.join("disks");
    let disk_manager = DiskManager::new(vm_id.to_string(), base_rootfs.to_path_buf(), vm_dir);

    let rootfs_path = disk_manager
        .create_cow_disk()
        .await
        .context("creating CoW disk")?;

    info!(rootfs = %rootfs_path.display(), "disk prepared");

    let vm_name = args.name.clone();
    info!(vm_name = %vm_name, vm_id = %vm_id, "creating VM manager");
    let mut vm_manager = VmManager::new(vm_id.to_string(), socket_path.to_path_buf(), None);

    // Set VM name for logging
    vm_manager.set_vm_name(vm_name);

    // Configure namespace isolation based on network type
    if let Some(bridged_net) = network.as_any().downcast_ref::<BridgedNetwork>() {
        // Bridged mode: use pre-created network namespace
        if let Some(ns_id) = bridged_net.namespace_id() {
            info!(namespace = %ns_id, "configuring VM to run in network namespace");
            vm_manager.set_namespace(ns_id.to_string());
        }
    } else if let Some(slirp_net) = network.as_any().downcast_ref::<SlirpNetwork>() {
        // Rootless mode: use slirp4netns with dual-TAP architecture
        // Creates user+net namespace with two TAPs: slirp0 (for slirp4netns) and tap0 (for Firecracker)
        // IP forwarding connects them for outbound connectivity
        let wrapper_cmd = slirp_net.build_wrapper_command();
        info!(wrapper = %slirp_net.wrapper_command_string(), "configuring VM for rootless operation with slirp4netns");
        vm_manager.set_namespace_wrapper(wrapper_cmd);
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
            "console=ttyS0 reboot=k panic=1 pci=off random.trust_cpu=1 ip={}::{}:255.255.255.252::eth0:off",
            guest_ip_clean, host_ip_clean
        )
    } else {
        "console=ttyS0 reboot=k panic=1 pci=off random.trust_cpu=1".to_string()
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
    network
        .post_start(vm_pid)
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
                "volumes": args.map,
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

    Ok(vm_manager)
}
