use std::net::ToSocketAddrs;

use anyhow::{Context, Result};
use tracing::{debug, info, warn};
use url::Url;

use crate::cli::RunArgs;
use crate::firecracker::VmManager;
use crate::network::{BridgedNetwork, NetworkManager, SlirpNetwork};
use crate::state::{StateManager, VmState};
use crate::storage::DiskManager;

use super::namespace::setup_rootless_namespace;
use super::types::VolumeMapping;

use crate::commands::common::VSOCK_VOLUME_PORT_BASE;

/// Read the current user's subordinate UID range start from /etc/subuid.
/// Returns None if the file doesn't exist or the user has no entry.
fn get_host_subuid_start() -> Option<u64> {
    parse_subid_file("/etc/subuid").map(|(start, _)| start)
}

/// Read the current user's subordinate UID range count from /etc/subuid.
fn get_host_subuid_count() -> Option<u64> {
    parse_subid_file("/etc/subuid").map(|(_, count)| count)
}

/// Parse /etc/subuid or /etc/subgid for the current user.
/// Format: username:start:count
fn parse_subid_file(path: &str) -> Option<(u64, u64)> {
    let username = std::env::var("USER")
        .or_else(|_| {
            nix::unistd::User::from_uid(nix::unistd::getuid())
                .ok()
                .flatten()
                .map(|u| u.name)
                .ok_or(())
        })
        .ok()?;
    let content = std::fs::read_to_string(path).ok()?;
    for line in content.lines() {
        let parts: Vec<&str> = line.split(':').collect();
        if parts.len() >= 3 && parts[0] == username {
            let start = parts[1].parse().ok()?;
            let count = parts[2].parse().ok()?;
            return Some((start, count));
        }
    }
    None
}

/// Resolve a proxy URL's hostname to an IP address.
///
/// VMs using slirp4netns with --enable-ipv6 can reach both IPv4 (via 10.0.2.2 gateway)
/// and IPv6 (via fd00::2 gateway) addresses. We prefer IPv4 but fall back to IPv6.
/// Returns None only if the hostname can't be resolved at all.
fn resolve_proxy_url(url: &str) -> Option<String> {
    // Proxies should include a scheme, but default to http for legacy inputs.
    let normalized = if url.contains("://") {
        url.to_string()
    } else {
        format!("http://{}", url)
    };

    let parsed = match Url::parse(&normalized) {
        Ok(parsed) => parsed,
        Err(e) => {
            warn!(url = %url, error = %e, "failed to parse proxy URL");
            return None;
        }
    };

    let host = match parsed.host_str() {
        Some(host) => host,
        None => {
            warn!(url = %url, "proxy URL has no host");
            return None;
        }
    };

    let port = match parsed.port_or_known_default() {
        Some(port) => port,
        None => {
            warn!(url = %url, "proxy URL has unknown default port");
            return None;
        }
    };

    // If the host is already an IP address (IPv4 or IPv6), use it directly.
    // This avoids double-resolution when detect_http_proxy() already resolved the hostname.
    match parsed.host() {
        Some(url::Host::Ipv4(ip)) => {
            debug!(url = %url, ip = %ip, "proxy URL already has IPv4 address");
            return Some(rebuild_proxy_url(&parsed, ip.to_string(), port));
        }
        Some(url::Host::Ipv6(ip)) => {
            debug!(url = %url, ip = %ip, "proxy URL already has IPv6 address");
            return Some(rebuild_proxy_url(&parsed, ip.to_string(), port));
        }
        _ => {}
    }

    // Try to resolve hostname to an IP address (prefer IPv4, fall back to IPv6)
    match (host, port).to_socket_addrs() {
        Ok(addrs) => {
            let addrs: Vec<_> = addrs.collect();

            // First pass: look for IPv4
            for addr in &addrs {
                if addr.is_ipv4() {
                    debug!(
                        original = %url,
                        resolved = %addr,
                        "resolved proxy hostname to IPv4"
                    );
                    return Some(rebuild_proxy_url(
                        &parsed,
                        addr.ip().to_string(),
                        addr.port(),
                    ));
                }
            }

            // Second pass: use IPv6 if no IPv4 available
            // With --enable-ipv6 and --outbound-addr6, VM can reach IPv6 via fd00::2 gateway
            for addr in &addrs {
                if addr.is_ipv6() {
                    info!(
                        original = %url,
                        resolved = %addr,
                        "resolved proxy hostname to IPv6 (no IPv4 available)"
                    );
                    return Some(rebuild_proxy_url(
                        &parsed,
                        addr.ip().to_string(),
                        addr.port(),
                    ));
                }
            }

            warn!(url = %url, "proxy resolved but no addresses found");
            None
        }
        Err(e) => {
            warn!(url = %url, error = %e, "failed to resolve proxy hostname");
            None
        }
    }
}

