use anyhow::{Context, Result};
use tokio::process::Command;
use tracing::{debug, info, warn};

use super::namespace::exec_in_namespace;

/// In-Namespace NAT configuration for clone egress
///
/// When clones are restored from a snapshot, they all have the same guest IP
/// (e.g., 172.30.90.2). With In-Namespace NAT, each clone's namespace:
/// 1. Has br0 assigned the guest's expected gateway IP (172.30.90.1)
/// 2. Has veth1 assigned a unique IP in the 10.x.y.0/30 range
/// 3. NATs outgoing traffic to the veth1 IP via MASQUERADE
/// 4. DNATs incoming traffic from veth IP to guest IP (for health checks)
///
/// This eliminates the need for CONNMARK/policy routing in the host namespace.
pub struct InNamespaceNatConfig {
    /// Gateway IP to assign to br0 (e.g., 172.30.90.1)
    pub gateway_ip: String,
    /// Guest IP inside the VM (e.g., 172.30.90.2)
    pub guest_ip: String,
    /// Unique veth IP inside namespace (e.g., 10.0.1.2/30)
    pub veth_ip_cidr: String,
    /// Corresponding host-side veth IP (e.g., 10.0.1.1/30)
    pub host_veth_ip_cidr: String,
}

/// Whether an interface of this name exists in the calling namespace.
///
/// `/sys/class/net` is per network namespace, so this answers for the
/// namespace the caller is in. Both ends of a veth pair are created in the
/// host namespace before the guest end is moved, so both names must be free
/// here at creation time.
pub fn link_exists(if_name: &str) -> bool {
    std::path::Path::new("/sys/class/net")
        .join(if_name)
        .exists()
}

/// Creates a veth pair and moves guest side into a namespace
///
/// Creates a pair of virtual ethernet devices. The host side remains in the
/// root namespace, while the guest side is moved into the VM's namespace.
pub async fn create_veth_pair(host_veth: &str, guest_veth: &str, ns_name: &str) -> Result<()> {
    debug!(
        host = %host_veth,
        guest = %guest_veth,
        namespace = %ns_name,
        "creating veth pair"
    );

    // Create veth pair in root namespace
    let output = Command::new("ip")
        .args([
            "link", "add", host_veth, "type", "veth", "peer", "name", guest_veth,
        ])
        .output()
        .await
        .context("executing ip link add veth")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("failed to create veth pair: {}", stderr);
    }

    // Move guest side into namespace
    let output = Command::new("ip")
        .args(["link", "set", guest_veth, "netns", ns_name])
        .output()
        .await
        .context("moving veth to namespace")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // Cleanup host veth on failure
        let _ = Command::new("ip")
            .args(["link", "del", host_veth])
            .output()
            .await;
        anyhow::bail!("failed to move veth to namespace: {}", stderr);
    }

    Ok(())
}

/// Check whether an `ip -o addr show` output line shows a fcvm-managed veth
/// (`veth0-*`) carrying exactly `ip`.
///
/// The IP is matched as `inet <ip>/` so that 172.30.0.1 never matches a parallel
/// VM's 172.30.0.13 (or its broadcast address) — a bare substring match here can
/// select another VM's veth and delete it during that VM's setup window.
fn line_has_exact_ip_on_managed_veth(line: &str, ip: &str) -> bool {
    let inet_pattern = format!("inet {}/", ip);
    line.contains(&inet_pattern) && line.contains("veth0-")
}

/// Extract the nexthop ("via") address from `ip route show <prefix>` output.
///
/// Returns None when the route has no gateway (direct/dev route) or the output
/// is empty. Token-exact comparison avoids substring false positives
/// (e.g. 10.0.1.2 vs 10.0.1.20).
fn route_nexthop(route_output: &str) -> Option<&str> {
    let mut tokens = route_output.split_whitespace();
    while let Some(token) = tokens.next() {
        if token == "via" {
            return tokens.next();
        }
    }
    None
}

