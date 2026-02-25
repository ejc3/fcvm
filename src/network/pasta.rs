use anyhow::{Context, Result};
use std::path::PathBuf;
use std::process::Stdio;
use tokio::process::{Child, Command};
use tracing::{debug, info, warn};

use super::{types::generate_mac, NetworkConfig, NetworkManager, PortMapping, Protocol};
use crate::paths;
use crate::state::truncate_id;

/// Guest network addressing — pasta provides L2↔L4 translation via bridge
const GUEST_IP: &str = "10.0.2.100";
const GUEST_GATEWAY: &str = "10.0.2.2";
const GUEST_DNS: &str = "10.0.2.3";
/// Namespace IP on bridge — enables nsenter health checks to route to guest
const NAMESPACE_IP: &str = "10.0.2.1";

/// Guest IPv6 addressing (pasta copies host IPv6 with fd00::/64 fallback)
const GUEST_IPV6: &str = "fd00::100";
const GUEST_IPV6_GATEWAY: &str = "fd00::2";

/// Bridge device name
const BRIDGE_DEVICE: &str = "br0";

/// TAP device name for pasta (replaces slirp0)
const PASTA_DEVICE_NAME: &str = "pasta0";

/// Timeout for waiting for pasta PID file (readiness signal)
const PASTA_READY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Rootless networking using pasta with bridge architecture
///
/// This mode uses user namespaces and pasta (from passt project) for true
/// unprivileged operation. No sudo/root required — everything runs in user
/// namespace via nsenter.
///
/// Architecture (L2 Bridge + L4 translation):
/// ```text
/// Host                    | User Namespace (unshare --user --map-root-user --net)
///                         |
/// pasta  <----------------+-- pasta0 --+
///   (L2↔L4 translation,   |            |
///    splice zero-copy)     |           br0 (L2 bridge)
///                         |            |
///                         |          tap-fc ---> Firecracker VM
///                         |                      (guest: 10.0.2.100)
/// ```
///
/// pasta provides near-native throughput via splice(2) zero-copy L4 translation,
/// replacing slirp4netns's slower userspace TCP/IP stack.
///
/// Port forwarding uses pasta's built-in host-side binding combined with
/// iptables DNAT inside the user namespace to reach the VM through the bridge.
///
/// Setup sequence:
/// 1. Spawn holder process: `unshare --user --map-root-user --net -- sleep infinity`
/// 2. Run pre-setup via nsenter: create Firecracker TAP only
/// 3. Start pasta: creates pasta0 TAP in namespace with L2↔L4 translation
/// 4. Run post-setup via nsenter: create bridge, add both TAPs, enable ip_forward
/// 5. Run Firecracker via nsenter: `nsenter -t HOLDER_PID -U -n -- firecracker ...`
/// 6. Health checks via nsenter: `nsenter -t HOLDER_PID -U -n -- curl guest_ip:80`
pub struct PastaNetwork {
    vm_id: String,
    tap_device: String,   // TAP device for Firecracker (tap-fc)
    pasta_device: String, // TAP device created by pasta (pasta0)
    port_mappings: Vec<PortMapping>,

    // Network addressing (IPv4) — guest uses 10.0.2.x via bridge
    guest_ip: String, // Guest VM IP (10.0.2.100)

    // Network addressing (IPv6)
    guest_ipv6: String, // fd00::100

    // State (populated during setup)
    pasta_process: Option<Child>,
    pid_file: Option<PathBuf>,
    loopback_ip: Option<String>, // Unique loopback IP for port forwarding (127.x.y.z)
}

