use anyhow::{Context, Result};
use tracing::{info, warn};

use super::namespace;
use super::types::generate_mac;
use super::veth;
use super::{NetworkConfig, NetworkManager, PortMapping};
use crate::state::truncate_id;

/// Guest network addressing (same as pasta/bridged for Firecracker compatibility)
const GUEST_IP: &str = "10.0.2.100";
const GUEST_GATEWAY: &str = "10.0.2.1";

/// Bridge device name
const BRIDGE_DEVICE: &str = "br0";

/// Routed networking using veth + IPv6 routing
///
/// Unlike pasta mode (which proxies all outbound traffic through userspace L4),
/// routed mode connects the VM's network namespace to the host via a veth pair.
/// Outbound IPv6 traffic goes through the kernel's routing stack at line rate.
///
/// Requires sudo (creates veth pairs and modifies host routing table).
///
/// Architecture:
/// ```text
/// Host namespace                     Namespace (ip netns)
///
/// eth0 (/64 subnet)
///   |
/// veth-host ←───veth pair────→ veth-ns
///   (proxy NDP)                    |
///                              br0 (10.0.2.1/24)
///                                  |
///                              tap-vm → Firecracker VM (10.0.2.100)
/// ```
pub struct RoutedNetwork {
    vm_id: String,
    tap_device: String,
    port_mappings: Vec<PortMapping>,

    // Network state (populated during setup)
    namespace_id: Option<String>,
    host_veth: Option<String>,
    vm_ipv6: Option<String>,
    socat_children: Vec<tokio::process::Child>,
}

impl RoutedNetwork {
    pub fn new(vm_id: String, tap_device: String, port_mappings: Vec<PortMapping>) -> Self {
        Self {
            vm_id,
            tap_device,
            port_mappings,
            namespace_id: None,
            host_veth: None,
            vm_ipv6: None,
            socat_children: Vec::new(),
        }
    }

    /// Get the network namespace ID (for setting Firecracker's namespace).
    pub fn namespace_id(&self) -> Option<&str> {
        self.namespace_id.as_deref()
    }

    /// Add a host route for a guest IPv6 address.
    ///
    /// Called during snapshot restore when the snapshot's guest IPv6 differs from
    /// the newly generated one (different VM IDs produce different IPv6 addresses,
    /// but the snapshot's boot params have the original).
    pub async fn add_guest_route(&self, guest_ipv6: &str) {
        if let Some(ref host_veth) = self.host_veth {
            let route = format!("{}/128", guest_ipv6);
            let _ = tokio::process::Command::new("ip")
                .args(["-6", "route", "replace", &route, "dev", host_veth])
                .output()
                .await;
            info!(guest_ipv6 = %guest_ipv6, veth = %host_veth, "added host route for snapshot guest IPv6");
        }
    }

    /// Detect host's global IPv6 address and /64 subnet.
    /// Returns (host_ip, subnet_prefix) e.g. ("2803:6084:2900:2534::1", "2803:6084:2900:2534")
    fn detect_host_ipv6() -> Option<(String, String)> {
        let output = std::process::Command::new("ip")
            .args(["-6", "addr", "show", "scope", "global"])
            .output()
            .ok()?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.lines() {
            let line = line.trim();
            if let Some(addr) = line.strip_prefix("inet6 ") {
                // Parse "addr/prefix scope global ..."
                if let Some(addr_cidr) = addr.split_whitespace().next() {
                    if let Some((addr, prefix_len)) = addr_cidr.split_once('/') {
                        if prefix_len == "64" && !addr.starts_with("fe80") {
                            // Extract /64 prefix (first 4 groups)
                            // Expand the address to get the prefix
                            if let Ok(ip) = addr.parse::<std::net::Ipv6Addr>() {
                                let segments = ip.segments();
                                let prefix = format!(
                                    "{:x}:{:x}:{:x}:{:x}",
                                    segments[0], segments[1], segments[2], segments[3]
                                );
                                return Some((addr.to_string(), prefix));
                            }
                        }
                    }
                }
            }
        }
        None
    }

