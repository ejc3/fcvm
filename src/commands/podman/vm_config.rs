use std::net::ToSocketAddrs;

use anyhow::{Context, Result};
use tracing::{debug, info, warn};
use url::Url;

use crate::cli::RunArgs;
use crate::hypervisor::{
    cloud_hypervisor::CloudHypervisorBackend, firecracker::FirecrackerBackend, Backend, Hypervisor,
};
use crate::network::{BridgedNetwork, NetworkManager, PastaNetwork};
use crate::state::{StateManager, VmState};
use crate::storage::DiskManager;

use super::namespace::setup_rootless_namespace;
use super::types::VolumeMapping;

use crate::commands::common::VSOCK_VOLUME_PORT_BASE;

/// Read the current user's subordinate UID range (start, count) from /etc/subuid.
/// Returns None if the file doesn't exist or the user has no entry.
fn get_host_subuid_range() -> Option<(u64, u64)> {
    parse_subid_file("/etc/subuid")
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
/// VMs using pasta with IPv6 can reach both IPv4 (via 10.0.2.2 gateway)
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
pub(crate) fn build_runtime_boot_args(
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
        // Only append DNS to ip= when it's IPv4. IPv6 addresses contain ':'
        // which conflicts with the ip= field delimiter, corrupting the address.
        // IPv6 DNS is passed separately via fcvm_dns= (uses '|' delimiter).
        let dns_suffix = network_config
            .dns_server
            .as_ref()
            .filter(|dns| !dns.contains(':'))
            .map(|dns| format!(":{}", dns))
            .unwrap_or_default();
        // Use /24 netmask for rootless pasta (10.0.2.0/24) or bridged (172.30.x.0/24)
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

    // Pass DNS servers to guest for resolv.conf configuration
    // Both rootless and bridged: use host DNS servers directly (reachable via
    // pasta's L4 translation or bridge/NAT respectively)
    {
        let dns_servers = if let Some(ref dns) = network_config.dns_server {
            // Use the network-mode-specific DNS (pasta forwarder or host DNS)
            vec![dns.clone()]
        } else {
            crate::network::get_host_dns_servers().unwrap_or_default()
        };

        if !dns_servers.is_empty() {
            if !boot_args.is_empty() {
                boot_args.push(' ');
            }
            // Use | delimiter since : is part of IPv6 addresses
            boot_args.push_str(&format!("fcvm_dns={}", dns_servers.join("|")));
        }

        // Pass search domains for short hostname resolution
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

    // Pass guest failpoints to fc-agent via kernel command line (test-only
    // deterministic-interleaving instrumentation — see the failpoint crate).
    // Validated up front: guest specs are sleep-only and whitespace-free, and a
    // bad spec must fail the run before a VM boots, not silently un-arm a test.
    if let Ok(spec) = std::env::var("FCVM_GUEST_FAILPOINT") {
        if let Err(e) = failpoint::validate_guest_spec(&spec) {
            panic!("invalid FCVM_GUEST_FAILPOINT: {e}");
        }
        if !boot_args.is_empty() {
            boot_args.push(' ');
        }
        boot_args.push_str(&format!("fcvm_failpoint={}", spec));
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
    hv: &mut dyn crate::hypervisor::Hypervisor,
    data_dir: &std::path::Path,
    image_disk_path: Option<&std::path::Path>,
    image_disk_read_only: bool,
    rebuild_disk_dir_images: bool,
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
        hv.add_drive(&crate::hypervisor::DriveSpec {
            drive_id: drive_id.clone(),
            path_on_host: abs_path.clone(),
            is_root_device: false,
            is_read_only: read_only,
        })
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

        // Create disk image in VM's data directory. On an in-place reboot relaunch
        // the image already exists and holds the guest's writes — rebuilding it from
        // the host directory would silently destroy them, so the relaunch re-attaches
        // the existing image instead.
        let disk_idx = disk_offset + i;
        let image_path = data_dir
            .join("disks")
            .join(format!("disk-dir-{}.raw", disk_idx));
        if rebuild_disk_dir_images || !image_path.exists() {
            super::image::create_disk_from_dir(source_dir, &image_path, false).await?;
        } else {
            info!(
                image = %image_path.display(),
                "re-attaching existing disk-dir image (in-place relaunch preserves guest writes)"
            );
        }

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
        hv.add_drive(&crate::hypervisor::DriveSpec {
            drive_id: drive_id.clone(),
            path_on_host: image_path.clone(),
            is_root_device: false,
            is_read_only: read_only,
        })
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
        hv.add_drive(&crate::hypervisor::DriveSpec {
            drive_id: drive_id.clone(),
            path_on_host: disk_path.to_path_buf(),
            is_root_device: false,
            is_read_only: image_disk_read_only,
        })
        .await?;
        Some(device)
    } else {
        None
    };

    Ok((extra_disks, image_device))
}

/// Candidate paths for the host's chrony configuration, most specific first.
const HOST_CHRONY_CONFS: &[&str] = &["/etc/chrony/chrony.conf", "/etc/chrony.conf"];

/// Used when the host has no chrony configuration of its own, so a guest is never
/// left with zero time sources. Matches the pool baked into the VM rootfs.
const DEFAULT_NTP_POOL: &str = "pool.ntp.org";

/// Cap on NTP addresses shipped to a guest. A `pool` directive resolves to many
/// addresses and chronyd needs only a handful to discipline a clock.
const NTP_SERVER_LIMIT: usize = 4;

/// Extract the `server`/`pool` hostnames from a chrony configuration.
///
/// Both directives are collected: some distributions (Ubuntu included) ship only
/// `pool` lines, and a guest that ignored those would end up with no time source.
fn parse_chrony_servers(conf: &str) -> Vec<String> {
    conf.lines()
        .map(str::trim)
        .filter_map(|line| {
            let rest = line
                .strip_prefix("server ")
                .or_else(|| line.strip_prefix("pool "))?;
            rest.split_whitespace().next().map(str::to_string)
        })
        .collect()
}

/// The host's NTP servers, resolved to addresses, for the guest's chronyd.
///
/// The guest cannot read the host's chrony.conf — nothing mounts the host's /etc into
/// a VM — so the boot plan carries the servers instead. Resolution happens here, on
/// the host, for the same reason proxy URLs are resolved here: the guest adds each
/// one with `chronyc add server <addr>`, and an address needs no guest-side DNS.
///
/// This is a blocking lookup on the launch path, so it was measured rather than
/// assumed: 0.2-0.5ms warm and 5.5ms on the first (cold-resolver) call on a c7g box
/// whose host chronyd keeps these names in the local resolver cache. That is in line
/// with the launch path's existing millisecond-scale steps; the cap below keeps it to
/// a single lookup in the common case, since one `pool` name already yields four
/// addresses.
fn host_ntp_servers() -> Vec<String> {
    let names = HOST_CHRONY_CONFS
        .iter()
        .find_map(|path| std::fs::read_to_string(path).ok())
        .map(|conf| parse_chrony_servers(&conf))
        .filter(|names| !names.is_empty())
        .unwrap_or_else(|| {
            debug!("no NTP servers in host chrony config; using {DEFAULT_NTP_POOL}");
            vec![DEFAULT_NTP_POOL.to_string()]
        });

    let mut addrs: Vec<String> = Vec::new();
    for name in names {
        match std::net::ToSocketAddrs::to_socket_addrs(&(name.as_str(), 123u16)) {
            Ok(resolved) => {
                for addr in resolved {
                    let ip = addr.ip().to_string();
                    if !addrs.contains(&ip) {
                        addrs.push(ip);
                    }
                    if addrs.len() >= NTP_SERVER_LIMIT {
                        return addrs;
                    }
                }
            }
            Err(e) => warn!(server = %name, error = %e, "failed to resolve host NTP server"),
        }
    }
    if addrs.is_empty() {
        warn!("no host NTP servers resolved; guest clock will rely on host-time sync only");
    }
    addrs
}

/// Build the boot-plan JSON (the `latest` object served to fc-agent).
///
/// Used by both transports: MMDS (`hv.publish_boot_plan`) and vsock
/// (`spawn_bootplan_listener`). User-input fields come from `launch_config`
/// (part of snapshot cache key); runtime-only values (network, proxies,
/// timestamps) are computed here.
pub(super) fn build_boot_plan_json(
    launch_config: &crate::firecracker::FirecrackerConfig,
    network_config: &crate::network::NetworkConfig,
    vm_state: &VmState,
    volume_mappings: &[VolumeMapping],
    image_device: Option<String>,
) -> serde_json::Value {
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
    // Resolve hostname to IP since VMs reach external addresses via pasta gateway.
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

    let subuid_range = if launch_config.user.is_some() {
        get_host_subuid_range()
    } else {
        None
    };

    let runtime = crate::firecracker::MmdsRuntime {
        volumes,
        extra_disks,
        nfs_mounts,
        image_device,
        http_proxy,
        https_proxy,
        no_proxy,
        subuid_start: subuid_range.map(|(start, _)| start),
        subuid_count: subuid_range.map(|(_, count)| count),
        host_time: chrono::Utc::now().timestamp().to_string(),
        ntp_servers: host_ntp_servers(),
    };

    launch_config.to_mmds_json(runtime)
}

/// Set up NFS exports for VM.
/// Creates /etc/exports.d/fcvm-{vm_id}.exports and refreshes exportfs.
///
/// `client_spec` is the exports(5) client field — the source address the
/// host's nfsd sees. Baseline VMs connect directly with their guest IP;
/// clones behind in-namespace NAT arrive masqueraded as their veth IP.
/// `insecure` allows non-privileged source ports (needed for clones:
/// MASQUERADE may renumber the client's privileged port above 1023).
pub(crate) async fn setup_nfs_exports(
    vm_id: &str,
    shares: &[crate::state::types::NfsShare],
    client_spec: &str,
    insecure: bool,
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

    // Self-heal: drop fcvm export files whose exported directories no longer
    // exist (left behind when a VM was SIGKILLed before its cleanup ran).
    // Stale entries make `exportfs -ra` fail with "Failed to stat ..." and can
    // keep THIS VM's brand-new export from taking effect — the guest's hard
    // NFS mount then hangs forever.
    if let Ok(mut entries) = tokio::fs::read_dir("/etc/exports.d").await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if !name.starts_with("fcvm-") || !name.ends_with(".exports") {
                continue;
            }
            let Ok(content) = tokio::fs::read_to_string(entry.path()).await else {
                continue;
            };
            let stale = content.lines().any(|line| {
                line.split_whitespace()
                    .next()
                    .is_some_and(|path| !std::path::Path::new(path).exists())
            });
            if stale {
                info!(
                    exports_file = %entry.path().display(),
                    "removing stale fcvm NFS exports (exported path no longer exists)"
                );
                let _ = tokio::fs::remove_file(entry.path()).await;
            }
        }
    }

    // Build exports file content
    let mut exports = String::new();
    for share in shares {
        let mut opts = if share.read_only {
            "ro,sync,no_subtree_check,no_root_squash".to_string()
        } else {
            "rw,sync,no_subtree_check,no_root_squash".to_string()
        };
        if insecure {
            opts.push_str(",insecure");
        }
        exports.push_str(&format!("{} {}({})\n", share.host_path, client_spec, opts));
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
        // A failed refresh means this VM's export is not active; the guest's
        // hard NFS mount would hang until the test budget kills it. Fail
        // loudly instead.
        anyhow::bail!(
            "exportfs -ra failed; NFS export for {} is not active (check /etc/exports.d for stale entries)",
            exports_path
        );
    }

    Ok(())
}