fn rebuild_proxy_url(parsed: &Url, host: String, port: u16) -> String {
    let mut authority = String::new();
    if !parsed.username().is_empty() {
        authority.push_str(parsed.username());
        if let Some(password) = parsed.password() {
            authority.push(':');
            authority.push_str(password);
        }
        authority.push('@');
    }

    // Preserve valid URL formatting for IPv6 literals.
    if host.contains(':') {
        authority.push('[');
        authority.push_str(&host);
        authority.push(']');
    } else {
        authority.push_str(&host);
    }
    authority.push(':');
    authority.push_str(&port.to_string());

    let mut rebuilt = format!("{}://{}{}", parsed.scheme(), authority, parsed.path());
    if let Some(query) = parsed.query() {
        rebuilt.push('?');
        rebuilt.push_str(query);
    }
    if let Some(fragment) = parsed.fragment() {
        rebuilt.push('#');
        rebuilt.push_str(fragment);
    }
    rebuilt
}

/// Build runtime boot arguments for the kernel command line.
///
/// These are per-instance values NOT included in the snapshot cache key:
/// network IP config, IPv6, DNS, strace, profile boot args, FUSE tuning.
pub(super) fn build_runtime_boot_args(
    args: &RunArgs,
    network_config: &crate::network::NetworkConfig,
    runtime_config: &crate::commands::common::RuntimeConfig,
) -> String {
    let mut boot_args = String::new();

    // Network configuration via kernel cmdline
    // Format: ip=<client-ip>:<server-ip>:<gw-ip>:<netmask>:<hostname>:<device>:<autoconf>:<dns0>
    if let (Some(guest_ip), Some(host_ip)) = (&network_config.guest_ip, &network_config.host_ip) {
        let guest_ip_clean = guest_ip.split('/').next().unwrap_or(guest_ip);
        let host_ip_clean = host_ip.split('/').next().unwrap_or(host_ip);
        let dns_suffix = network_config
            .dns_server
            .as_ref()
            .map(|dns| format!(":{}", dns))
            .unwrap_or_default();
        // Use /24 netmask for slirp4netns (10.0.2.0/24) or bridged (172.30.x.0/24)
        boot_args.push_str(&format!(
            "ip={}::{}:255.255.255.0::eth0:off{}",
            guest_ip_clean, host_ip_clean, dns_suffix
        ));
    }

    // IPv6 configuration via kernel cmdline (for rootless networking)
    // Format: ipv6=<client>|<gateway> - parsed by fc-agent to configure eth0
    // Uses | as delimiter since : is part of IPv6 addresses
    if let (Some(guest_ipv6), Some(host_ipv6)) =
        (&network_config.guest_ipv6, &network_config.host_ipv6)
    {
        if !boot_args.is_empty() {
            boot_args.push(' ');
        }
        boot_args.push_str(&format!("ipv6={}|{}", guest_ipv6, host_ipv6));
    }

    // Pass host DNS servers to guest for direct resolution (bypasses slirp's DNS proxy)
    // This is needed on IPv6-only hosts where slirp's 10.0.2.3 can't forward to IPv6 nameservers
    if let Ok(dns_servers) = crate::network::get_host_dns_servers() {
        if !boot_args.is_empty() {
            boot_args.push(' ');
        }
        // Use | delimiter since : is part of IPv6 addresses
        boot_args.push_str(&format!("fcvm_dns={}", dns_servers.join("|")));

        // Pass search domains for short hostname resolution (only when DNS servers are available)
        if let Ok(content) = std::fs::read_to_string("/run/systemd/resolve/resolv.conf")
            .or_else(|_| std::fs::read_to_string("/etc/resolv.conf"))
        {
            let search: Vec<&str> = content
                .lines()
                .filter_map(|l| l.trim().strip_prefix("search "))
                .next()
                .map(|s| s.split_whitespace().collect())
                .unwrap_or_default();
            if !search.is_empty() {
                if !boot_args.is_empty() {
                    boot_args.push(' ');
                }
                boot_args.push_str(&format!("fcvm_dns_search={}", search.join("|")));
            }
        }
    }

    // Enable fc-agent strace debugging if requested
    if args.strace_agent {
        if !boot_args.is_empty() {
            boot_args.push(' ');
        }
        boot_args.push_str("fc_agent_strace=1");
        info!("fc-agent strace debugging enabled - output will be in /tmp/fc-agent.strace");
    }

    // Additional boot args from RuntimeConfig (kernel profile) or FCVM_BOOT_ARGS env var
    let extra_boot_args = runtime_config
        .boot_args
        .clone()
        .or_else(|| std::env::var("FCVM_BOOT_ARGS").ok());
    if let Some(ref extra) = extra_boot_args {
        if !boot_args.is_empty() {
            boot_args.push(' ');
        }
        boot_args.push_str(extra);
    }

    // Pass FUSE reader count to fc-agent via kernel command line (from RuntimeConfig or env)
    let fuse_readers = runtime_config
        .fuse_readers
        .map(|r| r.to_string())
        .or_else(|| std::env::var("FCVM_FUSE_READERS").ok());
    if let Some(readers) = fuse_readers {
        if !boot_args.is_empty() {
            boot_args.push(' ');
        }
        boot_args.push_str(&format!("fuse_readers={}", readers));
    }

    // Pass FUSE trace rate to fc-agent via kernel command line.
    if let Ok(rate) = std::env::var("FCVM_FUSE_TRACE_RATE") {
        if !boot_args.is_empty() {
            boot_args.push(' ');
        }
        boot_args.push_str(&format!("fuse_trace_rate={}", rate));
    }

    // Pass FUSE max_write to fc-agent via kernel command line.
    if let Ok(max_write) = std::env::var("FCVM_FUSE_MAX_WRITE") {
        if !boot_args.is_empty() {
            boot_args.push(' ');
        }
        boot_args.push_str(&format!("fuse_max_write={}", max_write));
    }

    // Pass FUSE writeback cache disable flag to fc-agent via kernel command line.
    if std::env::var("FCVM_NO_WRITEBACK_CACHE").is_ok() {
        if !boot_args.is_empty() {
            boot_args.push(' ');
        }
        boot_args.push_str("no_writeback_cache=1");
    }

    boot_args
}

