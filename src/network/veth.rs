use anyhow::{Context, Result};
use tokio::process::Command;
use tracing::{info, warn};

use super::namespace::exec_in_namespace;

/// Creates a veth pair and moves guest side into a namespace
///
/// Creates a pair of virtual ethernet devices. The host side remains in the
/// root namespace, while the guest side is moved into the VM's namespace.
pub async fn create_veth_pair(host_veth: &str, guest_veth: &str, ns_name: &str) -> Result<()> {
    info!(
        host = %host_veth,
        guest = %guest_veth,
        namespace = %ns_name,
        "creating veth pair"
    );

    // Create veth pair in root namespace
    let output = Command::new("sudo")
        .args([
            "ip", "link", "add", host_veth, "type", "veth", "peer", "name", guest_veth,
        ])
        .output()
        .await
        .context("executing ip link add veth")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("failed to create veth pair: {}", stderr);
    }

    // Move guest side into namespace
    let output = Command::new("sudo")
        .args(["ip", "link", "set", guest_veth, "netns", ns_name])
        .output()
        .await
        .context("moving veth to namespace")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // Cleanup host veth on failure
        let _ = Command::new("sudo")
            .args(["ip", "link", "del", host_veth])
            .output()
            .await;
        anyhow::bail!("failed to move veth to namespace: {}", stderr);
    }

    Ok(())
}

/// Cleans up any stale veth interface with the same IP address
///
/// This is a proactive cleanup for cases where a previous fcvm process was killed
/// with SIGKILL and didn't get a chance to clean up its network resources.
/// Without this, the stale veth's IP would conflict with the new one, causing
/// routing issues (return traffic goes to wrong interface).
async fn cleanup_stale_veth_with_ip(ip_with_cidr: &str, exclude_veth: &str) -> Result<()> {
    // Extract just the IP (without CIDR)
    let ip = ip_with_cidr.split('/').next().unwrap_or(ip_with_cidr);

    // Find all interfaces with this IP
    let output = Command::new("ip")
        .args(["-o", "addr", "show"])
        .output()
        .await
        .context("listing interfaces")?;

    if !output.status.success() {
        return Ok(()); // Best effort - if we can't list, skip cleanup
    }

    let stdout = String::from_utf8_lossy(&output.stdout);

    // Parse output to find veth interfaces with matching IP
    // Format: "4: veth0-vm-abc@if3: <...> inet 172.30.90.5/30 ..."
    for line in stdout.lines() {
        if line.contains(ip) && line.contains("veth0-vm-") {
            // Extract interface name
            if let Some(iface) = line.split_whitespace().nth(1) {
                // Remove trailing @... and colon
                let iface_name = iface
                    .split('@')
                    .next()
                    .unwrap_or(iface)
                    .trim_end_matches(':');

                // Don't delete the veth we're about to configure
                if iface_name == exclude_veth {
                    continue;
                }

                warn!(
                    stale_veth = %iface_name,
                    ip = %ip,
                    "cleaning up stale veth with conflicting IP"
                );

                // Clean up CONNMARK routing rules for the stale veth
                // The stale veth has the same IP, so we can use it to compute the mark
                let _ = cleanup_connmark_routing(iface_name, ip).await;

                // Delete the FORWARD rule
                let _ = delete_veth_forward_rule(iface_name).await;

                // Delete the stale veth
                let _ = delete_veth_pair(iface_name).await;
            }
        }
    }

    Ok(())
}

