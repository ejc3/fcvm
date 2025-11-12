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
}

impl RootlessNetwork {
    pub fn new(vm_id: String, tap_device: String, port_mappings: Vec<PortMapping>) -> Self {
        Self {
            vm_id,
            tap_device,
            port_mappings,
        }
    }
}

#[async_trait::async_trait]
impl NetworkManager for RootlessNetwork {
    async fn setup(&mut self) -> Result<NetworkConfig> {
        info!(vm_id = %self.vm_id, "setting up rootless network with static IP and NAT");

        // Use unique /30 subnet per VM to avoid routing conflicts
        // Each VM gets a 4-IP subnet: network, host, guest, broadcast
        // Example: 172.16.X.0/30 where X is derived from vm_id hash
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let mut hasher = DefaultHasher::new();
        self.vm_id.hash(&mut hasher);
        let subnet_id = (hasher.finish() % 64) as u8; // Use 64 subnets: 172.16.0-63.0/30

        let subnet_base = subnet_id * 4;
        let host_ip = format!("172.16.0.{}", subnet_base + 1);
        let guest_ip = format!("172.16.0.{}", subnet_base + 2);
        let subnet = format!("172.16.0.{}/30", subnet_base);

        // Create TAP device and configure with unique static IP
        setup_tap_with_nat(&self.tap_device, &host_ip, &subnet, &guest_ip).await?;

        // Generate MAC address
        let guest_mac = generate_mac();

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
        cleanup_tap_with_nat(&self.tap_device).await?;
        Ok(())
    }

    fn tap_device(&self) -> &str {
        &self.tap_device
    }
}

/// Setup TAP device with static IP and NAT routing
/// Follows the official Firecracker networking pattern
async fn setup_tap_with_nat(
    tap_name: &str,
    host_ip: &str,
    subnet: &str,
    guest_ip: &str,
) -> Result<()> {
    info!(
        tap = tap_name,
        host_ip = host_ip,
        guest_ip = guest_ip,
        "setting up TAP device with static IP and NAT"
    );

    // 1. Create TAP device
    let output = Command::new("sudo")
        .args(&["ip", "tuntap", "add", tap_name, "mode", "tap"])
        .output()
        .await
        .context("creating TAP device")?;

    if !output.status.success() {
        anyhow::bail!(
            "failed to create TAP device: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    // 2. Assign static IP to TAP device (host IP with /24 netmask)
    let host_ip_with_cidr = format!("{}/24", host_ip);
    let output = Command::new("sudo")
        .args(&["ip", "addr", "add", &host_ip_with_cidr, "dev", tap_name])
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
        .args(&["ip", "link", "set", tap_name, "up"])
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
        .args(&["sysctl", "-w", "net.ipv4.ip_forward=1"])
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
        .args(&["route", "show", "default"])
        .output()
        .await
        .context("getting default route")?;

    let default_route = String::from_utf8_lossy(&output.stdout);
    let default_iface = default_route
        .split_whitespace()
        .skip_while(|&s| s != "dev")
        .nth(1)
        .unwrap_or("eth0");

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
    };

    // MASQUERADE rule for outbound traffic from this VM's entire subnet
    // Use the subnet (172.16.X.0/24) instead of just the guest IP to catch all traffic
    let _ = Command::new("sudo")
        .args(&[
            iptables_cmd,
            "-t",
            "nat",
            "-A",
            "POSTROUTING",
            "-o",
            default_iface,
            "-s",
            &subnet,
            "-j",
            "MASQUERADE",
        ])
        .output()
        .await;

    // Allow forwarding from TAP to external interface
    let _ = Command::new("sudo")
        .args(&[
            iptables_cmd,
            "-A",
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

    // Allow return traffic from external interface to TAP
    let _ = Command::new("sudo")
        .args(&[
            iptables_cmd,
            "-A",
            "FORWARD",
            "-i",
            default_iface,
            "-o",
            tap_name,
            "-j",
            "ACCEPT",
        ])
        .output()
        .await;

    // Allow established/related connections back
    let _ = Command::new("sudo")
        .args(&[
            iptables_cmd,
            "-A",
            "FORWARD",
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
    Ok(())
}

/// Cleanup TAP device and iptables rules
async fn cleanup_tap_with_nat(tap_name: &str) -> Result<()> {
    info!(tap = tap_name, "cleaning up TAP device and NAT rules");

    // Delete iptables rules (best effort - don't fail if they don't exist)
    let iptables_cmd = if Command::new("iptables-nft")
        .arg("--version")
        .output()
        .await
        .is_ok()
    {
        "iptables-nft"
    } else {
        "iptables"
    };

    let _ = Command::new("sudo")
        .args(&[
            iptables_cmd,
            "-t",
            "nat",
            "-D",
            "POSTROUTING",
            "-s",
            "172.16.0.2",
            "-j",
            "MASQUERADE",
        ])
        .output()
        .await;

    // Delete TAP device
    let output = Command::new("sudo")
        .args(&["ip", "link", "delete", tap_name])
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
