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

/// slirp4netns network addressing constants
/// slirp0 device is assigned this IP for routing to slirp4netns
const SLIRP_CIDR: &str = "10.0.2.100/24";

/// Guest network addressing (isolated per VM namespace)
const GUEST_SUBNET: &str = "192.168.1.0/24";
const GUEST_IP: &str = "192.168.1.2";
const NAMESPACE_IP: &str = "192.168.1.1";

/// Default TAP device name for slirp4netns
const SLIRP_DEVICE_NAME: &str = "slirp0";

/// Rootless networking using slirp4netns with dual-TAP architecture
///
/// This mode uses user namespaces and slirp4netns for true unprivileged operation.
/// No sudo/root required - everything runs in user namespace via nsenter.
///
/// Architecture (Dual-TAP):
/// ```text
/// Host                    | User Namespace (unshare --user --map-auto --net)
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
/// 1. Spawn holder process: `unshare --user --map-auto --net -- cat`
/// 2. Run setup via nsenter: create TAPs, iptables, IP forwarding
/// 3. Start slirp4netns attached to holder's namespace
/// 4. Run Firecracker via nsenter: `nsenter -t HOLDER_PID -U -n -- firecracker ...`
/// 5. Health checks via nsenter: `nsenter -t HOLDER_PID -U -n -- curl guest_ip:80`
pub struct SlirpNetwork {
    vm_id: String,
    tap_device: String,   // TAP device for Firecracker (tap0)
    slirp_device: String, // TAP device for slirp4netns (slirp0)
    port_mappings: Vec<PortMapping>,