/// Clean up NFS exports for VM
pub(crate) async fn cleanup_nfs_exports(vm_id: &str) {
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

/// Parameters for VM setup, grouping the many read-only inputs.
pub(super) struct VmSetupParams<'a> {
    pub args: &'a RunArgs,
    pub vm_id: &'a str,
    pub data_dir: &'a std::path::Path,
    pub base_rootfs: &'a std::path::Path,
    pub socket_path: &'a std::path::Path,
    pub kernel_path: &'a std::path::Path,
    pub initrd_path: &'a std::path::Path,
    pub network_config: &'a crate::network::NetworkConfig,
    pub cmd_args: Option<Vec<String>>,
    pub volume_mappings: &'a [VolumeMapping],
    pub vsock_socket_path: &'a std::path::Path,
    pub image_disk_path: Option<&'a std::path::Path>,
    pub fc_config: Option<crate::firecracker::FirecrackerConfig>,
    pub runtime_config: &'a crate::commands::common::RuntimeConfig,
}

/// Helper function that runs VM setup and returns VmManager on success.
/// This allows the caller to cleanup network resources on error.
/// For rootless mode, also returns the holder process that keeps the namespace alive.
///
/// On error, any Firecracker or namespace-holder process started by the failed setup
/// is killed and any NFS exports written for the VM are removed, so a partial setup
/// failure never leaks running processes or host export entries.
pub(super) async fn run_vm_setup(
    params: VmSetupParams<'_>,
    network: &mut dyn NetworkManager,
    state_manager: &StateManager,
    vm_state: &mut VmState,
) -> Result<(
    Box<dyn Hypervisor>,
    Option<tokio::process::Child>,
    super::types::RebootSpec,
    Option<tokio::task::JoinHandle<()>>,
)> {
    let vm_id = params.vm_id.to_string();

    // The inner setup publishes the Firecracker manager and holder process into these
    // slots as soon as they are created, so the error path below can kill them even
    // when the failure happens partway through configuration.
    let mut vm_manager_slot: Option<Box<dyn Hypervisor>> = None;
    let mut holder_slot: Option<tokio::process::Child> = None;
    // Inputs to replay the firecracker config on an in-place relaunch (guest reboot),
    // captured once the launch config is fully resolved.
    let mut reboot_spec_slot: Option<super::types::RebootSpec> = None;
    // Boot-plan vsock listener handle (Some only when serving the plan over vsock).
    let mut bootplan_handle_slot: Option<tokio::task::JoinHandle<()>> = None;

    let result = run_vm_setup_inner(
        params,
        network,
        state_manager,
        vm_state,
        &mut vm_manager_slot,
        &mut holder_slot,
        &mut reboot_spec_slot,
        &mut bootplan_handle_slot,
    )
    .await;

    if let Err(e) = result {
        // Firecracker and/or the namespace holder may already be running — kill them
        // so the failed setup doesn't leak processes (and the guest memory, TAP
        // device, and CoW disk they hold).
        if let Some(ref mut vm_manager) = vm_manager_slot {
            if let Err(kill_err) = vm_manager.kill().await {
                warn!("failed to kill VM process: {}", kill_err);
            }
        }
        if let Some(ref mut holder) = holder_slot {
            if let Err(kill_err) = holder.kill().await {
                warn!("failed to kill holder process: {}", kill_err);
            }
            let _ = holder.wait().await; // Clean up zombie
        }
        // Abort the boot-plan listener task if it was started.
        if let Some(handle) = bootplan_handle_slot.take() {
            handle.abort();
        }
        // NFS exports may have been written before the failing step; remove them
        // (no-op when the exports file was never created).
        cleanup_nfs_exports(&vm_id).await;
        return Err(e);
    }

    let vm_manager = vm_manager_slot.expect("run_vm_setup_inner sets vm_manager on success");
    let reboot_spec = reboot_spec_slot.expect("run_vm_setup_inner sets reboot_spec on success");
    Ok((vm_manager, holder_slot, reboot_spec, bootplan_handle_slot))
}

