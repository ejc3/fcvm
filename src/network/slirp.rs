use anyhow::{Context, Result};
use std::os::unix::io::AsRawFd;
use std::path::PathBuf;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::process::{Child, Command};
use tracing::{debug, info, warn};

use super::{types::generate_mac, NetworkConfig, NetworkManager, PortMapping};
use crate::paths;
use crate::state::truncate_id;

/// Guest network addressing (isolated per VM namespace)
const GUEST_SUBNET: &str = "192.168.1.0/24";
const GUEST_IP: &str = "192.168.1.2";
const NAMESPACE_IP: &str = "192.168.1.1";

/// Guest IPv6 addressing (ULA prefix for guest network)
/// slirp4netns uses fd00::/64 by default for IPv6 with gateway at fd00::2
const GUEST_IPV6_SUBNET: &str = "fd00:1::/64";
const GUEST_IPV6: &str = "fd00:1::2";
const NAMESPACE_IPV6: &str = "fd00:1::1";
/// slirp4netns IPv6 gateway (fixed by slirp4netns)
const SLIRP_IPV6_GATEWAY: &str = "fd00::2";

/// Default TAP device name for slirp4netns
const SLIRP_DEVICE_NAME: &str = "slirp0";

/// Rootless networking using slirp4netns with dual-TAP architecture
///
/// This mode uses user namespaces and slirp4netns for true unprivileged operation.
/// No sudo/root required - everything runs in user namespace via nsenter.
///
/// Architecture (Dual-TAP):
/// ```text
/// Host                    | User Namespace (unshare --user --map-root-user --net)
///                         |
/// slirp4netns <-----------+-- slirp0 (10.0.2.100/24) <--- IP forwarding <--- tap0
///   (userspace NAT)       |                                                     |
///                         |                                              Firecracker VM
///                         |                                              (guest: 192.168.x.2)
/// ```
///
/// Key insight: slirp4netns and Firecracker CANNOT share a TAP device (both need exclusive access).
/// Solution: Use two TAP devices with IP forwarding between them.
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

    // Network addressing (IPv4)
    guest_subnet: String, // tap0: 192.168.x.0/24 (derived from vm_id)
    guest_ip: String,     // Guest VM IP (192.168.x.2)
    namespace_ip: String, // Namespace host IP on tap0 (192.168.x.1)

    // Network addressing (IPv6)
    guest_ipv6_subnet: String, // fd00:1::/64
    guest_ipv6: String,        // fd00:1::2
    namespace_ipv6: String,    // fd00:1::1

    // State (populated during setup)
    api_socket_path: Option<PathBuf>,
    slirp_process: Option<Child>,
    loopback_ip: Option<String>, // Unique loopback IP for port forwarding (127.x.y.z)
}