    /// Generate a deterministic IPv6 for the VM from the host's /64 subnet.
    fn generate_vm_ipv6(prefix: &str, vm_id: &str) -> String {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let mut hasher = DefaultHasher::new();
        vm_id.hash(&mut hasher);
        let hash = hasher.finish();

        // Use lower 64 bits as the interface ID portion of the /64 prefix.
        // Format: prefix:XXXX:XXXX:XXXX:XXXX (4 hex groups = 64 bits)
        let w1 = ((hash >> 48) & 0xFFFF) as u16;
        let w2 = ((hash >> 32) & 0xFFFF) as u16;
        let w3 = ((hash >> 16) & 0xFFFF) as u16;
        let w4 = (hash & 0xFFFF) as u16;
        format!("{}:{:x}:{:x}:{:x}:{:x}", prefix, w1, w2, w3, w4)
    }
}

#[async_trait::async_trait]
impl NetworkManager for RoutedNetwork {
    async fn setup(&mut self) -> Result<NetworkConfig> {
        let vm_id_short = truncate_id(&self.vm_id, 5);
        let ns_name = format!("fcvm-{}", vm_id_short);
        let host_veth = format!("veth-{}", vm_id_short);
        let guest_veth = format!("vns-{}", vm_id_short);

        info!(
            vm_id = %self.vm_id,
            namespace = %ns_name,
            "setting up routed networking"
        );

        // Detect host IPv6 subnet
        let (host_ipv6, ipv6_prefix) = Self::detect_host_ipv6()
            .context("routed mode requires a host with a global IPv6 /64 subnet")?;

        let vm_ipv6 = Self::generate_vm_ipv6(&ipv6_prefix, &self.vm_id);
        info!(
            host_ipv6 = %host_ipv6,
            vm_ipv6 = %vm_ipv6,
            prefix = %ipv6_prefix,
            "IPv6 addresses for routed networking"
        );

        // 1. Create network namespace
        namespace::create_namespace(&ns_name).await?;

        // 2. Create veth pair and move guest side to namespace
        veth::create_veth_pair(&host_veth, &guest_veth, &ns_name).await?;

        // 3. Create TAP in namespace
        veth::create_tap_in_ns(&ns_name, &self.tap_device).await?;

        // 4. Create bridge with BOTH TAP and veth.
        //    The bridge does L2 forwarding: VM → TAP → bridge → veth → host.
        //    IPv6 for the bridge's own address (fd00::1) is locally delivered.
        //    IPv6 for external destinations traverses the bridge to the veth peer on the host.
        veth::connect_tap_to_veth(&ns_name, &self.tap_device, &guest_veth).await?;
        // Bring up all interfaces (connect_tap_to_veth only brings up the bridge)
        namespace::exec_in_namespace(&ns_name, &["ip", "link", "set", "lo", "up"]).await?;
        namespace::exec_in_namespace(&ns_name, &["ip", "link", "set", &self.tap_device, "up"]).await?;
        namespace::exec_in_namespace(&ns_name, &["ip", "link", "set", &guest_veth, "up"]).await?;

        // 5. Assign gateway IPs to bridge (VM connects here via TAP)
        let gw_cidr = format!("{}/24", GUEST_GATEWAY);
        namespace::exec_in_namespace(
            &ns_name,
            &["ip", "addr", "add", &gw_cidr, "dev", BRIDGE_DEVICE],
        )
        .await?;
        // nodad: skip Duplicate Address Detection so the address is usable immediately.
        // Without nodad, the address stays "tentative" for ~1-3s and the kernel silently
        // drops all IPv6 packets to it — breaking the VM's default route via fd00::1.
        namespace::exec_in_namespace(
            &ns_name,
            &["ip", "-6", "addr", "add", "fd00::1/64", "dev", BRIDGE_DEVICE, "nodad"],
        )
        .await?;

        // 6. Bring up host veth
        let output = tokio::process::Command::new("ip")
            .args(["link", "set", &host_veth, "up"])
            .output()
            .await
            .context("bringing up host veth")?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("failed to bring up host veth: {}", stderr);
        }
        // 7. Enable IPv6 forwarding on the host veth only (not all.forwarding —
        //    that prevents link-local auto-assignment on future interfaces).
        let _ = tokio::process::Command::new("sysctl")
            .args(["-w", &format!("net.ipv6.conf.{}.forwarding=1", host_veth)])
            .output()
            .await;

