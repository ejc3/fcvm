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

/// Read DNS servers from host system
///
/// Parses /etc/resolv.conf to extract nameserver entries. If only localhost
/// addresses are found (indicating systemd-resolved), falls back to reading
/// /run/systemd/resolve/resolv.conf for the real upstream DNS servers.
///
/// Returns an empty Vec if no DNS servers can be determined.
pub fn get_host_dns_servers() -> Vec<String> {
    // Try /etc/resolv.conf first
    let resolv = std::fs::read_to_string("/etc/resolv.conf").unwrap_or_default();

    let servers: Vec<String> = resolv
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            line.strip_prefix("nameserver ")
                .map(|s| s.trim().to_string())
        })
        .collect();

    // If only localhost (systemd-resolved), try real config
    if servers.iter().all(|s| s.starts_with("127.")) {
        if let Ok(real) = std::fs::read_to_string("/run/systemd/resolve/resolv.conf") {
            let real_servers: Vec<String> = real
                .lines()
                .filter_map(|line| {
                    line.trim()
                        .strip_prefix("nameserver ")
                        .map(|s| s.trim().to_string())
                })
                .filter(|s| !s.starts_with("127."))
                .collect();
            if !real_servers.is_empty() {
                return real_servers;
            }
        }
    }

    servers
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_host_dns_servers() {
        let servers = get_host_dns_servers();
        println!("DNS servers: {:?}", servers);
        // Should find at least one non-localhost server on this system
        assert!(!servers.is_empty(), "Expected to find DNS servers");
        // Should not include localhost (127.x.x.x) since we're on systemd-resolved
        assert!(
            servers.iter().all(|s| !s.starts_with("127.")),
            "Should have filtered out localhost DNS"
        );
    }
}
