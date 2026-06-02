use anyhow::{Context, Result};
use tracing::{info, warn};

use super::namespace;
use super::tcp_proxy;
use super::types::generate_mac;
use super::veth;
use super::{NetworkConfig, NetworkManager, PortMapping};
use crate::state::truncate_id;

/// Guest network addressing (same as pasta/bridged for Firecracker compatibility)
const GUEST_IP: &str = "10.0.2.100";
const GUEST_GATEWAY: &str = "10.0.2.1";

/// Host loopback alias for `--forward-localhost`. fc-agent's guest-side relay
/// connects to this address (pasta maps it to host loopback in rootless mode).
/// In routed mode nothing owns it by default, so setup assigns it to the bridge
/// and listens there, relaying to the host's 127.0.0.1.
const HOST_LOOPBACK_ALIAS: &str = "10.0.2.2";

/// Bridge device name
const BRIDGE_DEVICE: &str = "br0";

/// Routed networking using veth + IPv6 routing
///
/// Unlike pasta mode (which proxies all outbound traffic through userspace L4),
/// routed mode connects the VM's network namespace to the host via a veth pair.
/// Outbound IPv6 traffic goes through the kernel's routing stack at line rate.
///
/// Requires sudo (creates veth pairs and modifies host routing table).
///
/// Architecture:
/// ```text
/// Host namespace                     Namespace (ip netns)
///
/// eth0 (/64 subnet)
///   |
/// veth-host ←───veth pair────→ veth-ns
///   (proxy NDP)                    |
///                              br0 (10.0.2.1/24)
///                                  |
///                              tap-vm → Firecracker VM (10.0.2.100)
/// ```
pub struct RoutedNetwork {
    vm_id: String,
    tap_device: String,
    port_mappings: Vec<PortMapping>,
    loopback_ip: Option<String>,
    /// Explicit routable /64 prefix. Skips auto-detect and MASQUERADE.
    ipv6_prefix: Option<String>,
    /// Guest localhost ports forwarded to the host's 127.0.0.1 (--forward-localhost).
    forward_localhost: Vec<u16>,

    // Network state (populated during setup)
    namespace_id: Option<String>,
    host_veth: Option<String>,
    vm_ipv6: Option<String>,
    default_iface: Option<String>,
    proxy_handles: Vec<tokio::task::JoinHandle<()>>,
}

impl RoutedNetwork {
    pub fn new(vm_id: String, tap_device: String, port_mappings: Vec<PortMapping>) -> Self {
        Self {
            vm_id,
            tap_device,
            port_mappings,
            loopback_ip: None,
            ipv6_prefix: None,
            forward_localhost: Vec::new(),
            namespace_id: None,
            host_veth: None,
            vm_ipv6: None,
            default_iface: None,
            proxy_handles: Vec::new(),
        }
    }

    pub fn with_ipv6_prefix(mut self, prefix: String) -> Self {
        self.ipv6_prefix = Some(prefix);
        self
    }

    /// Set guest localhost ports to forward to the host's 127.0.0.1 (--forward-localhost).
    ///
    /// fc-agent relays guest 127.0.0.1:<port> to 10.0.2.2:<port>; setup() makes the
    /// namespace own 10.0.2.2 and relays each connection to the host's loopback.
    pub fn with_forward_localhost(mut self, ports: Vec<u16>) -> Self {
        self.forward_localhost = ports;
        self
    }

    /// Validate that a prefix string looks like a valid IPv6 /64 prefix
    /// (4 colon-separated groups of 1-4 hex digits, e.g. "2600:1f1c:494:201").
    fn validate_ipv6_prefix(prefix: &str) -> Result<()> {
        let groups: Vec<&str> = prefix.split(':').collect();
        if groups.len() != 4 {
            anyhow::bail!(
                "invalid --ipv6-prefix '{}': expected 4 colon-separated hex groups \
                 (e.g. 2600:1f1c:494:201)",
                prefix
            );
        }
        for group in &groups {
            if group.is_empty() || group.len() > 4 {
                anyhow::bail!(
                    "invalid --ipv6-prefix '{}': each group must be 1-4 hex digits",
                    prefix
                );
            }
            if u16::from_str_radix(group, 16).is_err() {
                anyhow::bail!(
                    "invalid --ipv6-prefix '{}': '{}' is not valid hex",
                    prefix,
                    group
                );
            }
        }
        Ok(())
    }

    /// Get the network namespace ID (for setting Firecracker's namespace).
    pub fn namespace_id(&self) -> Option<&str> {
        self.namespace_id.as_deref()
    }

    /// Set a unique loopback IP for port forwarding (127.x.y.z)
    ///
    /// Allocated by StateManager::allocate_loopback_ip() with lock-based
    /// coordination to prevent collisions across concurrent VM starts.
    pub fn with_loopback_ip(mut self, loopback_ip: String) -> Self {
        self.loopback_ip = Some(loopback_ip);
        self
    }

