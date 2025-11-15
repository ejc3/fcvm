use anyhow::{Context, Result};
use tracing::info;

use super::{
    namespace, portmap, veth,
    types::generate_mac,
    NetworkConfig, NetworkManager, PortMapping
};

/// Rootless networking using network namespace isolation with veth pairs
///
/// New architecture (post namespace migration):
/// - Each VM runs in dedicated network namespace (fcvm-{vm_id})
/// - veth pair connects host namespace to VM namespace
/// - TAP device created inside VM namespace
/// - TAP connected directly to veth (no bridge)
/// - Port mappings via iptables DNAT/FORWARD rules
/// - Firecracker process runs inside the namespace
pub struct RootlessNetwork {
    vm_id: String,
    tap_device: String,
    port_mappings: Vec<PortMapping>,
    guest_ip_override: Option<String>,

    // Network state (populated during setup)
    namespace_id: Option<String>,
    host_veth: Option<String>,
    guest_veth: Option<String>,
    host_ip: Option<String>,
    guest_ip: Option<String>,
    subnet_cidr: Option<String>,
    port_mapping_rules: Vec<String>,
}

impl RootlessNetwork {
    pub fn new(vm_id: String, tap_device: String, port_mappings: Vec<PortMapping>) -> Self {
        Self {
            vm_id,
            tap_device,
            port_mappings,
            guest_ip_override: None,
            namespace_id: None,
            host_veth: None,
            guest_veth: None,
            host_ip: None,
            guest_ip: None,
            subnet_cidr: None,
            port_mapping_rules: Vec::new(),
        }
    }

    /// Set guest IP to use (for clones - use same IP as original VM)
    pub fn with_guest_ip(mut self, guest_ip: String) -> Self {
        self.guest_ip_override = Some(guest_ip);
        self
    }

    /// Get the namespace ID for this network
    pub fn namespace_id(&self) -> Option<&str> {
        self.namespace_id.as_deref()
    }
}