/// Cleans up orphaned iptables rules for non-existent veth interfaces
///
/// When a VM is killed with SIGKILL, its CONNMARK rules remain pointing to
/// a non-existent veth interface. This function finds and removes such rules.
pub async fn cleanup_orphaned_veth_rules() -> Result<()> {
    // Get list of existing veth interfaces
    let output = Command::new("ip")
        .args(["-o", "link", "show", "type", "veth"])
        .output()
        .await
        .context("listing veth interfaces")?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let existing_veths: std::collections::HashSet<&str> = stdout
        .lines()
        .filter_map(|line| {
            // Format: "4: veth0-vm-abc@if3: <...>"
            line.split_whitespace()
                .nth(1)
                .and_then(|s| s.split('@').next())
                .map(|s| s.trim_end_matches(':'))
        })
        .filter(|s| s.starts_with("veth0-vm-"))
        .collect();

    // List all rules in mangle PREROUTING
    let output = Command::new("sudo")
        .args(["iptables", "-t", "mangle", "-S", "PREROUTING"])
        .output()
        .await
        .context("listing mangle PREROUTING rules")?;

    if !output.status.success() {
        return Ok(()); // Chain might not exist
    }

    let stdout = String::from_utf8_lossy(&output.stdout);

    // Find rules referencing non-existent veth interfaces
    for line in stdout.lines() {
        // Look for rules like: -A PREROUTING -i veth0-vm-xxxxx ...
        if line.contains("-i veth0-vm-") {
            // Extract the veth name
            let parts: Vec<&str> = line.split_whitespace().collect();
            if let Some(idx) = parts.iter().position(|&p| p == "-i") {
                if let Some(veth_name) = parts.get(idx + 1) {
                    if !existing_veths.contains(*veth_name) {
                        // This veth doesn't exist anymore - orphaned rule
                        warn!(
                            veth = %veth_name,
                            rule = %line,
                            "removing orphaned iptables rule for non-existent veth"
                        );

                        // Delete the rule (convert -A to -D)
                        let delete_args: Vec<&str> = line
                            .split_whitespace()
                            .skip(1) // Skip "-A"
                            .collect();

                        let _ = Command::new("sudo")
                            .arg("iptables")
                            .arg("-t")
                            .arg("mangle")
                            .arg("-D")
                            .args(&delete_args)
                            .output()
                            .await;
                    }
                }
            }
        }
    }

    Ok(())
}

/// Removes duplicate CONNMARK restore rules from a chain, keeping only one
///
/// The `-C` check can fail for various reasons, leading to duplicate rules.
/// This function ensures we only have one CONNMARK restore rule per chain.
async fn cleanup_duplicate_connmark_restore(chain: &str) -> Result<()> {
    // List the chain and count CONNMARK restore rules
    let output = Command::new("sudo")
        .args(["iptables", "-t", "mangle", "-S", chain])
        .output()
        .await
        .context("listing mangle chain")?;

    if !output.status.success() {
        return Ok(()); // Chain might not exist, that's fine
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let restore_rules: Vec<&str> = stdout
        .lines()
        .filter(|line| line.contains("CONNMARK") && line.contains("restore"))
        .collect();

    // If there's more than one, delete the extras (keep the first)
    if restore_rules.len() > 1 {
        let to_delete = restore_rules.len() - 1;
        info!(
            chain = %chain,
            total = restore_rules.len(),
            deleting = to_delete,
            "cleaning up duplicate CONNMARK restore rules"
        );

        for _ in 0..to_delete {
            // Delete one instance (iptables -D deletes the first match)
            let _ = Command::new("sudo")
                .args([
                    "iptables", "-t", "mangle", "-D", chain,
                    "-m", "conntrack", "--ctstate", "ESTABLISHED,RELATED",
                    "-j", "CONNMARK", "--restore-mark",
                ])
                .output()
                .await;
        }
    }

    Ok(())
}

/// Ensures global CONNMARK restore rules exist in mangle PREROUTING and OUTPUT
///
/// These rules restore the connection mark for reply packets, which is essential
/// for routing replies back to the correct clone veth. Only needs to be added once.
/// - PREROUTING: For replies from external hosts (packets entering the system)
/// - OUTPUT: For replies from local processes (e.g., local HTTP server)
///
/// Also cleans up:
/// - Duplicate restore rules that may have accumulated from concurrent starts
/// - Orphaned rules for veth interfaces that no longer exist (from SIGKILL'd VMs)
async fn ensure_connmark_restore_rule() -> Result<()> {
    // First, clean up any orphaned rules from crashed VMs
    let _ = cleanup_orphaned_veth_rules().await;

    // Check and add PREROUTING restore rule (for external replies)
    let output = Command::new("sudo")
        .args([
            "iptables", "-t", "mangle", "-C", "PREROUTING",
            "-m", "conntrack", "--ctstate", "ESTABLISHED,RELATED",
            "-j", "CONNMARK", "--restore-mark",
        ])
        .output()
        .await
        .context("checking CONNMARK restore rule in PREROUTING")?;

    if !output.status.success() {
        let output = Command::new("sudo")
            .args([
                "iptables", "-t", "mangle", "-I", "PREROUTING", "1",
                "-m", "conntrack", "--ctstate", "ESTABLISHED,RELATED",
                "-j", "CONNMARK", "--restore-mark",
            ])
            .output()
            .await
            .context("adding CONNMARK restore rule to PREROUTING")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("failed to add CONNMARK restore rule to PREROUTING: {}", stderr);
        }
        info!("added global CONNMARK restore rule to PREROUTING for external replies");
    }

    // Clean up any duplicate PREROUTING rules
    cleanup_duplicate_connmark_restore("PREROUTING").await?;

    // Check and add OUTPUT restore rule (for local replies)
    let output = Command::new("sudo")
        .args([
            "iptables", "-t", "mangle", "-C", "OUTPUT",
            "-m", "conntrack", "--ctstate", "ESTABLISHED,RELATED",
            "-j", "CONNMARK", "--restore-mark",
        ])
        .output()
        .await
        .context("checking CONNMARK restore rule in OUTPUT")?;

    if !output.status.success() {
        let output = Command::new("sudo")
            .args([
                "iptables", "-t", "mangle", "-I", "OUTPUT", "1",
                "-m", "conntrack", "--ctstate", "ESTABLISHED,RELATED",
                "-j", "CONNMARK", "--restore-mark",
            ])
            .output()
            .await
            .context("adding CONNMARK restore rule to OUTPUT")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("failed to add CONNMARK restore rule to OUTPUT: {}", stderr);
        }
        info!("added global CONNMARK restore rule to OUTPUT for local replies");
    }

    // Clean up any duplicate OUTPUT rules
    cleanup_duplicate_connmark_restore("OUTPUT").await?;

    Ok(())
}

