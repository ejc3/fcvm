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
    let delete_rule = to_delete_rule(rule);

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

fn to_delete_rule(rule: &str) -> String {
    if let Some(rest) = rule.strip_prefix("-A ") {
        format!("-D {}", rest)
    } else {
        rule.replacen(" -A ", " -D ", 1)
    }
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

/// Comment used to tag iptables rules that fcvm added.
///
/// `cleanup_global_nat_if_unused()` only deletes rules carrying this comment, so
/// fcvm never removes NAT configuration an admin or another tool put in place.
const FCVM_RULE_COMMENT: &str = "fcvm-bridged";

/// Ensures global NAT is enabled for VM traffic
///
/// Sets up:
/// 1. Verifies IP forwarding is enabled (errors if not — admin must configure)
/// 2. Enables per-interface forwarding on the outbound interface
/// 3. MASQUERADE rule for outbound traffic from VM subnet (tagged with an
///    fcvm comment so cleanup only ever removes rules fcvm created)
///
/// The check-then-add of the MASQUERADE rule runs under the host-level
/// `bridged-nat.lock`, serializing it against `cleanup_global_nat_if_unused()`
/// in other fcvm processes. Without the lock, a stopping VM that has already
/// listed interfaces (and saw no veths) could delete the rule right after this
/// check sees it, leaving this VM without outbound NAT.
///
/// This should be called once during fcvm initialization, not per-VM.
pub async fn ensure_global_nat(vm_subnet: &str, outbound_iface: &str) -> Result<()> {
    debug!(
        subnet = %vm_subnet,
        interface = %outbound_iface,
        "ensuring global NAT configuration"
    );

    // Verify IP forwarding is enabled (we don't set it — that's a system-wide
    // setting the admin should configure, e.g. via sysctl.conf or cloud-init)
    let output = Command::new("sysctl")
        .args(["-n", "net.ipv4.ip_forward"])
        .output()
        .await
        .context("checking IP forwarding")?;

    if output.status.success() && String::from_utf8_lossy(&output.stdout).trim() != "1" {
        anyhow::bail!(
            "IP forwarding is disabled. Bridged networking requires net.ipv4.ip_forward=1.\n\
             Enable it with: sudo sysctl -w net.ipv4.ip_forward=1\n\
             To persist across reboots: echo 'net.ipv4.ip_forward=1' | sudo tee /etc/sysctl.d/99-ip-forward.conf"
        );
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
        warn!(
            "failed to enable forwarding on {}: {}",
            outbound_iface, stderr
        );
    }

    // Serialize the check-then-add against cleanup_global_nat_if_unused() running
    // in a concurrently stopping fcvm process.
    let _nat_lock = super::acquire_host_network_lock("bridged-nat.lock")
        .await
        .context("acquiring bridged NAT lock")?;

    // Check if the fcvm-tagged MASQUERADE rule already exists
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
            "-m",
            "comment",
            "--comment",
            FCVM_RULE_COMMENT,
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
            "-m",
            "comment",
            "--comment",
            FCVM_RULE_COMMENT,
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

/// Removes global NAT rules if no bridged VMs are running
///
/// Checks for veth0-* interfaces (indicates active bridged VMs).
/// If none exist, removes the fcvm-tagged MASQUERADE rules for both subnets
/// (rules without the fcvm comment were added by someone else and are left alone).
/// IP forwarding is intentionally left enabled (other services may depend on it).
/// Best-effort — logs warnings but doesn't fail.
///
/// The list-then-delete sequence runs under the host-level `bridged-nat.lock`,
/// serializing it against `ensure_global_nat()` in concurrently starting fcvm
/// processes so this never deletes a MASQUERADE rule another VM just confirmed.
pub async fn cleanup_global_nat_if_unused() {
    // Serialize against ensure_global_nat() in other fcvm processes.
    let _nat_lock = match super::acquire_host_network_lock("bridged-nat.lock").await {
        Ok(lock) => lock,
        Err(e) => {
            warn!(error = %e, "failed to acquire bridged NAT lock, leaving global NAT rules in place");
            return;
        }
    };

    // Check if any veth0- interfaces exist (active bridged VMs)
    let output = match Command::new("ip")
        .args(["-o", "link", "show"])
        .output()
        .await
    {
        Ok(o) if o.status.success() => o,
        _ => return, // Can't check — leave rules in place
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    if stdout.lines().any(|line| line.contains("veth0-")) {
        debug!("other bridged VMs still running, keeping global NAT rules");
        return;
    }

    info!("no bridged VMs running, cleaning up global NAT rules");

    // Detect outbound interface for MASQUERADE rule deletion
    let outbound_iface = match detect_default_interface().await {
        Ok(iface) => iface,
        Err(e) => {
            warn!(error = %e, "failed to detect default interface for NAT cleanup");
            return;
        }
    };

    // Remove the fcvm-tagged MASQUERADE rules for both subnets
    for subnet in &["172.30.0.0/16", "10.0.0.0/8"] {
        let output = Command::new("iptables")
            .args([
                "-t",
                "nat",
                "-D",
                "POSTROUTING",
                "-s",
                subnet,
                "-o",
                &outbound_iface,
                "-m",
                "comment",
                "--comment",
                FCVM_RULE_COMMENT,
                "-j",
                "MASQUERADE",
            ])
            .output()
            .await;

        match output {
            Ok(o) if o.status.success() => {
                debug!(subnet = %subnet, "removed MASQUERADE rule");
            }
            Ok(o) => {
                let stderr = String::from_utf8_lossy(&o.stderr);
                // Rule may already be gone (or was never added by fcvm)
                if !stderr.contains("does not exist")
                    && !stderr.contains("No chain")
                    && !stderr.contains("Bad rule")
                {
                    warn!(subnet = %subnet, error = %stderr, "failed to remove MASQUERADE rule");
                }
            }
            Err(e) => {
                warn!(subnet = %subnet, error = %e, "failed to run iptables for NAT cleanup");
            }
        }
    }

    // Note: we intentionally do NOT disable ip_forward here.
    // Other services on the host (Docker, Kubernetes, VPNs, etc.) may depend on
    // IP forwarding being enabled. Since we can't know whether fcvm was the one
    // that enabled it, the safe default is to leave it on.

    debug!("global NAT cleanup complete");
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

    parse_default_interface(&String::from_utf8_lossy(&output.stdout))
}

/// Pick the interface out of `ip route show default` output.
///
/// Empty output gets its own diagnosis rather than being reported as an
/// unparseable route: a host that routes only over IPv6 has no IPv4 default
/// route at all, and "could not detect default interface from: " with nothing
/// after the colon reads like a parser bug instead of an unsupported host.
fn parse_default_interface(stdout: &str) -> Result<String> {
    // Output format: "default via 192.168.1.1 dev eth0 ..."
    if let Some(prev) = stdout.split_whitespace().position(|p| p == "dev") {
        if let Some(iface) = stdout.split_whitespace().nth(prev + 1) {
            return Ok(iface.to_string());
        }
    }

    if stdout.trim().is_empty() {
        anyhow::bail!(
            "this host has no IPv4 default route, so bridged networking cannot \
             pick an interface to NAT through. Hosts that route only over IPv6 \
             hit this; use --network rootless there, or add an IPv4 default \
             route. (`ip route show default` printed nothing.)"
        );
    }
    anyhow::bail!("could not detect default interface from: {}", stdout)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_ipv6_only_host_is_diagnosed_not_reported_as_a_parse_failure() {
        // Measured on a devserver whose only default route is IPv6: `ip route
        // show default` prints nothing, and the old message ended in a colon
        // with nothing after it.
        let error = parse_default_interface("").expect_err("no default route");
        let text = format!("{error:#}");
        assert!(text.contains("no IPv4 default route"), "{text}");
        assert!(
            text.contains("rootless"),
            "must name the way forward: {text}"
        );
    }

    #[test]
    fn an_ordinary_default_route_yields_its_interface() {
        assert_eq!(
            parse_default_interface("default via 10.0.0.1 dev eth0 proto dhcp\n").unwrap(),
            "eth0"
        );
    }

    #[test]
    fn output_without_a_device_still_reports_what_it_saw() {
        let error = parse_default_interface("default via 10.0.0.1\n").expect_err("no dev");
        assert!(format!("{error:#}").contains("10.0.0.1"));
    }

    #[test]
    fn test_delete_rule_conversion() {
        let forward_rule = "-A FORWARD -p tcp -d 172.30.1.2 --dport 80 -j ACCEPT";
        let dnat_rule =
            "-t nat -A PREROUTING -p tcp --dport 8080 -j DNAT --to-destination 172.30.1.2:80";

        let forward_delete = to_delete_rule(forward_rule);
        let dnat_delete = to_delete_rule(dnat_rule);

        assert_eq!(
            forward_delete,
            "-D FORWARD -p tcp -d 172.30.1.2 --dport 80 -j ACCEPT"
        );
        assert_eq!(
            dnat_delete,
            "-t nat -D PREROUTING -p tcp --dport 8080 -j DNAT --to-destination 172.30.1.2:80"
        );
    }

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