    /// Validate that the host meets requirements for routed networking.
    ///
    /// Call this early (before VM setup) to give clear error messages.
    /// When `--ipv6-prefix` was set (via `with_ipv6_prefix`), auto-detect and
    /// ip6tables checks are skipped.
    pub fn preflight_check(&self) -> Result<()> {
        // Must be root
        if !nix::unistd::getuid().is_root() {
            anyhow::bail!(
                "routed networking requires root (creates network namespaces and veth pairs). \
                 Run with sudo or use --network rootless instead."
            );
        }

        if let Some(ref prefix) = self.ipv6_prefix {
            Self::validate_ipv6_prefix(prefix)?;
            return Ok(()); // Explicit prefix — no auto-detect or ip6tables needed
        }

        // Must have global IPv6
        if Self::detect_host_ipv6().is_none() {
            anyhow::bail!(
                "routed networking requires a host with a global IPv6 address.\n\
                 The host needs a non-deprecated /64 (or a /128 with a /64 on-link route).\n\
                 Use --ipv6-prefix to specify a routable /64 prefix explicitly.\n\
                 Check with: ip -6 addr show scope global"
            );
        }

        // ip6tables must be available (for MASQUERADE)
        let ip6tables = std::process::Command::new("ip6tables")
            .args(["--version"])
            .output();
        if ip6tables.is_err() || !ip6tables.unwrap().status.success() {
            anyhow::bail!(
                "routed networking requires ip6tables for IPv6 MASQUERADE.\n\
                 Use --ipv6-prefix to specify a routable prefix (skips MASQUERADE).\n\
                 Install with: apt-get install iptables"
            );
        }

        Ok(())
    }

    /// Get the unique per-clone vm_ipv6 address.
    pub fn vm_ipv6(&self) -> Option<&str> {
        self.vm_ipv6.as_deref()
    }

    /// Detect host's global IPv6 address and /64 subnet for VM addressing.
    /// Returns (host_ip, subnet_prefix) e.g. ("2600:1f1c:494:201::1", "2600:1f1c:494:201")
    ///
    /// Skips deprecated addresses (preferred_lft 0). Supports:
    /// - Direct /64: host has an active address with /64 prefix length
    /// - /128 with on-link /64 route: AWS VPC, service networks
    ///
    /// For hosts where auto-detect fails (e.g. only deprecated /64s), use
    /// --ipv6-prefix to specify the routable prefix explicitly.
    fn detect_host_ipv6() -> Option<(String, String)> {
        let output = std::process::Command::new("ip")
            .args(["-6", "addr", "show", "scope", "global"])
            .output()
            .ok()?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        // First pass: look for /64 addresses (preferred over /128)
        if let Some(result) = Self::parse_host_ipv6(&stdout, false) {
            return Some(result);
        }
        // Second pass: check /128 addresses with on-link /64 routes
        Self::parse_host_ipv6(&stdout, true)
    }

    /// Parse `ip -6 addr show` output to find a usable global IPv6 address.
    /// When `check_onlink` is false, only returns /64 addresses.
    /// When `check_onlink` is true, returns /128 addresses that have on-link /64 routes.
    /// Skips deprecated, link-local, and ULA addresses.
    fn parse_host_ipv6(output: &str, check_onlink: bool) -> Option<(String, String)> {
        for line in output.lines() {
            let line = line.trim();
            if let Some(addr) = line.strip_prefix("inet6 ") {
                if line.contains("deprecated") {
                    continue;
                }
                if let Some(addr_cidr) = addr.split_whitespace().next() {
                    if let Some((addr, prefix_len)) = addr_cidr.split_once('/') {
                        if addr.starts_with("fe80") || addr.starts_with("fd") {
                            continue; // Skip link-local and ULA
                        }
                        if let Ok(ip) = addr.parse::<std::net::Ipv6Addr>() {
                            let segments = ip.segments();
                            let prefix = format!(
                                "{:x}:{:x}:{:x}:{:x}",
                                segments[0], segments[1], segments[2], segments[3]
                            );

                            if !check_onlink && prefix_len == "64" {
                                return Some((addr.to_string(), prefix));
                            }
                            if check_onlink
                                && prefix_len == "128"
                                && Self::has_onlink_64_route(&prefix)
                            {
                                info!(
                                    addr = %addr,
                                    prefix = %prefix,
                                    "using /128 address with /64 on-link route"
                                );
                                return Some((addr.to_string(), prefix));
                            }
                        }
                    }
                }
            }
        }
        None
    }

    /// Check if the kernel has a /64 on-link route for the given prefix.
    /// AWS VPC configures this via Router Advertisements.
    fn has_onlink_64_route(prefix: &str) -> bool {
        let route_prefix = format!("{prefix}::/64");
        let output = std::process::Command::new("ip")
            .args(["-6", "route", "show", &route_prefix])
            .output();
        match output {
            Ok(out) => {
                let stdout = String::from_utf8_lossy(&out.stdout);
                !stdout.trim().is_empty()
            }
            Err(_) => false,
        }
    }

