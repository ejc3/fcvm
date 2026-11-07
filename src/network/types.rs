use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NetworkConfig {
    #[serde(default)]
    pub tap_device: String,
    #[serde(default)]
    pub guest_mac: String,
    #[serde(default)]
    pub guest_ip: Option<String>,
    #[serde(default)]
    pub host_ip: Option<String>,
    #[serde(default)]
    pub host_veth: Option<String>,
    /// For rootless mode: unique loopback IP (127.x.y.z) for health checks
    /// When set, health checks use this IP instead of guest_ip + veth
    #[serde(default)]
    pub loopback_ip: Option<String>,
    /// DNS server for the guest to use
    /// Bridged: host_ip (dnsmasq on veth)
    /// Rootless: 10.0.2.3 (pasta DNS forwarding to host resolver)
    #[serde(default)]
    pub dns_server: Option<String>,
    /// Guest IPv6 address (for rootless networking with IPv6)
    #[serde(default)]
    pub guest_ipv6: Option<String>,
    /// Gateway IPv6 address (for rootless networking with IPv6)
    #[serde(default)]
    pub host_ipv6: Option<String>,
    /// DNS search domains for the guest
    /// Needed for resolving short hostnames in enterprise networks
    #[serde(default)]
    pub dns_search: Option<String>,
    /// Named network namespace (for routed mode health checks via `ip netns exec`)
    #[serde(default)]
    pub namespace_name: Option<String>,
    /// HTTP proxy URL for the guest to use
    /// Passed to fc-agent via MMDS for container pulls and exec
    #[serde(default)]
    pub http_proxy: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortMapping {
    pub host_ip: Option<String>,
    pub host_port: u16,
    pub guest_port: u16,
    pub proto: Protocol,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    Tcp,
    Udp,
}

/// A guest port and protocol a VM's guest was set up to publish: the guest side of a
/// mapping, apart from the host address and port a VM listens on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct GuestPort {
    pub port: u16,
    pub proto: Protocol,
}

impl GuestPort {
    /// The distinct guest ports of `mappings`, sorted.
    pub fn of(mappings: &[PortMapping]) -> Vec<Self> {
        let ports: std::collections::BTreeSet<Self> = mappings
            .iter()
            .map(|m| Self {
                port: m.guest_port,
                proto: m.proto,
            })
            .collect();
        ports.into_iter().collect()
    }
}

impl std::fmt::Display for GuestPort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.port, self.proto)
    }
}

impl std::fmt::Display for Protocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Protocol::Tcp => write!(f, "tcp"),
            Protocol::Udp => write!(f, "udp"),
        }
    }
}

/// The `--publish` spec that parses back to this mapping.
impl std::fmt::Display for PortMapping {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.host_ip.as_deref() {
            Some(ip) if ip.contains(':') => write!(f, "[{ip}]:")?,
            Some(ip) => write!(f, "{ip}:")?,
            None => {}
        }
        write!(f, "{}:{}/{}", self.host_port, self.guest_port, self.proto)
    }
}

impl PortMapping {
    /// Parse port mappings leniently, skipping invalid values with a warning.
    /// Used for cache key computation. `podman run` has parsed the same specs strictly
    /// by then (`publish_mappings` in `podman/mod.rs`), so nothing is skipped there.
    pub fn parse_all_lenient(specs: &[String]) -> Vec<Self> {
        specs
            .iter()
            .filter_map(|s| match Self::parse(s) {
                Ok(pm) => Some(pm),
                Err(e) => {
                    tracing::warn!(spec = %s, error = %e, "ignoring invalid port mapping in cache key");
                    None
                }
            })
            .collect()
    }

    /// The first two mappings, in the order given, that `same_socket` says are one socket.
    pub(crate) fn first_on_one_socket(
        mappings: &[Self],
        same_socket: impl Fn(&Self, &Self) -> bool,
    ) -> Option<(&Self, &Self)> {
        mappings.iter().enumerate().find_map(|(i, mapping)| {
            let earlier = mappings[..i]
                .iter()
                .find(|earlier| same_socket(earlier, mapping))?;
            Some((earlier, mapping))
        })
    }

