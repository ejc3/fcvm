use anyhow::{Context, Result};
use std::collections::HashSet;
use std::os::unix::io::AsRawFd;
use std::path::PathBuf;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::process::{Child, Command};
use tracing::{debug, info, warn};

use super::{types::generate_mac, NetworkConfig, NetworkManager, PortMapping};
use crate::paths;
use crate::state::{truncate_id, StateManager};

/// Rootless networking using slirp4netns with dual-TAP architecture
///
/// This mode uses user namespaces and slirp4netns for true unprivileged operation.
/// No sudo/root required - everything runs in user namespace.
///
/// Architecture (Dual-TAP):
/// ```
/// Host                    | User Namespace (unshare --user --map-root-user --net)
///                         |
/// slirp4netns ←───────────┤── slirp0 (10.0.2.100/24) ←─── IP forwarding ←─── tap0
///   (userspace NAT)       |                                                     │
///                         |                                              Firecracker VM
///                         |                                              (guest: 192.168.x.2)
/// ```
///
/// Key insight: slirp4netns and Firecracker CANNOT share a TAP device (both need exclusive access).
/// Solution: Use two TAP devices with IP forwarding between them.
///
/// Setup sequence:
/// 1. Create user+network namespace via wrapper script
/// 2. Create `slirp0` TAP inside namespace with 10.0.2.x addressing
/// 3. Start slirp4netns attached to `slirp0` (NOT --configure, TAP already exists)
/// 4. Enable IP forwarding inside namespace
/// 5. Firecracker starts, creates `tap0` with 192.168.x.x addressing
/// 6. Guest configures route: default via namespace host (192.168.x.1)
/// 7. Namespace host forwards packets: guest → tap0 → IP forward → slirp0 → slirp4netns
pub struct SlirpNetwork {
    vm_id: String,
    tap_device: String,       // TAP device for Firecracker (tap0)
    slirp_device: String,     // TAP device for slirp4netns (slirp0)
    port_mappings: Vec<PortMapping>,

    // Network addressing
    slirp_cidr: String,       // slirp0: 10.0.2.100/24, gateway 10.0.2.2
    guest_subnet: String,     // tap0: 192.168.x.0/24 (derived from vm_id)
    guest_ip: String,         // Guest VM IP (192.168.x.2)
    namespace_ip: String,     // Namespace host IP on tap0 (192.168.x.1)

    // State (populated during setup)
    api_socket_path: Option<PathBuf>,
    slirp_process: Option<Child>,
    loopback_ip: Option<String>,
}