    /// Generate a deterministic IPv6 for the VM from the host's /64 subnet.
    fn generate_vm_ipv6(prefix: &str, vm_id: &str) -> String {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let mut hasher = DefaultHasher::new();
        vm_id.hash(&mut hasher);
        let hash = hasher.finish();

        // Use lower 64 bits as the interface ID portion of the /64 prefix.
        // Format: prefix:XXXX:XXXX:XXXX:XXXX (4 hex groups = 64 bits)
        let w1 = ((hash >> 48) & 0xFFFF) as u16;
        let w2 = ((hash >> 32) & 0xFFFF) as u16;
        let w3 = ((hash >> 16) & 0xFFFF) as u16;
        let w4 = (hash & 0xFFFF) as u16;
        format!("{}:{:x}:{:x}:{:x}:{:x}", prefix, w1, w2, w3, w4)
    }
}

#[async_trait::async_trait]
impl NetworkManager for RoutedNetwork {
    async fn setup(&mut self) -> Result<NetworkConfig> {
        // Generate unique namespace/veth names. Use hash-based short ID with
        // collision detection: if the namespace already exists, bump a counter.
        // Linux interface names are max 15 chars, so we use 5-char suffixes.
        let (ns_name, host_veth, guest_veth) = {
            let base = truncate_id(&self.vm_id, 8);
            let mut ns = format!("fcvm-{}", base);
            let mut hv = format!("veth-{}", base);
            let mut gv = format!("vns-{}", base);

            // Check for collision (another VM with same truncated ID).
            // Bounded to 100 iterations to prevent infinite loops.
            let mut found = !std::path::Path::new(&format!("/var/run/netns/{}", ns)).exists();
            if !found {
                for i in 1u32..=100 {
                    warn!(namespace = %ns, "namespace collision, retrying with suffix");
                    let suffix = format!("{}{}", base, i);
                    // Truncate to keep within IFNAMSIZ (15 chars)
                    let short = &suffix[..suffix.len().min(10)];
                    ns = format!("fcvm-{}", short);
                    hv = format!("veth-{}", short);
                    gv = format!("vns-{}", short);
                    if !std::path::Path::new(&format!("/var/run/netns/{}", ns)).exists() {
                        found = true;
                        break;
                    }
                }
            }
            if !found {
                anyhow::bail!(
                    "could not find a free namespace name after 100 attempts \
                     (base={}, last tried={}). Check for stale namespaces with: ip netns list",
                    base,
                    ns
                );
            }
            (ns, hv, gv)
        };

        info!(
            vm_id = %self.vm_id,
            namespace = %ns_name,
            "setting up routed networking"
        );

        // Resolve IPv6 /64 prefix: explicit --ipv6-prefix or auto-detect from interfaces
        let (host_ipv6, ipv6_prefix) = if let Some(ref prefix) = self.ipv6_prefix {
            let host_addr = format!("{}::1", prefix);
            info!(prefix = %prefix, "using explicit --ipv6-prefix (routable, no MASQUERADE)");
            (host_addr, prefix.clone())
        } else {
            Self::detect_host_ipv6().context(
                "routed mode requires a global IPv6 /64 subnet. \
                          Use --ipv6-prefix to specify one explicitly.",
            )?
        };

        // Generate a unique IPv6 for this VM. Check for route collisions
        // (astronomically unlikely with 64-bit hash, but defend against it).
        let vm_ipv6 = {
            let mut candidate = Self::generate_vm_ipv6(&ipv6_prefix, &self.vm_id);
            for attempt in 0..10 {
                let route_check = tokio::process::Command::new("ip")
                    .args(["-6", "route", "show", &format!("{}/128", candidate)])
                    .output()
                    .await;
                let route_exists = route_check
                    .map(|o| !String::from_utf8_lossy(&o.stdout).trim().is_empty())
                    .unwrap_or(false);
                if !route_exists {
                    break;
                }
                warn!(
                    ipv6 = %candidate, attempt,
                    "IPv6 route collision detected, trying alternative"
                );
                // Mix in the attempt counter for a different hash
                candidate =
                    Self::generate_vm_ipv6(&ipv6_prefix, &format!("{}:{}", self.vm_id, attempt));
            }
            candidate
        };
        info!(
            host_ipv6 = %host_ipv6,
            vm_ipv6 = %vm_ipv6,
            prefix = %ipv6_prefix,
            "IPv6 addresses for routed networking"
        );

        // 1. Create network namespace
        namespace::create_namespace(&ns_name).await?;

        // 2. Create veth pair and move guest side to namespace
        veth::create_veth_pair(&host_veth, &guest_veth, &ns_name).await?;

        // 3. Create TAP in namespace
        veth::create_tap_in_ns(&ns_name, &self.tap_device).await?;

        // 4. Create bridge with BOTH TAP and veth.
        //    The bridge does L2 forwarding: VM → TAP → bridge → veth → host.
        //    IPv6 for the bridge's own address (fd00::1) is locally delivered.
        //    IPv6 for external destinations traverses the bridge to the veth peer on the host.
        veth::connect_tap_to_veth(&ns_name, &self.tap_device, &guest_veth).await?;
        // Bring up all interfaces (connect_tap_to_veth only brings up the bridge)
        namespace::exec_in_namespace(&ns_name, &["ip", "link", "set", "lo", "up"]).await?;
        namespace::exec_in_namespace(&ns_name, &["ip", "link", "set", &self.tap_device, "up"])
            .await?;
        namespace::exec_in_namespace(&ns_name, &["ip", "link", "set", &guest_veth, "up"]).await?;

        // 5. Assign gateway IPs to bridge (VM connects here via TAP)
        let gw_cidr = format!("{}/24", GUEST_GATEWAY);
        namespace::exec_in_namespace(
            &ns_name,
            &["ip", "addr", "add", &gw_cidr, "dev", BRIDGE_DEVICE],
        )
        .await?;
        // nodad: skip Duplicate Address Detection so the address is usable immediately.
        // Without nodad, the address stays "tentative" for ~1-3s and the kernel silently
        // drops all IPv6 packets to it — breaking the VM's default route via fd00::1.
        namespace::exec_in_namespace(
            &ns_name,
            &[
                "ip",
                "-6",
                "addr",
                "add",
                "fd00::1/64",
                "dev",
                BRIDGE_DEVICE,
                "nodad",
            ],
        )
        .await?;

        // 6. Bring up host veth
        let output = tokio::process::Command::new("ip")
            .args(["link", "set", &host_veth, "up"])
            .output()
            .await
            .context("bringing up host veth")?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("failed to bring up host veth: {}", stderr);
        }
        // 7. Enable IPv6 forwarding on the host veth only (not all.forwarding —
        //    that prevents link-local auto-assignment on future interfaces).
        let _ = tokio::process::Command::new("sysctl")
            .args(["-w", &format!("net.ipv6.conf.{}.forwarding=1", host_veth)])
            .output()
            .await;