/// Check if a network namespace has any running processes
async fn namespace_has_processes(ns_name: &str) -> bool {
    // Use ip netns pids to check for processes in the namespace
    let output = Command::new("ip")
        .args(["netns", "pids", ns_name])
        .output()
        .await;

    match output {
        Ok(result) if result.status.success() => {
            // If there are any PIDs output, the namespace is active
            let stdout = String::from_utf8_lossy(&result.stdout);
            !stdout.trim().is_empty()
        }
        _ => {
            // Namespace doesn't exist or command failed - consider it inactive
            false
        }
    }
}

/// Cleans up any stale veth interface with the same IP address
///
/// This is a proactive cleanup for cases where a previous fcvm process was killed
/// with SIGKILL and didn't get a chance to clean up its network resources.
/// Without this, the stale veth's IP would conflict with the new one, causing
/// routing issues (return traffic goes to wrong interface).
///
/// IMPORTANT: Only cleans up veths whose associated namespace has no running processes.
/// This prevents race conditions when multiple VMs from the same cache entry run in parallel.
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
    // Format: "4: veth0-vm-abc@if3: <...> inet 10.0.1.1/30 ..."
    for line in stdout.lines() {
        if line_has_exact_ip_on_managed_veth(line, ip) {
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

                // Extract vm_id suffix from veth name (veth0-vm-XXXXX -> vm-XXXXX)
                // and construct the namespace name (fcvm-vm-XXXXX)
                let ns_name = if let Some(suffix) = iface_name.strip_prefix("veth0-") {
                    format!("fcvm-{}", suffix)
                } else {
                    continue; // Not a veth we manage
                };

                // Check if the namespace still has running processes
                // If so, this veth belongs to an active VM - DON'T delete it!
                if namespace_has_processes(&ns_name).await {
                    debug!(
                        veth = %iface_name,
                        namespace = %ns_name,
                        ip = %ip,
                        "skipping veth cleanup - namespace has running processes (concurrent VM)"
                    );
                    continue;
                }

                warn!(
                    stale_veth = %iface_name,
                    ip = %ip,
                    "cleaning up stale veth with conflicting IP"
                );

                // Delete the FORWARD rule
                let _ = delete_veth_forward_rule(iface_name).await;

                // Delete the stale veth
                let _ = delete_veth_pair(iface_name).await;
            }
        }
    }

    Ok(())
}

/// Configures the host side of a veth pair
///
/// Sets up the host-side veth interface with an IP address and brings it up.
/// Includes proactive cleanup of stale veths with conflicting IPs.
pub async fn setup_host_veth(veth_name: &str, ip_with_cidr: &str) -> Result<()> {
    debug!(veth = %veth_name, ip = %ip_with_cidr, "configuring host veth");

    // Proactive cleanup: remove any stale veth with the same IP
    // This handles cases where a previous fcvm was killed with SIGKILL
    cleanup_stale_veth_with_ip(ip_with_cidr, veth_name).await?;

    // Bring up the interface
    let output = Command::new("ip")
        .args(["link", "set", veth_name, "up"])
        .output()
        .await
        .context("bringing up host veth")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("failed to bring up host veth {}: {}", veth_name, stderr);
    }

    // Assign primary IP address
    let output = Command::new("ip")
        .args(["addr", "add", ip_with_cidr, "dev", veth_name])
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

    // DNS: VMs now use host DNS servers directly (read from /etc/resolv.conf)
    // No dnsmasq needed - the host DNS servers are reachable via the veth bridge

    // Add FORWARD rule to allow outbound traffic from this veth
    let forward_rule = format!("-A FORWARD -i {} -j ACCEPT", veth_name);
    let output = Command::new("iptables")
        .args(["-t", "filter", "-C", "FORWARD"]) // Include chain name for -C check
        .args(forward_rule.split_whitespace().skip(2)) // Skip "-A FORWARD"
        .output()
        .await
        .context("checking FORWARD rule")?;

    if !output.status.success() {
        // Rule doesn't exist, add it
        let output = Command::new("iptables")
            .args(["-t", "filter"])
            .args(forward_rule.split_whitespace())
            .output()
            .await
            .context("adding FORWARD rule for veth")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("failed to add FORWARD rule for {}: {}", veth_name, stderr);
        }
        debug!(veth = %veth_name, "added FORWARD rule for outbound traffic");
    }

    // Enable per-interface forwarding on this veth
    // (required even when global ip_forward=1, as per-interface default may be 0)
    let iface_forwarding = format!("net.ipv4.conf.{}.forwarding=1", veth_name);
    let output = Command::new("sysctl")
        .args(["-w", &iface_forwarding])
        .output()
        .await
        .with_context(|| format!("enabling forwarding on {}", veth_name))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        warn!(veth = %veth_name, "failed to enable per-interface forwarding: {}", stderr);
    }

    Ok(())
}