/// Sets up CONNMARK-based routing for a clone
///
/// This enables multiple clones with the same guest IP to all have working egress.
/// Each clone gets:
/// 1. A unique mark (derived from host_ip last two octets)
/// 2. An iptables rule to mark packets from this veth and save to CONNMARK
/// 3. A policy routing rule (ip rule) to route marked packets to a dedicated table
/// 4. A route in the dedicated table for the guest's subnet
async fn setup_connmark_routing(
    veth_name: &str,
    host_ip_with_cidr: &str,
    guest_subnet: &str,
) -> Result<()> {
    // Extract host IP parts to create unique mark
    // Use last two octets to create a 16-bit mark (avoiding 0)
    let host_ip = host_ip_with_cidr.split('/').next().unwrap_or(host_ip_with_cidr);
    let parts: Vec<&str> = host_ip.split('.').collect();
    if parts.len() != 4 {
        anyhow::bail!("invalid host IP format: {}", host_ip);
    }

    let third: u32 = parts[2].parse().unwrap_or(0);
    let fourth: u32 = parts[3].parse().unwrap_or(0);
    // Create unique mark from last two octets (1-65535, avoiding 0)
    let mark = (third << 8) | fourth;
    if mark == 0 {
        anyhow::bail!("mark cannot be 0 for host IP: {}", host_ip);
    }

    // Table number: use mark value (but shifted to avoid system tables 0-255)
    let table = 100 + (mark % 65000);

    info!(veth = %veth_name, mark = mark, table = table, guest_subnet = %guest_subnet, "setting up CONNMARK routing");

    // Step 1: Ensure global CONNMARK restore rule exists
    ensure_connmark_restore_rule().await?;

    // Step 2: Add rule to mark packets from this veth and save to CONNMARK
    // Only mark NEW connections (ESTABLISHED/RELATED are handled by restore rule)
    let mark_str = mark.to_string();
    let output = Command::new("sudo")
        .args([
            "iptables", "-t", "mangle", "-C", "PREROUTING",
            "-i", veth_name,
            "-m", "conntrack", "--ctstate", "NEW",
            "-j", "MARK", "--set-mark", &mark_str,
        ])
        .output()
        .await
        .context("checking MARK rule")?;

    if !output.status.success() {
        // Add the rule
        let output = Command::new("sudo")
            .args([
                "iptables", "-t", "mangle", "-A", "PREROUTING",
                "-i", veth_name,
                "-m", "conntrack", "--ctstate", "NEW",
                "-j", "MARK", "--set-mark", &mark_str,
            ])
            .output()
            .await
            .context("adding MARK rule")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("failed to add MARK rule for {}: {}", veth_name, stderr);
        }
    }

    // Step 3: Add rule to save mark to CONNMARK (after setting it)
    let output = Command::new("sudo")
        .args([
            "iptables", "-t", "mangle", "-C", "PREROUTING",
            "-i", veth_name,
            "-m", "mark", "--mark", &mark_str,
            "-j", "CONNMARK", "--save-mark",
        ])
        .output()
        .await
        .context("checking CONNMARK save rule")?;

    if !output.status.success() {
        let output = Command::new("sudo")
            .args([
                "iptables", "-t", "mangle", "-A", "PREROUTING",
                "-i", veth_name,
                "-m", "mark", "--mark", &mark_str,
                "-j", "CONNMARK", "--save-mark",
            ])
            .output()
            .await
            .context("adding CONNMARK save rule")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("failed to add CONNMARK save rule for {}: {}", veth_name, stderr);
        }
    }

    // Step 4: Add policy routing rule (ip rule)
    let table_str = table.to_string();
    let output = Command::new("sudo")
        .args(["ip", "rule", "add", "fwmark", &mark_str, "table", &table_str])
        .output()
        .await
        .context("adding ip rule")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // Ignore "File exists" - rule already exists
        if !stderr.contains("File exists") && !stderr.contains("RTNETLINK answers: File exists") {
            anyhow::bail!("failed to add ip rule for mark {}: {}", mark, stderr);
        }
    }

    // Step 5: Add route for guest subnet in the dedicated table
    let output = Command::new("sudo")
        .args(["ip", "route", "replace", guest_subnet, "dev", veth_name, "table", &table_str])
        .output()
        .await
        .context("adding route to table")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "failed to add route {} via {} to table {}: {}",
            guest_subnet,
            veth_name,
            table,
            stderr
        );
    }

    Ok(())
}

