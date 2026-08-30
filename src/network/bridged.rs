use anyhow::{Context, Result};
use tracing::{debug, info, warn};

use super::{
    namespace, portmap, types::generate_mac, veth, NetworkConfig, NetworkManager, PortMapping,
};
use crate::state::truncate_id;

/// Derive the host-side IP for a given subnet_id
fn derive_host_ip(subnet_id: u16, is_clone: bool) -> String {
    let third_octet = (subnet_id / 64) as u8;
    let subnet_within_block = (subnet_id % 64) as u8;
    let subnet_base = subnet_within_block * 4;

    if is_clone {
        format!(
            "10.{}.{}.{}",
            third_octet,
            subnet_within_block,
            subnet_base + 1
        )
    } else {
        format!("172.30.{}.{}", third_octet, subnet_base + 1)
    }
}

/// Check if an IP address is already assigned to a veth interface
async fn is_ip_in_use_on_veth(ip: &str) -> bool {
    let output = match tokio::process::Command::new("ip")
        .args(["-o", "addr", "show"])
        .output()
        .await
    {
        Ok(o) if o.status.success() => o,
        _ => return false, // Can't check — assume no collision
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    // Match "inet <ip>/" exactly to avoid substring false positives
    // (e.g., checking 10.1.1.1 should not match 10.1.1.10 or 210.1.1.1)
    let inet_pattern = format!("inet {}/", ip);
    for line in stdout.lines() {
        if line.contains(&inet_pattern) && line.contains("veth0-") {
            return true;
        }
    }
    false
}

/// The /30 peer of a derived host-side veth address (.1 → .2).
fn peer_of(host_ip: &str) -> Option<String> {
    let (prefix, last) = host_ip.rsplit_once('.')?;
    let n: u8 = last.parse().ok()?;
    Some(format!("{}.{}", prefix, n.checked_add(1)?))
}

/// After the candidate /30 is assigned, ask the kernel where the pair's
/// namespace-side address routes NOW. Anything but our veth means a more
/// specific host route claims it and the subnet is unusable (#820): AWS DHCP
/// installs a /32 to the VPC resolver (`10.0.0.2 via <gw> dev <primary>` in a
/// 10.0.0.0/16 VPC), which beats the /30 drawn by subnet_id 0, so that
/// clone's forwarded ports timed out for its whole life — a 1-in-16384
/// silent death. No route-table policy is modeled here: the kernel answers
/// the exact question the data path will ask.
/// Fails closed: an unavailable verdict is an error, never an acceptance. The
/// whole point of the probe is that an unverified candidate costs a clone its
/// entire life on a hung port, and every other step here shells out to the
/// same `ip` binary, so a run that cannot ask this question was never going
/// to finish setup anyway.
async fn kernel_routes_peer_via_veth(veth_name: &str, peer_ip: &str) -> Result<bool> {
    let output = tokio::process::Command::new("ip")
        .args(["route", "get", peer_ip])
        .output()
        .await
        .with_context(|| format!("running `ip route get {peer_ip}`"))?;
    if !output.status.success() {
        anyhow::bail!(
            "`ip route get {peer_ip}` failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(route_get_names_dev(
        &String::from_utf8_lossy(&output.stdout),
        veth_name,
    ))
}

/// Token-exact scan of `ip route get` output for `dev <veth_name>`.
/// Token-exact so veth0-vm-abc12 never matches veth0-vm-abc123 (same trap as
/// veth.rs route_nexthop).
fn route_get_names_dev(route_get_output: &str, veth_name: &str) -> bool {
    let tokens: Vec<&str> = route_get_output.split_whitespace().collect();
    tokens
        .windows(2)
        .any(|w| w[0] == "dev" && w[1] == veth_name)
}

/// Bridged networking using network namespace isolation with veth pairs
///
/// This mode requires sudo/root for network namespace and iptables setup.
/// For true rootless operation (no sudo), use PastaNetwork instead.
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
    /// The resolver the guest is told to use, threaded in by the caller.
    /// Fresh boots pass the launch config's first hashed host_dns entry so
    /// the snapshot key and the guest-visible value cannot diverge; clone
    /// restores pass the resolver captured in the snapshot metadata (#863).
    dns_server: Option<String>,

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
            dns_server: None,
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

    /// Set the resolver setup() reports as the guest's DNS server. setup()
    /// deliberately has no fallback read of the host's resolv.conf: the
    /// caller resolved this value once, alongside whatever hashed or recorded
    /// it, and a second read here could disagree with that copy.
    pub fn with_dns_server(mut self, dns_server: Option<String>) -> Self {
        self.dns_server = dns_server;
        self
    }

    /// Set guest IP to use (for clones - use same IP as original VM)
    pub fn with_guest_ip(mut self, guest_ip: String) -> Self {
        self.guest_ip_override = Some(guest_ip);
        self.is_clone = true;
        self
    }

    /// The clone's in-namespace veth IP (set by setup() for clones only).
    /// Guest→host traffic leaves the namespace masqueraded as this address —
    /// it's the client IP the host's NFS server sees for clone mounts.
    pub fn veth_inner_ip(&self) -> Option<&str> {
        self.veth_inner_ip.as_deref()
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
        let mut subnet_id = (hasher.finish() % 16384) as u16;

        // Serialize subnet selection through host-IP assignment across fcvm processes.
        // is_ip_in_use_on_veth() only sees IPs already assigned to veth interfaces, and
        // the chosen IP is not assigned until setup_host_veth() in step 3 below. Without
        // a cross-process lock, two concurrently starting VMs could both pass the check
        // for the same subnet (check-then-act race) and end up with duplicate host IPs
        // and ambiguous DNAT/return routing. Same pattern as loopback-ip.lock.
        // Lock ordering: this lock may be held while portmap takes bridged-nat.lock
        // (cleanup() on the error paths below); the reverse never happens.
        let subnet_lock = super::acquire_host_network_lock("bridged-subnet.lock")
            .await
            .context("acquiring bridged subnet allocation lock")?;

        // Namespace and veth pair are subnet-independent; they exist before
        // subnet selection so each candidate /30 can be assigned to the real
        // veth and verified against the kernel's actual routing decision.
        let namespace_id = format!("fcvm-{}", truncate_id(&self.vm_id, 8));
        namespace::create_namespace(&namespace_id)
            .await
            .context("creating network namespace")?;
        self.namespace_id = Some(namespace_id.clone());

        let host_veth = format!("veth0-{}", truncate_id(&self.vm_id, 8));
        let guest_veth = format!("veth1-{}", truncate_id(&self.vm_id, 8));
        if let Err(e) = veth::create_veth_pair(&host_veth, &guest_veth, &namespace_id).await {
            let _ = self.cleanup().await;
            return Err(e).context("creating veth pair");
        }
        self.host_veth = Some(host_veth.clone());
        self.guest_veth = Some(guest_veth.clone());

        // Select a subnet: skip candidates whose address is already on a
        // veth (live VM), then assign the /30 and require the kernel to
        // route the pair's namespace-side address through OUR veth. A more
        // specific host route claiming it (#820: AWS DHCP's /32 to the VPC
        // resolver beats the /30 drawn by subnet_id 0) fails the probe and
        // the candidate is stripped and skipped instead of producing a
        // clone whose forwarded ports time out for its whole life.
        let subnet_id = {
            let mut attempts = 0u32;
            loop {
                let host_ip = derive_host_ip(subnet_id, self.is_clone);
                if !is_ip_in_use_on_veth(&host_ip).await {
                    let host_ip_with_cidr = format!("{}/30", host_ip);
                    if let Err(e) = veth::setup_host_veth(&host_veth, &host_ip_with_cidr).await {
                        let _ = self.cleanup().await;
                        return Err(e).context("configuring host veth");
                    }
                    // Derived addresses always end .1 with a .2 peer, so a
                    // candidate the derivation cannot pair is a bug in
                    // derive_host_ip, not a routing verdict — fail rather
                    // than accept it unprobed.
                    let peer = match peer_of(&host_ip) {
                        Some(peer) => peer,
                        None => {
                            let _ = self.cleanup().await;
                            anyhow::bail!("derived host IP {host_ip} has no /30 peer address");
                        }
                    };
                    match kernel_routes_peer_via_veth(&host_veth, &peer).await {
                        Ok(true) => break subnet_id,
                        Ok(false) => {}
                        Err(e) => {
                            let _ = self.cleanup().await;
                            return Err(e).context("verifying the candidate subnet's routing");
                        }
                    }
                    // Strip the losing address before trying the next /30.
                    // Checked, not discarded: a failed delete leaves the
                    // rejected address on the veth, and the next accepted
                    // candidate would return with that routed address still
                    // configured alongside it.
                    let del = tokio::process::Command::new("ip")
                        .args(["addr", "del", &host_ip_with_cidr, "dev", &host_veth])
                        .output()
                        .await;
                    match del {
                        Ok(o) if o.status.success() => {}
                        Ok(o) => {
                            let _ = self.cleanup().await;
                            anyhow::bail!(
                                "removing rejected address {host_ip_with_cidr} from {host_veth} \
                                 failed ({}): {}",
                                o.status,
                                String::from_utf8_lossy(&o.stderr).trim()
                            );
                        }
                        Err(e) => {
                            let _ = self.cleanup().await;
                            return Err(e).context(format!(
                                "removing rejected address {host_ip_with_cidr} from {host_veth}"
                            ));
                        }
                    }
                }
                attempts += 1;
                if attempts >= 100 {
                    let _ = self.cleanup().await;
                    anyhow::bail!(
                        "subnet allocation failed: no free subnet found after {} attempts",
                        attempts
                    );
                }
                warn!(
                    subnet_id = subnet_id,
                    host_ip = %host_ip,
                    attempt = attempts,
                    "subnet collision detected, trying next"
                );
                subnet_id = (subnet_id + 1) % 16384;
            }
        };

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

        // The winning host IP was assigned inside the selection loop, so other
        // processes' collision checks can see it. The remaining steps don't
        // touch subnet allocation state.
        drop(subnet_lock);

        // Step 4: Create TAP device inside namespace
        if let Err(e) = veth::create_tap_in_ns(&namespace_id, &self.tap_device).await {
            let _ = self.cleanup().await;
            return Err(e).context("creating TAP device in namespace");
        }

        // Step 5: Connect TAP to network - different for clones vs baseline
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

        // Step 7: The guest's DNS server, threaded in by the caller (see the
        // dns_server field). Not re-read from the host here.
        let dns_server = self.dns_server.clone();

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

        // Return network config with auto-generated health check URL
        // For clones, use the veth inner IP (which gets DNATed to guest)
        Ok(NetworkConfig {
            tap_device: self.tap_device.clone(),
            guest_mac,
            guest_ip: Some(guest_ip.clone()),
            host_ip: Some(host_ip.clone()),
            host_veth: self.host_veth.clone(),
            loopback_ip: None,
            dns_server,
            guest_ipv6: None, // Bridged mode doesn't support IPv6 yet
            host_ipv6: None,
            dns_search: None,
            http_proxy: None,
            namespace_name: None,
        })
    }

    async fn cleanup(&mut self) -> Result<()> {
        info!(vm_id = %self.vm_id, "cleaning up network namespace and resources");
        let mut errors = Vec::new();

        // Step 1: Cleanup port mapping rules (if any)
        if !self.port_mapping_rules.is_empty() {
            if let Err(e) = portmap::cleanup_port_mappings(&self.port_mapping_rules).await {
                warn!(vm_id = %self.vm_id, error = %e, "failed to cleanup port mappings");
                errors.push(format!("port mappings: {}", e));
            }
        }

        // Step 2: Delete host route to guest IP (for clones).
        // All clones of a snapshot share the same guest IP and the {guest_ip}/32 route
        // points at whichever clone last set it up. Only delete the route this clone
        // owns (via its veth_inner_ip) so a surviving clone keeps host -> guest access.
        if self.is_clone {
            if let (Some(guest_ip), Some(veth_inner_ip)) = (&self.guest_ip, &self.veth_inner_ip) {
                if let Err(e) = veth::delete_host_route_to_guest(guest_ip, veth_inner_ip).await {
                    warn!(vm_id = %self.vm_id, error = %e, "failed to delete host route");
                    errors.push(format!("host route: {}", e));
                }
            }
        }

        // Step 3: Delete FORWARD rule and veth pair
        if let Some(ref host_veth) = self.host_veth {
            if let Err(e) = veth::delete_veth_forward_rule(host_veth).await {
                warn!(vm_id = %self.vm_id, error = %e, "failed to delete forward rule");
                errors.push(format!("forward rule: {}", e));
            }
            if let Err(e) = veth::delete_veth_pair(host_veth).await {
                warn!(vm_id = %self.vm_id, error = %e, "failed to delete veth pair");
                errors.push(format!("veth pair: {}", e));
            }
        }

        // Step 4: Delete network namespace
        if let Some(ref namespace_id) = self.namespace_id {
            if let Err(e) = namespace::delete_namespace(namespace_id).await {
                warn!(vm_id = %self.vm_id, error = %e, "failed to delete namespace");
                errors.push(format!("namespace: {}", e));
            }
        }

        // Step 5: Remove global NAT rules if no other bridged VMs are running
        portmap::cleanup_global_nat_if_unused().await;

        if errors.is_empty() {
            debug!(vm_id = %self.vm_id, "network cleanup complete");
            Ok(())
        } else {
            anyhow::bail!(
                "network cleanup had {} error(s): {}",
                errors.len(),
                errors.join("; ")
            )
        }
    }

    fn tap_device(&self) -> &str {
        &self.tap_device
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The #820 collision as the kernel reports it, verbatim from the box
    /// that hit it: with the veth's 10.0.0.1/30 assigned, `ip route get
    /// 10.0.0.2` still resolves via the VPC gateway because AWS DHCP's /32
    /// host route to the VPC resolver beats the /30. The probe must reject.
    #[test]
    fn dhcp_claimed_peer_resolves_off_veth_and_fails_probe() {
        let out = "10.0.0.2 via 10.0.1.1 dev enP1s33 src 10.0.1.49 uid 0 \n    cache \n";
        assert!(!route_get_names_dev(out, "veth0-vm-7f55a"));
    }

    /// The healthy case: the connected /30 wins and the kernel names our
    /// veth as the device.
    #[test]
    fn unclaimed_peer_resolves_on_veth_and_passes_probe() {
        let out = "10.31.23.94 dev veth0-vm-8fe5a scope link src 10.31.23.93 uid 0 \n    cache \n";
        assert!(route_get_names_dev(out, "veth0-vm-8fe5a"));
    }

    /// Token-exact device match: a longer veth name sharing our prefix is a
    /// different interface (same trap as veth.rs route_nexthop).
    #[test]
    fn dev_match_is_token_exact() {
        let out = "10.31.23.94 dev veth0-vm-8fe5a4 scope link src 10.31.23.93 \n";
        assert!(!route_get_names_dev(out, "veth0-vm-8fe5a"));
    }

    /// The probe must FAIL, not accept, when it cannot get a verdict.
    ///
    /// RED before the fix: kernel_routes_peer_via_veth returned `true` on any
    /// spawn error or nonzero exit, so a candidate nobody could verify was
    /// taken as verified, the same silent acceptance #820 is about. The fake
    /// `ip` here exits 1; nextest runs each test in its own process, but
    /// plain `cargo test` does not, so the PATH mutation goes through the
    /// crate-wide environment lock, which excludes both the other mutators and
    /// the siblings that spawn the real `ip` by name. Prepending is deliberate:
    /// the rest of PATH stays intact, so a sibling spawning any other program
    /// by name is unaffected even inside the window.
    #[tokio::test]
    async fn an_unavailable_route_verdict_is_an_error_not_an_acceptance() {
        let mut env = crate::test_env::lock_process_env_async().await;
        let dir = std::env::temp_dir().join(format!("fcvm-fakeip-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let fake = dir.join("ip");
        std::fs::write(
            &fake,
            "#!/bin/sh\necho 'RTNETLINK answers: Network is unreachable' >&2\nexit 1\n",
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let prev = std::env::var("PATH").unwrap_or_default();
        env.set("PATH", format!("{}:{}", dir.display(), prev));

        let verdict = kernel_routes_peer_via_veth("veth0-vm-abc12", "10.0.0.2").await;

        drop(env);
        let _ = std::fs::remove_dir_all(&dir);
        let err = verdict.expect_err(
            "an `ip route get` that cannot answer must be an error; returning true \
             accepts an unverified subnet, which is the #820 failure itself",
        );
        assert!(
            format!("{err:#}").contains("ip route get"),
            "the error must name the probe that failed: {err:#}"
        );
    }

    /// #863: the bridged guest's resolver is threaded in from the launch
    /// config, the same value the snapshot key hashed. setup() must not read
    /// the host's resolv.conf itself: a mid-launch change would save the
    /// snapshot under one resolver's key while the guest boots with another.
    #[test]
    fn setup_does_not_reread_host_dns() {
        let src = include_str!("bridged.rs");
        let start = src
            .find("async fn setup")
            .expect("setup present in bridged.rs");
        let end = src[start..]
            .find("async fn cleanup")
            .expect("cleanup still follows setup")
            + start;
        for reader in [
            "RESOLV_CONF_SOURCES",
            "nameservers_from_sources",
            "ResolvSource",
        ] {
            assert!(
                !src[start..end].contains(reader),
                "BridgedNetwork::setup reads host DNS itself ({reader}); thread the \
                 launch config's hashed value via with_dns_server instead"
            );
        }
    }

    #[test]
    fn peer_of_increments_last_octet() {
        assert_eq!(peer_of("10.0.0.1").as_deref(), Some("10.0.0.2"));
        assert_eq!(peer_of("172.30.23.93").as_deref(), Some("172.30.23.94"));
        assert_eq!(peer_of("not-an-ip"), None);
        // .255 cannot have a /30 peer above it; refuse rather than wrap.
        assert_eq!(peer_of("10.0.0.255"), None);
    }
}
