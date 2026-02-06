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

/// Guest network addressing - uses slirp4netns network directly via bridge
/// Bridge mode: slirp0 and tap-fc are bridged at L2, no NAT needed
const GUEST_IP: &str = "10.0.2.100";
const GUEST_GATEWAY: &str = "10.0.2.2";
const GUEST_DNS: &str = "10.0.2.3";
/// Namespace IP on bridge - enables nsenter health checks to route to guest
const NAMESPACE_IP: &str = "10.0.2.1";

/// Guest IPv6 addressing (slirp4netns IPv6 network)
/// slirp4netns uses fd00::/64 by default for IPv6 with gateway at fd00::2
const GUEST_IPV6: &str = "fd00::100";
const GUEST_IPV6_GATEWAY: &str = "fd00::2";

/// Bridge device name
const BRIDGE_DEVICE: &str = "br0";

/// Default TAP device name for slirp4netns
const SLIRP_DEVICE_NAME: &str = "slirp0";

/// Custom slirp4netns path (built with libslirp 4.8.0+ for IPv6 DNS support)
const CUSTOM_SLIRP_PATH: &str = "/mnt/fcvm-btrfs/deps/bin/slirp4netns";

/// Find the best slirp4netns binary to use.
///
/// Prefers custom build at /mnt/fcvm-btrfs/deps/bin/slirp4netns if it exists,
/// as it has libslirp 4.8.0+ with IPv6 DNS support. Falls back to system binary.
fn find_slirp4netns() -> String {
    let custom_path = std::path::Path::new(CUSTOM_SLIRP_PATH);
    if custom_path.exists() && custom_path.is_file() {
        CUSTOM_SLIRP_PATH.to_string()
    } else {
        "slirp4netns".to_string()
    }
}

/// Rootless networking using slirp4netns with bridge architecture
///
/// This mode uses user namespaces and slirp4netns for true unprivileged operation.
/// No sudo/root required - everything runs in user namespace via nsenter.
///
/// Architecture (L2 Bridge - no NAT required):
/// ```text
/// Host                    | User Namespace (unshare --user --map-root-user --net)
///                         |
/// slirp4netns <-----------+-- slirp0 --+
///   (userspace NAT)       |            |
///                         |           br0 (L2 bridge)
///                         |            |
///                         |          tap-fc ---> Firecracker VM
///                         |                      (guest: 10.0.2.100)
/// ```
///
/// Key insight: slirp4netns and Firecracker CANNOT share a TAP device (both need exclusive access).
/// Solution: Bridge both TAP devices at L2 - no IP forwarding or iptables NAT needed!
/// The bridge forwards Ethernet frames directly, preserving MAC addresses.
///
/// Setup sequence (3-phase with nsenter):
/// 1. Spawn holder process: `unshare --user --map-root-user --net -- sleep infinity`
/// 2. Run setup via nsenter: create bridge, TAPs, add TAPs to bridge
/// 3. Start slirp4netns attached to holder's namespace
/// 4. Run Firecracker via nsenter: `nsenter -t HOLDER_PID -U -n -- firecracker ...`
/// 5. Health checks via nsenter: `nsenter -t HOLDER_PID -U -n -- curl guest_ip:80`
pub struct SlirpNetwork {
    vm_id: String,
    tap_device: String,   // TAP device for Firecracker (tap-fc)
    slirp_device: String, // TAP device for slirp4netns (slirp0)
    port_mappings: Vec<PortMapping>,

    // Network addressing (IPv4) - guest uses slirp4netns network directly
    guest_ip: String, // Guest VM IP (10.0.2.100)

    // Network addressing (IPv6)
    guest_ipv6: String, // fd00::100

    // State (populated during setup)
    api_socket_path: Option<PathBuf>,
    slirp_process: Option<Child>,
    loopback_ip: Option<String>, // Unique loopback IP for port forwarding (127.x.y.z)
}