/// Configures the guest side of a veth pair inside a namespace
///
/// Brings up the veth interface and loopback inside the namespace.
/// Note: Neither veth nor bridge get an IP - they are pure L2 devices.
/// The guest VM has the IP and routing happens inside the VM.
pub async fn setup_guest_veth_in_ns(ns_name: &str, veth_name: &str) -> Result<()> {
    debug!(
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
    debug!(
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
    debug!(
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

    debug!(bridge = %bridge_name, "bridge created and configured in namespace");

    Ok(())
}

/// Sets up In-Namespace NAT for clone egress
///
/// This is an alternative to `connect_tap_to_veth` for clones. Instead of bridging
/// TAP directly to veth (L2), this creates a routed setup with NAT:
///
/// Architecture:
/// ```text
/// TAP → br0 (gateway_ip, e.g. 172.30.90.1)  ← Guest sends here
///                    ↓ routing + NAT
/// veth1 (10.x.y.2) → veth0 (host, 10.x.y.1) ← Unique IP per clone
/// ```
///
/// The guest VM has IP 172.30.90.2 and gateway 172.30.90.1. When it sends traffic:
/// 1. TAP forwards to br0 (L2 bridge with only TAP attached)
/// 2. br0 has the gateway IP, so it accepts the packet
/// 3. Namespace routes the packet out via veth1
/// 4. MASQUERADE changes source IP from 172.30.90.2 to 10.x.y.2
/// 5. Packet reaches host with unique source IP (no CONNMARK needed!)
///
/// Returns on success, or error if setup fails.
pub async fn setup_in_namespace_nat(
    ns_name: &str,
    tap_name: &str,
    veth_name: &str,
    config: &InNamespaceNatConfig,
) -> Result<()> {
    info!(
        namespace = %ns_name,
        tap = %tap_name,
        veth = %veth_name,
        gateway_ip = %config.gateway_ip,
        veth_ip = %config.veth_ip_cidr,
        "setting up in-namespace NAT for clone"
    );

    let bridge_name = "br0";

    // Step 1: Create bridge and attach ONLY the TAP (not veth!)
    let output = exec_in_namespace(
        ns_name,
        &["ip", "link", "add", bridge_name, "type", "bridge"],
    )
    .await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !stderr.contains("File exists") {
            anyhow::bail!("failed to create bridge in namespace: {}", stderr);
        }
    }

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

    // Step 2: Assign gateway IP to bridge (e.g., 172.30.90.1/30)
    // This makes br0 act as the gateway for the guest VM
    let gateway_cidr = format!("{}/30", config.gateway_ip);
    let output = exec_in_namespace(
        ns_name,
        &["ip", "addr", "add", &gateway_cidr, "dev", bridge_name],
    )
    .await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !stderr.contains("File exists") {
            anyhow::bail!("failed to assign gateway IP to bridge: {}", stderr);
        }
    }

    // Step 3: Bring up bridge
    let output = exec_in_namespace(ns_name, &["ip", "link", "set", bridge_name, "up"]).await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("failed to bring up bridge: {}", stderr);
    }

    // Step 4: Configure veth inside namespace (NOT attached to bridge!)
    // This is the egress path with unique IP per clone
    let output = exec_in_namespace(ns_name, &["ip", "link", "set", veth_name, "up"]).await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("failed to bring up veth in namespace: {}", stderr);
    }

    // Assign unique IP to veth inside namespace (e.g., 10.0.1.2/30)
    let output = exec_in_namespace(
        ns_name,
        &["ip", "addr", "add", &config.veth_ip_cidr, "dev", veth_name],
    )
    .await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !stderr.contains("File exists") {
            anyhow::bail!("failed to assign IP to veth in namespace: {}", stderr);
        }
    }

    // Step 5: Add default route via veth peer (host side)
    // Extract host IP from host_veth_ip_cidr (e.g., "10.0.1.1/30" -> "10.0.1.1")
    let host_ip = config
        .host_veth_ip_cidr
        .split('/')
        .next()
        .unwrap_or(&config.host_veth_ip_cidr);
    let output = exec_in_namespace(
        ns_name,
        &[
            "ip", "route", "add", "default", "via", host_ip, "dev", veth_name,
        ],
    )
    .await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !stderr.contains("File exists") {
            anyhow::bail!("failed to add default route in namespace: {}", stderr);
        }
    }

    // Step 6: Enable IP forwarding inside namespace
    // Must enable both global and per-interface forwarding.
    // The host's net.ipv4.conf.default.forwarding=0 means new interfaces
    // inherit forwarding=0 even when ip_forward=1.
    let output = exec_in_namespace(ns_name, &["sysctl", "-w", "net.ipv4.ip_forward=1"]).await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("failed to enable IP forwarding in namespace: {}", stderr);
    }

    // Enable forwarding on all interfaces (catches any we might miss)
    let output =
        exec_in_namespace(ns_name, &["sysctl", "-w", "net.ipv4.conf.all.forwarding=1"]).await?;
    if !output.status.success() {
        warn!(
            "failed to enable conf.all.forwarding: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    // Enable forwarding on the bridge (where TAP is attached)
    let output = exec_in_namespace(
        ns_name,
        &[
            "sysctl",
            "-w",
            &format!("net.ipv4.conf.{}.forwarding=1", bridge_name),
        ],
    )
    .await?;
    if !output.status.success() {
        warn!(
            "failed to enable forwarding on {}: {}",
            bridge_name,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    // Enable forwarding on the veth (egress interface)
    let output = exec_in_namespace(
        ns_name,
        &[
            "sysctl",
            "-w",
            &format!("net.ipv4.conf.{}.forwarding=1", veth_name),
        ],
    )
    .await?;
    if !output.status.success() {
        warn!(
            "failed to enable forwarding on {}: {}",
            veth_name,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    // Step 7: Add MASQUERADE rule for NAT on veth outgoing
    // This changes source IP from guest IP (172.30.90.2) to veth IP (10.x.y.2)
    // IMPORTANT: Only MASQUERADE traffic going to the INTERNET, not to the local host.
    // Traffic to host_ip (10.x.y.1) is direct access responses - don't masquerade those.
    // Without this exclusion, direct guest IP access wouldn't work because the response
    // would have src=veth_ip instead of src=guest_ip.
    let output = exec_in_namespace(
        ns_name,
        &[
            "iptables",
            "-t",
            "nat",
            "-A",
            "POSTROUTING",
            "-s",
            &config.guest_ip,
            "!",
            "-d",
            host_ip,
            "-o",
            veth_name,
            "-j",
            "MASQUERADE",
        ],
    )
    .await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("failed to add MASQUERADE rule in namespace: {}", stderr);
    }

    // Step 8: Add DNAT rules for incoming traffic to reach the guest
    // This allows health checks from host to reach the guest via the veth IP
    // Host connects to veth_ip:80 → DNAT to guest_ip:80
    // We add rules for both TCP and UDP so port forwarding works for all protocols
    let veth_ip = config
        .veth_ip_cidr
        .split('/')
        .next()
        .unwrap_or(&config.veth_ip_cidr);
    for proto in ["tcp", "udp"] {
        let output = exec_in_namespace(
            ns_name,
            &[
                "iptables",
                "-t",
                "nat",
                "-A",
                "PREROUTING",
                "-d",
                veth_ip,
                "-p",
                proto,
                "-j",
                "DNAT",
                "--to-destination",
                &config.guest_ip,
            ],
        )
        .await?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("failed to add {} DNAT rule in namespace: {}", proto, stderr);
        }
    }

    // Step 9: Give the guest the same gateway-is-the-host contact surface a
    // baseline VM has. In a baseline namespace the gateway IP lives on the
    // host side of the veth (init netns), so guest→gateway:PORT reaches host
    // services — NFS exports in particular. In a clone namespace br0 owns
    // that IP, so such traffic would terminate here with nothing listening.
    // DNAT it to the host's veth IP, and MASQUERADE those flows so replies
    // route back over this clone's own /30. (Clones of one snapshot share
    // the guest IP; without the masquerade, replies would depend on the
    // shared guest-IP /32 host route, which only one clone can hold.)
    for proto in ["tcp", "udp"] {
        let output = exec_in_namespace(
            ns_name,
            &[
                "iptables",
                "-t",
                "nat",
                "-A",
                "PREROUTING",
                "-s",
                &config.guest_ip,
                "-d",
                &config.gateway_ip,
                "-p",
                proto,
                "-j",
                "DNAT",
                "--to-destination",
                host_ip,
            ],
        )
        .await?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!(
                "failed to add {} gateway DNAT rule in namespace: {}",
                proto,
                stderr
            );
        }
    }
    let output = exec_in_namespace(
        ns_name,
        &[
            "iptables",
            "-t",
            "nat",
            "-A",
            "POSTROUTING",
            "-s",
            &config.guest_ip,
            "-d",
            host_ip,
            "-o",
            veth_name,
            "-j",
            "MASQUERADE",
        ],
    )
    .await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "failed to add gateway MASQUERADE rule in namespace: {}",
            stderr
        );
    }

    info!(
        namespace = %ns_name,
        bridge = %bridge_name,
        gateway = %config.gateway_ip,
        guest_ip = %config.guest_ip,
        veth = %veth_name,
        veth_ip = %config.veth_ip_cidr,
        "in-namespace NAT configured successfully"
    );

    Ok(())
}