    /// The address a mapping's host listener binds, as far as overlap is concerned.
    fn listen_address(&self) -> ListenAddress<'_> {
        use std::net::IpAddr;
        match self.host_ip.as_deref() {
            None => ListenAddress::VmLoopback,
            Some(text) => match text.parse::<IpAddr>() {
                Ok(IpAddr::V4(addr)) => ListenAddress::V4(addr),
                Ok(IpAddr::V6(addr)) => match addr.to_ipv4_mapped() {
                    Some(addr) => ListenAddress::V4(addr),
                    None => ListenAddress::V6(addr),
                },
                Err(_) => ListenAddress::Unparsed(text),
            },
        }
    }

    /// Why the host listeners of `a` and `b` cannot both be bound under `mode`, or None
    /// when they can.
    ///
    /// Bridged networking listens on the VM's veth address whatever HOSTIP says, so one
    /// host port and protocol is one socket there. Rootless (pasta) and routed
    /// networking listen on HOSTIP, or on the VM's own 127.x.y.z when there is none: a
    /// specific IPv4 address that no other VM holds. A wildcard holds every address of
    /// its family. Routed clears IPV6_V6ONLY on its listeners (`tcp_proxy.rs`), so its
    /// `[::]` also holds the IPv4 port; pasta's IPv6 listeners are v6only. An
    /// IPv4-mapped IPv6 address is its IPv4 address.
    fn collision(a: &Self, b: &Self, mode: crate::firecracker::FcNetworkMode) -> Option<String> {
        use crate::firecracker::FcNetworkMode;
        use ListenAddress::{VmLoopback, V4, V6};
        if a.host_port != b.host_port || a.proto != b.proto {
            return None;
        }
        if mode == FcNetworkMode::Bridged {
            return Some(
                "claim the same host port: bridged networking listens on the VM's veth \
                 address whatever HOSTIP says, so they would be one address and port. Use \
                 different host ports."
                    .to_string(),
            );
        }
        let (a_addr, b_addr) = (a.listen_address(), b.listen_address());
        if a_addr == b_addr {
            return Some("claim the same host address and port".to_string());
        }
        let ipv4_wildcard = |addr: &ListenAddress| *addr == V4(std::net::Ipv4Addr::UNSPECIFIED);
        let ipv6_wildcard = |addr: &ListenAddress| *addr == V6(std::net::Ipv6Addr::UNSPECIFIED);
        let ipv4 = |addr: &ListenAddress| matches!(addr, V4(_) | VmLoopback);
        let ipv6 = |addr: &ListenAddress| matches!(addr, V6(_));
        for (wild, other) in [(a, b), (b, a)] {
            let (wild_addr, other_addr) = (wild.listen_address(), other.listen_address());
            if ipv4_wildcard(&wild_addr) && ipv4(&other_addr) {
                return Some(format!(
                    "cannot both be bound: {wild} listens on every IPv4 address of the \
                     host, {other}'s included"
                ));
            }
            if ipv6_wildcard(&wild_addr) && ipv6(&other_addr) {
                return Some(format!(
                    "cannot both be bound: {wild} listens on every IPv6 address of the \
                     host, {other}'s included"
                ));
            }
            if ipv6_wildcard(&wild_addr) && ipv4(&other_addr) && mode == FcNetworkMode::Routed {
                return Some(format!(
                    "cannot both be bound: routed networking's {wild} listener also takes \
                     the IPv4 port, {other}'s included"
                ));
            }
        }
        None
    }

    /// Whether the host listeners of `a` and `b` can both be bound under `mode`.
    pub fn can_both_bind(a: &Self, b: &Self, mode: crate::firecracker::FcNetworkMode) -> bool {
        Self::collision(a, b, mode).is_none()
    }

    /// Refuse the first two mappings, in the order given, whose host listeners cannot
    /// both be bound under `mode` ([`Self::can_both_bind`]).
    ///
    /// Left to network setup, routed and rootless networking fail the second bind after
    /// the VM's network exists, and bridged networking installs both DNAT rules and
    /// delivers only the first. `podman run` and `snapshot run` call this on their
    /// parsed mappings before anything of the VM exists, and bridged setup again on
    /// the mappings it is given.
    pub fn require_bindable(
        mappings: &[Self],
        mode: crate::firecracker::FcNetworkMode,
    ) -> anyhow::Result<()> {
        let collide = |earlier: &Self, mapping: &Self| !Self::can_both_bind(earlier, mapping, mode);
        if let Some((earlier, mapping)) = Self::first_on_one_socket(mappings, collide) {
            let reason = Self::collision(earlier, mapping, mode).unwrap_or_default();
            anyhow::bail!("port mappings {earlier} and {mapping} {reason}");
        }
        Ok(())
    }

    /// Parse port mapping from string: [HOSTIP:]HOSTPORT:GUESTPORT[/PROTO]
    ///
    /// HOSTIP is an IP address, never a host name. An IPv6 one goes in brackets
    /// (`[::]:80:80`, `[::1]:8080:80/tcp`), because its own colons would read as field
    /// separators. `host_ip` holds the address in its canonical text form, without the
    /// brackets.
    pub fn parse(s: &str) -> anyhow::Result<Self> {
        const GRAMMAR: &str =
            "expected [HOSTIP:]HOSTPORT:GUESTPORT[/PROTO], with an IPv6 HOSTIP in brackets";

        let (host_ip, ports): (Option<String>, Vec<&str>) = match s.strip_prefix('[') {
            Some(rest) => {
                let Some((addr, ports)) = rest.split_once("]:") else {
                    anyhow::bail!("invalid port mapping {s}: {GRAMMAR}");
                };
                let Ok(addr) = addr.parse::<std::net::Ipv6Addr>() else {
                    anyhow::bail!("invalid port mapping {s}: [{addr}] is not an IPv6 address");
                };
                (Some(addr.to_string()), ports.split(':').collect())
            }
            None => {
                let mut parts: Vec<&str> = s.split(':').collect();
                let host_ip = if parts.len() == 3 {
                    let addr = parts.remove(0);
                    let Ok(addr) = addr.parse::<std::net::Ipv4Addr>() else {
                        anyhow::bail!(
                            "invalid port mapping {s}: HOSTIP {addr:?} is not an IPv4 address \
                             (an IPv6 address goes in brackets)"
                        );
                    };
                    Some(addr.to_string())
                } else {
                    None
                };
                (host_ip, parts)
            }
        };
        let &[host_port_str, guest_port_str] = ports.as_slice() else {
            anyhow::bail!("invalid port mapping {s}: {GRAMMAR}");
        };

        // Protocol suffix on the guest port; TCP when there is none.
        let (guest_port_str, proto) = match guest_port_str.split_once('/') {
            None => (guest_port_str, Protocol::Tcp),
            Some((port, "tcp")) => (port, Protocol::Tcp),
            Some((port, "udp")) => (port, Protocol::Udp),
            Some((_, other)) => {
                anyhow::bail!("invalid port mapping {s}: invalid protocol {other}")
            }
        };

        let host_port = host_port_str.parse().map_err(|_| {
            anyhow::anyhow!("invalid port mapping {s}: invalid host port {host_port_str}")
        })?;
        let guest_port = guest_port_str.parse().map_err(|_| {
            anyhow::anyhow!("invalid port mapping {s}: invalid guest port {guest_port_str}")
        })?;

        Ok(Self {
            host_ip,
            host_port,
            guest_port,
            proto,
        })
    }
}