/// Attach extra disks (--disk, --disk-dir, image archive) to the VM.
///
/// Returns the list of extra disks and optionally the image archive device path.
pub(super) async fn attach_extra_disks(
    args: &RunArgs,
    client: &crate::firecracker::FirecrackerClient,
    data_dir: &std::path::Path,
    image_disk_path: Option<&std::path::Path>,
    image_disk_read_only: bool,
) -> Result<(Vec<crate::state::types::ExtraDisk>, Option<String>)> {
    let mut extra_disks = Vec::new();

    // Extra disks (appear as /dev/vdb, /dev/vdc, etc.)
    // Parse format: HOST_PATH:GUEST_MOUNT[:ro]
    for (i, disk_spec) in args.disk.iter().enumerate() {
        // Check for :ro suffix
        let (spec_without_ro, read_only) = if disk_spec.ends_with(":ro") {
            (&disk_spec[..disk_spec.len() - 3], true)
        } else {
            (disk_spec.as_str(), false)
        };

        // Split HOST_PATH:GUEST_MOUNT
        let parts: Vec<&str> = spec_without_ro.splitn(2, ':').collect();
        if parts.len() != 2 {
            anyhow::bail!(
                "Invalid disk spec '{}'. Expected format: HOST_PATH:GUEST_MOUNT[:ro]",
                disk_spec
            );
        }
        let path_str = parts[0];
        let mount_path = parts[1].to_string();

        // Validate mount path is absolute
        if !mount_path.starts_with('/') {
            anyhow::bail!(
                "Disk mount path must be absolute: {} (got '{}')",
                disk_spec,
                mount_path
            );
        }

        let drive_id = format!("disk{}", i);
        let disk_path = std::path::Path::new(path_str);
        if !disk_path.exists() {
            anyhow::bail!("Disk not found: {}", disk_path.display());
        }
        let abs_path = disk_path.canonicalize().context(format!(
            "Failed to resolve disk path: {}",
            disk_path.display()
        ))?;

        extra_disks.push(crate::state::types::ExtraDisk {
            path: abs_path.display().to_string(),
            mount_path: mount_path.clone(),
            read_only,
        });

        info!(
            "Adding extra disk: {} -> /dev/vd{} -> {} ({})",
            abs_path.display(),
            (b'b' + i as u8) as char,
            mount_path,
            if read_only { "ro" } else { "rw" }
        );
        client
            .add_drive(
                &drive_id,
                crate::firecracker::api::Drive {
                    drive_id: drive_id.clone(),
                    path_on_host: abs_path.display().to_string(),
                    is_root_device: false,
                    is_read_only: read_only,
                    partuuid: None,
                    rate_limiter: None,
                },
            )
            .await?;
    }

    // Process --disk-dir: create disk images from directories
    // Images are stored in VM's data directory (cleaned up on exit)
    let disk_offset = args.disk.len();
    for (i, dir_spec) in args.disk_dir.iter().enumerate() {
        // Check for :ro suffix
        let (spec_without_ro, read_only) = if dir_spec.ends_with(":ro") {
            (&dir_spec[..dir_spec.len() - 3], true)
        } else {
            (dir_spec.as_str(), false)
        };

        // Split HOST_DIR:GUEST_MOUNT
        let parts: Vec<&str> = spec_without_ro.splitn(2, ':').collect();
        if parts.len() != 2 {
            anyhow::bail!(
                "Invalid disk-dir spec '{}'. Expected format: HOST_DIR:GUEST_MOUNT[:ro]",
                dir_spec
            );
        }
        let source_dir = std::path::Path::new(parts[0]);
        let mount_path = parts[1].to_string();

        // Validate source directory exists
        if !source_dir.is_dir() {
            anyhow::bail!(
                "Source directory does not exist or is not a directory: {}",
                source_dir.display()
            );
        }

        // Validate mount path is absolute
        if !mount_path.starts_with('/') {
            anyhow::bail!(
                "Disk mount path must be absolute: {} (got '{}')",
                dir_spec,
                mount_path
            );
        }

        // Create disk image in VM's data directory
        let disk_idx = disk_offset + i;
        let image_path = data_dir
            .join("disks")
            .join(format!("disk-dir-{}.raw", disk_idx));
        super::image::create_disk_from_dir(source_dir, &image_path, false).await?;

        let drive_id = format!("disk{}", disk_idx);

        extra_disks.push(crate::state::types::ExtraDisk {
            path: image_path.display().to_string(),
            mount_path: mount_path.clone(),
            read_only,
        });

        info!(
            "Adding disk from dir: {} -> {} -> /dev/vd{} -> {} ({})",
            source_dir.display(),
            image_path.display(),
            (b'b' + disk_idx as u8) as char,
            mount_path,
            if read_only { "ro" } else { "rw" }
        );
        client
            .add_drive(
                &drive_id,
                crate::firecracker::api::Drive {
                    drive_id: drive_id.clone(),
                    path_on_host: image_path.display().to_string(),
                    is_root_device: false,
                    is_read_only: read_only,
                    partuuid: None,
                    rate_limiter: None,
                },
            )
            .await?;
    }

    // Attach image disk as a block device.
    let image_device = if let Some(disk_path) = image_disk_path {
        let disk_idx = args.disk.len() + args.disk_dir.len();
        let drive_id = format!("disk{}", disk_idx);
        let device = format!("/dev/vd{}", (b'b' + disk_idx as u8) as char);

        info!(
            "Attaching image disk as block device: {} -> {} (read_only={})",
            disk_path.display(),
            device,
            image_disk_read_only,
        );
        client
            .add_drive(
                &drive_id,
                crate::firecracker::api::Drive {
                    drive_id: drive_id.clone(),
                    path_on_host: disk_path.display().to_string(),
                    is_root_device: false,
                    is_read_only: image_disk_read_only,
                    partuuid: None,
                    rate_limiter: None,
                },
            )
            .await?;
        Some(device)
    } else {
        None
    };

    Ok((extra_disks, image_device))
}