/// Adds a host route to reach the guest IP via the namespace's veth IP
///
/// For clones using in-namespace NAT, the guest IP (e.g., 172.30.90.2) is not
/// directly reachable from the host because it's behind the namespace's bridge.
/// We add a route that uses the namespace veth IP as the nexthop:
///   ip route add 172.30.90.2/32 via 10.x.y.2
///
/// This works because:
/// - Host ARPs for 10.x.y.2 (namespace veth IP) which IS on the same L2 segment
/// - Packet is delivered to namespace with dst=172.30.90.2
/// - Namespace routes it to br0 → TAP → guest
///
/// Using just "dev veth0" doesn't work because the host would ARP for 172.30.90.2
/// directly, but only devices on the veth L2 segment can respond, and the guest
/// is on a different L2 segment (br0/TAP).
pub async fn add_host_route_to_guest(
    host_veth: &str,
    guest_ip: &str,
    veth_inner_ip: &str,
) -> Result<()> {
    debug!(
        veth = %host_veth,
        guest_ip = %guest_ip,
        via = %veth_inner_ip,
        "adding host route to guest IP via namespace veth"
    );

    // Check for existing route and clean up if it's stale (points to dead veth)
    let route = format!("{}/32", guest_ip);
    let check_output = Command::new("ip")
        .args(["route", "show", &route])
        .output()
        .await
        .context("checking existing route")?;

    if check_output.status.success() {
        let existing_route = String::from_utf8_lossy(&check_output.stdout);
        if !existing_route.trim().is_empty() {
            // Route exists - check if it points to our veth or a different one
            // (token-exact via match: 10.0.1.2 must not "match" a route via 10.0.1.20)
            if route_nexthop(&existing_route) != Some(veth_inner_ip) {
                // Route points to a different gateway - likely stale from crashed VM
                // Check if the device in the route still exists
                if let Some(dev_start) = existing_route.find("dev ") {
                    let dev_part = &existing_route[dev_start + 4..];
                    let old_dev = dev_part.split_whitespace().next().unwrap_or("");

                    // Check if the old device exists
                    let dev_check = Command::new("ip")
                        .args(["link", "show", old_dev])
                        .output()
                        .await;

                    let device_exists = dev_check.map(|o| o.status.success()).unwrap_or(false);

                    if !device_exists {
                        // Stale route pointing to dead device - delete it
                        warn!(
                            guest_ip = %guest_ip,
                            old_route = %existing_route.trim(),
                            "removing stale route to dead device"
                        );
                    } else {
                        // Route exists and device is alive - likely a snapshot clone
                        // replacing a base VM or another clone with the same guest IP.
                        // We must replace the route so the new clone is reachable.
                        warn!(
                            guest_ip = %guest_ip,
                            existing_route = %existing_route.trim(),
                            "replacing route to live device with new clone route"
                        );
                    }
                    let _ = Command::new("ip")
                        .args(["route", "del", &route])
                        .output()
                        .await;
                }
            } else {
                // Route already points to the correct gateway
                debug!(guest_ip = %guest_ip, "route already exists with correct gateway");
                return Ok(());
            }
        }
    }

    let output = Command::new("ip")
        .args(["route", "add", &route, "via", veth_inner_ip])
        .output()
        .await
        .context("adding host route to guest IP")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // Ignore "File exists" - race condition, route was added between check and add
        if !stderr.contains("File exists") {
            anyhow::bail!("failed to add host route to {}: {}", guest_ip, stderr);
        }
    }

    info!(
        veth = %host_veth,
        guest_ip = %guest_ip,
        via = %veth_inner_ip,
        "host can now reach guest IP directly"
    );

    Ok(())
}

