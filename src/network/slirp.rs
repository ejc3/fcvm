use anyhow::{Context, Result};
use std::os::unix::io::AsRawFd;
use std::path::PathBuf;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::process::{Child, Command};
use tracing::{debug, info, warn};

use super::{get_host_dns_search, get_host_dns_servers, types::generate_mac, NetworkConfig, NetworkManager, PortMapping};

/// Get the path to the slirp4netns binary.
/// Prefer the deps version with newer libslirp (4.8.0) for better IPv6 support.
fn get_slirp4netns_path() -> PathBuf {
    // Check if deps version exists (has libslirp 4.8.0 with better IPv6 DNS support)
    // Fall back to system slirp4netns if not available
    let deps_path = paths::assets_dir().join("deps/bin/slirp4netns");
    if deps_path.exists() {
        deps_path
    } else {
        PathBuf::from("slirp4netns")
    }
}

/// Get the host's primary IPv6 address for outbound traffic.
/// Returns the first non-link-local, non-deprecated global IPv6 address.
fn get_host_ipv6_address() -> Option<String> {
    // Run `ip -6 addr show` and parse the output
    let output = std::process::Command::new("ip")
        .args(["-6", "addr", "show", "scope", "global"])
        .output()
        .ok()?;

    let stdout = String::from_utf8_lossy(&output.stdout);

    // Find first non-deprecated global IPv6 address
    // Lines look like: "    inet6 2001:db8::1/64 scope global"
    for line in stdout.lines() {
        let line = line.trim();
        if line.starts_with("inet6 ") && !line.contains("deprecated") {
            // Extract the address (everything after "inet6 " and before the "/")
            if let Some(addr_part) = line.strip_prefix("inet6 ") {
                if let Some(addr) = addr_part.split('/').next() {
                    // Skip link-local addresses (fe80::)
                    if !addr.starts_with("fe80:") {
                        return Some(addr.to_string());
                    }
                }
            }
        }
    }

    // If no non-deprecated address found, try deprecated ones
    for line in stdout.lines() {
        let line = line.trim();
        if line.starts_with("inet6 ") {
            if let Some(addr_part) = line.strip_prefix("inet6 ") {
                if let Some(addr) = addr_part.split('/').next() {
                    if !addr.starts_with("fe80:") {
                        return Some(addr.to_string());
                    }
                }
            }
        }
    }

    None
}

use crate::paths;
use crate::state::truncate_id;

/// Guest network addressing - all on slirp's 10.0.2.0/24 subnet
/// This avoids NAT which doesn't work in user namespaces (iptables fails)
const GUEST_SUBNET: &str = "10.0.2.0/24";
const GUEST_IP: &str = "10.0.2.15";
const NAMESPACE_IP: &str = "10.0.2.1";

/// Default TAP device name for slirp4netns
const SLIRP_DEVICE_NAME: &str = "slirp0";

/// Rootless networking using slirp4netns with bridge-based architecture
///
/// This mode uses user namespaces and slirp4netns for true unprivileged operation.
/// No sudo/root required - everything runs in user namespace via nsenter.
///
/// Architecture (Bridge-based L2 forwarding):
/// ```text
/// Host                    | User Namespace (unshare --user --map-root-user --net)
///                         |
/// slirp4netns <-----------+-- slirp0 <---> br0 <---> tap0
///   (userspace NAT)       |    (bridge preserves MAC addresses)
///                         |                           |
///                         |                    Firecracker VM
///                         |                    (guest: 10.0.2.15)
/// ```
///
/// Key insight: slirp4netns and Firecracker CANNOT share a TAP device (both need exclusive access).
/// Solution: Use two TAP devices bridged together (L2 forwarding preserves MACs).
///
/// Setup sequence (3-phase with nsenter):
/// 1. Spawn holder process: `unshare --user --map-root-user --net -- sleep infinity`
/// 2. Run setup via nsenter: create TAPs, iptables, IP forwarding
/// 3. Start slirp4netns attached to holder's namespace
/// 4. Run Firecracker via nsenter: `nsenter -t HOLDER_PID -U -n -- firecracker ...`
/// 5. Health checks via nsenter: `nsenter -t HOLDER_PID -U -n -- curl guest_ip:80`
pub struct SlirpNetwork {
    vm_id: String,
    tap_device: String,   // TAP device for Firecracker (tap0)
    slirp_device: String, // TAP device for slirp4netns (slirp0)
    port_mappings: Vec<PortMapping>,