impl SlirpNetwork {
    pub fn new(vm_id: String, tap_device: String, port_mappings: Vec<PortMapping>) -> Self {
        // Guest subnet is always 192.168.1.0/24 - no conflicts because each VM
        // runs in its own isolated user namespace
        Self {
            vm_id,
            tap_device,
            slirp_device: "slirp0".to_string(),
            port_mappings,
            slirp_cidr: "10.0.2.100/24".to_string(),
            guest_subnet: "192.168.1.0/24".to_string(),
            guest_ip: "192.168.1.2".to_string(),
            namespace_ip: "192.168.1.1".to_string(),
            api_socket_path: None,
            slirp_process: None,
            loopback_ip: None,  // Allocated in setup() - must be unique on host
        }
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

    /// Find the next available loopback IP by reading existing VM states
    /// Returns a 127.0.0.X IP that's not currently in use
    async fn find_next_loopback_ip() -> String {
        let state_manager = StateManager::new(paths::state_dir());
        let used_ips: HashSet<String> = state_manager
            .list_vms()
            .await
            .unwrap_or_default()
            .into_iter()
            .filter_map(|vm| vm.config.network.loopback_ip)
            .collect();

        // Sequential allocation: 127.0.0.2, 127.0.0.3, ... 127.0.0.254
        // Then 127.0.1.2, 127.0.1.3, ... etc.
        for b2 in 0..=255u8 {
            for b3 in 2..=254u8 {
                // Skip 127.0.0.1 (localhost)
                let ip = format!("127.0.{}.{}", b2, b3);
                if !used_ips.contains(&ip) {
                    return ip;
                }
            }
        }

        // Fallback if all IPs are used (very unlikely - 65,000+ IPs)
        warn!("all loopback IPs in use, reusing 127.0.0.2");
        "127.0.0.2".to_string()
    }

    /// Get the loopback IP assigned to this VM
    pub fn loopback_ip(&self) -> Option<&str> {
        self.loopback_ip.as_deref()
    }

    /// Get API socket path for port forwarding
    pub fn api_socket_path(&self) -> Option<&PathBuf> {
        self.api_socket_path.as_ref()
    }

    /// Build the namespace wrapper command for rootless networking
    ///
    /// This returns a wrapper command that:
    /// 1. Creates user+network namespace via unshare
    /// 2. Runs a setup script that creates both TAP devices:
    ///    - slirp0: For slirp4netns (10.0.2.x subnet)
    ///    - tap-vm-xxx: For Firecracker (192.168.x.x subnet)
    /// 3. Enables IP forwarding and sets up NAT
    /// 4. Sets up DNAT for inbound port forwarding from slirp to guest
    /// 5. Execs Firecracker (args appended by VmManager)
    ///
    /// The returned command expects Firecracker binary and args to be appended.
    /// VmManager will add: firecracker --api-sock /path/to/sock [other args]
    pub fn build_wrapper_command(&self) -> Vec<String> {
        // Build the setup script that runs inside the namespace
        // Note: $@ will contain Firecracker command and args appended by VmManager
        let setup_script = format!(
            r#"
# Enable error handling
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

# Enable IP forwarding (works in user namespaces!)
echo 1 > /proc/sys/net/ipv4/ip_forward

# Set up iptables MASQUERADE for traffic from guest subnet
# This NATs guest traffic (192.168.x.x) to slirp0's address (10.0.2.100)
iptables -t nat -A POSTROUTING -s {guest_subnet} -o {slirp_dev} -j MASQUERADE 2>/dev/null || true

# Set up DNAT for inbound connections from slirp4netns
# When slirp4netns forwards traffic to 10.0.2.100, redirect it to the actual guest IP
# This enables port forwarding: host -> slirp4netns -> 10.0.2.100 -> DNAT -> guest (192.168.x.2)
iptables -t nat -A PREROUTING -d 10.0.2.100 -j DNAT --to-destination {guest_ip} 2>/dev/null || true

# Signal ready by writing PID to stdout, then exec Firecracker
echo "NAMESPACE_PID=$$"
exec "$@"
"#,
            slirp_dev = self.slirp_device,
            slirp_ip = self.slirp_cidr,
            fc_tap = self.tap_device,
            ns_ip = self.namespace_ip,
            guest_subnet = self.guest_subnet,
            guest_ip = self.guest_ip,
        );

        // Build wrapper command: unshare creates user+net namespace, then runs setup script
        // The "--" marks end of bash options, and "$@" in script receives remaining args
        vec![
            "unshare".to_string(),
            "--user".to_string(),
            "--map-root-user".to_string(),
            "--net".to_string(),
            "--".to_string(),
            "bash".to_string(),
            "-c".to_string(),
            setup_script,
            "--".to_string(), // Separator for bash -c script to receive args as $@
        ]
    }

    /// Get a human-readable representation of the wrapper command
    pub fn wrapper_command_string(&self) -> String {
        "unshare --user --map-root-user --net -- bash -c '<setup-script>' -- <firecracker-cmd>"
            .to_string()
    }

    /// Start slirp4netns process attached to the namespace
    /// Called after Firecracker has started (so we have the namespace PID)
    pub async fn start_slirp(&mut self, namespace_pid: u32) -> Result<()> {
        let api_socket = paths::base_dir()
            .join(format!("slirp-{}.sock", truncate_id(&self.vm_id, 8)));

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
        let read_result = tokio::task::spawn_blocking(move || {
            nix::unistd::read(ready_read_raw, &mut ready_buf)
        })
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
    async fn setup_port_forwarding(&self) -> Result<()> {
        let api_socket = self.api_socket_path.as_ref()
            .context("API socket not configured")?;
        let loopback_ip = self.loopback_ip.as_ref()
            .context("loopback IP not configured")?;

        for mapping in &self.port_mappings {
            let bind_addr = match &mapping.host_ip {
                Some(ip) if ip == "0.0.0.0" => "0.0.0.0",
                Some(ip) if ip == "127.0.0.1" => loopback_ip.as_str(),
                Some(ip) => ip.as_str(),
                None => loopback_ip.as_str(),
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

    /// Add a loopback IP address to the host's lo interface
    /// This is required for slirp4netns to be able to bind to the IP
    async fn add_loopback_address(&self, ip: &str) -> Result<()> {
        info!(ip = %ip, "adding loopback address to host lo interface");

        let output = Command::new("ip")
            .args(["addr", "add", &format!("{}/32", ip), "dev", "lo"])
            .output()
            .await
            .context("failed to run ip addr add")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            // Ignore "already exists" errors
            if !stderr.contains("File exists") {
                warn!(stderr = %stderr, "failed to add loopback address (may already exist)");
            }
        }

        Ok(())
    }

    /// Remove a loopback IP address from the host's lo interface
    async fn remove_loopback_address(&self, ip: &str) -> Result<()> {
        info!(ip = %ip, "removing loopback address from host lo interface");

        let output = Command::new("ip")
            .args(["addr", "del", &format!("{}/32", ip), "dev", "lo"])
            .output()
            .await
            .context("failed to run ip addr del")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            // Ignore "not found" errors
            if !stderr.contains("Cannot assign requested address") {
                warn!(stderr = %stderr, "failed to remove loopback address");
            }
        }

        Ok(())
    }

    /// Setup health check port forward (port 80)
    /// Uses slirp4netns API to forward from loopback IP to 10.0.2.100,
    /// then iptables DNAT in the namespace redirects to the actual guest IP.
    async fn setup_health_check_forward(&self) -> Result<()> {
        let api_socket = self.api_socket_path.as_ref()
            .context("API socket not configured")?;
        let loopback_ip = self.loopback_ip.as_ref()
            .context("loopback IP not configured")?;

        // First, add the loopback IP to the host's lo interface
        // This is required for slirp4netns to be able to bind to it
        self.add_loopback_address(loopback_ip).await?;

        // Forward health check port (80) from loopback to slirp's internal IP
        // The DNAT rule in the namespace will then redirect to the actual guest
        let request = serde_json::json!({
            "execute": "add_hostfwd",
            "arguments": {
                "proto": "tcp",
                "host_addr": loopback_ip,
                "host_port": 80,
                "guest_addr": "10.0.2.100",
                "guest_port": 80
            }
        });

        info!(
            loopback_ip = %loopback_ip,
            guest_ip = %self.guest_ip,
            "setting up health check port forward (80) via slirp4netns + DNAT"
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

        // Allocate a unique loopback IP on the host for health checks
        // (guest subnet is always 192.168.1.0/24 - isolated per namespace)
        let loopback_ip = Self::find_next_loopback_ip().await;
        self.loopback_ip = Some(loopback_ip.clone());

        info!(
            loopback_ip = %loopback_ip,
            guest_ip = %self.guest_ip,
            "network configuration"
        );

        let guest_mac = generate_mac();

        Ok(NetworkConfig {
            tap_device: self.tap_device.clone(),
            guest_mac,
            guest_ip: Some(format!("{}/24", self.guest_ip)),
            host_ip: Some(self.namespace_ip.clone()),
            host_veth: None,
            loopback_ip: Some(loopback_ip),
            health_check_port: Some(80),
        })
    }

    async fn post_start(&mut self, vm_pid: u32) -> Result<()> {
        info!(vm_pid = vm_pid, "starting slirp4netns for rootless networking");

        self.start_slirp(vm_pid).await?;
        self.setup_health_check_forward().await?;

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

        // Remove loopback address from host lo interface
        if let Some(ref loopback_ip) = self.loopback_ip {
            if let Err(e) = self.remove_loopback_address(loopback_ip).await {
                warn!("failed to remove loopback address: {}", e);
            }
        }

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
        let net = SlirpNetwork::new(
            "vm-test123".to_string(),
            "tap0".to_string(),
            vec![],
        );

        assert_eq!(net.tap_device, "tap0");
        assert_eq!(net.slirp_device, "slirp0");
        // Fixed IPs - all VMs use same subnet (isolated per namespace)
        assert_eq!(net.guest_ip, "192.168.1.2");
        assert_eq!(net.namespace_ip, "192.168.1.1");
        assert_eq!(net.guest_subnet, "192.168.1.0/24");
    }

    #[test]
    fn test_with_guest_ip() {
        let net = SlirpNetwork::new(
            "vm-test123".to_string(),
            "tap0".to_string(),
            vec![],
        );

        // Clones can override guest IP if snapshot used different subnet
        let net = net.with_guest_ip("192.168.42.2/24".to_string());

        assert_eq!(net.guest_ip, "192.168.42.2");
        assert_eq!(net.namespace_ip, "192.168.42.1");
        assert_eq!(net.guest_subnet, "192.168.42.0/24");
    }
}