/// Build the FirecrackerConfig used to cold-boot a VM from a disk.
///
/// Single source of truth for launch-config construction, shared by:
///   * initial `fcvm podman run` (run_vm_setup_inner, cache miss)
///   * the snapshot-restore path's up-front reboot plan (a rebooted restored clone
///     cold-boots from its current provisioned disk — disk-only-clone semantics)
pub(crate) fn build_launch_config(
    args: &RunArgs,
    rootfs_path: &std::path::Path,
    kernel_path: &std::path::Path,
    initrd_path: &std::path::Path,
    cmd_args: &Option<Vec<String>>,
    runtime_config: &crate::commands::common::RuntimeConfig,
) -> crate::firecracker::FirecrackerConfig {
    use crate::firecracker::{BootSource, Drive, FcNetworkMode, FirecrackerConfig, MachineConfig};
    let network_mode: FcNetworkMode = args.network.into();
    // Collect extra disk specifications
    let mut extra_disks: Vec<String> = Vec::new();
    extra_disks.extend(args.disk.iter().cloned());
    extra_disks.extend(args.disk_dir.iter().cloned());
    extra_disks.extend(args.nfs.iter().cloned());

    let port_mappings = crate::network::PortMapping::parse_all_lenient(&args.publish);

    FirecrackerConfig {
        boot_source: BootSource {
            kernel_image_path: kernel_path.to_path_buf(),
            initrd_path: initrd_path.to_path_buf(),
            ..Default::default()
        },
        machine_config: MachineConfig {
            vcpu_count: args.cpu,
            mem_size_mib: args.mem,
            huge_pages: if args.hugepages {
                Some("2M".to_string())
            } else {
                None
            },
        },
        drives: vec![Drive {
            drive_id: "rootfs".to_string(),
            path_on_host: rootfs_path.to_path_buf(),
            is_root_device: true,
            is_read_only: false,
        }],
        container_image_name: args.image.clone(),
        container_image: args.image.clone(),
        container_cmd: cmd_args.clone(),
        network_mode,
        data_dir: crate::paths::data_dir(),
        extra_disks,
        env_vars: args.env.to_vec(),
        volume_mounts: args.map.to_vec(),
        privileged: args.privileged,
        tty: args.tty,
        interactive: args.interactive,
        non_blocking_output: args.non_blocking_output,
        rootfs_size: args.rootfs_size.clone(),
        health_check_url: args.health_check.clone(),
        user: args.user.clone(),
        forward_localhost: args.forward_localhost.clone(),
        ipv6_prefix: args.ipv6_prefix.clone(),
        portable_volumes: args.portable_volumes,
        image_mode: super::resolve_image_mode(args),
        rootfs_type: super::resolve_rootfs_type(args),
        port_mappings,
        firecracker_bin: runtime_config.firecracker_bin.clone(),
        // Cache-key isolation for guest failpoints (see field docs): the spec is
        // forwarded to the guest by build_runtime_boot_args from the same env var.
        guest_failpoint: std::env::var("FCVM_GUEST_FAILPOINT").ok(),
    }
}

