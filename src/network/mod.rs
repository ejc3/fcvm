pub mod bridged;
pub mod namespace;
pub mod portmap;
pub mod slirp;
pub mod types;
pub mod veth;

pub use bridged::BridgedNetwork;
pub use slirp::SlirpNetwork;
pub use types::*;

use anyhow::Result;
use std::net::IpAddr;

/// Network manager trait
#[async_trait::async_trait]
pub trait NetworkManager: Send + Sync {
    /// Setup network before VM start
    async fn setup(&mut self) -> Result<NetworkConfig>;

    /// Post-VM-start setup (e.g., start slirp4netns after Firecracker creates namespace)
    /// Called with the PID of the VM process (Firecracker or unshare wrapper).
    /// Default implementation does nothing.
    async fn post_start(&mut self, _vm_pid: u32) -> Result<()> {
        Ok(())
    }

    /// Cleanup network after VM stop
    async fn cleanup(&mut self) -> Result<()>;

    /// Get the TAP device name
    fn tap_device(&self) -> &str;

    /// Get a reference to Any for downcasting
    fn as_any(&self) -> &dyn std::any::Any;
}

/// Get host DNS servers for VMs
///
/// Returns DNS servers that VMs can use. Checks /run/systemd/resolve/resolv.conf
/// first (which has real upstream DNS when using systemd-resolved), then falls
/// back to /etc/resolv.conf.
///
/// Returns error if only localhost DNS (127.0.0.53) is available, since VMs
/// can't use the host's stub resolver.
pub fn get_host_dns_servers() -> anyhow::Result<Vec<String>> {
    // Try systemd-resolved upstream config first (has real DNS servers)
    let resolv_content = std::fs::read_to_string("/run/systemd/resolve/resolv.conf")
        .or_else(|_| std::fs::read_to_string("/etc/resolv.conf"))
        .map_err(|e| anyhow::anyhow!("failed to read resolv.conf: {}", e))?;

    let servers: Vec<String> = resolv_content
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            match (parts.next(), parts.next()) {
                (Some("nameserver"), Some(addr)) => Some(addr.to_string()),
                _ => None,
            }
        })
        .filter(|s| !is_loopback_nameserver(s))
        .collect();

    if servers.is_empty() {
        anyhow::bail!(
            "no usable DNS servers found. If using systemd-resolved, mount \
             /run/systemd/resolve:/run/systemd/resolve:ro in container"
        );
    }

    Ok(servers)
}

fn is_loopback_nameserver(server: &str) -> bool {
    match server.parse::<IpAddr>() {
        Ok(ip) => ip.is_loopback(),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_nameserver_parsing_and_loopback_filtering() {
        let resolv = r#"
            # comment
            nameserver 127.0.0.53
            nameserver ::1
            nameserver    8.8.8.8
            nameserver	2001:4860:4860::8888
            options edns0
        "#;

        let servers: Vec<String> = resolv
            .lines()
            .filter_map(|line| {
                let mut parts = line.split_whitespace();
                match (parts.next(), parts.next()) {
                    (Some("nameserver"), Some(addr)) => Some(addr.to_string()),
                    _ => None,
                }
            })
            .filter(|s| !is_loopback_nameserver(s))
            .collect();

        assert_eq!(servers, vec!["8.8.8.8", "2001:4860:4860::8888"]);
    }

    #[test]
    fn test_get_host_dns_servers() {
        let result = get_host_dns_servers();
        println!("Host DNS servers: {:?}", result);
        // This may fail in containers without the systemd-resolve mount
        if let Ok(servers) = result {
            assert!(!servers.is_empty());
            for server in &servers {
                assert!(!is_loopback_nameserver(server), "Should filter localhost");
            }
        }
    }
}