impl PastaNetwork {
    pub fn new(vm_id: String, tap_device: String, port_mappings: Vec<PortMapping>) -> Self {
        Self {
            vm_id,
            tap_device,
            pasta_device: PASTA_DEVICE_NAME.to_string(),
            port_mappings,
            guest_ip: GUEST_IP.to_string(),
            guest_ipv6: GUEST_IPV6.to_string(),
            pasta_process: None,
            pid_file: None,
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

    /// Build the pre-pasta setup script to run inside the namespace via nsenter
    ///
    /// Creates only the Firecracker TAP device. The bridge and pasta0 TAP
    /// are set up after pasta starts (pasta creates its own TAP).
    /// Run via: nsenter -t HOLDER_PID -U -n -- bash -c '<this script>'
    pub fn build_setup_script(&self) -> String {
        format!(
            r#"
set -e

# Create TAP device for Firecracker (pasta creates its own TAP separately)
ip tuntap add {fc_tap} mode tap
ip link set {fc_tap} up

# Set up loopback
ip link set lo up
"#,
            fc_tap = self.tap_device,
        )
    }

    /// Build the post-pasta setup script that creates the bridge after pasta is ready
    ///
    /// Connects pasta's TAP and Firecracker's TAP via an L2 bridge.
    /// Port forwarding works via pasta's L2 translation (--no-splice forces this):
    /// pasta binds on the host, creates L2 frames, sends through pasta0 TAP →
    /// bridge → tap-fc → VM. No iptables required.
    pub fn build_bridge_script(&self) -> String {
        let script = format!(
            r#"
set -e

# Bring pasta0 up (pasta creates it but doesn't bring it up without --config-net)
ip link set {pasta_dev} up

# Create L2 bridge — connects pasta0 and Firecracker TAP
ip link add {bridge} type bridge
ip link set {bridge} up

# Add pasta's TAP to bridge (pasta created this device)
ip link set {pasta_dev} master {bridge}

# Add Firecracker's TAP to bridge
ip link set {fc_tap} master {bridge}

# Add IP to bridge for health checks (namespace needs route to reach guest)
ip addr add {namespace_ip}/24 dev {bridge}

# Enable IP forwarding
echo 1 > /proc/sys/net/ipv4/ip_forward
"#,
            bridge = BRIDGE_DEVICE,
            pasta_dev = self.pasta_device,
            fc_tap = self.tap_device,
            namespace_ip = NAMESPACE_IP,
        );

        script
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

    /// Detect host's global IPv6 address for pasta outbound traffic
    fn detect_host_ipv6() -> Option<String> {
        let output = std::process::Command::new("ip")
            .args(["-6", "addr", "show", "scope", "global"])
            .output()
            .ok()?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.lines() {
            let line = line.trim();
            if line.starts_with("inet6 ") {
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
        let proxy_url = std::env::var("HTTP_PROXY")
            .or_else(|_| std::env::var("http_proxy"))
            .or_else(|_| std::env::var("HTTPS_PROXY"))
            .or_else(|_| std::env::var("https_proxy"))
            .ok()?;

        if let Some(rest) = proxy_url.strip_prefix("http://") {
            let host_port = rest.trim_end_matches('/');

            if host_port.starts_with('[') {
                return Some(proxy_url);
            }

            if let Some((host, port)) = host_port.rsplit_once(':') {
                if let Ok(output) = std::process::Command::new("getent")
                    .args(["hosts", host])
                    .output()
                {
                    let stdout = String::from_utf8_lossy(&output.stdout);
                    if let Some(ipv6) = stdout.split_whitespace().next() {
                        if ipv6.contains(':') {
                            return Some(format!("http://[{}]:{}", ipv6, port));
                        }
                    }
                }
                return Some(proxy_url);
            }
        }

        Some(proxy_url)
    }

    /// Start pasta process attached to the namespace
    ///
    /// pasta creates its own TAP device (pasta0) in the namespace and provides
    /// L2↔L4 translation to the host. Uses PID file for readiness signaling.
    pub async fn start_pasta(&mut self, namespace_pid: u32) -> Result<()> {
        let pid_file = paths::data_dir().join(format!("pasta-{}.pid", truncate_id(&self.vm_id, 8)));

        if pid_file.exists() {
            tokio::fs::remove_file(&pid_file).await?;
        }

        let host_ipv6 = Self::detect_host_ipv6();

        info!(
            namespace_pid = namespace_pid,
            pasta_tap = %self.pasta_device,
            pid_file = %pid_file.display(),
            host_ipv6 = ?host_ipv6,
            port_mappings = self.port_mappings.len(),
            "starting pasta for rootless networking"
        );

        let mut cmd = Command::new("pasta");
        cmd.arg("--foreground")
            .arg("--quiet")
            .arg("-P")
            .arg(&pid_file);

        // When running as root (e.g., sudo in tests), pasta drops to nobody by
        // default and then can't access the user namespace. Tell it to stay as root.
        if nix::unistd::geteuid().is_root() {
            cmd.arg("--runas").arg("0:0");
        }

        // Don't use --config-net: it sets an IP on pasta0's kernel interface, which
        // conflicts with the bridge (kernel responds to ARP for that IP via bridge's
        // weak host model, stealing traffic from pasta's userspace L2 handler).
        // Instead, pasta creates the TAP but we bring it up in build_bridge_script().
        //
        // -a must be the VM's actual IP (GUEST_IP), not the gateway. pasta uses -a
        // as the "guest address" and ignores ARP requests for it (don't resolve self).
        // If -a == gateway, pasta ignores ARP for the gateway and the VM can't route.
        cmd.arg("--ns-ifname")
            .arg(&self.pasta_device)
            .arg("-a")
            .arg(GUEST_IP) // VM's actual IP — pasta ignores ARP for this address
            .arg("-n")
            .arg("255.255.255.0")
            .arg("-g")
            .arg(GUEST_GATEWAY) // Gateway — pasta responds to ARP for this
            .arg("--dns-forward")
            .arg(GUEST_DNS) // Forward DNS queries sent to 10.0.2.3 to host resolver
            .arg("--no-dhcp")
            // Disable splice bypass: pasta's default L4 socket bypass creates
            // connections directly in the namespace, but the VM is behind a bridge.
            // --no-splice forces all traffic (including port forwarding) through
            // the L2 TAP path: pasta → pasta0 → br0 → tap-fc → VM.
            .arg("--no-splice");

        // If host has global IPv6, configure pasta for IPv6 outbound
        if let Some(ref ipv6) = host_ipv6 {
            // Add IPv6 guest address and gateway so pasta handles IPv6 L2↔L4 translation.
            // -a/-g can each be specified twice (once IPv4, once IPv6).
            cmd.arg("-a")
                .arg(GUEST_IPV6) // Guest IPv6 address — pasta ignores NDP for this
                .arg("-g")
                .arg(GUEST_IPV6_GATEWAY) // IPv6 gateway — pasta responds to NDP for this
                .arg("-o")
                .arg(ipv6); // Outbound source address for IPv6

            // Keep NDP enabled: the guest needs NDP Neighbor Solicitation/Advertisement
            // to resolve the IPv6 gateway's MAC address (like ARP for IPv4).
            // Disable only RA (router advertisements) and DHCPv6 — we configure the
            // guest's IPv6 address statically via kernel cmdline, not SLAAC.
            cmd.arg("--no-ra").arg("--no-dhcpv6");
        } else {
            // No host IPv6 — disable IPv6 entirely
            cmd.arg("--ipv4-only")
                // NDP/RA/DHCPv6 are moot with --ipv4-only, but be explicit
                .arg("--no-ndp")
                .arg("--no-dhcpv6")
                .arg("--no-ra");
        }

        // Port forwarding: pasta binds on host, L2 frames go through bridge to VM
        if self.port_mappings.is_empty() {
            cmd.arg("-t").arg("none").arg("-u").arg("none");
        } else {
            let mut tcp_specs = Vec::new();
            let mut udp_specs = Vec::new();

            for mapping in &self.port_mappings {
                let bind_addr = match &mapping.host_ip {
                    Some(ip) => ip.as_str(),
                    None => self.loopback_ip.as_deref().unwrap_or("127.0.0.1"),
                };

                // pasta spec: "bind_addr/host_port:guest_port"
                let spec = format!("{}/{}:{}", bind_addr, mapping.host_port, mapping.guest_port);

                match mapping.proto {
                    Protocol::Tcp => tcp_specs.push(spec),
                    Protocol::Udp => udp_specs.push(spec),
                }

                info!(
                    proto = ?mapping.proto,
                    host = %format!("{}:{}", bind_addr, mapping.host_port),
                    guest = %format!("{}:{}", self.guest_ip, mapping.guest_port),
                    "adding port forward"
                );
            }

            if tcp_specs.is_empty() {
                cmd.arg("-t").arg("none");
            } else {
                cmd.arg("-t").arg(tcp_specs.join(","));
            }
            if udp_specs.is_empty() {
                cmd.arg("-u").arg("none");
            } else {
                cmd.arg("-u").arg(udp_specs.join(","));
            }
        }

        // Disable host→namespace port forwarding (reverse direction).
        // These don't affect outbound traffic — pasta's L2↔L4 translation handles
        // that independently. Matches Podman's invocation pattern.
        cmd.arg("-T").arg("none").arg("-U").arg("none");

        // Attach to the holder's namespace
        cmd.arg(namespace_pid.to_string());

        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());

        debug!(cmd = ?cmd, "pasta command");
        let mut child = cmd.spawn().context("failed to spawn pasta")?;

        // Wait for PID file to appear (signals pasta is ready)
        let deadline = std::time::Instant::now() + PASTA_READY_TIMEOUT;
        loop {
            if pid_file.exists() {
                info!("pasta ready (PID file created)");
                // Drop stderr to prevent pipe buffer deadlock
                drop(child.stderr.take());
                break;
            }

            // Check if pasta died during startup
            match child.try_wait() {
                Ok(Some(status)) => {
                    let output = child.wait_with_output().await?;
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    let stderr = stderr.trim();
                    if stderr.is_empty() {
                        anyhow::bail!("pasta exited before becoming ready (status: {})", status);
                    } else {
                        anyhow::bail!(
                            "pasta exited before becoming ready (status: {}): {}",
                            status,
                            stderr
                        );
                    }
                }
                Ok(None) => {} // Still running
                Err(e) => anyhow::bail!("failed to check pasta status: {}", e),
            }

            if std::time::Instant::now() > deadline {
                let _ = child.kill().await;
                anyhow::bail!(
                    "pasta did not become ready within {:?}",
                    PASTA_READY_TIMEOUT
                );
            }

            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }

        self.pasta_process = Some(child);
        self.pid_file = Some(pid_file);

        Ok(())
    }

    /// Get guest IP address for kernel boot args
    pub fn guest_ip(&self) -> &str {
        &self.guest_ip
    }

    /// Get gateway IP for guest (pasta gateway)
    pub fn gateway_ip(&self) -> &str {
        GUEST_GATEWAY
    }
}

#[async_trait::async_trait]
impl NetworkManager for PastaNetwork {
    async fn setup(&mut self) -> Result<NetworkConfig> {
        info!(vm_id = %self.vm_id, "setting up rootless networking with pasta (bridge mode)");

        info!(
            guest_ip = %self.guest_ip,
            gateway = %GUEST_GATEWAY,
            loopback_ip = ?self.loopback_ip,
            "network configuration (pasta bridge mode, nsenter health checks)"
        );

        let guest_mac = generate_mac();

        // Check if host has IPv6 — pasta handles it natively
        let (guest_ipv6, host_ipv6) = if Self::detect_host_ipv6().is_some() {
            (
                Some(self.guest_ipv6.clone()),
                Some(GUEST_IPV6_GATEWAY.to_string()),
            )
        } else {
            (None, None)
        };

        let http_proxy = Self::detect_http_proxy();
        if let Some(ref proxy) = http_proxy {
            info!(proxy = %proxy, "detected HTTP proxy for IPv6-only network");
        }

        Ok(NetworkConfig {
            tap_device: self.tap_device.clone(),
            guest_mac,
            guest_ip: Some(format!("{}/24", self.guest_ip)),
            host_ip: Some(GUEST_GATEWAY.to_string()),
            host_veth: None,
            loopback_ip: self.loopback_ip.clone(),
            dns_server: Some(GUEST_DNS.to_string()),
            guest_ipv6,
            host_ipv6,
            dns_search: None,
            http_proxy,
        })
    }

    async fn post_start(&mut self, holder_pid: u32) -> Result<()> {
        info!(
            holder_pid = holder_pid,
            "starting pasta for rootless networking"
        );

        // Phase 1: Start pasta (creates pasta0 TAP in namespace)
        self.start_pasta(holder_pid).await?;

        // Phase 2: Create bridge connecting pasta0 and Firecracker's TAP, add DNAT rules
        let bridge_script = self.build_bridge_script();
        let nsenter_prefix = self.build_nsenter_prefix(holder_pid);

        debug!(
            holder_pid = holder_pid,
            script = %bridge_script.lines().filter(|l| !l.trim().is_empty() && !l.trim().starts_with('#')).collect::<Vec<_>>().join("; "),
            "running bridge setup script"
        );

        let output = Command::new(&nsenter_prefix[0])
            .args(&nsenter_prefix[1..])
            .arg("bash")
            .arg("-c")
            .arg(&bridge_script)
            .output()
            .await
            .context("running bridge setup via nsenter")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("bridge setup failed: {}", stderr.trim());
        }

        info!(holder_pid = holder_pid, "pasta + bridge setup complete");
        Ok(())
    }

    async fn cleanup(&mut self) -> Result<()> {
        info!(vm_id = %self.vm_id, "cleaning up pasta resources");

        if let Some(mut process) = self.pasta_process.take() {
            if let Err(e) = process.kill().await {
                warn!("failed to kill pasta: {}", e);
            }
            let _ = process.wait().await;
        }

        if let Some(ref pid_file) = self.pid_file {
            if pid_file.exists() {
                if let Err(e) = tokio::fs::remove_file(pid_file).await {
                    warn!("failed to remove pasta PID file: {}", e);
                }
            }
        }

        info!(vm_id = %self.vm_id, "pasta cleanup complete");
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
        let net = PastaNetwork::new("vm-test123".to_string(), "tap0".to_string(), vec![]);

        assert_eq!(net.tap_device, "tap0");
        assert_eq!(net.pasta_device, "pasta0");
        assert_eq!(net.guest_ip, "10.0.2.100");
        assert_eq!(net.gateway_ip(), "10.0.2.2");
    }
}
