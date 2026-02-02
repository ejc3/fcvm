use anyhow::{Context, Result};
use tracing::{debug, info};

use super::{
    get_host_dns_search, get_host_dns_servers, namespace, portmap, types::generate_mac, veth,
    NetworkConfig, NetworkManager, PortMapping,
};
use crate::state::truncate_id;

/// Bridged networking using network namespace isolation with veth pairs
///
/// This mode requires sudo/root for network namespace and iptables setup.
/// For true rootless operation (no sudo), use SlirpNetwork instead.
///
/// Architecture for baseline VMs:
/// - Each VM runs in dedicated network namespace (fcvm-{vm_id})
/// - veth pair connects host namespace to VM namespace
/// - TAP device created inside VM namespace
/// - TAP connected to veth via L2 bridge (no IP on bridge)
/// - Port mappings via iptables DNAT/FORWARD rules
/// - Firecracker process runs inside the namespace
///
/// Architecture for clones (In-Namespace NAT):
/// - TAP connected to br0 which has the guest's expected gateway IP
/// - veth pair has unique 10.x.y.0/30 IPs (not connected to bridge)
/// - NAT inside namespace changes source IP to veth IP
/// - Host routes 10.x.y.0/30 to the veth (no CONNMARK needed!)
pub struct BridgedNetwork {
    vm_id: String,
    tap_device: String,
    port_mappings: Vec<PortMapping>,
    guest_ip_override: Option<String>,
    /// VM ID to use for subnet calculation (for cache restore with fresh networking)
    network_vm_id: Option<String>,

    // Network state (populated during setup)
    namespace_id: Option<String>,
    host_veth: Option<String>,
    guest_veth: Option<String>,
    host_ip: Option<String>,
    guest_ip: Option<String>,
    subnet_cidr: Option<String>,
    port_mapping_rules: Vec<String>,
    is_clone: bool,
    /// For clones: the veth IP inside the namespace (used for port forwarding)
    veth_inner_ip: Option<String>,
}

impl BridgedNetwork {
    pub fn new(vm_id: String, tap_device: String, port_mappings: Vec<PortMapping>) -> Self {
        Self {
            vm_id,
            tap_device,
            port_mappings,
            guest_ip_override: None,
            network_vm_id: None,
            namespace_id: None,
            host_veth: None,
            guest_veth: None,
            host_ip: None,
            guest_ip: None,
            subnet_cidr: None,
            port_mapping_rules: Vec::new(),
            is_clone: false,
            veth_inner_ip: None,
        }
    }

    /// Set guest IP to use (for clones - use same IP as original VM)
    pub fn with_guest_ip(mut self, guest_ip: String) -> Self {
        self.guest_ip_override = Some(guest_ip);
        self.is_clone = true;
        self
    }

    /// Use a specific VM ID for network subnet calculation (for cache restore).
    /// This allows restored VMs to get the same subnet/IPs as the original
    /// while keeping fresh VM networking (not clone networking with NAT).
    /// The new vm_id is still used for namespace/TAP naming (isolation).
    pub fn with_network_vm_id(mut self, network_vm_id: String) -> Self {
        self.network_vm_id = Some(network_vm_id);
        self
    }

    /// Get the namespace ID for this network
    pub fn namespace_id(&self) -> Option<&str> {
        self.namespace_id.as_deref()
    }
}