    // Network addressing (all on slirp's 10.0.2.0/24 subnet)
    guest_subnet: String, // 10.0.2.0/24 (slirp's internal subnet)
    guest_ip: String,     // Guest VM IP (10.0.2.15)
    namespace_ip: String, // Namespace host IP on br0 (10.0.2.1)

    // State (populated during setup)
    api_socket_path: Option<PathBuf>,
    slirp_process: Option<Child>,
    loopback_ip: Option<String>, // Unique loopback IP for port forwarding (127.x.y.z)
}

impl SlirpNetwork {
    pub fn new(vm_id: String, tap_device: String, port_mappings: Vec<PortMapping>) -> Self {
        // Guest subnet is always 10.0.2.0/24 (slirp's internal subnet)
        // No conflicts because each VM runs in its own isolated user namespace
        Self {
            vm_id,
            tap_device,
            slirp_device: SLIRP_DEVICE_NAME.to_string(),
            port_mappings,
            guest_subnet: GUEST_SUBNET.to_string(),
            guest_ip: GUEST_IP.to_string(),
            namespace_ip: NAMESPACE_IP.to_string(),
            api_socket_path: None,
            slirp_process: None,
            loopback_ip: None,
        }
    }

    /// Set a unique loopback IP for port forwarding (127.x.y.z)
    ///
    /// Each VM gets a unique loopback IP so multiple VMs can forward the same
    /// port numbers (e.g., all VMs can have -p 8080:80).
    ///
    /// On Linux, the entire 127.0.0.0/8 range routes to loopback without needing
    /// `ip addr add`. We just bind directly to 127.0.0.2:8080, 127.0.0.3:8080, etc.
    /// This is fully rootless!
    pub fn with_loopback_ip(mut self, loopback_ip: String) -> Self {
        self.loopback_ip = Some(loopback_ip);
        self
    }

    /// Get the loopback IP assigned to this VM for port forwarding
    pub fn loopback_ip(&self) -> Option<&str> {
        self.loopback_ip.as_deref()
    }

    /// Configure specific guest IP for clone operations
    ///
    /// When cloning from a snapshot, the guest already has its IP configured.
    /// This method sets the network to use that same IP so that DNAT rules
    /// forward traffic to the correct destination.
    ///
    /// Supports both:
    /// - New 10.0.2.x format (slirp4netns native subnet)
    /// - Legacy 192.168.x.x format (old veth-based architecture)
    pub fn with_guest_ip(mut self, guest_ip: String) -> Self {
        // Parse the IP (strip CIDR notation if present)
        let ip_only = guest_ip.split('/').next().unwrap_or(&guest_ip);

        // Parse IP into octets
        let parts: Vec<&str> = ip_only.split('.').collect();
        if parts.len() == 4 {
            // Build subnet and namespace IP from the actual IP given
            // E.g., "10.0.2.15" -> subnet "10.0.2.0/24", namespace "10.0.2.1"
            // E.g., "192.168.5.2" -> subnet "192.168.5.0/24", namespace "192.168.5.1"
            self.guest_subnet = format!("{}.{}.{}.0/24", parts[0], parts[1], parts[2]);
            self.guest_ip = ip_only.to_string();
            self.namespace_ip = format!("{}.{}.{}.1", parts[0], parts[1], parts[2]);
        }

        self
    }