        // 8. Assign link-local to host veth manually (auto-assignment fails when
        //    all.forwarding=1 from a previous run). Use EUI-64 from MAC + nodad.
        let host_ll = generate_link_local_from_mac(&host_veth).await
            .context("failed to generate link-local for host veth")?;
        let host_ll_cidr = format!("{}/64", host_ll);
        let _ = tokio::process::Command::new("ip")
            .args(["-6", "addr", "add", &host_ll_cidr, "dev", &host_veth, "scope", "link", "nodad"])
            .output()
            .await;
        info!(host_ll = %host_ll, "assigned link-local to host veth");

        // 9. Add routable source IPv6 to bridge (for namespace-originated traffic).
        //    Uses a separate address from the VM's — the namespace needs its own
        //    routable source so return traffic can find it.
        let ns_source_ipv6 = format!("{}::face:1", ipv6_prefix);
        let ns_source_cidr = format!("{}/128", ns_source_ipv6);
        namespace::exec_in_namespace(
            &ns_name,
            &["ip", "-6", "addr", "add", &ns_source_cidr, "dev", BRIDGE_DEVICE, "nodad"],
        ).await?;

        // 10. In namespace: default IPv6 route goes through bridge → veth → host.
        //     The bridge forwards L2 frames from br0 to veth (bridge member).
        //     The host veth receives them and the host kernel routes to eth0.
        //     Use the host veth's link-local as nexthop (reachable via NDP on bridge).
        namespace::exec_in_namespace(
            &ns_name,
            &["ip", "-6", "route", "add", "default", "via", &host_ll, "dev", BRIDGE_DEVICE],
        ).await?;

        // 11. Enable IPv6 forwarding in namespace (for VM traffic forwarding)
        namespace::exec_in_namespace(
            &ns_name,
            &["sysctl", "-w", "net.ipv6.conf.all.forwarding=1"],
        ).await?;

        // 12. On host: route VM's IPv6 back through veth to the namespace
        let vm_route = format!("{}/128", vm_ipv6);
        let _ = tokio::process::Command::new("ip")
            .args(["-6", "route", "replace", &vm_route, "dev", &host_veth])
            .output()
            .await;
        // Also route the namespace's source address
        let _ = tokio::process::Command::new("ip")
            .args(["-6", "route", "replace", &ns_source_cidr, "dev", &host_veth])
            .output()
            .await;

        // 13. Add proxy NDP so the network fabric routes VM's IPv6 to this host
        let default_iface = detect_default_ipv6_interface().await.unwrap_or_else(|| "eth0".to_string());
        let _ = tokio::process::Command::new("ip")
            .args(["-6", "neigh", "add", "proxy", &vm_ipv6, "dev", &default_iface])
            .output()
            .await;
        info!(vm_ipv6 = %vm_ipv6, iface = %default_iface, "added proxy NDP");

        // 14. Port forwarding: socat listens on host loopback, connects to VM
        //     inside the namespace (via `ip netns exec`). The veth is a bridge member
        //     so host-side IPv4 routing to 10.0.2.100 doesn't work — socat must run
        //     the connect side inside the namespace where the bridge is directly reachable.
        let loopback_ip = format!("127.0.0.{}", 2 + (std::process::id() % 252) as u8);
        for mapping in &self.port_mappings {
            // Shell script: socat listens on host, connects inside namespace
            let script = format!(
                "socat TCP-LISTEN:{},bind={},fork,reuseaddr \
                 EXEC:'ip netns exec {} socat STDIO TCP\\:{}\\:{}'",
                mapping.host_port, loopback_ip, ns_name, GUEST_IP, mapping.guest_port
            );
            match tokio::process::Command::new("sh")
                .args(["-c", &script])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
            {
                Ok(child) => {
                    info!(host_port = mapping.host_port, guest_port = mapping.guest_port, bind = %loopback_ip, "port forwarding via socat");
                    self.socat_children.push(child);
                }
                Err(e) => {
                    warn!(host_port = mapping.host_port, guest_port = mapping.guest_port, error = %e, "failed to start socat port forwarder");
                }
            }
        }