#[async_trait::async_trait]
impl NetworkManager for BridgedNetwork {
    async fn setup(&mut self) -> Result<NetworkConfig> {
        info!(vm_id = %self.vm_id, is_clone = %self.is_clone, "setting up network namespace");

        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        // Use network_vm_id for subnet calculation if set (for cache restore)
        // This allows restored VMs to get the same IPs as the original VM
        let id_for_subnet = self.network_vm_id.as_ref().unwrap_or(&self.vm_id);
        let mut hasher = DefaultHasher::new();
        id_for_subnet.hash(&mut hasher);
        let subnet_id = (hasher.finish() % 16384) as u16;

        // For clones, use In-Namespace NAT with unique 10.x.y.0/30 for veth
        // For baseline VMs, use 172.30.x.y/30 with L2 bridge
        let (host_ip, veth_subnet, guest_ip, guest_gateway_ip, veth_inner_ip) = if self.is_clone {
            // Clone case: veth gets unique 10.x.y.0/30 IP
            // Guest keeps its original 172.30.x.y IP from snapshot
            let third_octet = (subnet_id / 64) as u8;
            let subnet_within_block = (subnet_id % 64) as u8;
            let subnet_base = subnet_within_block * 4;

            // Use 10.x.y.0/30 for veth IPs (unique per clone)
            // host_ip = .1 (host side), veth_inner_ip = .2 (namespace side)
            let host_ip = format!(
                "10.{}.{}.{}",
                third_octet,
                subnet_within_block,
                subnet_base + 1
            );
            let veth_inner_ip = format!(
                "10.{}.{}.{}",
                third_octet,
                subnet_within_block,
                subnet_base + 2
            );
            let veth_subnet = format!(
                "10.{}.{}.{}/30",
                third_octet, subnet_within_block, subnet_base
            );

            // Guest IP from snapshot (what the guest OS expects)
            let guest_ip = self.guest_ip_override.clone().unwrap_or_default();

            // Calculate the original gateway IP that guest expects (guest_ip - 1 in the /30)
            let parts: Vec<&str> = guest_ip.split('.').collect();
            let orig_third: u8 = parts.get(2).and_then(|s| s.parse().ok()).unwrap_or(0);
            let orig_fourth: u8 = parts.get(3).and_then(|s| s.parse().ok()).unwrap_or(0);
            let orig_gateway = format!("172.30.{}.{}", orig_third, orig_fourth.saturating_sub(1));

            debug!(
                guest_ip = %guest_ip,
                guest_gateway = %orig_gateway,
                veth_host_ip = %host_ip,
                veth_inner_ip = %veth_inner_ip,
                veth_subnet = %veth_subnet,
                "clone using In-Namespace NAT"
            );

            (
                host_ip,
                veth_subnet,
                guest_ip,
                Some(orig_gateway),
                Some(veth_inner_ip),
            )
        } else {
            // Baseline VM case: use 172.30.x.y/30 for everything
            let third_octet = (subnet_id / 64) as u8;
            let subnet_within_block = (subnet_id % 64) as u8;
            let subnet_base = subnet_within_block * 4;

            let host_ip = format!("172.30.{}.{}", third_octet, subnet_base + 1);
            let veth_subnet = format!("172.30.{}.{}/30", third_octet, subnet_base);
            let guest_ip = format!("172.30.{}.{}", third_octet, subnet_base + 2);

            (host_ip, veth_subnet, guest_ip, None, None)
        };

        // Extract CIDR for host IP assignment
        let cidr_bits = veth_subnet.split('/').nth(1).unwrap_or("30");
        let host_ip_with_cidr = format!("{}/{}", host_ip, cidr_bits);

        // Store state progressively for cleanup on error
        self.host_ip = Some(host_ip.clone());
        self.guest_ip = Some(guest_ip.clone());
        self.subnet_cidr = Some(veth_subnet.clone());
        self.veth_inner_ip = veth_inner_ip.clone();

        // Step 1: Create network namespace
        let namespace_id = format!("fcvm-{}", truncate_id(&self.vm_id, 8));
        namespace::create_namespace(&namespace_id)
            .await
            .context("creating network namespace")?;
        self.namespace_id = Some(namespace_id.clone());

        // Step 2: Create veth pair
        let host_veth = format!("veth0-{}", truncate_id(&self.vm_id, 8));
        let guest_veth = format!("veth1-{}", truncate_id(&self.vm_id, 8));

        if let Err(e) = veth::create_veth_pair(&host_veth, &guest_veth, &namespace_id).await {
            let _ = self.cleanup().await;
            return Err(e).context("creating veth pair");
        }
        self.host_veth = Some(host_veth.clone());
        self.guest_veth = Some(guest_veth.clone());

        // Step 3: Configure host side of veth
        if let Err(e) = veth::setup_host_veth(&host_veth, &host_ip_with_cidr).await {
            let _ = self.cleanup().await;
            return Err(e).context("configuring host veth");
        }

        // Step 4: Create TAP device inside namespace
        if let Err(e) = veth::create_tap_in_ns(&namespace_id, &self.tap_device).await {
            let _ = self.cleanup().await;
            return Err(e).context("creating TAP device in namespace");
        }

        // Step 5: Connect TAP to network - different for clones vs baseline
        // For clones, we'll use a different health check IP (the veth inner IP)
        let mut health_check_ip = guest_ip.clone();

        if self.is_clone {
            // Clone: Use In-Namespace NAT
            // br0 gets gateway IP, veth1 gets unique IP, NAT inside namespace
            let gateway_ip = guest_gateway_ip
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("clone missing gateway IP"))?;

            // Calculate veth IP inside namespace (host_ip + 1)
            let parts: Vec<&str> = host_ip.split('.').collect();
            let last_octet: u8 = parts[3].parse().unwrap_or(1);
            let veth_inner_ip =
                format!("{}.{}.{}.{}", parts[0], parts[1], parts[2], last_octet + 1);
            let veth_inner_ip_cidr = format!("{}/30", veth_inner_ip);

            // Health checks for clones go to the veth inner IP, which gets DNATed to guest
            health_check_ip = veth_inner_ip.clone();

            let nat_config = veth::InNamespaceNatConfig {
                gateway_ip: gateway_ip.clone(),
                guest_ip: guest_ip.clone(),
                veth_ip_cidr: veth_inner_ip_cidr,
                host_veth_ip_cidr: host_ip_with_cidr.clone(),
            };

            if let Err(e) = veth::setup_in_namespace_nat(
                &namespace_id,
                &self.tap_device,
                &guest_veth,
                &nat_config,
            )
            .await
            {
                let _ = self.cleanup().await;
                return Err(e).context("setting up in-namespace NAT");
            }

            // Add host route to guest IP for direct access
            // This allows curling the guest IP directly from the host
            // Traffic: host → veth0 → veth1 (namespace) → br0 → TAP → guest
            if let Err(e) =
                veth::add_host_route_to_guest(&host_veth, &guest_ip, &veth_inner_ip).await
            {
                let _ = self.cleanup().await;
                return Err(e).context("adding host route to guest IP");
            }
        } else {
            // Baseline VM: Configure guest side of veth and connect via L2 bridge
            if let Err(e) = veth::setup_guest_veth_in_ns(&namespace_id, &guest_veth).await {
                let _ = self.cleanup().await;
                return Err(e).context("configuring guest veth");
            }

            if let Err(e) =
                veth::connect_tap_to_veth(&namespace_id, &self.tap_device, &guest_veth).await
            {
                let _ = self.cleanup().await;
                return Err(e).context("connecting TAP to veth");
            }
        }

