use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkConfig {
    pub tap_device: String,
    pub guest_mac: String,
    pub guest_ip: Option<String>,
    pub host_ip: Option<String>,
    pub host_veth: Option<String>,
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