/// Deletes the host route to a guest IP, but only the route this clone owns
///
/// This removes the route added by `add_host_route_to_guest`. Must be called during
/// cleanup to prevent stale routes from interfering with future VMs using the same guest IP.
///
/// Bridged clones restored from the same snapshot share one guest IP, and
/// `add_host_route_to_guest` deliberately lets the newest clone replace the
/// {guest_ip}/32 route. Deleting unconditionally on cleanup would remove the route
/// out from under a surviving clone, breaking host -> guest access (and HTTP health
/// checks) for it. The delete is therefore qualified with `via {veth_inner_ip}`:
/// it only matches the route that points at this clone's namespace veth, so a route
/// owned by another clone is left in place. The kernel evaluates the qualifier
/// atomically, so this stays correct even if another clone replaces the route
/// concurrently.
pub async fn delete_host_route_to_guest(guest_ip: &str, veth_inner_ip: &str) -> Result<()> {
    debug!(
        guest_ip = %guest_ip,
        via = %veth_inner_ip,
        "deleting host route to guest IP (if owned by this clone)"
    );

    let route = format!("{}/32", guest_ip);
    let output = Command::new("ip")
        .args(["route", "del", &route, "via", veth_inner_ip])
        .output()
        .await
        .context("deleting host route to guest IP")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // Ignore "No such process" - route already deleted, never existed,
        // or is owned by another clone (different nexthop)
        if !stderr.contains("No such process") {
            warn!(guest_ip = %guest_ip, error = %stderr, "failed to delete host route (non-fatal)");
        }
    }

    Ok(())
}