    // Network addressing
    slirp_cidr: String,   // slirp0: 10.0.2.100/24, gateway 10.0.2.2
    guest_subnet: String, // tap0: 192.168.x.0/24 (derived from vm_id)
    guest_ip: String,     // Guest VM IP (192.168.x.2)
    namespace_ip: String, // Namespace host IP on tap0 (192.168.x.1)

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
            slirp_cidr: SLIRP_CIDR.to_string(),
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
    /// UID detection for container compatibility:
    /// - UID 0 (root or inside container): use --map-root-user (simple 1:1 mapping)
    /// - UID != 0 (unprivileged user): use --map-auto (needs subordinate UIDs from /etc/subuid)
    ///
    /// This allows fcvm to work both natively and inside containers.
    pub fn build_holder_command(&self) -> Vec<String> {
        // Detect if we're running as root (either real root or inside a container)
        let uid = unsafe { libc::getuid() };
        let map_flag = if uid == 0 {
            "--map-root-user" // Simple 1:1 mapping, works in containers
        } else {
            "--map-auto" // Uses /etc/subuid, works for unprivileged users
        };

        vec![
            "unshare".to_string(),
            "--user".to_string(),
            map_flag.to_string(),
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
    pub fn build_setup_script(&self) -> String {
        format!(
            r#"
set -e

# Create slirp0 TAP for slirp4netns connectivity
ip tuntap add {slirp_dev} mode tap
ip addr add {slirp_ip} dev {slirp_dev}
ip link set {slirp_dev} up

# Create TAP device for Firecracker (must exist before Firecracker starts)
ip tuntap add {fc_tap} mode tap
ip addr add {ns_ip}/24 dev {fc_tap}
ip link set {fc_tap} up

# Set up loopback
ip link set lo up

# Set default route via slirp gateway
ip route add default via 10.0.2.2 dev {slirp_dev}

# Set up iptables MASQUERADE for traffic from guest subnet
# This NATs guest traffic (192.168.x.x) to slirp0's address (10.0.2.100)
iptables -t nat -A POSTROUTING -s {guest_subnet} -o {slirp_dev} -j MASQUERADE 2>/dev/null || true

# Set up DNAT for inbound connections from slirp4netns
# When slirp4netns forwards traffic to 10.0.2.100, redirect it to the actual guest IP
# This enables port forwarding: host -> slirp4netns -> 10.0.2.100 -> DNAT -> guest (192.168.x.2)
iptables -t nat -A PREROUTING -d 10.0.2.100 -j DNAT --to-destination {guest_ip} 2>/dev/null || true
"#,
            slirp_dev = self.slirp_device,
            slirp_ip = self.slirp_cidr,
            fc_tap = self.tap_device,
            ns_ip = self.namespace_ip,
            guest_subnet = self.guest_subnet,
            guest_ip = self.guest_ip,
        )
    }

    /// Build the nsenter prefix command for running processes in the namespace
    ///
    /// Returns: ["nsenter", "-t", "PID", "-U", "-n", "--"]
    /// Append firecracker command and args after this.
    pub fn build_nsenter_prefix(&self, holder_pid: u32) -> Vec<String> {
        vec![
            "nsenter".to_string(),
            "-t".to_string(),
            holder_pid.to_string(),
            "-U".to_string(),
            "-n".to_string(),
            "--".to_string(),
        ]
    }

    /// Get a human-readable representation of the rootless networking flow
    pub fn rootless_flow_string(&self) -> String {
        "holder(unshare --map-auto) + nsenter for setup/firecracker".to_string()
    }

    /// Start slirp4netns process attached to the namespace
    /// Called after Firecracker has started (so we have the namespace PID)
    pub async fn start_slirp(&mut self, namespace_pid: u32) -> Result<()> {
        let api_socket =
            paths::base_dir().join(format!("slirp-{}.sock", truncate_id(&self.vm_id, 8)));

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
            "starting slirp4netns (attaching to existing TAP)"
        );

        // Start slirp4netns WITHOUT --configure (TAP already exists and is configured)
        // slirp4netns will attach to the existing TAP device
        let mut cmd = Command::new("slirp4netns");
        cmd.arg("--ready-fd")
            .arg(ready_write_raw.to_string())
            .arg("--api-socket")
            .arg(&api_socket)
            .arg(namespace_pid.to_string())
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

    /// Setup health check port forward (loopback_ip:8080 → guest:80)
    ///
    /// Uses port 8080 on host (unprivileged) forwarding to port 80 in guest.
    /// This is fully rootless - no capabilities or sudo needed.
    /// Linux routes all of 127.0.0.0/8 to loopback without needing `ip addr add`.
    async fn setup_health_check_forward(&self, loopback_ip: &str) -> Result<()> {
        let api_socket = self
            .api_socket_path
            .as_ref()
            .context("API socket not configured")?;

        // Forward from unprivileged port 8080 on host to port 80 in guest
        // Port 8080 doesn't require CAP_NET_BIND_SERVICE
        let request = serde_json::json!({
            "execute": "add_hostfwd",
            "arguments": {
                "proto": "tcp",
                "host_addr": loopback_ip,
                "host_port": 8080,
                "guest_addr": "10.0.2.100",
                "guest_port": 80
            }
        });

        info!(
            loopback_ip = %loopback_ip,
            guest_ip = %self.guest_ip,
            "setting up health check port forward (8080 -> 80) - fully rootless!"
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

        debug!(response = %response_line.trim(), "slirp4netns health check forward response");

        if response_line.contains("error") {
            warn!(response = %response_line.trim(), "health check port forwarding may have failed");
        } else {
            info!("health check port forwarding configured successfully");
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

        Ok(NetworkConfig {
            tap_device: self.tap_device.clone(),
            guest_mac,
            guest_ip: Some(format!("{}/24", self.guest_ip)),
            host_ip: Some(self.namespace_ip.clone()),
            host_veth: None,
            loopback_ip: self.loopback_ip.clone(), // For port forwarding (no ip addr add needed!)
            health_check_port: Some(8080), // Unprivileged port, forwards to guest:80
        })
    }

    async fn post_start(&mut self, holder_pid: u32) -> Result<()> {
        info!(
            holder_pid = holder_pid,
            "starting slirp4netns for rootless networking"
        );

        self.start_slirp(holder_pid).await?;

        // Set up health check port forward (loopback_ip:80 → guest:80)
        // No ip addr add needed - Linux routes all of 127.0.0.0/8 to loopback!
        if let Some(loopback_ip) = &self.loopback_ip {
            self.setup_health_check_forward(loopback_ip).await?;
        }

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