/// Apply a launch plan to a freshly-spawned VMM child and boot the VM.
///
/// This is the SHARED configure-and-boot primitive used by three flows:
///   * initial `fcvm podman run` (via run_vm_setup_inner)
///   * disk-only clone cold boot (via prepare_vm -> run_vm_setup_inner)
///   * in-place relaunch after a guest reboot (via run_vm_loop)
///
/// The caller must have already spawned the VMM (`hv.spawn(...)`); this
/// replays the full per-child API configuration (apply -> attach disks -> add eth0 ->
/// metadata service -> vsock -> boot plan -> entropy -> balloon -> boot).
///
/// `network` gates the host-once-only steps:
///   * `Some(network)` (initial boot / clone): runs NFS exports + pasta `post_start`.
///   * `None` (reboot relaunch): the host substrate (disk, namespace/holder, pasta,
///     NFS exports, listeners, persisted state) is already live and is reused untouched.
///
/// Returns the boot-plan vsock listener's task handle when `bootplan_over_vsock` is set
/// (the caller owns its lifetime and aborts it on cleanup); `None` for the MMDS path.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn configure_and_boot_vm(
    hv: &mut dyn crate::hypervisor::Hypervisor,
    plan: &super::types::RebootSpec,
    args: &RunArgs,
    network_config: &crate::network::NetworkConfig,
    vm_state: &mut VmState,
    data_dir: &std::path::Path,
    vm_id: &str,
    volume_mappings: &[VolumeMapping],
    network: Option<&mut dyn NetworkManager>,
    bootplan_over_vsock: bool,
) -> Result<Option<tokio::task::JoinHandle<()>>> {
    let vm_pid = hv.pid()?;

    info!("configuring VM via hypervisor API");
    hv.apply_launch_config(&plan.launch_config, &plan.boot_args, plan.track_dirty_pages)
        .await?;

    // Attach extra disks and image disk (read-only for all image modes). disk-dir
    // images are only BUILT on the initial boot (network = Some); a reboot relaunch
    // re-attaches the existing images so guest writes survive the reboot.
    let image_disk_read_only = true;
    let (extra_disks, image_device) = attach_extra_disks(
        args,
        hv,
        data_dir,
        plan.image_disk_path.as_deref(),
        image_disk_read_only,
        network.is_some(),
    )
    .await?;
    vm_state.config.extra_disks = extra_disks;

    // Host-once-only steps — skipped on an in-place reboot relaunch (network=None).
    if let Some(network) = network {
        // Process --nfs: export directories via NFS for the guest to mount.
        let mut nfs_shares = Vec::new();
        for nfs_spec in args.nfs.iter() {
            let (spec_without_ro, read_only) = if nfs_spec.ends_with(":ro") {
                (&nfs_spec[..nfs_spec.len() - 3], true)
            } else {
                (nfs_spec.as_str(), false)
            };
            let parts: Vec<&str> = spec_without_ro.splitn(2, ':').collect();
            if parts.len() != 2 {
                anyhow::bail!(
                    "Invalid NFS spec '{}'. Expected format: HOST_DIR:GUEST_MOUNT[:ro]",
                    nfs_spec
                );
            }
            let host_dir = std::path::Path::new(parts[0]);
            let mount_path = parts[1].to_string();
            if !host_dir.is_dir() {
                anyhow::bail!(
                    "NFS source directory does not exist or is not a directory: {}",
                    host_dir.display()
                );
            }
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
        if !nfs_shares.is_empty() {
            let guest_ip = network_config
                .guest_ip
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("No guest IP configured for NFS"))?;
            setup_nfs_exports(vm_id, &nfs_shares, guest_ip, false).await?;
        }
        vm_state.config.nfs_shares = nfs_shares;

        // For rootless mode with pasta: post_start starts pasta + bridge in the namespace.
        // For bridged mode: post_start is a no-op (TAP already created by BridgedNetwork).
        // Use holder_pid for rootless (pasta attaches to the holder's namespace).
        let post_start_pid = vm_state.holder_pid.unwrap_or(vm_pid);
        network
            .post_start(post_start_pid)
            .await
            .context("post-start network setup")?;
    }

    // Network interface - required for MMDS V2 in all modes.
    hv.add_network_interface(&crate::hypervisor::NetIfaceSpec {
        iface_id: "eth0".to_string(),
        host_dev_name: network_config.tap_device.clone(),
        guest_mac: Some(network_config.guest_mac.clone()),
    })
    .await?;

    // Metadata service for boot-plan delivery (Firecracker: MMDS V2 on eth0).
    hv.configure_metadata_service().await?;

    // Always configure vsock device for status channel (and optionally volumes).
    // The backend removes any stale host-side uds_path socket before binding (a reboot
    // relaunch reuses the same path); the per-port listener sockets are untouched.
    info!(
        "Configuring vsock device at {:?} (status + {} volume(s))",
        plan.vsock_socket_path,
        volume_mappings.len()
    );
    hv.set_vsock(
        crate::hypervisor::firecracker::default_guest_cid(),
        &plan.vsock_socket_path,
    )
    .await?;

    // Build the container boot plan and deliver it. For VMMs without a metadata service
    // (Cloud Hypervisor), serve it over vsock and return the listener handle; otherwise
    // publish it via MMDS (Firecracker).
    let plan_json = build_boot_plan_json(
        &plan.launch_config,
        network_config,
        vm_state,
        volume_mappings,
        image_device,
    );
    // Wrapped in an abort-on-drop guard: if a later boot-config step (entropy/balloon/
    // boot) returns Err, the guard drops and aborts the listener task instead of leaking
    // it. On success we disarm and hand the handle to the caller.
    let bootplan_guard = AbortOnDrop(if bootplan_over_vsock {
        // fc-agent reads the inner `latest` object; the host listens on
        // `{vsock_socket}_{VSOCK_BOOTPLAN_PORT}` (Firecracker/CH vsock proxy naming).
        // Bind happens synchronously here so a bind failure fails setup fast rather than
        // leaving the guest looping forever waiting for a plan that is never served.
        let inner = plan_json
            .get("latest")
            .cloned()
            .unwrap_or_else(|| plan_json.clone());
        let socket = format!(
            "{}_{}",
            plan.vsock_socket_path.display(),
            crate::commands::common::VSOCK_BOOTPLAN_PORT
        );
        Some(
            super::listeners::spawn_bootplan_listener(&socket, &inner)
                .context("starting boot-plan vsock listener")?,
        )
    } else {
        hv.publish_boot_plan(plan_json).await?;
        None
    });

    // Configure entropy device (virtio-rng) for better random number generation.
    hv.add_entropy_device().await?;

    // Balloon (if specified).
    if let Some(balloon_mib) = args.balloon {
        hv.add_balloon(balloon_mib).await?;
    }

    // Start VM.
    hv.boot().await?;

    Ok(bootplan_guard.disarm())
}