        // Detect default interface early — used for sysctl checks AND proxy NDP below.
        let default_iface = detect_default_ipv6_interface()
            .await
            .unwrap_or_else(|| "eth0".to_string());

        // Verify host routing is set up correctly. These sysctls are the user's
        // responsibility (host sysctl configuration), not fcvm's — but warn
        // loudly if they're wrong because IPv6 egress silently fails without them.
        if self.ipv6_prefix.is_none() {
            if let Ok(val) =
                tokio::fs::read_to_string("/proc/sys/net/ipv6/conf/all/forwarding").await
            {
                if val.trim() != "1" {
                    warn!(
                        "net.ipv6.conf.all.forwarding={} (need 1) — fix host sysctls",
                        val.trim()
                    );
                }
            }
            if let Ok(val) = tokio::fs::read_to_string(format!(
                "/proc/sys/net/ipv6/conf/{}/accept_ra",
                default_iface
            ))
            .await
            {
                if val.trim() != "2" {
                    warn!(
                        "net.ipv6.conf.{}.accept_ra={} (need 2) — IPv6 routing may fail after reboot",
                        default_iface,
                        val.trim()
                    );
                }
            }
            let route_check = tokio::process::Command::new("ip")
                .args(["-6", "route", "show", "default"])
                .output()
                .await;
            if let Ok(out) = route_check {
                if !String::from_utf8_lossy(&out.stdout).contains("default via") {
                    warn!("no default IPv6 route — fix host sysctls to fix accept_ra");
                }
            }
        }

        // 8. Assign link-local to host veth manually (auto-assignment fails when
        //    all.forwarding=1 from a previous run). Use EUI-64 from MAC + nodad.
        let host_ll = generate_link_local_from_mac(&host_veth)
            .await
            .context("failed to generate link-local for host veth")?;
        let host_ll_cidr = format!("{}/64", host_ll);
        let _ = tokio::process::Command::new("ip")
            .args([
                "-6",
                "addr",
                "add",
                &host_ll_cidr,
                "dev",
                &host_veth,
                "scope",
                "link",
                "nodad",
            ])
            .output()
            .await;
        info!(host_ll = %host_ll, "assigned link-local to host veth");

        // 9. In namespace: default IPv6 route goes through bridge → veth → host.
        //     The bridge forwards L2 frames from br0 to veth (bridge member).
        //     The host veth receives them and the host kernel routes to eth0.
        //     Use the host veth's link-local as nexthop (reachable via NDP on bridge).
        namespace::exec_in_namespace(
            &ns_name,
            &[
                "ip",
                "-6",
                "route",
                "add",
                "default",
                "via",
                &host_ll,
                "dev",
                BRIDGE_DEVICE,
            ],
        )
        .await?;

        // 10. Enable IPv6 forwarding in namespace (for VM traffic forwarding)
        namespace::exec_in_namespace(
            &ns_name,
            &["sysctl", "-w", "net.ipv6.conf.all.forwarding=1"],
        )
        .await?;

        // IPv4 stays internal to the namespace (bridge at 10.0.2.1 for health checks).
        // All external traffic uses IPv6 — each clone gets a unique IPv6 address,
        // so return routing works naturally without CONNMARK or ECMP workarounds.