/// Cleans up CONNMARK routing rules for a veth
///
/// Removes the iptables rules, ip rule, and routing table entries for a clone.
pub async fn cleanup_connmark_routing(veth_name: &str, host_ip: &str) -> Result<()> {
    // Extract mark from host IP
    let parts: Vec<&str> = host_ip.split('.').collect();
    if parts.len() != 4 {
        return Ok(()); // Invalid IP, nothing to clean
    }

    let third: u32 = parts[2].parse().unwrap_or(0);
    let fourth: u32 = parts[3].parse().unwrap_or(0);
    let mark = (third << 8) | fourth;
    if mark == 0 {
        return Ok(());
    }

    let mark_str = mark.to_string();
    let table = 100 + (mark % 65000);
    let table_str = table.to_string();

    // Remove MARK rule
    let _ = Command::new("sudo")
        .args([
            "iptables", "-t", "mangle", "-D", "PREROUTING",
            "-i", veth_name,
            "-m", "conntrack", "--ctstate", "NEW",
            "-j", "MARK", "--set-mark", &mark_str,
        ])
        .output()
        .await;

    // Remove CONNMARK save rule
    let _ = Command::new("sudo")
        .args([
            "iptables", "-t", "mangle", "-D", "PREROUTING",
            "-i", veth_name,
            "-m", "mark", "--mark", &mark_str,
            "-j", "CONNMARK", "--save-mark",
        ])
        .output()
        .await;

    // Remove ip rule
    let _ = Command::new("sudo")
        .args(["ip", "rule", "del", "fwmark", &mark_str, "table", &table_str])
        .output()
        .await;

    // Flush the routing table
    let _ = Command::new("sudo")
        .args(["ip", "route", "flush", "table", &table_str])
        .output()
        .await;

    Ok(())
}

/// Configures the host side of a veth pair
///
/// Sets up the host-side veth interface with an IP address and brings it up.
/// Includes proactive cleanup of stale veths with conflicting IPs.
///
/// For clones, `secondary_ip` should be the guest's expected gateway IP from the
/// snapshot. This allows the veth to respond to ARP requests from the guest.
pub async fn setup_host_veth(veth_name: &str, ip_with_cidr: &str) -> Result<()> {
    setup_host_veth_with_gateway(veth_name, ip_with_cidr, None).await
}

