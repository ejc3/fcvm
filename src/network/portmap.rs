use anyhow::{Context, Result};
use tokio::process::Command;
use tracing::{debug, info, warn};

use super::types::{PortMapping, Protocol};

/// Sets up port mapping rules for a VM
///
/// Creates iptables DNAT rules to forward traffic from host ports to guest ports.
/// Returns a list of rule specifications that can be used for cleanup.
pub async fn setup_port_mappings(guest_ip: &str, mappings: &[PortMapping]) -> Result<Vec<String>> {
    if mappings.is_empty() {
        return Ok(Vec::new());
    }

    debug!(
        guest_ip = %guest_ip,
        mappings = mappings.len(),
        "setting up port mappings"
    );

    let mut created_rules: Vec<String> = Vec::new();

    for mapping in mappings {
        let proto_str = match mapping.proto {
            Protocol::Tcp => "tcp",
            Protocol::Udp => "udp",
        };

        // DNAT rule: Rewrite destination for incoming traffic
        // If host_ip is specified, only match packets destined to that IP (security: prevents exposing on all interfaces)
        let dnat_rule = if let Some(ref host_ip) = mapping.host_ip {
            format!(
                "-t nat -A PREROUTING -d {} -p {} --dport {} -j DNAT --to-destination {}:{}",
                host_ip, proto_str, mapping.host_port, guest_ip, mapping.guest_port
            )
        } else {
            format!(
                "-t nat -A PREROUTING -p {} --dport {} -j DNAT --to-destination {}:{}",
                proto_str, mapping.host_port, guest_ip, mapping.guest_port
            )
        };

        let output = Command::new("iptables")
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

        // OUTPUT DNAT rule: Rewrite destination for locally-generated traffic (localhost access)
        let output_dnat_rule = if let Some(ref host_ip) = mapping.host_ip {
            format!(
                "-t nat -A OUTPUT -d {} -p {} --dport {} -j DNAT --to-destination {}:{}",
                host_ip, proto_str, mapping.host_port, guest_ip, mapping.guest_port
            )
        } else {
            format!(
                "-t nat -A OUTPUT -p {} --dport {} -j DNAT --to-destination {}:{}",
                proto_str, mapping.host_port, guest_ip, mapping.guest_port
            )
        };

        let output = Command::new("iptables")
            .args(output_dnat_rule.split_whitespace())
            .output()
            .await
            .with_context(|| format!("adding OUTPUT DNAT rule for port {}", mapping.host_port))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            // Cleanup previously created rules
            for rule in &created_rules {
                let _ = delete_rule(rule).await;
            }
            anyhow::bail!("failed to add OUTPUT DNAT rule: {}", stderr);
        }

        created_rules.push(output_dnat_rule);

        // MASQUERADE rule: SNAT locally-generated traffic to guest so return path works
        // Without this, localhost -> guest traffic would have source 127.0.0.1 which
        // the guest can't respond to
        let masq_rule = format!(
            "-t nat -A POSTROUTING -d {} -p {} --dport {} -j MASQUERADE",
            guest_ip, proto_str, mapping.guest_port
        );

        let output = Command::new("iptables")
            .args(masq_rule.split_whitespace())
            .output()
            .await
            .with_context(|| format!("adding MASQUERADE rule for port {}", mapping.guest_port))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            for rule in &created_rules {
                let _ = delete_rule(rule).await;
            }
            anyhow::bail!("failed to add MASQUERADE rule: {}", stderr);
        }

        created_rules.push(masq_rule);

        // FORWARD rule: Allow forwarded traffic to guest
        let forward_rule = format!(
            "-A FORWARD -p {} -d {} --dport {} -j ACCEPT",
            proto_str, guest_ip, mapping.guest_port
        );

        let output = Command::new("iptables")
            .args(forward_rule.split_whitespace())
            .output()
            .await
            .with_context(|| format!("adding FORWARD rule for port {}", mapping.guest_port))?;

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

