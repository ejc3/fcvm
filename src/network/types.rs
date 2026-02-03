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
    /// For rootless mode: port on loopback_ip where guest port 80 is forwarded
    #[serde(default)]
    pub health_check_port: Option<u16>,
    /// Auto-generated health check URL based on network type
    /// Bridged: http://{guest_ip}:80/
    /// Rootless: http://{loopback_ip}:8080/
    #[serde(default)]
    pub health_check_url: Option<String>,
    /// DNS server for the guest to use
    /// Bridged: host_ip (dnsmasq on veth)
    /// Rootless: 10.0.2.3 (slirp4netns built-in DNS)
    #[serde(default)]
    pub dns_server: Option<String>,
    /// Guest IPv6 address (for rootless networking with IPv6)
    #[serde(default)]
    pub guest_ipv6: Option<String>,
    /// Gateway IPv6 address (for rootless networking with IPv6)
    #[serde(default)]
    pub host_ipv6: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PortMapping {
    pub host_ip: Option<String>,
    pub host_port: u16,
    pub guest_port: u16,
    pub proto: Protocol,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

impl PortMapping {
    /// Parse port mapping from string: [HOSTIP:]HOSTPORT:GUESTPORT[/PROTO]
    pub fn parse(s: &str) -> anyhow::Result<Self> {
        let parts: Vec<&str> = s.split(':').collect();

        let (host_ip, host_port_str, guest_port_str) = match parts.len() {
            2 => (None, parts[0], parts[1]),
            3 => (Some(parts[0].to_string()), parts[1], parts[2]),
            _ => anyhow::bail!("invalid port mapping format: {}", s),
        };

        // Parse protocol suffix from guest_port
        let (guest_port_str, proto) = if let Some(idx) = guest_port_str.find('/') {
            let (port, proto_str) = guest_port_str.split_at(idx);
            let proto = match &proto_str[1..] {
                "tcp" => Protocol::Tcp,
                "udp" => Protocol::Udp,
                _ => anyhow::bail!("invalid protocol: {}", &proto_str[1..]),
            };
            (port, proto)
        } else {
            (guest_port_str, Protocol::Tcp) // default to TCP
        };

        let host_port = host_port_str
            .parse()
            .map_err(|_| anyhow::anyhow!("invalid host port: {}", host_port_str))?;
        let guest_port = guest_port_str
            .parse()
            .map_err(|_| anyhow::anyhow!("invalid guest port: {}", guest_port_str))?;

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
    fn test_parse_port_mapping() {
        let pm = PortMapping::parse("8080:80").unwrap();
        assert_eq!(pm.host_port, 8080);
        assert_eq!(pm.guest_port, 80);
        assert_eq!(pm.proto, Protocol::Tcp);
        assert!(pm.host_ip.is_none());

        let pm = PortMapping::parse("127.0.0.1:8080:80").unwrap();
        assert_eq!(pm.host_ip, Some("127.0.0.1".to_string()));
        assert_eq!(pm.host_port, 8080);
        assert_eq!(pm.guest_port, 80);

        let pm = PortMapping::parse("8080:80/udp").unwrap();
        assert_eq!(pm.proto, Protocol::Udp);

        let pm = PortMapping::parse("0.0.0.0:53:53/udp").unwrap();
        assert_eq!(pm.host_ip, Some("0.0.0.0".to_string()));
        assert_eq!(pm.host_port, 53);
        assert_eq!(pm.guest_port, 53);
        assert_eq!(pm.proto, Protocol::Udp);
    }
}