    /// Get API socket path for port forwarding
    pub fn api_socket_path(&self) -> Option<&PathBuf> {
        self.api_socket_path.as_ref()
    }

    /// Build the holder command for creating the namespace
    ///
    /// Returns command to spawn a holder process that keeps the namespace alive.
    /// The holder runs `sleep infinity` which blocks forever until killed.
    /// Note: We use sleep instead of cat because cat requires stdin management.
    ///
    /// Uses --map-root-user for simple 1:1 UID mapping (current user → UID 0 inside namespace).
    /// This works for both root and unprivileged users.
    ///
    /// Note: --map-auto was considered but it maps to subordinate UIDs (100000+) which doesn't
    /// include the current user's UID, causing permission issues with KVM and file access.
    pub fn build_holder_command(&self) -> Vec<String> {
        vec![
            "unshare".to_string(),
            "--user".to_string(),
            "--map-root-user".to_string(),
            "--net".to_string(),
            "--".to_string(),
            "sleep".to_string(),
            "infinity".to_string(),
        ]
    }

    /// Build the setup script to run inside the namespace via nsenter
    ///
    /// This script creates both TAP devices and configures networking.
    /// Run via: nsenter -t HOLDER_PID -U -n -- bash -c '<this script>'
    ///
    /// Architecture: Bridge-based (L2 forwarding)
    /// ```
    /// Guest eth0 <-> tap0 <-> br0 <-> slirp0 <-> slirp4netns
    ///              (bridge preserves MAC addresses)
    /// ```
    ///
    /// Why bridge instead of IP forwarding?
    /// - slirp4netns learns MAC addresses from ARP (IPv4) and NDP (IPv6)
    /// - With IP forwarding, the kernel rewrites MACs when forwarding packets
    /// - slirp learns the intermediate interface's MAC, not the guest's MAC
    /// - IPv4 ARP proxying works around this, but IPv6 NDP proxying doesn't
    /// - Bridge operates at L2 and preserves MAC addresses, so slirp learns correctly
    ///
    /// For IPv6, the guest must send a gratuitous NDP Neighbor Advertisement
    /// at boot to teach slirp its MAC address. fc-agent handles this.
    pub fn build_setup_script(&self) -> String {
        format!(
            r#"
set -e

# Ensure standard paths are available (nsenter may have limited PATH)
export PATH="/usr/sbin:/sbin:/usr/bin:/bin:$PATH"

# Create slirp0 TAP for slirp4netns connectivity
# slirp4netns manages its own internal addresses:
# - IPv4: 10.0.2.2 (gateway), 10.0.2.3 (DNS)
# - IPv6: fd00::2 (gateway), fd00::3 (DNS)
# No IP addresses on TAPs - bridge handles L2 forwarding
ip tuntap add {slirp_dev} mode tap
ip link set {slirp_dev} up

# Create TAP device for Firecracker (must exist before Firecracker starts)
ip tuntap add {fc_tap} mode tap
ip link set {fc_tap} up

# Create bridge to connect slirp0 and tap0
# Bridge operates at L2, preserving MAC addresses for proper ARP/NDP
ip link add br0 type bridge
ip link set br0 up

# Add both TAPs to the bridge
ip link set {slirp_dev} master br0
ip link set {fc_tap} master br0

# Add IP to bridge for health checks (namespace needs route to reach guest)
# namespace_ip is on the same subnet as guest and slirp (10.0.2.2/3)
ip addr add {namespace_ip}/24 dev br0

# Set up loopback
ip link set lo up

"#,
            slirp_dev = self.slirp_device,
            fc_tap = self.tap_device,
            namespace_ip = self.namespace_ip,
        )
    }

