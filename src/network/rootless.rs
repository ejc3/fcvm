use anyhow::{Context, Result};
use tokio::process::Command;
use tracing::info;

use super::{types::generate_mac, NetworkConfig, NetworkManager, PortMapping};

/// Rootless networking using TAP device with static IP and NAT
/// This follows the standard Firecracker networking pattern:
/// - TAP device with static IP (requires sudo)
/// - iptables NAT for outbound routing
/// - Static IP configuration in guest
pub struct RootlessNetwork {
    vm_id: String,
    tap_device: String,
    #[allow(dead_code)]
    port_mappings: Vec<PortMapping>,
    guest_ip_override: Option<String>,
    host_ip: Option<String>,
    subnet_cidr: Option<String>,
    default_iface: Option<String>,
    iptables_cmd: Option<String>,
}

impl RootlessNetwork {
    pub fn new(vm_id: String, tap_device: String, port_mappings: Vec<PortMapping>) -> Self {
        Self {
            vm_id,
            tap_device,
            port_mappings,
            guest_ip_override: None,
            host_ip: None,
            subnet_cidr: None,
            default_iface: None,
            iptables_cmd: None,
        }
    }

    /// Set guest IP to use (for clones - use same IP as original VM)
    pub fn with_guest_ip(mut self, guest_ip: String) -> Self {
        self.guest_ip_override = Some(guest_ip);
        self
    }
}

#[async_trait::async_trait]
impl NetworkManager for RootlessNetwork {
    async fn setup(&mut self) -> Result<NetworkConfig> {
        info!(vm_id = %self.vm_id, "setting up rootless network with static IP and NAT");

        // Determine guest IP: use override if set (for clones), otherwise generate new
        let (host_ip, guest_ip, subnet) = if let Some(ref override_ip) = self.guest_ip_override {
            // Clone case: use same IP as original VM
            // Extract subnet from guest IP (e.g., "172.16.0.62" -> 60)
            let parts: Vec<&str> = override_ip.split('.').collect();
            if parts.len() != 4 {
                anyhow::bail!("invalid guest IP format: {}", override_ip);
            }
            let third_octet: u8 = parts[2].parse()
                .with_context(|| format!("parsing guest IP third octet: {}", override_ip))?;
            let fourth_octet: u8 = parts[3].parse()
                .with_context(|| format!("parsing guest IP fourth octet: {}", override_ip))?;

            // Guest IP should be subnet_base + 2, so subnet_base = guest_ip - 2
            let subnet_base = fourth_octet.saturating_sub(2);
            let host_ip = format!("172.30.{}.{}", third_octet, subnet_base + 1);
            let subnet = format!("172.30.{}.{}/30", third_octet, subnet_base);

            info!(
                guest_ip = %override_ip,
                host_ip = %host_ip,
                "using saved network config from snapshot"
            );

            (host_ip, override_ip.clone(), subnet)
        } else {
            // New VM case: generate unique subnet from vm_id hash
            // Use 172.30.x.y/30 subnet scheme with 16,384 possible subnets
            use std::collections::hash_map::DefaultHasher;
            use std::hash::{Hash, Hasher};

            let mut hasher = DefaultHasher::new();
            self.vm_id.hash(&mut hasher);
            let subnet_id = (hasher.finish() % 16384) as u16; // 16,384 subnets across 172.30.0-63.x

            // Each /30 subnet needs 4 IPs, so we can fit 64 subnets per /24 block
            // subnet_id 0-63 -> 172.30.0.0/30 through 172.30.0.252/30
            // subnet_id 64-127 -> 172.30.1.0/30 through 172.30.1.252/30
            // ...
            let third_octet = (subnet_id / 64) as u8;  // Which /24 block (0-255)
            let subnet_within_block = (subnet_id % 64) as u8;  // Which /30 within that /24
            let subnet_base = subnet_within_block * 4;

            let host_ip = format!("172.30.{}.{}", third_octet, subnet_base + 1);
            let guest_ip = format!("172.30.{}.{}", third_octet, subnet_base + 2);
            let subnet = format!("172.30.{}.{}/30", third_octet, subnet_base);

            (host_ip, guest_ip, subnet)
        };

        // Create TAP device and configure with unique static IP
        let nat_setup = setup_tap_with_nat(&self.tap_device, &host_ip, &subnet, &guest_ip).await?;

        // Generate MAC address
        let guest_mac = generate_mac();

        self.host_ip = Some(host_ip.clone());
        self.subnet_cidr = Some(subnet.clone());
        self.default_iface = Some(nat_setup.default_iface);
        self.iptables_cmd = Some(nat_setup.iptables_cmd);

        // Return network config with unique static IPs
        Ok(NetworkConfig {
            tap_device: self.tap_device.clone(),
            guest_mac,
            guest_ip: Some(guest_ip),
            host_ip: Some(host_ip),
        })
    }

    async fn cleanup(&mut self) -> Result<()> {
        // Clean up TAP device and iptables rules
        cleanup_tap_with_nat(
            &self.tap_device,
            self.subnet_cidr.as_deref(),
            self.default_iface.as_deref(),
            self.iptables_cmd.as_deref(),
        )
        .await?;
        Ok(())
    }

    fn tap_device(&self) -> &str {
        &self.tap_device
    }
}

struct NatSetup {
    default_iface: String,
    iptables_cmd: String,
}