/// Configures the host side of a veth pair with an optional secondary IP
///
/// The secondary IP is used for clones where the guest expects a different gateway
/// than the veth's primary IP. The veth will respond to ARP for both IPs.
///
/// For clones, also sets up CONNMARK-based routing so multiple clones with the
/// same guest IP can all have working egress. Each clone gets a unique mark and
/// routing table based on the last octet of its host_ip.
pub async fn setup_host_veth_with_gateway(
    veth_name: &str,
    ip_with_cidr: &str,
    guest_gateway_ip: Option<&str>,
) -> Result<()> {
    info!(veth = %veth_name, ip = %ip_with_cidr, gateway = ?guest_gateway_ip, "configuring host veth");

    // Proactive cleanup: remove any stale veth with the same IP
    // This handles cases where a previous fcvm was killed with SIGKILL
    cleanup_stale_veth_with_ip(ip_with_cidr, veth_name).await?;

    // Bring up the interface
    let output = Command::new("sudo")
        .args(["ip", "link", "set", veth_name, "up"])
        .output()
        .await
        .context("bringing up host veth")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("failed to bring up host veth {}: {}", veth_name, stderr);
    }

    // Assign primary IP address
    let output = Command::new("sudo")
        .args(["ip", "addr", "add", ip_with_cidr, "dev", veth_name])
        .output()
        .await
        .context("assigning IP to host veth")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // Ignore "File exists" - IP already assigned
        if !stderr.contains("File exists") {
            anyhow::bail!("failed to assign IP to host veth {}: {}", veth_name, stderr);
        }
    }

    // For clones: Add the guest's expected gateway IP as secondary address
    // This allows the veth to respond to ARP requests from the guest OS,
    // which expects its original gateway IP from the snapshot.
    if let Some(gateway_ip) = guest_gateway_ip {
        // Calculate the gateway IP with /32 CIDR (point-to-point)
        let gateway_with_cidr = format!("{}/32", gateway_ip);

        let output = Command::new("sudo")
            .args(["ip", "addr", "add", &gateway_with_cidr, "dev", veth_name])
            .output()
            .await
            .context("assigning secondary gateway IP to host veth")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            // Ignore "File exists" - IP already assigned
            if !stderr.contains("File exists") {
                anyhow::bail!(
                    "failed to assign gateway IP {} to host veth {}: {}",
                    gateway_ip,
                    veth_name,
                    stderr
                );
            }
        }

        // Also add a route for the guest's original /30 subnet through this veth
        // This ensures return traffic from the host reaches the correct clone.
        // Use "replace" instead of "add" to override any existing conflicting route
        // (e.g., if a baseline VM or previous clone had the same guest IP).
        // Parse gateway IP to calculate the subnet base
        let parts: Vec<&str> = gateway_ip.split('.').collect();
        if parts.len() == 4 {
            let third: u8 = parts[2].parse().unwrap_or(0);
            let fourth: u8 = parts[3].parse().unwrap_or(0);
            // In a /30, the gateway is .1 in the subnet, so subnet base is gateway - 1
            let subnet_base = fourth.saturating_sub(1);
            let guest_subnet = format!("172.30.{}.{}/30", third, subnet_base);

            // Use "replace" to override any existing route (from baseline or previous clone)
            let output = Command::new("sudo")
                .args(["ip", "route", "replace", &guest_subnet, "dev", veth_name])
                .output()
                .await
                .context("replacing route for guest subnet")?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                anyhow::bail!(
                    "failed to replace route for {} via {}: {}",
                    guest_subnet,
                    veth_name,
                    stderr
                );
            }
            // Set up CONNMARK-based routing for this clone
            // This allows multiple clones with the same guest IP to all have working egress
            setup_connmark_routing(veth_name, ip_with_cidr, &guest_subnet).await?;

            info!(veth = %veth_name, gateway = %gateway_ip, subnet = %guest_subnet, "added secondary gateway IP, route, and CONNMARK routing for clone");
        } else {
            info!(veth = %veth_name, gateway = %gateway_ip, "added secondary gateway IP for clone");
        }
    }

    // Add FORWARD rule to allow outbound traffic from this veth
    let forward_rule = format!("-A FORWARD -i {} -j ACCEPT", veth_name);
    let output = Command::new("sudo")
        .args(["iptables", "-t", "filter", "-C", "FORWARD"]) // Include chain name for -C check
        .args(forward_rule.split_whitespace().skip(2)) // Skip "-A FORWARD"
        .output()
        .await
        .context("checking FORWARD rule")?;

    if !output.status.success() {
        // Rule doesn't exist, add it
        let output = Command::new("sudo")
            .args(["iptables", "-t", "filter"])
            .args(forward_rule.split_whitespace())
            .output()
            .await
            .context("adding FORWARD rule for veth")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("failed to add FORWARD rule for {}: {}", veth_name, stderr);
        }
        info!(veth = %veth_name, "added FORWARD rule for outbound traffic");
    }

    Ok(())
}