/// Build MMDS data (container plan) and send it to Firecracker.
///
/// User-input fields come from `launch_config` (part of snapshot cache key).
/// Runtime-only values (network, proxies, timestamps) are computed here.
pub(super) async fn build_and_send_mmds(
    launch_config: &crate::firecracker::FirecrackerConfig,
    client: &crate::firecracker::FirecrackerClient,
    network_config: &crate::network::NetworkConfig,
    vm_state: &VmState,
    volume_mappings: &[VolumeMapping],
    image_device: Option<String>,
) -> Result<()> {
    // Build volume mount info for MMDS
    // Format: { guest_path, vsock_port, read_only }
    let volumes: Vec<serde_json::Value> = volume_mappings
        .iter()
        .enumerate()
        .map(|(idx, v)| {
            serde_json::json!({
                "guest_path": v.guest_path,
                "vsock_port": VSOCK_VOLUME_PORT_BASE + idx as u32,
                "read_only": v.read_only,
            })
        })
        .collect();

    // Build extra disk info for MMDS
    // Format: { device, mount_path, read_only }
    // Disks are added as /dev/vdb, /dev/vdc, etc.
    let extra_disks: Vec<serde_json::Value> = vm_state
        .config
        .extra_disks
        .iter()
        .enumerate()
        .map(|(idx, disk)| {
            serde_json::json!({
                "device": format!("/dev/vd{}", (b'b' + idx as u8) as char),
                "mount_path": &disk.mount_path,
                "read_only": disk.read_only,
            })
        })
        .collect();

    // NFS mounts for guest
    // Format: { host_ip, host_path, mount_path, read_only }
    let nfs_mounts: Vec<serde_json::Value> = vm_state
        .config
        .nfs_shares
        .iter()
        .map(|share| {
            serde_json::json!({
                "host_ip": network_config.host_ip.as_ref().unwrap_or(&"".to_string()),
                "host_path": &share.host_path,
                "mount_path": &share.mount_path,
                "read_only": share.read_only,
            })
        })
        .collect();

    // Resolve proxy URLs — runtime behavior, not part of cache key.
    // Use network-provided proxy, or fall back to environment variables.
    // Resolve hostname to IPv4 since slirp VMs can only reach IPv4 addresses.
    let http_proxy = network_config
        .http_proxy
        .clone()
        .or_else(|| std::env::var("http_proxy").ok())
        .or_else(|| std::env::var("HTTP_PROXY").ok())
        .and_then(|url| resolve_proxy_url(&url));
    let https_proxy = network_config
        .http_proxy
        .clone()
        .or_else(|| std::env::var("https_proxy").ok())
        .or_else(|| std::env::var("HTTPS_PROXY").ok())
        .or_else(|| std::env::var("http_proxy").ok())
        .or_else(|| std::env::var("HTTP_PROXY").ok())
        .and_then(|url| resolve_proxy_url(&url));
    let no_proxy = std::env::var("no_proxy")
        .or_else(|_| std::env::var("NO_PROXY"))
        .ok();

    let runtime = crate::firecracker::MmdsRuntime {
        volumes,
        extra_disks,
        nfs_mounts,
        image_device,
        http_proxy,
        https_proxy,
        no_proxy,
        subuid_start: launch_config
            .user
            .as_ref()
            .and_then(|_| get_host_subuid_start()),
        subuid_count: launch_config
            .user
            .as_ref()
            .and_then(|_| get_host_subuid_count()),
        host_time: chrono::Utc::now().timestamp().to_string(),
    };

    let mmds_data = launch_config.to_mmds_json(runtime);
    client.put_mmds(mmds_data).await?;
    Ok(())
}

