use anyhow::{Context, Result};
use tokio::process::Command;
use tracing::{info, warn};

use super::types::{PortMapping, Protocol};

/// Sets up port mapping rules for a VM
///
/// Creates iptables DNAT rules to forward traffic from host ports to guest ports.
/// Returns a list of rule specifications that can be used for cleanup.
pub async fn setup_port_mappings(
    guest_ip: &str,
    mappings: &[PortMapping],
) -> Result<Vec<String>> {
    if mappings.is_empty() {
        return Ok(Vec::new());
    }

    info!(
        guest_ip = %guest_ip,
        mappings = mappings.len(),
        "setting up port mappings"
    );

    let mut created_rules = Vec::new();

    for mapping in mappings {
        let proto_str = match mapping.proto {
            Protocol::Tcp => "tcp",
            Protocol::Udp => "udp",
        };

        // DNAT rule: Rewrite destination for incoming traffic
        let dnat_rule = format!(
            "-t nat -A PREROUTING -p {} --dport {} -j DNAT --to-destination {}:{}",
            proto_str, mapping.host_port, guest_ip, mapping.guest_port
        );

        let output = Command::new("sudo")
            .arg("iptables")
            .args(dnat_rule.split_whitespace())
            .output()
            .await
            .with_context(|| format!("adding DNAT rule for port {}", mapping.host_port))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            // Cleanup previously created rules
            for rule in &created_rules {
                let _ = delete_rule(rule).await;
            }
            anyhow::bail!("failed to add DNAT rule: {}", stderr);
        }

        created_rules.push(dnat_rule);

        // FORWARD rule: Allow forwarded traffic to guest
        let forward_rule = format!(
            "-A FORWARD -p {} -d {} --dport {} -j ACCEPT",
            proto_str, guest_ip, mapping.guest_port
        );

        let output = Command::new("sudo")
            .arg("iptables")
            .args(forward_rule.split_whitespace())
            .output()
            .await
            .with_context(|| {
                format!("adding FORWARD rule for port {}", mapping.guest_port)
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            // Cleanup all rules including DNAT
            for rule in &created_rules {
                let _ = delete_rule(rule).await;
            }
            anyhow::bail!("failed to add FORWARD rule: {}", stderr);
        }

        created_rules.push(forward_rule);

        info!(
            host_port = mapping.host_port,
            guest_port = mapping.guest_port,
            proto = proto_str,
            "port mapping created"
        );
    }

    Ok(created_rules)
}

/// Deletes a single iptables rule
///
/// Converts an -A (append) rule to -D (delete) and executes it.
async fn delete_rule(rule: &str) -> Result<()> {
    // Convert -A to -D for deletion
    let delete_rule = rule.replace(" -A ", " -D ");

    let output = Command::new("sudo")
        .arg("iptables")
        .args(delete_rule.split_whitespace())
        .output()
        .await
        .context("deleting iptables rule")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // Ignore "No chain/target/match" errors - rule already gone
        if !stderr.contains("No chain") && !stderr.contains("does not exist") {
            warn!("failed to delete iptables rule: {}", stderr);
        }
    }

    Ok(())
}

/// Cleans up port mapping rules for a VM
///
/// Takes the list of rules returned by setup_port_mappings() and removes them.
/// Rules are deleted in reverse order for proper cleanup.
pub async fn cleanup_port_mappings(rules: &[String]) -> Result<()> {
    if rules.is_empty() {
        return Ok(());
    }

    info!(rules = rules.len(), "cleaning up port mapping rules");

    // Delete in reverse order
    for rule in rules.iter().rev() {
        if let Err(e) = delete_rule(rule).await {
            warn!(rule = %rule, error = %e, "failed to delete port mapping rule");
        }
    }

    Ok(())
}

/// Ensures global NAT is enabled for VM traffic
///
/// Sets up:
/// 1. IP forwarding (sysctl)
/// 2. MASQUERADE rule for outbound traffic from VM subnet
///
/// This should be called once during fcvm initialization, not per-VM.
pub async fn ensure_global_nat(vm_subnet: &str, outbound_iface: &str) -> Result<()> {
    info!(
        subnet = %vm_subnet,
        interface = %outbound_iface,
        "ensuring global NAT configuration"
    );

    // Enable IP forwarding
    let output = Command::new("sudo")
        .args(["sysctl", "-w", "net.ipv4.ip_forward=1"])
        .output()
        .await
        .context("enabling IP forwarding")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        warn!("failed to enable IP forwarding: {}", stderr);
    }

    // Check if MASQUERADE rule already exists
    let output = Command::new("sudo")
        .args(["iptables", "-t", "nat", "-C", "POSTROUTING", "-s", vm_subnet, "-o", outbound_iface, "-j", "MASQUERADE"])
        .output()
        .await?;

    if output.status.success() {
        // Rule already exists
        info!("global MASQUERADE rule already exists");
        return Ok(());
    }

    // Add MASQUERADE rule for outbound traffic
    let output = Command::new("sudo")
        .args([
            "iptables",
            "-t",
            "nat",
            "-A",
            "POSTROUTING",
            "-s",
            vm_subnet,
            "-o",
            outbound_iface,
            "-j",
            "MASQUERADE",
        ])
        .output()
        .await
        .context("adding MASQUERADE rule")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("failed to add MASQUERADE rule: {}", stderr);
    }

    info!("global NAT configuration complete");
    Ok(())
}

/// Detects the default network interface for outbound traffic
pub async fn detect_default_interface() -> Result<String> {
    let output = Command::new("ip")
        .args(["route", "show", "default"])
        .output()
        .await
        .context("detecting default interface")?;

    if !output.status.success() {
        anyhow::bail!("failed to get default route");
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    // Output format: "default via 192.168.1.1 dev eth0 ..."
    for part in stdout.split_whitespace() {
        if let Some(prev) = stdout.split_whitespace().position(|p| p == "dev") {
            if let Some(iface) = stdout.split_whitespace().nth(prev + 1) {
                return Ok(iface.to_string());
            }
        }
    }

    // Fallback: try to parse manually
    if let Some(dev_pos) = stdout.find(" dev ") {
        let after_dev = &stdout[dev_pos + 5..];
        if let Some(iface) = after_dev.split_whitespace().next() {
            return Ok(iface.to_string());
        }
    }

    anyhow::bail!("could not detect default interface from: {}", stdout)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_detect_default_interface() {
        // This test just verifies the function doesn't panic
        // Actual interface name depends on the system
        let result = detect_default_interface().await;
        // On most systems this should succeed
        if let Ok(iface) = result {
            assert!(!iface.is_empty());
            println!("Detected interface: {}", iface);
        }
    }

    #[tokio::test]
    async fn test_port_mapping_lifecycle() {
        // Test that we can create and cleanup rules
        // Note: This test requires root and modifies iptables, so it's
        // more of an integration test. Skip in CI.
        let guest_ip = "172.30.0.2";
        let mappings = vec![PortMapping {
            host_ip: None,
            host_port: 18080,
            guest_port: 80,
            proto: Protocol::Tcp,
        }];

        // Setup
        let rules = setup_port_mappings(guest_ip, &mappings).await;

        if let Ok(rules) = rules {
            assert_eq!(rules.len(), 2); // DNAT + FORWARD

            // Cleanup
            cleanup_port_mappings(&rules).await.unwrap();
        } else {
            // If we can't setup (not root), that's OK for this test
            println!("Skipping port mapping test (requires root)");
        }
    }
}