/// Configures the guest side of a veth pair inside a namespace
///
/// Brings up the veth interface and loopback inside the namespace.
/// Note: Neither veth nor bridge get an IP - they are pure L2 devices.
/// The guest VM has the IP and routing happens inside the VM.
pub async fn setup_guest_veth_in_ns(ns_name: &str, veth_name: &str) -> Result<()> {
    info!(
        namespace = %ns_name,
        veth = %veth_name,
        "configuring guest veth in namespace"
    );

    // Bring up loopback interface
    let output = exec_in_namespace(ns_name, &["ip", "link", "set", "lo", "up"]).await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        warn!(
            "failed to bring up loopback (may already be up): {}",
            stderr
        );
    }

    // Bring up veth interface
    let output = exec_in_namespace(ns_name, &["ip", "link", "set", veth_name, "up"]).await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "failed to bring up guest veth {} in namespace: {}",
            veth_name,
            stderr
        );
    }

    // NOTE: Neither veth nor bridge get an IP - they are pure L2 devices.
    // No default route needed in namespace - routing happens in the guest VM.
    // The namespace only does L2 bridging between TAP and veth.

    Ok(())
}

/// Creates a TAP device inside a namespace
///
/// Creates a TAP device that Firecracker will use. The TAP is created inside
/// the VM's namespace so it's isolated from other VMs.
pub async fn create_tap_in_ns(ns_name: &str, tap_name: &str) -> Result<()> {
    info!(
        namespace = %ns_name,
        tap = %tap_name,
        "creating TAP device in namespace"
    );

    // Create TAP device
    let output =
        exec_in_namespace(ns_name, &["ip", "tuntap", "add", tap_name, "mode", "tap"]).await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "failed to create TAP device {} in namespace: {}",
            tap_name,
            stderr
        );
    }

    // Bring up TAP device
    let output = exec_in_namespace(ns_name, &["ip", "link", "set", tap_name, "up"]).await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "failed to bring up TAP device {} in namespace: {}",
            tap_name,
            stderr
        );
    }

    Ok(())
}

/// Connects TAP device to veth interface via bridge inside namespace
///
/// Creates a Linux bridge to connect the TAP device (used by Firecracker) to the
/// veth device (connected to host). This allows L2 forwarding between VM and host.
///
/// IMPORTANT: The bridge is a pure L2 device with NO IP address. If we assign
/// the guest IP to the bridge, it will respond to ARP requests instead of
/// forwarding them to the VM, breaking connectivity.
pub async fn connect_tap_to_veth(ns_name: &str, tap_name: &str, veth_name: &str) -> Result<()> {
    info!(
        namespace = %ns_name,
        tap = %tap_name,
        veth = %veth_name,
        "connecting TAP to veth via bridge in namespace"
    );

    let bridge_name = "br0";

    // Create bridge
    let output = exec_in_namespace(
        ns_name,
        &["ip", "link", "add", bridge_name, "type", "bridge"],
    )
    .await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // Ignore if bridge already exists
        if !stderr.contains("File exists") {
            anyhow::bail!("failed to create bridge in namespace: {}", stderr);
        }
    }

    // NOTE: Bridge has NO IP address - it's a pure L2 device.
    // The guest VM has the IP, and packets are forwarded via the bridge.

    // Attach TAP to bridge
    let output = exec_in_namespace(
        ns_name,
        &["ip", "link", "set", tap_name, "master", bridge_name],
    )
    .await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("failed to attach TAP to bridge: {}", stderr);
    }

    // Attach veth to bridge
    let output = exec_in_namespace(
        ns_name,
        &["ip", "link", "set", veth_name, "master", bridge_name],
    )
    .await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("failed to attach veth to bridge: {}", stderr);
    }

    // Bring up bridge
    let output = exec_in_namespace(ns_name, &["ip", "link", "set", bridge_name, "up"]).await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("failed to bring up bridge: {}", stderr);
    }

    info!(bridge = %bridge_name, "bridge created and configured in namespace");

    Ok(())
}