        // 11. On host: route VM's IPv6 back through veth to the namespace (per-VM, no collision)
        let vm_route = format!("{}/128", vm_ipv6);
        let _ = tokio::process::Command::new("ip")
            .args(["-6", "route", "replace", &vm_route, "dev", &host_veth])
            .output()
            .await;

        // 12. Add proxy NDP so the network fabric routes VM's IPv6 to this host
        // (default_iface already detected above)
        // Enable proxy NDP on the interface so the kernel actually responds
        // to neighbor solicitations for our proxy entries.
        let _ = tokio::process::Command::new("sysctl")
            .args([
                "-w",
                &format!("net.ipv6.conf.{}.proxy_ndp=1", default_iface),
            ])
            .output()
            .await;
        let _ = tokio::process::Command::new("ip")
            .args([
                "-6",
                "neigh",
                "add",
                "proxy",
                &vm_ipv6,
                "dev",
                &default_iface,
            ])
            .output()
            .await;
        info!(vm_ipv6 = %vm_ipv6, iface = %default_iface, "added proxy NDP");

        // 13. MASQUERADE outbound IPv6 traffic from the namespace.
        //     On AWS, source/dest check drops packets with unassigned source IPs.
        //     MASQUERADE rewrites the source to the host's IP so the VPC fabric
        //     accepts the traffic. IPv4 is not routed externally — only IPv6.
        //     Skipped when --ipv6-prefix is set: the prefix is directly routable
        //     and the VM's source IP matches the cert's IP SANs.
        if self.ipv6_prefix.is_some() {
            info!(iface = %default_iface, "skipping MASQUERADE (--ipv6-prefix is routable)");
        } else {
            let _ = tokio::process::Command::new("ip6tables")
                .args([
                    "-t",
                    "nat",
                    "-A",
                    "POSTROUTING",
                    "-o",
                    &default_iface,
                    "-s",
                    &format!("{}/128", vm_ipv6),
                    "-j",
                    "MASQUERADE",
                ])
                .output()
                .await;
            info!(iface = %default_iface, "added IPv6 MASQUERADE for outbound traffic");
        }

        // 14. Port forwarding: TCP proxy listens on host loopback, connects to VM
        //     inside the namespace via setns(2). The veth is a bridge member so
        //     host-side IPv4 routing to 10.0.2.100 doesn't work — the connect side
        //     must run inside the namespace where the bridge is directly reachable.
        //     Loopback IP is allocated by StateManager with lock-based coordination.
        let loopback_ip = self
            .loopback_ip
            .clone()
            .unwrap_or_else(|| "127.0.0.2".to_string());
        if !self.port_mappings.is_empty() {
            let handles = tcp_proxy::start_port_forwards(
                &loopback_ip,
                &self.port_mappings,
                &ns_name,
                GUEST_IP,
            )
            .await
            .context("starting port forward proxies")?;
            self.proxy_handles.extend(handles);
        }

        // 15. Localhost forwarding (--forward-localhost): fc-agent's guest relay
        //     connects to 10.0.2.2:<port> (the pasta-style host gateway). Nothing
        //     owns 10.0.2.2 in routed mode, so assign it to the bridge and listen
        //     there inside the namespace, relaying each connection to the host's
        //     127.0.0.1:<port> from the host namespace.
        if !self.forward_localhost.is_empty() {
            let alias_cidr = format!("{}/32", HOST_LOOPBACK_ALIAS);
            let output = namespace::exec_in_namespace(
                &ns_name,
                &["ip", "addr", "add", &alias_cidr, "dev", BRIDGE_DEVICE],
            )
            .await?;
            if !output.status.success() {
                anyhow::bail!(
                    "failed to assign host loopback alias {} to {}: {}",
                    alias_cidr,
                    BRIDGE_DEVICE,
                    String::from_utf8_lossy(&output.stderr).trim()
                );
            }

            let handles = tcp_proxy::start_localhost_forwards(
                &ns_name,
                HOST_LOOPBACK_ALIAS,
                &self.forward_localhost,
            )
            .await
            .context("starting localhost forward proxies")?;
            self.proxy_handles.extend(handles);
            info!(
                ports = ?self.forward_localhost,
                alias = %HOST_LOOPBACK_ALIAS,
                "forwarding guest localhost ports to host loopback"
            );
        }

        // 16. Reverse proxy relay: TCP proxy listens inside the namespace on the
        //     gateway and connects to the actual proxy from the HOST namespace.
        //     Host-side BPF programs intercept connect() syscalls and inject
        //     client certs for proxy auth. Without this relay, the VM's direct
        //     connections bypass these hooks and get rejected (407).
        if let Some(proxy) = std::env::var("HTTPS_PROXY")
            .ok()
            .or_else(|| std::env::var("https_proxy").ok())
        {
            match tcp_proxy::parse_proxy_addr(&proxy) {
                Ok(proxy_addr) => {
                    let handle = tcp_proxy::start_proxy_relay(&ns_name, GUEST_GATEWAY, proxy_addr)
                        .await
                        .context("starting proxy relay")?;
                    self.proxy_handles.push(handle);
                }
                Err(e) => {
                    warn!(proxy = %proxy, error = %e, "failed to parse proxy address, skipping relay");
                }
            }
        }