impl SlirpNetwork {
    pub fn new(vm_id: String, tap_device: String, port_mappings: Vec<PortMapping>) -> Self {
        // With bridge architecture, guest is directly on slirp4netns network
        // No per-VM subnet needed - each VM is in its own namespace
        Self {
            vm_id,
            tap_device,
            slirp_device: SLIRP_DEVICE_NAME.to_string(),
            port_mappings,
            guest_ip: GUEST_IP.to_string(),
            guest_ipv6: GUEST_IPV6.to_string(),
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
    /// This script creates a bridge and both TAP devices for L2 forwarding.
    /// No iptables NAT needed - the bridge handles Ethernet frame forwarding.
    /// Run via: nsenter -t HOLDER_PID -U -n -- bash -c '<this script>'
    pub fn build_setup_script(&self) -> String {
        format!(
            r#"
set -e

# Create L2 bridge - connects slirp0 and Firecracker TAP
ip link add {bridge} type bridge
ip link set {bridge} up

# Create slirp0 TAP for slirp4netns and add to bridge
# No IP on slirp0 - it's just a bridge port
ip tuntap add {slirp_dev} mode tap
ip link set {slirp_dev} master {bridge}
ip link set {slirp_dev} up

# Create TAP device for Firecracker and add to bridge
# No IP on fc_tap - it's just a bridge port
ip tuntap add {fc_tap} mode tap
ip link set {fc_tap} master {bridge}
ip link set {fc_tap} up

# Set up loopback
ip link set lo up

# Add IP to bridge for health checks (namespace needs route to reach guest)
# This enables nsenter to curl guest directly via the 10.0.2.x subnet
ip addr add {namespace_ip}/24 dev {bridge}

# No IP forwarding or iptables NAT needed!
# Bridge handles L2 forwarding directly.
# Guest uses slirp4netns network (10.0.2.x) directly.
"#,
            bridge = BRIDGE_DEVICE,
            slirp_dev = self.slirp_device,
            fc_tap = self.tap_device,
            namespace_ip = NAMESPACE_IP,
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

    /// Detect HTTP proxy from host environment
    ///
    /// On IPv6-only hosts, traffic must go through a proxy.
    /// Returns the proxy URL with IPv6 address resolved from hostname.
    fn detect_http_proxy() -> Option<String> {
        // Check environment variables for proxy
        let proxy_url = std::env::var("HTTP_PROXY")
            .or_else(|_| std::env::var("http_proxy"))
            .or_else(|_| std::env::var("HTTPS_PROXY"))
            .or_else(|_| std::env::var("https_proxy"))
            .ok()?;

        // Parse the proxy URL to get hostname and port
        // Format: http://hostname:port or http://[ipv6]:port
        if let Some(rest) = proxy_url.strip_prefix("http://") {
            let host_port = rest.trim_end_matches('/');

            // If it's already an IPv6 literal, return as-is
            if host_port.starts_with('[') {
                return Some(proxy_url);
            }

            // Otherwise, try to resolve the hostname to IPv6
            if let Some((host, port)) = host_port.rsplit_once(':') {
                // Use getent to resolve hostname to IPv6 address
                if let Ok(output) = std::process::Command::new("getent")
                    .args(["hosts", host])
                    .output()
                {
                    let stdout = String::from_utf8_lossy(&output.stdout);
                    // getent output: "2001:db8::1 proxy.example.com"
                    if let Some(ipv6) = stdout.split_whitespace().next() {
                        // Check if it's IPv6 (contains ::)
                        if ipv6.contains(':') {
                            return Some(format!("http://[{}]:{}", ipv6, port));
                        }
                    }
                }
                // Fall back to original URL if resolution fails
                return Some(proxy_url);
            }
        }

        Some(proxy_url)
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
        //
        // Prefer custom slirp4netns with newer libslirp (for IPv6 DNS support).
        // System slirp4netns on RHEL9/CentOS9 has libslirp 4.4.0 which can't
        // forward DNS queries to IPv6 nameservers. Custom build has 4.8.0+.
        let slirp_path = find_slirp4netns();
        debug!(slirp_path = %slirp_path, "using slirp4netns binary");
        let mut cmd = Command::new(&slirp_path);
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

            // Port forward directly to guest IP (10.0.2.100)
            // With bridge mode, guest is directly on slirp network
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

    /// Get gateway IP for guest (slirp4netns gateway)
    pub fn gateway_ip(&self) -> &str {
        GUEST_GATEWAY
    }
}

#[async_trait::async_trait]
impl NetworkManager for SlirpNetwork {
    async fn setup(&mut self) -> Result<NetworkConfig> {
        info!(vm_id = %self.vm_id, "setting up rootless networking with slirp4netns (bridge mode)");

        // Health checks use nsenter (don't need loopback)
        // Port forwarding uses loopback IP for unique binding per VM
        info!(
            guest_ip = %self.guest_ip,
            gateway = %GUEST_GATEWAY,
            loopback_ip = ?self.loopback_ip,
            "network configuration (bridge mode, nsenter health checks)"
        );

        let guest_mac = generate_mac();

        // Generate health check URL from loopback IP if available
        let health_check_url = self
            .loopback_ip
            .as_ref()
            .map(|ip| format!("http://{}:8080/", ip));

        // Check if host has IPv6 - if so, we'll configure it in the guest too
        let (guest_ipv6, host_ipv6) = if Self::detect_host_ipv6().is_some() {
            // Guest gets fd00::100, gateway is fd00::2 (slirp4netns IPv6 gateway)
            (
                Some(self.guest_ipv6.clone()),
                Some(GUEST_IPV6_GATEWAY.to_string()),
            )
        } else {
            (None, None)
        };

        // Detect proxy for IPv6-only hosts
        let http_proxy = Self::detect_http_proxy();
        if let Some(ref proxy) = http_proxy {
            info!(proxy = %proxy, "detected HTTP proxy for IPv6-only network");
        }

        Ok(NetworkConfig {
            tap_device: self.tap_device.clone(),
            guest_mac,
            guest_ip: Some(format!("{}/24", self.guest_ip)),
            host_ip: Some(GUEST_GATEWAY.to_string()), // slirp4netns gateway
            host_veth: None,
            loopback_ip: self.loopback_ip.clone(), // For port forwarding (no ip addr add needed!)
            health_check_port: Some(8080),         // Unprivileged port, forwards to guest:80
            health_check_url,
            dns_server: Some(GUEST_DNS.to_string()), // slirp4netns built-in DNS forwarder
            guest_ipv6,
            host_ipv6,
            dns_search: None,
            http_proxy,
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
        // With bridge mode, guest is directly on slirp4netns network
        assert_eq!(net.guest_ip, "10.0.2.100");
        assert_eq!(net.gateway_ip(), "10.0.2.2");
    }
}