/// Aborts the wrapped task on drop unless [`Self::disarm`]ed. Ensures the boot-plan vsock
/// listener spawned mid-`configure_and_boot_vm` is not leaked if a later step fails.
struct AbortOnDrop(Option<tokio::task::JoinHandle<()>>);

impl AbortOnDrop {
    /// Take the handle out, defusing the abort-on-drop (the caller now owns it).
    fn disarm(mut self) -> Option<tokio::task::JoinHandle<()>> {
        self.0.take()
    }
}

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        if let Some(handle) = self.0.take() {
            handle.abort();
        }
    }
}

/// Inner VM setup: creates the CoW disk, starts Firecracker, and configures the VM.
///
/// The Firecracker manager and (for rootless mode) the namespace-holder child are
/// stored into the provided slots as soon as they exist so that `run_vm_setup` can
/// kill them if a later step fails.
#[allow(clippy::too_many_arguments)]
async fn run_vm_setup_inner(
    params: VmSetupParams<'_>,
    network: &mut dyn NetworkManager,
    state_manager: &StateManager,
    vm_state: &mut VmState,
    vm_manager_slot: &mut Option<Box<dyn Hypervisor>>,
    holder_slot: &mut Option<tokio::process::Child>,
    reboot_spec_slot: &mut Option<super::types::RebootSpec>,
    bootplan_handle_slot: &mut Option<tokio::task::JoinHandle<()>>,
) -> Result<()> {
    let VmSetupParams {
        args,
        vm_id,
        data_dir,
        base_rootfs,
        socket_path,
        kernel_path,
        initrd_path,
        network_config,
        cmd_args,
        volume_mappings,
        vsock_socket_path,
        image_disk_path,
        fc_config,
        runtime_config,
    } = params;
    // Setup storage - just need CoW copy (fc-agent is injected via initrd at boot)
    let vm_dir = data_dir.join("disks");
    let disk_manager =
        DiskManager::new(vm_id.to_string(), base_rootfs.to_path_buf(), vm_dir.clone());

    let rootfs_path = disk_manager
        .create_cow_disk()
        .await
        .context("creating CoW disk")?;

    // Estimate space needed for container image extraction inside VM.
    // Overlay mode uses a separate block device — no rootfs impact.
    // Btrfs and archive modes load the archive onto the rootfs via podman load.
    // podman load extracts layers to /var/tmp first, then copies to storage,
    // so we need ~3x the archive size for safety margin.
    let resolved_mode = super::resolve_image_mode(args);
    let image_overhead = if matches!(
        resolved_mode,
        crate::firecracker::ImageMode::Archive | crate::firecracker::ImageMode::Btrfs
    ) {
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
    // VMM debug log (named firecracker.log for both backends for tooling compatibility).
    let vmm_log_path = data_dir.join("firecracker.log");
    let _ = std::fs::File::create(&vmm_log_path);

    // Select the VMM backend (#632) and its binary.
    let backend: Backend = args.hypervisor.into();
    let vmm_bin = match backend {
        Backend::Firecracker => crate::commands::common::find_firecracker(runtime_config)?,
        Backend::CloudHypervisor => crate::commands::common::find_cloud_hypervisor()?,
    };

    // Firecracker extra args (e.g. --enable-nv2 from the kernel profile, or
    // FCVM_FIRECRACKER_ARGS) are Firecracker-specific; Cloud Hypervisor ignores them.
    let vmm_extra_args: Option<String> = match backend {
        Backend::Firecracker => runtime_config
            .firecracker_args
            .clone()
            .or_else(|| std::env::var("FCVM_FIRECRACKER_ARGS").ok()),
        Backend::CloudHypervisor => None,
    };

    // Build the process-spawn spec: binary, extra args, and namespace isolation. The
    // namespace fields are VMM-neutral (any VMM child runs in the same namespaces).
    let mut process_spec = crate::hypervisor::ProcessSpec {
        binary: vmm_bin.clone(),
        extra_args: vmm_extra_args.clone(),
        vm_name: Some(vm_name),
        ..Default::default()
    };

    // Configure namespace isolation based on network type.
    if let Some(bridged_net) = network.as_any().downcast_ref::<BridgedNetwork>() {
        // Bridged mode: use pre-created network namespace
        if let Some(ns_id) = bridged_net.namespace_id() {
            info!(namespace = %ns_id, "configuring VM to run in network namespace");
            process_spec.namespace_id = Some(ns_id.to_string());
        }
    } else if let Some(routed_net) = network
        .as_any()
        .downcast_ref::<crate::network::RoutedNetwork>()
    {
        // Routed mode: use pre-created network namespace (like bridged)
        if let Some(ns_id) = routed_net.namespace_id() {
            info!(namespace = %ns_id, "configuring VM to run in routed network namespace");
            process_spec.namespace_id = Some(ns_id.to_string());
        }
    } else if let Some(pasta_net) = network.as_any().downcast_ref::<PastaNetwork>() {
        *holder_slot = Some(
            setup_rootless_namespace(pasta_net, network_config, &mut process_spec, vm_state)
                .await?,
        );
    }

    let vm_manager = vm_manager_slot.insert(match backend {
        Backend::Firecracker => Box::new(FirecrackerBackend::new(
            vm_id.to_string(),
            socket_path.to_path_buf(),
            Some(vmm_log_path),
        )) as Box<dyn Hypervisor>,
        Backend::CloudHypervisor => Box::new(CloudHypervisorBackend::new(
            vm_id.to_string(),
            socket_path.to_path_buf(),
            Some(vmm_log_path),
        )) as Box<dyn Hypervisor>,
    });

    vm_manager
        .spawn(&process_spec)
        .await
        .context("starting Firecracker")?;

    // Boot-plan transport: VMMs without a native metadata service (Cloud Hypervisor)
    // must receive the plan over vsock; Firecracker uses MMDS. FCVM_BOOTPLAN=vsock forces
    // the vsock path on Firecracker too (exercised by the P0.5 regression test).
    let bootplan_over_vsock = !vm_manager.capabilities().native_metadata_service
        || std::env::var("FCVM_BOOTPLAN").as_deref() == Ok("vsock");

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
            build_launch_config(
                args,
                &rootfs_path,
                kernel_path,
                initrd_path,
                &cmd_args,
                runtime_config,
            )
        });

    let mut runtime_boot_args = build_runtime_boot_args(args, network_config, runtime_config);
    if bootplan_over_vsock {
        // fc-agent reads this kernel arg to select the vsock boot-plan transport.
        runtime_boot_args.push_str(" fcvm_bootplan=vsock");
    }

    // The launch plan: everything needed to (re)boot this VM. Built UP FRONT so the
    // exact same descriptor boots the VM now and relaunches it on a guest reboot, and
    // is shared with the disk-only clone path (which boots via prepare_vm -> here).
    let plan = super::types::RebootSpec {
        firecracker_bin: vmm_bin,
        fc_args: vmm_extra_args,
        launch_config,
        boot_args: runtime_boot_args,
        track_dirty_pages,
        image_disk_path: image_disk_path.map(|p| p.to_path_buf()),
        vsock_socket_path: vsock_socket_path.to_path_buf(),
        bootplan_over_vsock,
    };

    // Apply the plan and bring the VM up via the shared primitive. `Some(network)`
    // runs the host-once-only steps (NFS exports + pasta post_start); an in-place
    // reboot relaunch passes `None` and reuses the already-live host substrate.
    *bootplan_handle_slot = configure_and_boot_vm(
        vm_manager.as_mut(),
        &plan,
        args,
        network_config,
        vm_state,
        data_dir,
        vm_id,
        volume_mappings,
        Some(network),
        bootplan_over_vsock,
    )
    .await?;

    // Save VM state with complete network configuration
    crate::commands::common::save_vm_state_with_network(state_manager, vm_state, network_config)
        .await?;

    // Publish the launch plan so a guest reboot can relaunch the VM in place.
    *reboot_spec_slot = Some(plan);

    Ok(())
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
        let username = std::env::var("USER").ok().unwrap_or_else(|| {
            nix::unistd::User::from_uid(nix::unistd::getuid())
                .ok()
                .flatten()
                .map(|u| u.name)
                .expect("could not determine current username")
        });
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

    /// Ubuntu's stock chrony.conf uses `pool`, not `server`. Ignoring `pool` would
    /// send an empty NTP list to every guest — the silent zero-sources failure.
    #[test]
    fn test_parse_chrony_servers_collects_pool_and_server() {
        let conf = "\
# comment
pool ntp.ubuntu.com        iburst maxsources 4
server 10.0.0.1 iburst
  pool 0.ubuntu.pool.ntp.org iburst maxsources 1
serverfoo notadirective
driftfile /var/lib/chrony/drift
";
        assert_eq!(
            parse_chrony_servers(conf),
            vec!["ntp.ubuntu.com", "10.0.0.1", "0.ubuntu.pool.ntp.org"]
        );
    }

    #[test]
    fn test_parse_chrony_servers_empty_without_directives() {
        assert!(
            parse_chrony_servers("driftfile /var/lib/chrony/drift\nmakestep 1 -1\n").is_empty()
        );
    }

    /// A guest with no NTP source silently drifts, so the plan must never be empty
    /// on a host that can resolve names.
    #[test]
    fn test_host_ntp_servers_resolves_to_addresses() {
        for addr in host_ntp_servers() {
            assert!(
                addr.parse::<std::net::IpAddr>().is_ok(),
                "boot plan must carry resolved addresses, got {addr:?}"
            );
        }
    }
}