        // Step 6: Ensure global NAT is configured
        let default_iface = match portmap::detect_default_interface().await {
            Ok(iface) => iface,
            Err(e) => {
                let _ = self.cleanup().await;
                return Err(e).context("detecting default network interface");
            }
        };

        // NAT for baseline VMs (172.30.x.x)
        if let Err(e) = portmap::ensure_global_nat("172.30.0.0/16", &default_iface).await {
            let _ = self.cleanup().await;
            return Err(e).context("ensuring global NAT for 172.30.0.0/16");
        }

        // NAT for clone veth traffic (10.x.x.x) - only needed for clones but harmless for baseline
        if let Err(e) = portmap::ensure_global_nat("10.0.0.0/8", &default_iface).await {
            let _ = self.cleanup().await;
            return Err(e).context("ensuring global NAT for 10.0.0.0/8");
        }

        // Step 7: Get DNS server for VM
        let dns_servers = get_host_dns_servers().context("getting DNS servers")?;
        let dns_server = dns_servers.first().cloned();

        // Step 8: Setup port mappings if any
        if !self.port_mappings.is_empty() {
            // For clones: DNAT to veth_inner_ip (host-reachable), blanket DNAT in namespace
            //             already forwards veth_inner_ip → guest_ip (set up in step 5)
            // For baseline: DNAT directly to guest_ip (host can route to it)
            let target_ip = if self.is_clone {
                self.veth_inner_ip
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("clone missing veth_inner_ip"))?
                    .clone()
            } else {
                guest_ip.clone()
            };