/// Set up NFS exports for VM.
/// Creates /etc/exports.d/fcvm-{vm_id}.exports and refreshes exportfs.
pub(super) async fn setup_nfs_exports(
    vm_id: &str,
    shares: &[crate::state::types::NfsShare],
    network_config: &crate::network::NetworkConfig,
) -> Result<()> {
    use std::io::Write;

    // Ensure NFS server is running
    let status = tokio::process::Command::new("systemctl")
        .args(["is-active", "nfs-server"])
        .output()
        .await?;

    if !status.status.success() {
        info!("Starting NFS server...");
        let start = tokio::process::Command::new("systemctl")
            .args(["start", "nfs-server"])
            .status()
            .await?;
        if !start.success() {
            anyhow::bail!("Failed to start NFS server. Run: sudo apt install nfs-kernel-server");
        }
    }

    // Create exports directory if needed
    tokio::fs::create_dir_all("/etc/exports.d").await.ok();

    // Guest IP for access control (use /30 subnet for the VM)
    let guest_ip = network_config
        .guest_ip
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("No guest IP configured for NFS"))?;

    // Build exports file content
    let mut exports = String::new();
    for share in shares {
        let opts = if share.read_only {
            "ro,sync,no_subtree_check,no_root_squash"
        } else {
            "rw,sync,no_subtree_check,no_root_squash"
        };
        exports.push_str(&format!("{} {}({})\n", share.host_path, guest_ip, opts));
    }

    // Write exports file
    let exports_path = format!("/etc/exports.d/fcvm-{}.exports", vm_id);
    let mut file = std::fs::File::create(&exports_path)?;
    file.write_all(exports.as_bytes())?;

    info!("Created NFS exports: {}", exports_path);

    // Refresh exports
    let refresh = tokio::process::Command::new("exportfs")
        .arg("-ra")
        .status()
        .await?;

    if !refresh.success() {
        warn!("exportfs -ra failed, NFS mounts may not work");
    }

    Ok(())
}