    /// Build the nsenter prefix command for running processes in the namespace
    ///
    /// Returns: ["nsenter", "-t", "PID", "-U", "-n", "--preserve-credentials", "--"]
    /// The --preserve-credentials flag keeps UID/GID/groups (including kvm) for KVM access.
    /// Append command and args after this.
    pub fn build_nsenter_prefix(&self, holder_pid: u32) -> Vec<String> {
        vec![
            "nsenter".to_string(),
            "-t".to_string(),
            holder_pid.to_string(),
            "-U".to_string(),
            "-n".to_string(),
            "--preserve-credentials".to_string(),
            "--".to_string(),
        ]
    }

    /// Get a human-readable representation of the rootless networking flow
    pub fn rootless_flow_string(&self) -> String {
        "holder(unshare --map-root-user) + nsenter for setup/firecracker".to_string()
    }

    /// Start slirp4netns process attached to the namespace
    /// Called after Firecracker has started (so we have the namespace PID)
    pub async fn start_slirp(&mut self, namespace_pid: u32) -> Result<()> {
        let api_socket =
            paths::data_dir().join(format!("slirp-{}.sock", truncate_id(&self.vm_id, 8)));

        if api_socket.exists() {
            tokio::fs::remove_file(&api_socket).await?;
        }

        // Create ready pipe
        let (ready_read_fd, ready_write_fd) = nix::unistd::pipe()?;
        let ready_read_raw = ready_read_fd.as_raw_fd();
        let ready_write_raw = ready_write_fd.as_raw_fd();

        info!(
            namespace_pid = namespace_pid,
            slirp_tap = %self.slirp_device,
            api_socket = %api_socket.display(),
            "starting slirp4netns (creating TAP, no IP assignment)"
        );

        // Start slirp4netns WITHOUT --configure so it doesn't assign an IP
        // This avoids the issue where DNAT doesn't work for local addresses
        // The TAP is created and connected, but we handle routing ourselves
        // Enable IPv6 for hosts that use IPv6-only DNS/networking
        let slirp_path = get_slirp4netns_path();
        let mut cmd = Command::new(slirp_path);
        cmd.arg("--ready-fd")
            .arg(ready_write_raw.to_string())
            .arg("--api-socket")
            .arg(&api_socket)
            .arg("--enable-ipv6"); // Enable IPv6 (fd00::/64, DNS at fd00::3)

        // If host has IPv6 connectivity, tell slirp4netns which address to use
        // for outbound IPv6 connections. This enables IPv6 forwarding on IPv6-only hosts.
        if let Some(ipv6_addr) = get_host_ipv6_address() {
            info!(outbound_addr6 = %ipv6_addr, "enabling slirp4netns IPv6 outbound");
            cmd.arg("--outbound-addr6").arg(&ipv6_addr);
        }

        cmd.arg(namespace_pid.to_string())
            .arg(&self.slirp_device)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let child = cmd.spawn().context("failed to spawn slirp4netns")?;

        drop(ready_write_fd);

        // Wait for ready signal
        let mut ready_buf = [0u8; 1];
        let read_result =
            tokio::task::spawn_blocking(move || nix::unistd::read(ready_read_raw, &mut ready_buf))
                .await?;

        drop(ready_read_fd);

        match read_result {
            Ok(1) => info!("slirp4netns ready"),
            Ok(0) => anyhow::bail!("slirp4netns exited before becoming ready"),
            Ok(n) => anyhow::bail!("unexpected read from slirp4netns ready_fd: {} bytes", n),
            Err(e) => anyhow::bail!("failed to read from slirp4netns ready_fd: {}", e),
        }

        self.slirp_process = Some(child);
        self.api_socket_path = Some(api_socket);

        Ok(())
    }