/// The address a mapping's host listener binds, as overlap between listeners sees it.
#[derive(Debug, PartialEq, Eq)]
enum ListenAddress<'a> {
    /// No HOSTIP: the VM's own 127.x.y.z, a specific IPv4 address no other VM holds.
    VmLoopback,
    V4(std::net::Ipv4Addr),
    V6(std::net::Ipv6Addr),
    /// A HOSTIP that is not an address, which `parse` never produces. Equal only to itself.
    Unparsed(&'a str),
}

/// Generate a random MAC address for the guest
pub fn generate_mac() -> String {
    use rand::Rng;
    let mut rng = rand::thread_rng();

    // Use locally administered unicast MAC (first byte is 0x02)
    format!(
        "02:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        rng.gen::<u8>(),
        rng.gen::<u8>(),
        rng.gen::<u8>(),
        rng.gen::<u8>(),
        rng.gen::<u8>()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_accepts_every_form_of_the_grammar() {
        // (spec, host_ip, host_port, guest_port, proto)
        let cases = [
            ("8080:80", None, 8080, 80, Protocol::Tcp),
            ("8080:80/tcp", None, 8080, 80, Protocol::Tcp),
            ("8080:80/udp", None, 8080, 80, Protocol::Udp),
            (
                "127.0.0.1:8080:80",
                Some("127.0.0.1"),
                8080,
                80,
                Protocol::Tcp,
            ),
            ("0.0.0.0:53:53/udp", Some("0.0.0.0"), 53, 53, Protocol::Udp),
            ("[::]:80:80", Some("::"), 80, 80, Protocol::Tcp),
            ("[::1]:8080:80/tcp", Some("::1"), 8080, 80, Protocol::Tcp),
            (
                "[2001:db8::1]:53:53/udp",
                Some("2001:db8::1"),
                53,
                53,
                Protocol::Udp,
            ),
            // An IPv6 address is stored in its canonical text form.
            (
                "[0:0:0:0:0:0:0:1]:8080:80",
                Some("::1"),
                8080,
                80,
                Protocol::Tcp,
            ),
            (
                "[2001:DB8::1]:53:53",
                Some("2001:db8::1"),
                53,
                53,
                Protocol::Tcp,
            ),
        ];
        for (spec, host_ip, host_port, guest_port, proto) in cases {
            let parsed = PortMapping::parse(spec).unwrap_or_else(|e| panic!("{spec}: {e}"));
            let expected = PortMapping {
                host_ip: host_ip.map(str::to_string),
                host_port,
                guest_port,
                proto,
            };
            assert_eq!(parsed, expected, "{spec}");
        }
    }

    #[test]
    fn a_mapping_is_written_as_the_spec_that_parses_back_to_it() {
        for (spec, written) in [
            ("8080:80", "8080:80/tcp"),
            ("8080:80/udp", "8080:80/udp"),
            ("127.0.0.1:8080:80", "127.0.0.1:8080:80/tcp"),
            ("[::]:80:80", "[::]:80:80/tcp"),
            ("[2001:db8::1]:53:53/udp", "[2001:db8::1]:53:53/udp"),
        ] {
            let mapping = PortMapping::parse(spec).unwrap();
            assert_eq!(mapping.to_string(), written, "{spec}");
            assert_eq!(PortMapping::parse(written).unwrap(), mapping, "{spec}");
        }
    }

    /// Which pairs of mappings on one host port and protocol collide, per network mode
    /// (#1039). Bridged networking listens on the VM's veth address whatever HOSTIP
    /// says, so every such pair collides there. Rootless (pasta) and routed networking
    /// listen on HOSTIP, or on the VM's own 127.x.y.z without one; a wildcard holds
    /// every address of its family, routed's `[::]` also holds the IPv4 port
    /// (IPV6_V6ONLY cleared) and pasta's does not. An IPv4-mapped IPv6 address is its
    /// IPv4 address.
    #[test]
    fn bindable_matrix_covers_every_pair_on_one_port_in_every_mode() {
        use crate::firecracker::FcNetworkMode::{Bridged, Rootless, Routed};
        // (a, b, collide under bridged, rootless, routed)
        let cases: &[(&str, &str, [bool; 3])] = &[
            // One address twice.
            ("8080:80", "8080:443", [true, true, true]),
            (
                "127.0.0.1:8080:80",
                "127.0.0.1:8080:443",
                [true, true, true],
            ),
            ("[::]:8080:80", "[::]:8080:443", [true, true, true]),
            (
                "[::1]:8080:80",
                "[0:0:0:0:0:0:0:1]:8080:443",
                [true, true, true],
            ),
            // 0.0.0.0 and any IPv4 address, or no HOSTIP.
            ("0.0.0.0:8080:80", "127.0.0.1:8080:443", [true, true, true]),
            ("0.0.0.0:8080:80", "192.0.2.1:8080:443", [true, true, true]),
            ("0.0.0.0:8080:80", "8080:443", [true, true, true]),
            // [::] and any IPv6 address.
            ("[::]:8080:80", "[::1]:8080:443", [true, true, true]),
            ("[::]:8080:80", "[2001:db8::1]:8080:443", [true, true, true]),
            // [::] and any IPv4 address, or no HOSTIP: routed's [::] is dual-stack.
            ("[::]:8080:80", "127.0.0.1:8080:443", [true, false, true]),
            ("[::]:8080:80", "0.0.0.0:8080:443", [true, false, true]),
            ("[::]:8080:80", "8080:443", [true, false, true]),
            ("[::]:53:53/udp", "0.0.0.0:53:53/udp", [true, false, true]),
            // IPv4-mapped IPv6 addresses are their IPv4 address.
            (
                "[::ffff:127.0.0.1]:8080:80",
                "127.0.0.1:8080:443",
                [true, true, true],
            ),
            (
                "[::ffff:0.0.0.0]:8080:80",
                "192.0.2.1:8080:443",
                [true, true, true],
            ),
            (
                "[::ffff:192.0.2.1]:8080:80",
                "[::]:8080:443",
                [true, false, true],
            ),
            (
                "[::ffff:192.0.2.1]:8080:80",
                "[::ffff:192.0.2.2]:8080:443",
                [true, false, false],
            ),
            // Two specific addresses, or a specific address and the VM's own loopback.
            (
                "127.0.0.1:8080:80",
                "127.0.0.2:8080:443",
                [true, false, false],
            ),
            ("127.0.0.1:8080:80", "8080:443", [true, false, false]),
            ("[::1]:8080:80", "127.0.0.1:8080:443", [true, false, false]),
            ("[::1]:8080:80", "0.0.0.0:8080:443", [true, false, false]),
            ("[::1]:8080:80", "8080:443", [true, false, false]),
            (
                "[::1]:8080:80",
                "[2001:db8::1]:8080:443",
                [true, false, false],
            ),
            // Another host port or another protocol is another socket everywhere.
            ("0.0.0.0:8080:80", "[::]:8081:80", [false, false, false]),
            ("0.0.0.0:8080:80", "8080:80/udp", [false, false, false]),
            ("[::]:8080:80", "8080:80/udp", [false, false, false]),
            ("8080:80", "8081:80", [false, false, false]),
        ];
        let mut wrong = Vec::new();
        for &(a, b, collide) in cases {
            let (a, b) = (
                PortMapping::parse(a).unwrap(),
                PortMapping::parse(b).unwrap(),
            );
            for (mode, collide) in [Bridged, Rootless, Routed].into_iter().zip(collide) {
                for (first, second) in [(&a, &b), (&b, &a)] {
                    if PortMapping::can_both_bind(first, second, mode) == collide {
                        wrong.push(format!(
                            "{mode:?}: {first} then {second} must {}",
                            if collide { "collide" } else { "both bind" }
                        ));
                    }
                    let checked =
                        PortMapping::require_bindable(&[first.clone(), second.clone()], mode);
                    match (collide, checked) {
                        (true, Err(error)) => {
                            let error = error.to_string();
                            if !(error.contains(&first.to_string())
                                && error.contains(&second.to_string()))
                            {
                                wrong
                                    .push(format!("{mode:?}: refusal does not name both: {error}"));
                            }
                        }
                        (false, Ok(())) => {}
                        (true, Ok(())) => {
                            wrong.push(format!("{mode:?}: [{first}, {second}] not refused"))
                        }
                        (false, Err(error)) => {
                            wrong.push(format!("{mode:?}: [{first}, {second}] refused: {error}"))
                        }
                    }
                }
            }
        }
        assert!(wrong.is_empty(), "{}", wrong.join("\n"));
    }

    /// The refusal names the first colliding pair in the order given, and a list of
    /// mappings that collide nowhere passes.
    #[test]
    fn require_bindable_names_the_first_colliding_pair() {
        use crate::firecracker::FcNetworkMode::Routed;
        let mappings: Vec<PortMapping> = ["8443:443", "0.0.0.0:8080:80", "[::]:9090:90", "8080:81"]
            .iter()
            .map(|spec| PortMapping::parse(spec).unwrap())
            .collect();
        let error = PortMapping::require_bindable(&mappings, Routed)
            .expect_err("0.0.0.0 holds the VM's loopback address")
            .to_string();
        assert!(
            error.contains("port mappings 0.0.0.0:8080:80/tcp and 8080:81/tcp"),
            "{error}"
        );
        PortMapping::require_bindable(&mappings[..3], Routed).unwrap();
    }

    /// Every rejection names the spec it rejected, so one bad entry in a
    /// comma-separated --publish can be found.
    #[test]
    fn parse_rejects_malformed_specs_and_names_them() {
        let cases = [
            "8080",                // one field
            "1:2:3:4",             // too many fields
            "::1:8080:80",         // IPv6 without brackets
            "[::1:8080:80",        // no closing bracket
            "::1]:8080:80",        // no opening bracket
            "[::1]8080:80",        // no ':' after the bracket
            "[]:8080:80",          // nothing in the brackets
            "[127.0.0.1]:8080:80", // brackets around an IPv4 address
            "[::g]:8080:80",       // not an address
            "[[::1]]:8080:80",     // doubled brackets
            "[::1]:8080",          // one port after the address
            "[::1]:8080:80:90",    // three ports after the address
            "[::1]:8080:80]",      // stray bracket in a port
            "8080:80/sctp",        // unknown protocol
            "http:80",             // host port is not a number
            "8080:http",           // guest port is not a number
            "65536:80",            // host port out of range
            "localhost:8080:80",   // HOSTIP is a host name
            ":8080:80",            // HOSTIP is empty
            "127.1:8080:80",       // HOSTIP is not a whole IPv4 address
            "256.0.0.1:8080:80",   // HOSTIP is out of range
        ];
        for spec in cases {
            match PortMapping::parse(spec) {
                Ok(parsed) => panic!("{spec} must be rejected, parsed as {parsed:?}"),
                Err(e) => assert!(e.to_string().contains(spec), "{spec}: {e}"),
            }
        }
    }
}