        let guest_mac = generate_mac();
        let guest_ip = format!("{}/{}", GUEST_IP, "24");

        // Store state for cleanup
        self.namespace_id = Some(ns_name);
        self.host_veth = Some(host_veth);
        self.vm_ipv6 = Some(vm_ipv6.clone());

        Ok(NetworkConfig {
            tap_device: self.tap_device.clone(),
            guest_mac,
            guest_ip: Some(guest_ip),
            host_ip: Some(GUEST_GATEWAY.to_string()),
            host_veth: self.host_veth.clone(),
            loopback_ip: if self.port_mappings.is_empty() { None } else { Some(loopback_ip) },
            dns_server: None, // Use host DNS servers directly (kernel-routed)
            guest_ipv6: Some(vm_ipv6),
            // fd00::1 is on the bridge inside the namespace. The VM uses it as IPv6 gateway.
            // NDP resolves it on the VM's local link (TAP → bridge → fd00::1 responds).
            // The namespace kernel then forwards to the veth → host.
            host_ipv6: Some("fd00::1".to_string()),
            dns_search: None,
            http_proxy: None,
            namespace_name: self.namespace_id.clone(),
        })
    }

    async fn cleanup(&mut self) -> Result<()> {
        info!(vm_id = %self.vm_id, "cleaning up routed network resources");

        // Kill socat port forwarders
        for mut child in self.socat_children.drain(..) {
            let _ = child.kill().await;
        }

        // Remove host IPv4 route to VM subnet
        let _ = tokio::process::Command::new("ip")
            .args(["route", "del", "10.0.2.0/24"])
            .output()
            .await;

        // Remove proxy NDP
        if let Some(ref vm_ipv6) = self.vm_ipv6 {
            let default_iface = detect_default_ipv6_interface().await.unwrap_or_else(|| "eth0".to_string());
            let _ = tokio::process::Command::new("ip")
                .args(["-6", "neigh", "del", "proxy", vm_ipv6, "dev", &default_iface])
                .output()
                .await;

            // Remove host route
            let _ = tokio::process::Command::new("ip")
                .args(["-6", "route", "del", &format!("{}/128", vm_ipv6)])
                .output()
                .await;
        }

        // Delete veth pair (auto-deletes peer)
        if let Some(ref host_veth) = self.host_veth {
            let _ = veth::delete_veth_pair(host_veth).await;
        }

        // Delete namespace
        if let Some(ref ns_name) = self.namespace_id {
            let _ = namespace::delete_namespace(ns_name).await;
        }

        Ok(())
    }

    fn tap_device(&self) -> &str {
        &self.tap_device
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// Generate EUI-64 link-local address from interface MAC address.
/// Needed because IPv6 link-local auto-assignment fails when forwarding=1.
async fn generate_link_local_from_mac(iface: &str) -> Option<String> {
    let output = tokio::process::Command::new("ip")
        .args(["link", "show", iface])
        .output()
        .await
        .ok()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("link/ether ") {
            if let Some(mac) = rest.split_whitespace().next() {
                let bytes: Vec<u8> = mac
                    .split(':')
                    .filter_map(|h| u8::from_str_radix(h, 16).ok())
                    .collect();
                if bytes.len() == 6 {
                    // EUI-64: flip bit 6 of first byte, insert ff:fe in middle
                    let b0 = bytes[0] ^ 0x02;
                    return Some(format!(
                        "fe80::{:02x}{:02x}:{:02x}ff:fe{:02x}:{:02x}{:02x}",
                        b0, bytes[1], bytes[2], bytes[3], bytes[4], bytes[5]
                    ));
                }
            }
        }
    }
    None
}

/// Detect the default IPv6 outgoing interface
async fn detect_default_ipv6_interface() -> Option<String> {
    let output = tokio::process::Command::new("ip")
        .args(["-6", "route", "show", "default"])
        .output()
        .await
        .ok()?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    // Parse "default via fe80::face:b00c dev eth0 ..."
    for line in stdout.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if let Some(idx) = parts.iter().position(|&p| p == "dev") {
            return parts.get(idx + 1).map(|s| s.to_string());
        }
    }
    None
}