        let guest_mac = generate_mac();
        let guest_ip = format!("{}/{}", GUEST_IP, "24");

        // Store state for cleanup
        self.namespace_id = Some(ns_name);
        self.host_veth = Some(host_veth);
        self.vm_ipv6 = Some(vm_ipv6.clone());
        self.default_iface = Some(default_iface);

        Ok(NetworkConfig {
            tap_device: self.tap_device.clone(),
            guest_mac,
            guest_ip: Some(guest_ip),
            host_ip: Some(GUEST_GATEWAY.to_string()),
            host_veth: self.host_veth.clone(),
            loopback_ip: if self.port_mappings.is_empty() {
                None
            } else {
                Some(loopback_ip)
            },
            // IPv4 DNS (e.g. VPC's 10.0.0.2) is unreachable without MASQUERADE.
            // Detect an IPv6 DNS server reachable via native routed IPv6.
            dns_server: detect_ipv6_dns().await,
            // Include /128 prefix so fc-agent uses a host route instead of /64 on-link.
            // With /64, the VM would try NDP for other addresses in the host's subnet
            // directly on eth0, which fails (they're behind the veth + physical network).
            guest_ipv6: Some(format!("{}/128", vm_ipv6)),
            // fd00::1 is on the bridge inside the namespace. The VM uses it as IPv6 gateway.
            // NDP resolves it on the VM's local link (TAP → bridge → fd00::1 responds).
            // The namespace kernel then forwards to the veth → host.
            host_ipv6: Some("fd00::1".to_string()),
            dns_search: None,
            // Override proxy to use the gateway relay (TCP proxy on host side
            // goes through BPF hooks for client cert injection).
            http_proxy: if std::env::var("HTTPS_PROXY").is_ok()
                || std::env::var("https_proxy").is_ok()
            {
                Some(format!(
                    "http://{}:{}",
                    GUEST_GATEWAY,
                    super::tcp_proxy::PROXY_RELAY_PORT
                ))
            } else {
                None
            },
            namespace_name: self.namespace_id.clone(),
        })
    }

    async fn cleanup(&mut self) -> Result<()> {
        info!(vm_id = %self.vm_id, "cleaning up routed network resources");

        // Abort TCP proxy tasks (port forwarders + proxy relay)
        for handle in self.proxy_handles.drain(..) {
            handle.abort();
        }

        // Remove IPv6 MASQUERADE and proxy NDP
        if let Some(ref vm_ipv6) = self.vm_ipv6 {
            let default_iface = self.default_iface.as_deref().unwrap_or("eth0");

            // Remove IPv6 MASQUERADE rule (only if we set one — skipped with --ipv6-prefix)
            if self.ipv6_prefix.is_none() {
                match tokio::process::Command::new("ip6tables")
                    .args([
                        "-t",
                        "nat",
                        "-D",
                        "POSTROUTING",
                        "-o",
                        default_iface,
                        "-s",
                        &format!("{}/128", vm_ipv6),
                        "-j",
                        "MASQUERADE",
                    ])
                    .output()
                    .await
                {
                    Ok(o) if !o.status.success() => {
                        warn!(stderr = %String::from_utf8_lossy(&o.stderr).trim(), "ip6tables MASQUERADE cleanup failed");
                    }
                    Err(e) => warn!(error = %e, "ip6tables command failed"),
                    _ => {}
                }
            }

            // Remove proxy NDP
            match tokio::process::Command::new("ip")
                .args(["-6", "neigh", "del", "proxy", vm_ipv6, "dev", default_iface])
                .output()
                .await
            {
                Ok(o) if !o.status.success() => {
                    warn!(stderr = %String::from_utf8_lossy(&o.stderr).trim(), "proxy NDP cleanup failed");
                }
                Err(e) => warn!(error = %e, "proxy NDP command failed"),
                _ => {}
            }

            // Remove host route (use dev qualifier for parallel safety)
            if let Some(ref host_veth) = self.host_veth {
                match tokio::process::Command::new("ip")
                    .args([
                        "-6",
                        "route",
                        "del",
                        &format!("{}/128", vm_ipv6),
                        "dev",
                        host_veth,
                    ])
                    .output()
                    .await
                {
                    Ok(o) if !o.status.success() => {
                        warn!(stderr = %String::from_utf8_lossy(&o.stderr).trim(), "host route cleanup failed");
                    }
                    Err(e) => warn!(error = %e, "host route cleanup command failed"),
                    _ => {}
                }
            }
        }

        // Delete veth pair (auto-deletes peer)
        if let Some(ref host_veth) = self.host_veth {
            if let Err(e) = veth::delete_veth_pair(host_veth).await {
                warn!(error = %e, host_veth = %host_veth, "veth pair cleanup failed");
            }
        }

        // Delete namespace
        if let Some(ref ns_name) = self.namespace_id {
            if let Err(e) = namespace::delete_namespace(ns_name).await {
                warn!(error = %e, namespace = %ns_name, "namespace cleanup failed");
            }
        }

        Ok(())
    }

    fn tap_device(&self) -> &str {
        &self.tap_device
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// Generate EUI-64 link-local address from interface MAC address.
/// Needed because IPv6 link-local auto-assignment fails when forwarding=1.
async fn generate_link_local_from_mac(iface: &str) -> Option<String> {
    let output = tokio::process::Command::new("ip")
        .args(["link", "show", iface])
        .output()
        .await
        .ok()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("link/ether ") {
            if let Some(mac) = rest.split_whitespace().next() {
                let bytes: Vec<u8> = mac
                    .split(':')
                    .filter_map(|h| u8::from_str_radix(h, 16).ok())
                    .collect();
                if bytes.len() == 6 {
                    // EUI-64: flip bit 6 of first byte, insert ff:fe in middle
                    let b0 = bytes[0] ^ 0x02;
                    return Some(format!(
                        "fe80::{:02x}{:02x}:{:02x}ff:fe{:02x}:{:02x}{:02x}",
                        b0, bytes[1], bytes[2], bytes[3], bytes[4], bytes[5]
                    ));
                }
            }
        }
    }
    None
}