/// Enables route_localnet on a network interface
///
/// This is required for localhost port forwarding to work. By default, Linux
/// doesn't route packets with 127.0.0.0/8 source to external interfaces.
/// Enabling route_localnet allows DNAT'd packets from localhost to be routed
/// to the guest VM.
pub async fn enable_route_localnet(interface: &str) -> Result<()> {
    let sysctl_path = format!("net.ipv4.conf.{}.route_localnet", interface);

    let output = Command::new("sysctl")
        .args(["-w", &format!("{}=1", sysctl_path)])
        .output()
        .await
        .with_context(|| format!("enabling route_localnet on {}", interface))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        warn!(
            "failed to enable route_localnet on {}: {}",
            interface, stderr
        );
    } else {
        info!(
            interface = %interface,
            "enabled route_localnet for localhost port forwarding"
        );
    }

    Ok(())
}

/// Deletes a single iptables rule
///
/// Converts an -A (append) rule to -D (delete) and executes it.
async fn delete_rule(rule: &str) -> Result<()> {
    // Convert -A to -D for deletion
    let delete_rule = rule.replace(" -A ", " -D ");

    let output = Command::new("iptables")
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

    debug!(rules = rules.len(), "cleaning up port mapping rules");

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
    debug!(
        subnet = %vm_subnet,
        interface = %outbound_iface,
        "ensuring global NAT configuration"
    );

    // Enable IP forwarding globally
    let output = Command::new("sysctl")
        .args(["-w", "net.ipv4.ip_forward=1"])
        .output()
        .await
        .context("enabling IP forwarding")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        warn!("failed to enable IP forwarding: {}", stderr);
    }

    // Enable forwarding on the outbound interface specifically
    // (per-interface forwarding may be disabled even when global ip_forward=1)
    let iface_forwarding = format!("net.ipv4.conf.{}.forwarding=1", outbound_iface);
    let output = Command::new("sysctl")
        .args(["-w", &iface_forwarding])
        .output()
        .await
        .with_context(|| format!("enabling forwarding on {}", outbound_iface))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        warn!("failed to enable forwarding on {}: {}", outbound_iface, stderr);
    }

    // Check if MASQUERADE rule already exists
    let output = Command::new("iptables")
        .args([
            "-t",
            "nat",
            "-C",
            "POSTROUTING",
            "-s",
            vm_subnet,
            "-o",
            outbound_iface,
            "-j",
            "MASQUERADE",
        ])
        .output()
        .await?;

    if output.status.success() {
        // Rule already exists
        debug!("global MASQUERADE rule already exists");
        return Ok(());
    }

    // Add MASQUERADE rule for outbound traffic
    let output = Command::new("iptables")
        .args([
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

    debug!("global NAT configuration complete");
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
    if let Some(prev) = stdout.split_whitespace().position(|p| p == "dev") {
        if let Some(iface) = stdout.split_whitespace().nth(prev + 1) {
            return Ok(iface.to_string());
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

    #[cfg(feature = "privileged-tests")]
    #[tokio::test]
    async fn test_port_mapping_lifecycle() {
        // Test that we can create and cleanup rules (requires root for iptables)
        // Use a scoped host_ip so rules don't conflict with parallel tests
        let veth_ip = "172.30.99.1"; // Fake veth IP for testing
        let guest_ip = "172.30.99.2";
        let mappings = vec![PortMapping {
            host_ip: Some(veth_ip.to_string()), // Scope DNAT to this IP
            host_port: 8080,
            guest_port: 80,
            proto: Protocol::Tcp,
        }];

        // Setup
        let rules = setup_port_mappings(guest_ip, &mappings)
            .await
            .expect("setup port mappings (requires root)");

        assert_eq!(rules.len(), 4); // DNAT (PREROUTING) + DNAT (OUTPUT) + MASQUERADE + FORWARD

        // Cleanup
        cleanup_port_mappings(&rules).await.unwrap();
    }
}