/// Deletes a veth pair
///
/// Deleting the host side automatically removes the peer (if it still exists).
/// If the peer was moved to a namespace that was deleted, this still works.
pub async fn delete_veth_pair(host_veth: &str) -> Result<()> {
    info!(veth = %host_veth, "deleting veth pair");

    let output = Command::new("sudo")
        .args(["ip", "link", "del", host_veth])
        .output()
        .await
        .context("deleting veth pair")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // Ignore "Cannot find device" - already deleted
        if stderr.contains("Cannot find device") {
            warn!(veth = %host_veth, "veth already deleted");
            return Ok(());
        }
        anyhow::bail!("failed to delete veth {}: {}", host_veth, stderr);
    }

    Ok(())
}

/// Deletes the FORWARD rule for a veth interface
///
/// This removes the iptables FORWARD rule that allows outbound traffic from the veth interface.
/// Should be called before deleting the veth pair to avoid accumulating orphaned rules.
pub async fn delete_veth_forward_rule(veth_name: &str) -> Result<()> {
    info!(veth = %veth_name, "deleting FORWARD rule");

    let forward_rule = format!("-D FORWARD -i {} -j ACCEPT", veth_name);
    let output = Command::new("sudo")
        .args(["iptables", "-t", "filter"])
        .args(forward_rule.split_whitespace())
        .output()
        .await
        .context("deleting FORWARD rule")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // Ignore "No chain/target/match" or "does not exist" - rule already gone
        if stderr.contains("No chain")
            || stderr.contains("does not exist")
            || stderr.contains("Bad rule")
        {
            warn!(veth = %veth_name, "FORWARD rule already deleted or never existed");
            return Ok(());
        }
        anyhow::bail!(
            "failed to delete FORWARD rule for {}: {}",
            veth_name,
            stderr
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::network::namespace::{create_namespace, delete_namespace};

    #[tokio::test]
    async fn test_veth_lifecycle() {
        if unsafe { libc::geteuid() } != 0 {
            eprintln!("Skipping test_veth_lifecycle - requires root");
            return;
        }

        let ns_name = "fcvm-test-veth";
        let host_veth = "veth-host-test";
        let guest_veth = "veth-ns-test";

        // Cleanup from previous runs
        let _ = delete_veth_pair(host_veth).await;
        let _ = delete_namespace(ns_name).await;

        // Create namespace
        create_namespace(ns_name).await.unwrap();

        // Create veth pair
        create_veth_pair(host_veth, guest_veth, ns_name)
            .await
            .unwrap();

        // Setup host side
        setup_host_veth(host_veth, "172.30.0.1/30").await.unwrap();

        // Setup guest side
        setup_guest_veth_in_ns(ns_name, guest_veth).await.unwrap();

        // Verify host veth exists
        let output = Command::new("ip")
            .args(["link", "show", host_veth])
            .output()
            .await
            .unwrap();
        assert!(output.status.success());

        // Verify guest veth exists in namespace
        let output = exec_in_namespace(ns_name, &["ip", "link", "show", guest_veth])
            .await
            .unwrap();
        assert!(output.status.success());

        // Cleanup
        delete_veth_pair(host_veth).await.unwrap();
        delete_namespace(ns_name).await.unwrap();
    }

    #[tokio::test]
    async fn test_tap_creation() {
        if unsafe { libc::geteuid() } != 0 {
            eprintln!("Skipping test_tap_creation - requires root");
            return;
        }

        let ns_name = "fcvm-test-tap";
        let tap_name = "tap-test";

        // Cleanup from previous runs
        let _ = delete_namespace(ns_name).await;

        // Create namespace
        create_namespace(ns_name).await.unwrap();

        // Create TAP device
        create_tap_in_ns(ns_name, tap_name).await.unwrap();

        // Verify TAP exists in namespace
        let output = exec_in_namespace(ns_name, &["ip", "link", "show", tap_name])
            .await
            .unwrap();
        assert!(output.status.success());
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains(tap_name));

        // Cleanup
        delete_namespace(ns_name).await.unwrap();
    }
}