            // Scope DNAT rules to the veth's host IP - this allows parallel VMs to use
            // the same port since each VM has a unique veth IP
            let scoped_mappings: Vec<_> = self
                .port_mappings
                .iter()
                .map(|m| super::PortMapping {
                    host_ip: Some(host_ip.clone()),
                    ..m.clone()
                })
                .collect();

            match portmap::setup_port_mappings(&target_ip, &scoped_mappings).await {
                Ok(rules) => self.port_mapping_rules = rules,
                Err(e) => {
                    let _ = self.cleanup().await;
                    return Err(e).context("setting up port mappings");
                }
            }
        }

        // Generate MAC address
        let guest_mac = generate_mac();

        info!(
            namespace = %namespace_id,
            host_ip = %host_ip,
            guest_ip = %guest_ip,
            is_clone = %self.is_clone,
            "network namespace configured successfully"
        );

        // Get search domains for VM
        let search_domains = get_host_dns_search();
        let dns_search = if search_domains.is_empty() {
            None
        } else {
            Some(search_domains.join(","))
        };

        // Return network config with auto-generated health check URL
        // For clones, use the veth inner IP (which gets DNATed to guest)
        Ok(NetworkConfig {
            tap_device: self.tap_device.clone(),
            guest_mac,
            guest_ip: Some(guest_ip.clone()),
            host_ip: Some(host_ip.clone()),
            host_veth: self.host_veth.clone(),
            loopback_ip: None,
            health_check_port: Some(80),
            health_check_url: Some(format!("http://{}:80/", health_check_ip)),
            dns_server,
            dns_search,
            http_proxy: None, // Bridged mode has direct network access, no proxy needed
        })
    }

    async fn cleanup(&mut self) -> Result<()> {
        info!(vm_id = %self.vm_id, "cleaning up network namespace and resources");

        // Step 1: Cleanup port mapping rules (if any)
        if !self.port_mapping_rules.is_empty() {
            portmap::cleanup_port_mappings(&self.port_mapping_rules).await?;
        }

        // Step 2: Delete host route to guest IP (for clones)
        // This route was added to allow direct access to the guest IP from the host.
        // Must be deleted before the veth to prevent stale routes.
        if self.is_clone {
            if let Some(ref guest_ip) = self.guest_ip {
                veth::delete_host_route_to_guest(guest_ip).await?;
            }
        }

        // Step 3: Delete FORWARD rule and veth pair
        // Note: With In-Namespace NAT, all clone-specific rules are inside the namespace
        // and get cleaned up automatically when the namespace is deleted.
        if let Some(ref host_veth) = self.host_veth {
            // Delete FORWARD rule to avoid accumulating orphaned rules
            veth::delete_veth_forward_rule(host_veth).await?;
            // Then delete the veth pair (this will also remove the peer in the namespace)
            veth::delete_veth_pair(host_veth).await?;
        }

        // Step 4: Delete network namespace (this cleans up everything inside it)
        // Including all NAT rules, bridge, and veth peer
        if let Some(ref namespace_id) = self.namespace_id {
            namespace::delete_namespace(namespace_id).await?;
        }

        debug!(vm_id = %self.vm_id, "network cleanup complete");
        Ok(())
    }

    fn tap_device(&self) -> &str {
        &self.tap_device
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}