/// Find a DNS server reachable over IPv6 for routed mode.
///
/// IPv4 DNS (e.g. VPC's 10.0.0.2) is unreachable from the VM without MASQUERADE.
/// This function discovers an IPv6 DNS server by:
/// 1. Checking host resolv.conf for existing IPv6 nameservers
/// 2. Probing known cloud IPv6 DNS endpoints (e.g. AWS fd00:ec2::253)
async fn detect_ipv6_dns() -> Option<String> {
    // Check host DNS config for IPv6 nameservers
    let resolv = std::fs::read_to_string("/run/systemd/resolve/resolv.conf")
        .or_else(|_| std::fs::read_to_string("/etc/resolv.conf"))
        .ok()?;

    for line in resolv.lines() {
        if let Some(server) = line.strip_prefix("nameserver ") {
            let server = server.trim();
            if server.contains(':') {
                return Some(server.to_string());
            }
        }
    }

    // No IPv6 nameserver in resolv.conf. Probe known cloud IPv6 DNS endpoints.
    // AWS VPCs provide dual-stack DNS at fd00:ec2::253.
    let probe = tokio::process::Command::new("dig")
        .args([
            "+short",
            "+timeout=2",
            "+tries=1",
            "@fd00:ec2::253",
            "example.com",
        ])
        .output()
        .await
        .ok()?;

    if probe.status.success() && !String::from_utf8_lossy(&probe.stdout).trim().is_empty() {
        info!("detected IPv6 DNS at fd00:ec2::253 (AWS VPC)");
        return Some("fd00:ec2::253".to_string());
    }

    None
}

