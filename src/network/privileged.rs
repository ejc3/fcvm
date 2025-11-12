use anyhow::{Context, Result};
use tracing::info;

use super::{types::generate_mac, NetworkConfig, NetworkManager, PortMapping};

/// Privileged networking using bridge + nftables
pub struct PrivilegedNetwork {
    vm_id: String,
    tap_device: String,
    bridge: String,
    guest_ip: String,
    host_ip: String,
    port_mappings: Vec<PortMapping>,
}

impl PrivilegedNetwork {
    pub fn new(
        vm_id: String,
        tap_device: String,
        bridge: String,
        guest_ip: String,
        host_ip: String,
        port_mappings: Vec<PortMapping>,
    ) -> Self {
        Self {
            vm_id,
            tap_device,
            bridge,
            guest_ip,
            host_ip,
            port_mappings,
        }
    }
}

#[async_trait::async_trait]
impl NetworkManager for PrivilegedNetwork {
    async fn setup(&mut self) -> Result<NetworkConfig> {
        info!(vm_id = %self.vm_id, "setting up privileged network");

        // Create TAP device
        create_tap_device(&self.tap_device).await?;

        // Add to bridge
        add_to_bridge(&self.tap_device, &self.bridge).await?;

        // Setup NAT rules for port forwarding
        for mapping in &self.port_mappings {
            setup_nat_rule(mapping, &self.guest_ip).await?;
        }

        let guest_mac = generate_mac();

        Ok(NetworkConfig {
            tap_device: self.tap_device.clone(),
            guest_mac,
            guest_ip: Some(self.guest_ip.clone()),
            host_ip: Some(self.host_ip.clone()),
        })
    }

    async fn cleanup(&mut self) -> Result<()> {
        info!(vm_id = %self.vm_id, "cleaning up privileged network");

        // Remove NAT rules
        for mapping in &self.port_mappings {
            let _ = remove_nat_rule(mapping, &self.guest_ip).await;
        }

        // Remove from bridge
        let _ = remove_from_bridge(&self.tap_device, &self.bridge).await;

        // Delete TAP device
        let _ = delete_tap_device(&self.tap_device).await;

        Ok(())
    }

    fn tap_device(&self) -> &str {
        &self.tap_device
    }
}

/// Create a TAP device
async fn create_tap_device(tap_name: &str) -> Result<()> {
    use std::process::Command;

    let output = Command::new("ip")
        .args(&["tuntap", "add", tap_name, "mode", "tap"])
        .output()
        .context("creating TAP device")?;

    if !output.status.success() {
        anyhow::bail!(
            "failed to create TAP device: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    // Bring it up
    let output = Command::new("ip")
        .args(&["link", "set", tap_name, "up"])
        .output()
        .context("bringing up TAP device")?;

    if !output.status.success() {
        anyhow::bail!(
            "failed to bring up TAP device: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    info!(tap = tap_name, "created TAP device");
    Ok(())
}

/// Delete a TAP device
async fn delete_tap_device(tap_name: &str) -> Result<()> {
    use std::process::Command;

    let output = Command::new("ip")
        .args(&["link", "delete", tap_name])
        .output()
        .context("deleting TAP device")?;

    if !output.status.success() {
        anyhow::bail!(
            "failed to delete TAP device: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    info!(tap = tap_name, "deleted TAP device");
    Ok(())
}

/// Add TAP device to bridge
async fn add_to_bridge(tap_name: &str, bridge: &str) -> Result<()> {
    use std::process::Command;

    let output = Command::new("ip")
        .args(&["link", "set", tap_name, "master", bridge])
        .output()
        .context("adding TAP to bridge")?;

    if !output.status.success() {
        anyhow::bail!(
            "failed to add TAP to bridge: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    info!(tap = tap_name, bridge = bridge, "added TAP to bridge");
    Ok(())
}

/// Remove TAP device from bridge
async fn remove_from_bridge(tap_name: &str, _bridge: &str) -> Result<()> {
    use std::process::Command;

    let output = Command::new("ip")
        .args(&["link", "set", tap_name, "nomaster"])
        .output()
        .context("removing TAP from bridge")?;

    if !output.status.success() {
        anyhow::bail!(
            "failed to remove TAP from bridge: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    Ok(())
}

/// Setup NAT rule for port forwarding
async fn setup_nat_rule(mapping: &PortMapping, guest_ip: &str) -> Result<()> {
    use std::process::Command;

    let host_ip = mapping.host_ip.as_deref().unwrap_or("0.0.0.0");

    // Add DNAT rule using nftables
    let rule = format!(
        "add rule ip nat PREROUTING {} dport {} dnat to {}:{}",
        mapping.proto, mapping.host_port, guest_ip, mapping.guest_port
    );

    let output = Command::new("nft")
        .arg(&rule)
        .output()
        .context("setting up NAT rule")?;

    if !output.status.success() {
        anyhow::bail!(
            "failed to setup NAT rule: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    info!(
        host = %host_ip,
        host_port = mapping.host_port,
        guest_ip = %guest_ip,
        guest_port = mapping.guest_port,
        proto = %mapping.proto,
        "setup NAT rule"
    );

    Ok(())
}

/// Remove NAT rule
async fn remove_nat_rule(mapping: &PortMapping, guest_ip: &str) -> Result<()> {
    use std::process::Command;

    // Delete DNAT rule
    let rule = format!(
        "delete rule ip nat PREROUTING {} dport {} dnat to {}:{}",
        mapping.proto, mapping.host_port, guest_ip, mapping.guest_port
    );

    let output = Command::new("nft")
        .arg(&rule)
        .output()
        .context("removing NAT rule")?;

    if !output.status.success() {
        // Don't fail if rule doesn't exist
        return Ok(());
    }

    Ok(())
}