#[async_trait::async_trait]
impl NetworkManager for RootlessNetwork {
    async fn setup(&mut self) -> Result<NetworkConfig> {
        info!(vm_id = %self.vm_id, "setting up network namespace with veth pair isolation");

        // Step 1: Determine IPs (same logic as before)
        let (host_ip, guest_ip, subnet) = if let Some(ref override_ip) = self.guest_ip_override {
            // Clone case: parse saved guest IP to determine subnet
            let parts: Vec<&str> = override_ip.split('.').collect();
            if parts.len() != 4 {
                anyhow::bail!("invalid guest IP format: {}", override_ip);
            }
            let third_octet: u8 = parts[2].parse()
                .with_context(|| format!("parsing guest IP third octet: {}", override_ip))?;
            let fourth_octet: u8 = parts[3].parse()
                .with_context(|| format!("parsing guest IP fourth octet: {}", override_ip))?;

            let subnet_base = fourth_octet.saturating_sub(2);
            let host_ip = format!("172.30.{}.{}", third_octet, subnet_base + 1);
            let subnet = format!("172.30.{}.{}/30", third_octet, subnet_base);

            info!(
                guest_ip = %override_ip,
                host_ip = %host_ip,
                "reusing network config from snapshot"
            );

            (host_ip, override_ip.clone(), subnet)
        } else {
            // New VM case: generate unique subnet from vm_id hash
            use std::collections::hash_map::DefaultHasher;
            use std::hash::{Hash, Hasher};

            let mut hasher = DefaultHasher::new();
            self.vm_id.hash(&mut hasher);
            let subnet_id = (hasher.finish() % 16384) as u16;

            let third_octet = (subnet_id / 64) as u8;
            let subnet_within_block = (subnet_id % 64) as u8;
            let subnet_base = subnet_within_block * 4;

            let host_ip = format!("172.30.{}.{}", third_octet, subnet_base + 1);
            let guest_ip = format!("172.30.{}.{}", third_octet, subnet_base + 2);
            let subnet = format!("172.30.{}.{}/30", third_octet, subnet_base);

            (host_ip, guest_ip, subnet)
        };

        // Extract CIDR for IP assignment
        let cidr_bits = subnet.split('/').nth(1).unwrap_or("30");
        let host_ip_with_cidr = format!("{}/{}", host_ip, cidr_bits);
        let guest_ip_with_cidr = format!("{}/{}", guest_ip, cidr_bits);

        // Step 2: Create network namespace
        let namespace_id = format!("fcvm-{}", &self.vm_id[..8]);
        namespace::create_namespace(&namespace_id).await
            .context("creating network namespace")?;

        // Step 3: Create veth pair
        // Linux interface names are limited to 15 chars (IFNAMSIZ = 16 including null)
        let host_veth = format!("veth0-{}", &self.vm_id[..8]);
        let guest_veth = format!("veth1-{}", &self.vm_id[..8]);

        veth::create_veth_pair(&host_veth, &guest_veth, &namespace_id).await
            .context("creating veth pair")?;

        // Step 4: Configure host side of veth
        veth::setup_host_veth(&host_veth, &host_ip_with_cidr).await
            .context("configuring host veth")?;

        // Step 5: Configure guest side of veth inside namespace
        veth::setup_guest_veth_in_ns(&namespace_id, &guest_veth, &guest_ip_with_cidr, &host_ip).await
            .context("configuring guest veth")?;

        // Step 6: Create TAP device inside namespace
        veth::create_tap_in_ns(&namespace_id, &self.tap_device).await
            .context("creating TAP device in namespace")?;

        // Step 7: Connect TAP to veth inside namespace
        // The guest (Firecracker) will use the TAP, and it routes through veth to host
        let tap_ip_with_cidr = guest_ip_with_cidr.clone();
        veth::connect_tap_to_veth(&namespace_id, &self.tap_device, &guest_veth, &tap_ip_with_cidr).await
            .context("connecting TAP to veth")?;

        // Step 8: Ensure global NAT is configured
        let default_iface = portmap::detect_default_interface().await
            .context("detecting default network interface")?;

        portmap::ensure_global_nat("172.30.0.0/16", &default_iface).await
            .context("ensuring global NAT configuration")?;

        // Step 9: Setup port mappings if any
        let port_mapping_rules = if !self.port_mappings.is_empty() {
            portmap::setup_port_mappings(&guest_ip, &self.port_mappings).await
                .context("setting up port mappings")?
        } else {
            Vec::new()
        };

        // Generate MAC address
        let guest_mac = generate_mac();

        // Store state for cleanup
        self.namespace_id = Some(namespace_id.clone());
        self.host_veth = Some(host_veth);
        self.guest_veth = Some(guest_veth);
        self.host_ip = Some(host_ip.clone());
        self.guest_ip = Some(guest_ip.clone());
        self.subnet_cidr = Some(subnet);
        self.port_mapping_rules = port_mapping_rules;

        info!(
            namespace = %namespace_id,
            host_ip = %host_ip,
            guest_ip = %guest_ip,
            "network namespace configured successfully"
        );

        // Return network config
        Ok(NetworkConfig {
            tap_device: self.tap_device.clone(),
            guest_mac,
            guest_ip: Some(guest_ip),
            host_ip: Some(host_ip),
            host_veth: self.host_veth.clone(),
        })
    }

    async fn cleanup(&mut self) -> Result<()> {
        info!(vm_id = %self.vm_id, "cleaning up network namespace and resources");

        // Step 1: Cleanup port mapping rules (if any)
        if !self.port_mapping_rules.is_empty() {
            portmap::cleanup_port_mappings(&self.port_mapping_rules).await?;
        }

        // Step 2: Delete veth pair (this will also remove the peer in the namespace)
        if let Some(ref host_veth) = self.host_veth {
            veth::delete_veth_pair(host_veth).await?;
        }

        // Step 3: Delete network namespace (this will cleanup everything inside it)
        if let Some(ref namespace_id) = self.namespace_id {
            namespace::delete_namespace(namespace_id).await?;
        }

        info!(vm_id = %self.vm_id, "network cleanup complete");
        Ok(())
    }

    fn tap_device(&self) -> &str {
        &self.tap_device
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}