/// Clean up NFS exports for VM
pub(super) async fn cleanup_nfs_exports(vm_id: &str) {
    let exports_path = format!("/etc/exports.d/fcvm-{}.exports", vm_id);
    if std::path::Path::new(&exports_path).exists() {
        if let Err(e) = tokio::fs::remove_file(&exports_path).await {
            warn!("Failed to remove NFS exports file: {}", e);
        } else {
            // Refresh exports to unregister
            let _ = tokio::process::Command::new("exportfs")
                .arg("-ra")
                .status()
                .await;
            debug!("Cleaned up NFS exports: {}", exports_path);
        }
    }
}

/// Helper function that runs VM setup and returns VmManager on success.
/// This allows the caller to cleanup network resources on error.
/// For rootless mode, also returns the holder process that keeps the namespace alive.
#[allow(clippy::too_many_arguments)]
pub(super) async fn run_vm_setup(
    args: &RunArgs,
    vm_id: &str,
    data_dir: &std::path::Path,
    base_rootfs: &std::path::Path,
    socket_path: &std::path::Path,
    kernel_path: &std::path::Path,
    initrd_path: &std::path::Path,
    network_config: &crate::network::NetworkConfig,
    network: &mut dyn NetworkManager,
    cmd_args: Option<Vec<String>>,
    state_manager: &StateManager,
    vm_state: &mut VmState,
    volume_mappings: &[VolumeMapping],
    vsock_socket_path: &std::path::Path,
    image_disk_path: Option<&std::path::Path>,
    fc_config: Option<crate::firecracker::FirecrackerConfig>,
    runtime_config: &crate::commands::common::RuntimeConfig,
) -> Result<(VmManager, Option<tokio::process::Child>)> {
    // Setup storage - just need CoW copy (fc-agent is injected via initrd at boot)
    let vm_dir = data_dir.join("disks");
    let disk_manager =
        DiskManager::new(vm_id.to_string(), base_rootfs.to_path_buf(), vm_dir.clone());

    let rootfs_path = disk_manager
        .create_cow_disk()
        .await
        .context("creating CoW disk")?;

    // Estimate space needed for container image extraction inside VM.
    // Overlay and btrfs modes use separate block devices — no rootfs impact.
    // Archive mode extracts layers onto the rootfs via podman load.
    // podman load extracts layers to /var/tmp first, then copies to storage,
    // so we need ~3x the archive size for safety margin.
    let resolved_mode = super::resolve_image_mode(args, runtime_config);
    let image_overhead = if resolved_mode == crate::firecracker::ImageMode::Archive {
        if let Some(disk_path) = image_disk_path {
            match tokio::fs::metadata(disk_path).await {
                Ok(meta) => meta.len() * 3,
                Err(_) => 0,
            }
        } else {
            0
        }
    } else {
        0
    };

    // Ensure minimum free space (from --rootfs-size) plus room for the container image
    crate::storage::disk::ensure_free_space(&rootfs_path, &args.rootfs_size, image_overhead)
        .await
        .context("ensuring rootfs free space")?;

    info!(rootfs = %rootfs_path.display(), "disk prepared (fc-agent baked into Layer 2)");

    let vm_name = args.name.clone();
    info!(vm_name = %vm_name, vm_id = %vm_id, "creating VM manager");
    // Enable Firecracker debug logging
    let fc_log_path = data_dir.join("firecracker.log");
    let _ = std::fs::File::create(&fc_log_path);
    let mut vm_manager = VmManager::new(
        vm_id.to_string(),
        socket_path.to_path_buf(),
        Some(fc_log_path),
    );

    // Set VM name for logging
    vm_manager.set_vm_name(vm_name);

    // Configure namespace isolation based on network type
    let holder_child: Option<tokio::process::Child>;

    if let Some(bridged_net) = network.as_any().downcast_ref::<BridgedNetwork>() {
        // Bridged mode: use pre-created network namespace
        holder_child = None;
        if let Some(ns_id) = bridged_net.namespace_id() {
            info!(namespace = %ns_id, "configuring VM to run in network namespace");
            vm_manager.set_namespace(ns_id.to_string());
        }
    } else if let Some(slirp_net) = network.as_any().downcast_ref::<SlirpNetwork>() {
        holder_child = Some(
            setup_rootless_namespace(slirp_net, network_config, &mut vm_manager, vm_state).await?,
        );
    } else {
        holder_child = None;
    }

    let firecracker_bin = crate::commands::common::find_firecracker(runtime_config)?;

    // Use RuntimeConfig firecracker_args (from --kernel-profile), falling back to env var
    let fc_args_env = std::env::var("FCVM_FIRECRACKER_ARGS").ok();
    let fc_args = runtime_config
        .firecracker_args
        .as_deref()
        .or(fc_args_env.as_deref());

    vm_manager
        .start(&firecracker_bin, None, fc_args)
        .await
        .context("starting Firecracker")?;

    let vm_pid = vm_manager.pid()?;
    let client = vm_manager.client()?;

    // Configure VM via API
    info!("configuring VM via Firecracker API");

    // Build FirecrackerConfig for launch (single source of truth for VM config)
    // Use fc_config from cache check if available, otherwise build fresh.
    // IMPORTANT: fc_config uses content-addressed base_rootfs path for cache key,
    // but launch must use per-instance CoW copy path (rootfs_path).
    //
    // Enable dirty page tracking only when snapshots are enabled (fc_config is Some).
    // With hugepages, dirty tracking splits 2MB Stage 2 block mappings to 4K,
    // so we avoid it when snapshots are disabled (--no-snapshot / FCVM_NO_SNAPSHOT).
    let track_dirty_pages = fc_config.is_some();
    let launch_config = fc_config
        .map(|config| config.with_rootfs_path(rootfs_path.to_path_buf()))
        .unwrap_or_else(|| {
            use crate::firecracker::FcNetworkMode;
            let network_mode = match args.network {
                crate::cli::args::NetworkMode::Bridged => FcNetworkMode::Bridged,
                crate::cli::args::NetworkMode::Rootless => FcNetworkMode::Rootless,
            };
            // Collect extra disk specifications
            let mut extra_disks: Vec<String> = Vec::new();
            extra_disks.extend(args.disk.iter().cloned());
            extra_disks.extend(args.disk_dir.iter().cloned());
            extra_disks.extend(args.nfs.iter().cloned());
            // Collect env vars and volume mounts for cache key
            let env_vars: Vec<String> = args.env.to_vec();
            let volume_mounts: Vec<String> = args.map.to_vec();
            let image_mode = super::resolve_image_mode(args, runtime_config);

            crate::firecracker::FirecrackerConfig::new(
                kernel_path.to_path_buf(),
                initrd_path.to_path_buf(),
                rootfs_path.to_path_buf(),
                args.image.clone(),
                cmd_args.clone(),
                args.cpu,
                args.mem,
                network_mode,
                crate::paths::data_dir(),
                extra_disks,
                env_vars,
                volume_mounts,
                args.privileged,
                args.tty,
                args.interactive,
                args.rootfs_size.clone(),
                args.health_check.clone(),
                args.hugepages,
                args.user.clone(),
                args.forward_localhost.clone(),
                image_mode,
            )
        });

    // Build runtime boot args and apply FirecrackerConfig
    let runtime_boot_args = build_runtime_boot_args(args, network_config, runtime_config);
    launch_config
        .apply(client, &runtime_boot_args, track_dirty_pages)
        .await?;

    // Attach extra disks and image disk.
    // Btrfs mode needs read-write (podman creates new subvolumes in graphroot).
    // Overlay and archive modes are read-only.
    let image_disk_read_only = launch_config.image_mode != crate::firecracker::ImageMode::Btrfs;
    let (extra_disks, image_device) =
        attach_extra_disks(args, client, data_dir, image_disk_path, image_disk_read_only).await?;
    vm_state.config.extra_disks = extra_disks;

    // Process --nfs: export directories via NFS for guest to mount
    let mut nfs_shares = Vec::new();
    for nfs_spec in args.nfs.iter() {
        // Check for :ro suffix
        let (spec_without_ro, read_only) = if nfs_spec.ends_with(":ro") {
            (&nfs_spec[..nfs_spec.len() - 3], true)
        } else {
            (nfs_spec.as_str(), false)
        };

        // Split HOST_DIR:GUEST_MOUNT
        let parts: Vec<&str> = spec_without_ro.splitn(2, ':').collect();
        if parts.len() != 2 {
            anyhow::bail!(
                "Invalid NFS spec '{}'. Expected format: HOST_DIR:GUEST_MOUNT[:ro]",
                nfs_spec
            );
        }
        let host_dir = std::path::Path::new(parts[0]);
        let mount_path = parts[1].to_string();

        // Validate host directory exists
        if !host_dir.is_dir() {
            anyhow::bail!(
                "NFS source directory does not exist or is not a directory: {}",
                host_dir.display()
            );
        }

        // Validate mount path is absolute
        if !mount_path.starts_with('/') {
            anyhow::bail!(
                "NFS mount path must be absolute: {} (got '{}')",
                nfs_spec,
                mount_path
            );
        }

        let abs_path = host_dir.canonicalize().context(format!(
            "Failed to resolve NFS path: {}",
            host_dir.display()
        ))?;

        nfs_shares.push(crate::state::types::NfsShare {
            host_path: abs_path.display().to_string(),
            mount_path: mount_path.clone(),
            read_only,
        });

        info!(
            "NFS share: {} -> {} ({})",
            abs_path.display(),
            mount_path,
            if read_only { "ro" } else { "rw" }
        );
    }

    // Set up NFS exports if we have any shares
    if !nfs_shares.is_empty() {
        setup_nfs_exports(vm_id, &nfs_shares, network_config).await?;
    }
    vm_state.config.nfs_shares = nfs_shares;

    // For rootless mode with slirp4netns: post_start starts slirp4netns in the namespace
    // For bridged mode: post_start is a no-op (TAP already created by BridgedNetwork)
    // Use holder_pid for rootless (slirp4netns attaches to holder's namespace)
    let post_start_pid = vm_state.holder_pid.unwrap_or(vm_pid);
    network
        .post_start(post_start_pid)
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

    // Always configure vsock device for status channel (and optionally volumes)
    info!(
        "Configuring vsock device at {:?} (status + {} volume(s))",
        vsock_socket_path,
        volume_mappings.len()
    );
    client
        .set_vsock(crate::firecracker::api::Vsock {
            guest_cid: 3, // Guest CID (host is always 2)
            uds_path: vsock_socket_path.display().to_string(),
        })
        .await?;

    // Build and send MMDS data (container plan)
    build_and_send_mmds(
        &launch_config,
        client,
        network_config,
        vm_state,
        volume_mappings,
        image_device,
    )
    .await?;

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
    crate::commands::common::save_vm_state_with_network(state_manager, vm_state, network_config)
        .await?;

    Ok((vm_manager, holder_child))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rebuild_proxy_url_preserves_auth_and_query() {
        let parsed = Url::parse("http://user:pass@proxy.example/path?q=1").unwrap();
        let rebuilt = rebuild_proxy_url(&parsed, "127.0.0.1".to_string(), 3128);
        assert_eq!(rebuilt, "http://user:pass@127.0.0.1:3128/path?q=1");
    }

    #[test]
    fn test_resolve_proxy_url_applies_default_port() {
        let resolved_http = resolve_proxy_url("http://localhost")
            .expect("localhost proxy URL should resolve with default port");
        assert!(resolved_http.starts_with("http://"));
        assert!(resolved_http.contains(":80"));

        let resolved_https = resolve_proxy_url("https://localhost/proxy")
            .expect("localhost proxy URL should resolve with default https port");
        assert!(resolved_https.starts_with("https://"));
        assert!(resolved_https.contains(":443"));
        assert!(resolved_https.contains("/proxy"));
    }

    #[test]
    fn test_parse_subid_file_finds_current_user() {
        let username = std::env::var("USER").expect("USER env var must be set");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("subuid");
        std::fs::write(
            &path,
            format!(
                "nobody:100000:65536\n{}:1879048192:65536\nother:200000:65536\n",
                username
            ),
        )
        .unwrap();
        let result = parse_subid_file(path.to_str().unwrap());
        assert_eq!(result, Some((1879048192, 65536)));
    }

    #[test]
    fn test_parse_subid_file_returns_none_for_missing_user() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("subuid");
        std::fs::write(&path, "nobody:100000:65536\nother:200000:65536\n").unwrap();
        let result = parse_subid_file(path.to_str().unwrap());
        assert_eq!(result, None);
    }

    #[test]
    fn test_parse_subid_file_returns_none_for_missing_file() {
        let result = parse_subid_file("/tmp/nonexistent-subuid-test-file");
        assert_eq!(result, None);
    }
}