    /// Setup port forwarding via slirp4netns API socket
    ///
    /// For rootless mode, each VM gets a unique loopback IP (127.x.y.z) so multiple
    /// VMs can all forward the same port (e.g., all VMs can have -p 8080:80).
    /// On Linux, the entire 127.0.0.0/8 range routes to loopback without needing
    /// `ip addr add` - we just bind directly. Fully rootless!
    async fn setup_port_forwarding(&self) -> Result<()> {
        let api_socket = self
            .api_socket_path
            .as_ref()
            .context("API socket not configured")?;

        for mapping in &self.port_mappings {
            // Use VM's unique loopback IP so multiple VMs can use same port
            // User can override with explicit IP (0.0.0.0 for all interfaces)
            let bind_addr = match &mapping.host_ip {
                Some(ip) => ip.as_str(),
                None => self.loopback_ip.as_deref().unwrap_or("127.0.0.1"),
            };

            let proto = match mapping.proto {
                super::Protocol::Tcp => "tcp",
                super::Protocol::Udp => "udp",
            };

            // Port forward to the actual guest IP (slirp4netns forwards directly to guest)
            let request = serde_json::json!({
                "execute": "add_hostfwd",
                "arguments": {
                    "proto": proto,
                    "host_addr": bind_addr,
                    "host_port": mapping.host_port,
                    "guest_addr": &self.guest_ip,
                    "guest_port": mapping.guest_port
                }
            });

            info!(
                proto = proto,
                host = %format!("{}:{}", bind_addr, mapping.host_port),
                guest = %format!("{}:{}", self.guest_ip, mapping.guest_port),
                "adding port forward"
            );

            let mut stream = UnixStream::connect(api_socket)
                .await
                .context("connecting to slirp4netns API socket")?;

            let request_str = serde_json::to_string(&request)? + "\n";
            stream.write_all(request_str.as_bytes()).await?;
            stream.shutdown().await?;

            let mut reader = BufReader::new(stream);
            let mut response_line = String::new();
            reader.read_line(&mut response_line).await?;

            debug!(response = %response_line.trim(), "slirp4netns API response");

            if response_line.contains("error") {
                warn!(response = %response_line.trim(), "port forwarding may have failed");
            }
        }

        Ok(())
    }

    /// Get guest IP address for kernel boot args
    pub fn guest_ip(&self) -> &str {
        &self.guest_ip
    }

    /// Get namespace host IP (gateway for guest)
    pub fn namespace_ip(&self) -> &str {
        &self.namespace_ip
    }
}

#[async_trait::async_trait]
impl NetworkManager for SlirpNetwork {
    async fn setup(&mut self) -> Result<NetworkConfig> {
        info!(vm_id = %self.vm_id, "setting up rootless networking with slirp4netns");

        // Health checks use nsenter (don't need loopback)
        // Port forwarding uses loopback IP for unique binding per VM
        info!(
            guest_ip = %self.guest_ip,
            namespace_ip = %self.namespace_ip,
            loopback_ip = ?self.loopback_ip,
            "network configuration (nsenter health checks, loopback port forwarding)"
        );

        let guest_mac = generate_mac();

        // Generate health check URL from loopback IP if available
        let health_check_url = self
            .loopback_ip
            .as_ref()
            .map(|ip| format!("http://{}:8080/", ip));

        // Get DNS servers for the guest to use
        // slirp4netns provides internal DNS forwarders at 10.0.2.3 (IPv4) and fd00::3 (IPv6).
        // The guest sends a gratuitous NDP NA at boot to teach slirp its MAC address,
        // so slirp can route DNS responses back correctly.
        let dns_server = match get_host_dns_servers() {
            Ok(servers) if !servers.is_empty() => {
                let all_ipv6 = servers.iter().all(|s| s.contains("::"));
                if all_ipv6 {
                    // IPv6-only host: use slirp's IPv6 DNS forwarder
                    // Guest's NDP NA teaches slirp how to reach fd00::100
                    info!(servers = ?servers, "host DNS is IPv6-only, using slirp IPv6 DNS (fd00::3)");
                    Some("fd00::3".to_string())
                } else {
                    // Mixed network: use IPv4 DNS first, IPv6 as fallback
                    info!(servers = ?servers, "using slirp DNS forwarders");
                    Some("10.0.2.3,fd00::3".to_string())
                }
            }
            _ => {
                // Fallback to slirp's internal DNS forwarders
                info!("using slirp4netns internal DNS forwarder");
                Some("10.0.2.3,fd00::3".to_string())
            }
        };

        // Get search domains - critical for resolving short hostnames in enterprise networks
        let search_domains = get_host_dns_search();
        let dns_search = if search_domains.is_empty() {
            None
        } else {
            Some(search_domains.join(","))
        };

        Ok(NetworkConfig {
            tap_device: self.tap_device.clone(),
            guest_mac,
            guest_ip: Some(format!("{}/24", self.guest_ip)),
            host_ip: Some(self.namespace_ip.clone()),
            host_veth: None,
            loopback_ip: self.loopback_ip.clone(), // For port forwarding (no ip addr add needed!)
            health_check_port: Some(8080),         // Unprivileged port, forwards to guest:80
            health_check_url,
            dns_server,
            dns_search,
            http_proxy: None,
        })
    }