impl SlirpNetwork {
    pub fn new(vm_id: String, tap_device: String, port_mappings: Vec<PortMapping>) -> Self {
        // Guest subnet is always 192.168.1.0/24 - no conflicts because each VM
        // runs in its own isolated user namespace
        Self {
            vm_id,
            tap_device,
            slirp_device: SLIRP_DEVICE_NAME.to_string(),
            port_mappings,
            guest_subnet: GUEST_SUBNET.to_string(),
            guest_ip: GUEST_IP.to_string(),
            namespace_ip: NAMESPACE_IP.to_string(),
            guest_ipv6_subnet: GUEST_IPV6_SUBNET.to_string(),
            guest_ipv6: GUEST_IPV6.to_string(),
            namespace_ipv6: NAMESPACE_IPV6.to_string(),
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
    /// The guest_ip should include CIDR notation (e.g., "192.168.155.2/24")
    /// but the /24 is stripped when parsing since we always use /24 subnets.
    pub fn with_guest_ip(mut self, guest_ip: String) -> Self {
        // Parse the IP (strip CIDR notation if present)
        let ip_only = guest_ip.split('/').next().unwrap_or(&guest_ip);

        // Extract subnet from IP (e.g., "192.168.155.2" -> subnet 155)
        let parts: Vec<&str> = ip_only.split('.').collect();
        if parts.len() == 4 {
            if let Ok(subnet_id) = parts[2].parse::<u8>() {
                self.guest_subnet = format!("192.168.{}.0/24", subnet_id);
                self.guest_ip = format!("192.168.{}.2", subnet_id);
                self.namespace_ip = format!("192.168.{}.1", subnet_id);
            }
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
    /// This script creates both TAP devices and configures networking with IPv4 and IPv6.
    /// Run via: nsenter -t HOLDER_PID -U -n -- bash -c '<this script>'
    pub fn build_setup_script(&self) -> String {
        format!(
            r#"
set -e

# Create slirp0 TAP for slirp4netns connectivity
# Use 10.0.2.100 as the address for DNAT to work with port forwarding
# fd00::100 for IPv6 (slirp4netns uses fd00::/64 subnet with gateway fd00::2)
ip tuntap add {slirp_dev} mode tap
ip addr add 10.0.2.100/24 dev {slirp_dev}
ip -6 addr add fd00::100/64 dev {slirp_dev}
ip link set {slirp_dev} up

# Create TAP device for Firecracker (must exist before Firecracker starts)
ip tuntap add {fc_tap} mode tap
ip addr add {ns_ip}/24 dev {fc_tap}
ip -6 addr add {ns_ipv6}/64 dev {fc_tap}
ip link set {fc_tap} up

# Set up loopback
ip link set lo up

# Enable IP forwarding (required for NAT to work)
# Must enable both global and per-interface forwarding.
# The host's net.ipv4.conf.default.forwarding=0 means new interfaces
# inherit forwarding=0 even when ip_forward=1.
sysctl -w net.ipv4.ip_forward=1
sysctl -w net.ipv4.conf.all.forwarding=1
sysctl -w net.ipv4.conf.{slirp_dev}.forwarding=1
sysctl -w net.ipv4.conf.{fc_tap}.forwarding=1

# Enable IPv6 forwarding
sysctl -w net.ipv6.conf.all.forwarding=1
sysctl -w net.ipv6.conf.{slirp_dev}.forwarding=1 2>/dev/null || true
sysctl -w net.ipv6.conf.{fc_tap}.forwarding=1 2>/dev/null || true

# Set default route via slirp gateway (10.0.2.2 is slirp4netns internal gateway)
ip route add default via 10.0.2.2 dev {slirp_dev}

# Set IPv6 default route via slirp gateway (fd00::2 is slirp4netns IPv6 gateway)
ip -6 route add default via {slirp_ipv6_gw} dev {slirp_dev} 2>/dev/null || true

# Allow forwarding between slirp0 and FC TAP (IPv4)
iptables -A FORWARD -i {slirp_dev} -o {fc_tap} -j ACCEPT 2>/dev/null || true
iptables -A FORWARD -i {fc_tap} -o {slirp_dev} -j ACCEPT 2>/dev/null || true

# Allow forwarding between slirp0 and FC TAP (IPv6)
ip6tables -A FORWARD -i {slirp_dev} -o {fc_tap} -j ACCEPT 2>/dev/null || true
ip6tables -A FORWARD -i {fc_tap} -o {slirp_dev} -j ACCEPT 2>/dev/null || true

# Set up iptables MASQUERADE for traffic from guest subnet (egress)
# This NATs guest traffic (192.168.x.x) to slirp0's address (10.0.2.100)
iptables -t nat -A POSTROUTING -s {guest_subnet} -o {slirp_dev} -j MASQUERADE 2>/dev/null || true

# IPv6 NAT66 for guest traffic
ip6tables -t nat -A POSTROUTING -s {guest_ipv6_subnet} -o {slirp_dev} -j MASQUERADE 2>/dev/null || true

# Set up DNAT for inbound connections from slirp4netns
# When slirp4netns forwards traffic to 10.0.2.100, redirect it to the actual guest IP
# This enables port forwarding: host -> slirp4netns -> 10.0.2.100 -> DNAT -> guest (192.168.x.2)
iptables -t nat -A PREROUTING -d 10.0.2.100 -j DNAT --to-destination {guest_ip} 2>/dev/null || true
"#,
            slirp_dev = self.slirp_device,
            fc_tap = self.tap_device,
            ns_ip = self.namespace_ip,
            ns_ipv6 = self.namespace_ipv6,
            guest_subnet = self.guest_subnet,
            guest_ip = self.guest_ip,
            guest_ipv6_subnet = self.guest_ipv6_subnet,
            slirp_ipv6_gw = SLIRP_IPV6_GATEWAY,
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

    /// Detect host's global IPv6 address for slirp4netns outbound traffic
    fn detect_host_ipv6() -> Option<String> {
        // Parse ip -6 addr output to find global scope address
        let output = std::process::Command::new("ip")
            .args(["-6", "addr", "show", "scope", "global"])
            .output()
            .ok()?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.lines() {
            let line = line.trim();
            if line.starts_with("inet6 ") {
                // Format: "inet6 2600:1f1c:494:201:68a2:a82b:d28:c354/128 scope global ..."
                if let Some(addr_part) = line.strip_prefix("inet6 ") {
                    if let Some(addr) = addr_part.split('/').next() {
                        // Skip link-local (fe80::) and ULA (fd00::)
                        if !addr.starts_with("fe80:") && !addr.starts_with("fd") {
                            return Some(addr.to_string());
                        }
                    }
                }
            }
        }
        None
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

        // Detect host's global IPv6 address for outbound traffic
        let host_ipv6 = Self::detect_host_ipv6();

        info!(
            namespace_pid = namespace_pid,
            slirp_tap = %self.slirp_device,
            api_socket = %api_socket.display(),
            host_ipv6 = ?host_ipv6,
            "starting slirp4netns with IPv6 (creating TAP, no IP assignment)"
        );

        // Start slirp4netns WITHOUT --configure so it doesn't assign an IP
        // This avoids the issue where DNAT doesn't work for local addresses
        // The TAP is created and connected, but we handle routing ourselves
        // Use --enable-ipv6 for IPv6 egress support
        let mut cmd = Command::new("slirp4netns");
        cmd.arg("--ready-fd")
            .arg(ready_write_raw.to_string())
            .arg("--api-socket")
            .arg(&api_socket)
            .arg("--enable-ipv6");

        // If host has global IPv6, tell slirp4netns to use it for outbound connections
        if let Some(ref ipv6) = host_ipv6 {
            cmd.arg("--outbound-addr6").arg(ipv6);
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

            // Port forward to slirp's internal guest IP (10.0.2.100)
            // which then gets routed to the actual guest via IP forwarding
            let request = serde_json::json!({
                "execute": "add_hostfwd",
                "arguments": {
                    "proto": proto,
                    "host_addr": bind_addr,
                    "host_port": mapping.host_port,
                    "guest_addr": "10.0.2.100",
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

        // Check if host has IPv6 - if so, we'll configure it in the guest too
        let (guest_ipv6, host_ipv6) = if Self::detect_host_ipv6().is_some() {
            // Guest gets fd00:1::2, gateway is fd00:1::1 (the tap device in namespace)
            (
                Some(self.guest_ipv6.clone()),
                Some(self.namespace_ipv6.clone()),
            )
        } else {
            (None, None)
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
            dns_server: Some("10.0.2.3".to_string()), // slirp4netns built-in DNS forwarder
            guest_ipv6,
            host_ipv6,
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
        // Fixed IPs - all VMs use same subnet (isolated per namespace)
        assert_eq!(net.guest_ip, "192.168.1.2");
        assert_eq!(net.namespace_ip, "192.168.1.1");
        assert_eq!(net.guest_subnet, "192.168.1.0/24");
    }

    #[test]
    fn test_with_guest_ip() {
        let net = SlirpNetwork::new("vm-test123".to_string(), "tap0".to_string(), vec![]);

        // Clones can override guest IP if snapshot used different subnet
        let net = net.with_guest_ip("192.168.42.2/24".to_string());

        assert_eq!(net.guest_ip, "192.168.42.2");
        assert_eq!(net.namespace_ip, "192.168.42.1");
        assert_eq!(net.guest_subnet, "192.168.42.0/24");
    }
}
