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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    Tcp,
    Udp,
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

    /// Refuse two mappings that claim the same host address and port for one protocol.
    ///
    /// Left to network setup, routed and rootless networking fail the second bind after
    /// the VM's network exists, and bridged networking installs both DNAT rules and
    /// delivers only the first.
    pub fn require_distinct_host_sockets(mappings: &[Self]) -> anyhow::Result<()> {
        let same_socket = |earlier: &Self, mapping: &Self| {
            earlier.host_ip == mapping.host_ip
                && earlier.host_port == mapping.host_port
                && earlier.proto == mapping.proto
        };
        if let Some((earlier, mapping)) = Self::first_on_one_socket(mappings, same_socket) {
            anyhow::bail!(
                "port mappings {earlier} and {mapping} claim the same host address and port"
            );
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

    /// One host address and port, for one protocol, goes to one guest port.
    #[test]
    fn two_mappings_cannot_claim_one_host_socket() {
        let mappings = |specs: &[&str]| -> Vec<PortMapping> {
            specs
                .iter()
                .map(|spec| PortMapping::parse(spec).unwrap())
                .collect()
        };
        for specs in [
            &["8080:80", "8443:443", "8080:443"][..],
            &["[::]:80:80", "[::]:80:8080"][..],
            &["127.0.0.1:53:53/udp", "127.0.0.1:53:5353/udp"][..],
        ] {
            let error = PortMapping::require_distinct_host_sockets(&mappings(specs))
                .expect_err("one host socket is claimed twice")
                .to_string();
            let (first, last) = (
                mappings(specs)[0].to_string(),
                mappings(specs)[specs.len() - 1].to_string(),
            );
            assert!(
                error.contains(&first) && error.contains(&last),
                "{specs:?}: {error}"
            );
        }
        // Another protocol, another port or another address is another socket.
        for specs in [
            &["8080:80", "8080:80/udp"][..],
            &["8080:80", "8081:80"][..],
            &["127.0.0.1:8080:80", "8080:80", "[::1]:8080:80"][..],
        ] {
            PortMapping::require_distinct_host_sockets(&mappings(specs))
                .unwrap_or_else(|e| panic!("{specs:?}: {e}"));
        }
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