/// Deletes a veth pair
///
/// Deleting the host side automatically removes the peer (if it still exists).
/// If the peer was moved to a namespace that was deleted, this still works.
pub async fn delete_veth_pair(host_veth: &str) -> Result<()> {
    debug!(veth = %host_veth, "deleting veth pair");

    let output = Command::new("ip")
        .args(["link", "del", host_veth])
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
    debug!(veth = %veth_name, "deleting FORWARD rule");

    let forward_rule = format!("-D FORWARD -i {} -j ACCEPT", veth_name);
    let output = Command::new("iptables")
        .args(["-t", "filter"])
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
mod parse_tests {
    use super::*;

    #[test]
    fn exact_ip_on_managed_veth_matches() {
        let line = "4: veth0-vm-abc12345    inet 172.30.0.1/30 brd 172.30.0.3 scope global veth0-vm-abc12345";
        assert!(line_has_exact_ip_on_managed_veth(line, "172.30.0.1"));
    }

    #[test]
    fn prefix_ip_does_not_match_other_vm() {
        // 172.30.0.1 must NOT match a parallel VM's veth carrying 172.30.0.13
        // (substring match here previously deleted the other VM's veth)
        let line = "7: veth0-vm-def67890    inet 172.30.0.13/30 brd 172.30.0.15 scope global veth0-vm-def67890";
        assert!(!line_has_exact_ip_on_managed_veth(line, "172.30.0.1"));
    }

    #[test]
    fn broadcast_address_does_not_match() {
        // The brd field must not trigger a match either
        let line = "7: veth0-vm-def67890    inet 172.30.0.13/30 brd 172.30.0.15 scope global veth0-vm-def67890";
        assert!(!line_has_exact_ip_on_managed_veth(line, "172.30.0.15"));
    }

    #[test]
    fn unmanaged_interface_does_not_match() {
        let line = "2: eth0    inet 172.30.0.1/30 brd 172.30.0.3 scope global eth0";
        assert!(!line_has_exact_ip_on_managed_veth(line, "172.30.0.1"));
    }

    #[test]
    fn route_nexthop_extracts_via() {
        let output = "172.30.90.2 via 10.0.1.2 dev veth0-vm-abc12345 \n";
        assert_eq!(route_nexthop(output), Some("10.0.1.2"));
    }

    #[test]
    fn route_nexthop_none_for_dev_route() {
        let output = "172.30.90.2 dev veth0-vm-abc12345 scope link \n";
        assert_eq!(route_nexthop(output), None);
    }

    #[test]
    fn route_nexthop_none_for_empty_output() {
        assert_eq!(route_nexthop(""), None);
    }

    #[test]
    fn route_nexthop_is_token_exact() {
        // 10.0.1.2 must not be considered the nexthop of a route via 10.0.1.20
        let output = "172.30.90.2 via 10.0.1.20 dev veth0-vm-def67890 \n";
        assert_ne!(route_nexthop(output), Some("10.0.1.2"));
        assert_eq!(route_nexthop(output), Some("10.0.1.20"));
    }
}

#[cfg(test)]
#[cfg(feature = "privileged-tests")]
mod tests {
    use super::*;
    use crate::network::namespace::{
        create_namespace, delete_namespace, exec_in_namespace, NamespaceCreation,
    };

    #[tokio::test]
    async fn test_veth_lifecycle() {
        let ns_name = "fcvm-test-veth";
        let host_veth = "veth-host-test";
        let guest_veth = "veth-ns-test";

        // Cleanup from previous runs
        let _ = delete_veth_pair(host_veth).await;
        let _ = delete_namespace(ns_name).await;

        // Create namespace
        assert_eq!(
            create_namespace(ns_name).await.unwrap(),
            NamespaceCreation::Created
        );

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
        let ns_name = "fcvm-test-tap";
        let tap_name = "tap-test";

        // Cleanup from previous runs
        let _ = delete_namespace(ns_name).await;

        // Create namespace
        assert_eq!(
            create_namespace(ns_name).await.unwrap(),
            NamespaceCreation::Created
        );

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