/// Setup TAP device with static IP and NAT routing
/// Follows the official Firecracker networking pattern
async fn setup_tap_with_nat(
    tap_name: &str,
    host_ip: &str,
    subnet: &str,
    guest_ip: &str,
) -> Result<NatSetup> {
    info!(
        tap = tap_name,
        host_ip = host_ip,
        guest_ip = guest_ip,
        "setting up TAP device with static IP and NAT"
    );

    // 1. Create TAP device
    let output = Command::new("sudo")
        .args(["ip", "tuntap", "add", tap_name, "mode", "tap"])
        .output()
        .await
        .context("creating TAP device")?;

    if !output.status.success() {
        anyhow::bail!(
            "failed to create TAP device: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    // 2. Assign static IP to TAP device with subnet mask
    // Extract the CIDR from subnet (e.g., "172.16.0.0/30" -> "/30")
    let cidr_mask = subnet.split('/').nth(1).unwrap_or("30");
    let host_ip_with_cidr = format!("{}/{}", host_ip, cidr_mask);
    let output = Command::new("sudo")
        .args(["ip", "addr", "add", &host_ip_with_cidr, "dev", tap_name])
        .output()
        .await
        .context("assigning IP to TAP device")?;

    if !output.status.success() {
        anyhow::bail!(
            "failed to assign IP to TAP: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    // 3. Bring TAP device up
    let output = Command::new("sudo")
        .args(["ip", "link", "set", tap_name, "up"])
        .output()
        .await
        .context("bringing up TAP device")?;

    if !output.status.success() {
        anyhow::bail!(
            "failed to bring up TAP device: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    // 4. Enable IPv4 forwarding (required for routing)
    let output = Command::new("sudo")
        .args(["sysctl", "-w", "net.ipv4.ip_forward=1"])
        .output()
        .await
        .context("enabling IP forwarding")?;

    if !output.status.success() {
        anyhow::bail!(
            "failed to enable IP forwarding: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    // 5. Get default network interface for NAT
    let output = Command::new("ip")
        .args(["route", "show", "default"])
        .output()
        .await
        .context("getting default route")?;

    let default_route = String::from_utf8_lossy(&output.stdout);
    let default_iface = default_route
        .split_whitespace()
        .skip_while(|&s| s != "dev")
        .nth(1)
        .unwrap_or("eth0")
        .to_string();

    info!(interface = default_iface, "using default interface for NAT");

    // 6. Setup iptables NAT rules (use iptables-nft if available, fallback to iptables)
    let iptables_cmd = if Command::new("iptables-nft")
        .arg("--version")
        .output()
        .await
        .is_ok()
    {
        "iptables-nft"
    } else {
        "iptables"
    }
    .to_string();

    // MASQUERADE rule for outbound traffic from this VM's entire subnet
    // Use the subnet (172.16.X.0/24) instead of just the guest IP to catch all traffic
    let _ = Command::new("sudo")
        .args([
            iptables_cmd.as_str(),
            "-t",
            "nat",
            "-A",
            "POSTROUTING",
            "-o",
            default_iface.as_str(),
            "-s",
            subnet,
            "-j",
            "MASQUERADE",
        ])
        .output()
        .await;

    // Allow forwarding from TAP to external interface
    let _ = Command::new("sudo")
        .args([
            iptables_cmd.as_str(),
            "-A",
            "FORWARD",
            "-i",
            tap_name,
            "-o",
            default_iface.as_str(),
            "-j",
            "ACCEPT",
        ])
        .output()
        .await;

    // Allow established/related connections back
    let _ = Command::new("sudo")
        .args([
            iptables_cmd.as_str(),
            "-A",
            "FORWARD",
            "-i",
            default_iface.as_str(),
            "-o",
            tap_name,
            "-m",
            "conntrack",
            "--ctstate",
            "RELATED,ESTABLISHED",
            "-j",
            "ACCEPT",
        ])
        .output()
        .await;

    info!(
        tap = tap_name,
        host_ip = host_ip,
        guest_ip = guest_ip,
        "TAP device configured with NAT"
    );
    Ok(NatSetup {
        default_iface,
        iptables_cmd,
    })
}

/// Cleanup TAP device and iptables rules
async fn cleanup_tap_with_nat(
    tap_name: &str,
    subnet: Option<&str>,
    default_iface: Option<&str>,
    iptables_cmd: Option<&str>,
) -> Result<()> {
    info!(tap = tap_name, "cleaning up TAP device and NAT rules");

    // Delete iptables rules (best effort - don't fail if they don't exist)
    if let (Some(subnet), Some(default_iface), Some(iptables_cmd)) =
        (subnet, default_iface, iptables_cmd)
    {
        let _ = Command::new("sudo")
            .args([
                iptables_cmd,
                "-t",
                "nat",
                "-D",
                "POSTROUTING",
                "-o",
                default_iface,
                "-s",
                subnet,
                "-j",
                "MASQUERADE",
            ])
            .output()
            .await;

        let _ = Command::new("sudo")
            .args([
                iptables_cmd,
                "-D",
                "FORWARD",
                "-i",
                tap_name,
                "-o",
                default_iface,
                "-j",
                "ACCEPT",
            ])
            .output()
            .await;

        let _ = Command::new("sudo")
            .args([
                iptables_cmd,
                "-D",
                "FORWARD",
                "-i",
                default_iface,
                "-o",
                tap_name,
                "-m",
                "conntrack",
                "--ctstate",
                "RELATED,ESTABLISHED",
                "-j",
                "ACCEPT",
            ])
            .output()
            .await;
    }

    // Delete TAP device
    let output = Command::new("sudo")
        .args(["ip", "link", "delete", tap_name])
        .output()
        .await
        .context("deleting TAP device")?;

    if !output.status.success() {
        // Don't fail if device doesn't exist
        return Ok(());
    }

    info!(tap = tap_name, "TAP device cleaned up");
    Ok(())
}