    async fn post_start(&mut self, holder_pid: u32) -> Result<()> {
        info!(
            holder_pid = holder_pid,
            "starting slirp4netns for rootless networking"
        );

        self.start_slirp(holder_pid).await?;

        // Health checks now use nsenter to curl the guest directly
        // No port forwarding needed for health checks

        // User-specified port mappings still use slirp4netns port forwarding
        if !self.port_mappings.is_empty() {
            self.setup_port_forwarding().await?;
        }

        Ok(())
    }

    async fn cleanup(&mut self) -> Result<()> {
        info!(vm_id = %self.vm_id, "cleaning up slirp4netns resources");

        if let Some(mut process) = self.slirp_process.take() {
            if let Err(e) = process.kill().await {
                warn!("failed to kill slirp4netns: {}", e);
            }
            let _ = process.wait().await;
        }

        if let Some(ref socket_path) = self.api_socket_path {
            if socket_path.exists() {
                if let Err(e) = tokio::fs::remove_file(socket_path).await {
                    warn!("failed to remove slirp API socket: {}", e);
                }
            }
        }

        // No loopback address cleanup needed - we don't allocate host loopback IPs anymore

        info!(vm_id = %self.vm_id, "slirp4netns cleanup complete");
        Ok(())
    }

    fn tap_device(&self) -> &str {
        &self.tap_device
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_network_creation() {
        let net = SlirpNetwork::new("vm-test123".to_string(), "tap0".to_string(), vec![]);

        assert_eq!(net.tap_device, "tap0");
        assert_eq!(net.slirp_device, "slirp0");
        // Fixed IPs on slirp's 10.0.2.x subnet (isolated per user namespace)
        assert_eq!(net.guest_ip, "10.0.2.15");
        assert_eq!(net.namespace_ip, "10.0.2.1");
        assert_eq!(net.guest_subnet, "10.0.2.0/24");
    }

    #[test]
    fn test_with_guest_ip_legacy() {
        let net = SlirpNetwork::new("vm-test123".to_string(), "tap0".to_string(), vec![]);

        // Legacy 192.168.x.x format from old veth-based architecture
        let net = net.with_guest_ip("192.168.42.2/24".to_string());

        assert_eq!(net.guest_ip, "192.168.42.2");
        assert_eq!(net.namespace_ip, "192.168.42.1");
        assert_eq!(net.guest_subnet, "192.168.42.0/24");
    }

    #[test]
    fn test_with_guest_ip_slirp_native() {
        let net = SlirpNetwork::new("vm-test123".to_string(), "tap0".to_string(), vec![]);

        // New 10.0.2.x format from slirp4netns native subnet
        let net = net.with_guest_ip("10.0.2.15/24".to_string());

        // Should preserve the actual IP, not convert to 192.168.x.x
        assert_eq!(net.guest_ip, "10.0.2.15");
        assert_eq!(net.namespace_ip, "10.0.2.1");
        assert_eq!(net.guest_subnet, "10.0.2.0/24");
    }
}