/// Detect the default IPv6 outgoing interface
async fn detect_default_ipv6_interface() -> Option<String> {
    let output = tokio::process::Command::new("ip")
        .args(["-6", "route", "show", "default"])
        .output()
        .await
        .ok()?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    // Parse "default via fe80::face:b00c dev eth0 ..."
    for line in stdout.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if let Some(idx) = parts.iter().position(|&p| p == "dev") {
            return parts.get(idx + 1).map(|s| s.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- validate_ipv6_prefix tests ---

    #[test]
    fn test_validate_ipv6_prefix_valid() {
        assert!(RoutedNetwork::validate_ipv6_prefix("2600:1f1c:494:201").is_ok());
        assert!(RoutedNetwork::validate_ipv6_prefix("2803:6084:7058:46f6").is_ok());
        assert!(RoutedNetwork::validate_ipv6_prefix("0:0:0:0").is_ok());
        assert!(RoutedNetwork::validate_ipv6_prefix("ffff:ffff:ffff:ffff").is_ok());
        assert!(RoutedNetwork::validate_ipv6_prefix("a:b:c:d").is_ok());
    }

    #[test]
    fn test_validate_ipv6_prefix_wrong_group_count() {
        let err = RoutedNetwork::validate_ipv6_prefix("2600:1f1c:494").unwrap_err();
        assert!(err
            .to_string()
            .contains("expected 4 colon-separated hex groups"));

        let err = RoutedNetwork::validate_ipv6_prefix("2600:1f1c:494:201:abcd").unwrap_err();
        assert!(err
            .to_string()
            .contains("expected 4 colon-separated hex groups"));

        let err = RoutedNetwork::validate_ipv6_prefix("2600").unwrap_err();
        assert!(err
            .to_string()
            .contains("expected 4 colon-separated hex groups"));

        let err = RoutedNetwork::validate_ipv6_prefix("").unwrap_err();
        assert!(err
            .to_string()
            .contains("expected 4 colon-separated hex groups"));
    }

    #[test]
    fn test_validate_ipv6_prefix_invalid_hex() {
        // Non-hex characters
        let err = RoutedNetwork::validate_ipv6_prefix("zzzz:1f1c:494:201").unwrap_err();
        assert!(err.to_string().contains("not valid hex"));

        // Empty group (consecutive colons) — splits to 4 groups but one is empty
        let err = RoutedNetwork::validate_ipv6_prefix("2600::494:201").unwrap_err();
        assert!(err
            .to_string()
            .contains("each group must be 1-4 hex digits"));

        // Group too long (5 digits)
        let err = RoutedNetwork::validate_ipv6_prefix("26000:1f1c:494:201").unwrap_err();
        assert!(err
            .to_string()
            .contains("each group must be 1-4 hex digits"));
    }

    #[test]
    fn test_validate_ipv6_prefix_full_address_rejected() {
        // Full IPv6 address (8 groups) should be rejected
        let err = RoutedNetwork::validate_ipv6_prefix("2600:1f1c:494:201:1:2:3:4").unwrap_err();
        assert!(err
            .to_string()
            .contains("expected 4 colon-separated hex groups"));

        // Compressed full address
        let err = RoutedNetwork::validate_ipv6_prefix("2600:1f1c:494:201::1").unwrap_err();
        assert!(err
            .to_string()
            .contains("expected 4 colon-separated hex groups"));
    }

    // --- generate_vm_ipv6 tests ---

    #[test]
    fn test_generate_vm_ipv6_deterministic() {
        let a1 = RoutedNetwork::generate_vm_ipv6("2600:1f1c:494:201", "vm-abc");
        let a2 = RoutedNetwork::generate_vm_ipv6("2600:1f1c:494:201", "vm-abc");
        assert_eq!(a1, a2, "same inputs must produce same output");

        let b = RoutedNetwork::generate_vm_ipv6("2600:1f1c:494:201", "vm-xyz");
        assert_ne!(a1, b, "different vm_ids must produce different addresses");

        let c = RoutedNetwork::generate_vm_ipv6("2803:6084:7058:46f6", "vm-abc");
        assert_ne!(a1, c, "different prefixes must produce different addresses");
    }

    #[test]
    fn test_generate_vm_ipv6_format() {
        let addr = RoutedNetwork::generate_vm_ipv6("2600:1f1c:494:201", "vm-test");
        assert!(
            addr.starts_with("2600:1f1c:494:201:"),
            "address must start with prefix: {}",
            addr
        );
        // Should have 8 colon-separated groups total (4 prefix + 4 interface ID)
        let groups: Vec<&str> = addr.split(':').collect();
        assert_eq!(groups.len(), 8, "IPv6 must have 8 groups: {}", addr);
        // Each interface ID group should be valid hex
        for group in &groups[4..] {
            assert!(
                u16::from_str_radix(group, 16).is_ok(),
                "group '{}' is not valid hex in: {}",
                group,
                addr
            );
        }
    }

    // --- parse_host_ipv6 tests (deprecated address filtering) ---

    #[test]
    fn test_parse_host_ipv6_skips_deprecated() {
        let output = "\
2: eth0: <BROADCAST,MULTICAST,UP,LOWER_UP> mtu 9001 state UP
    inet6 2600:1f1c:494:201::1/64 scope global deprecated dynamic noprefixroute
       valid_lft 3552sec preferred_lft 0sec
    inet6 2803:6084:7058:46f6::1/64 scope global dynamic noprefixroute
       valid_lft 3552sec preferred_lft 3552sec";

        let result = RoutedNetwork::parse_host_ipv6(output, false);
        assert!(result.is_some(), "should find non-deprecated address");
        let (addr, prefix) = result.unwrap();
        assert_eq!(addr, "2803:6084:7058:46f6::1");
        assert_eq!(prefix, "2803:6084:7058:46f6");
    }

    #[test]
    fn test_parse_host_ipv6_skips_link_local_and_ula() {
        let output = "\
    inet6 fe80::1/64 scope global
    inet6 fd00::1/64 scope global
    inet6 2600:1f1c:494:201::5/64 scope global dynamic";

        let result = RoutedNetwork::parse_host_ipv6(output, false);
        assert!(result.is_some());
        let (addr, _) = result.unwrap();
        assert_eq!(addr, "2600:1f1c:494:201::5");
    }

    #[test]
    fn test_parse_host_ipv6_all_deprecated_returns_none() {
        let output = "\
    inet6 2600:1f1c:494:201::1/64 scope global deprecated dynamic
    inet6 2803:6084:7058:46f6::1/64 scope global deprecated dynamic";

        let result = RoutedNetwork::parse_host_ipv6(output, false);
        assert!(result.is_none(), "all deprecated should return None");
    }

    #[test]
    fn test_parse_host_ipv6_extracts_prefix() {
        let output = "    inet6 2600:1f1c:0494:0201::abcd/64 scope global dynamic";

        let result = RoutedNetwork::parse_host_ipv6(output, false);
        assert!(result.is_some());
        let (addr, prefix) = result.unwrap();
        assert_eq!(addr, "2600:1f1c:0494:0201::abcd");
        // Prefix is normalized through Ipv6Addr parsing (leading zeros stripped)
        assert_eq!(prefix, "2600:1f1c:494:201");
    }
}
