pub mod bridged;
pub mod egress_proxy;
pub mod namespace;
pub mod pasta;
pub mod portmap;
pub mod routed;
pub mod tcp_proxy;
pub mod types;
pub mod veth;

pub use bridged::BridgedNetwork;
pub use pasta::PastaNetwork;
pub use routed::RoutedNetwork;
pub use types::*;

use anyhow::{Context, Result};
use std::net::IpAddr;

/// Acquire a cross-process lock serializing host-global bridged network configuration.
///
/// Bridged networking mutates state shared by every fcvm process on the host
/// (veth host IPs, global MASQUERADE rules). The check-then-act sequences on that
/// state must hold one of these locks so two fcvm processes cannot interleave.
///
/// Lock files live in the state directory, following the same flock pattern as
/// `loopback-ip.lock` (world-writable so root and non-root processes can coordinate,
/// never deleted to avoid the recreate-while-locked race).
///
/// Lock ordering: `bridged-subnet.lock` may be held while `bridged-nat.lock` is
/// acquired (setup error paths that call cleanup()); the reverse never happens.
pub(crate) async fn acquire_host_network_lock(
    name: &str,
) -> Result<nix::fcntl::Flock<std::fs::File>> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    let dir = crate::paths::state_dir();
    tokio::fs::create_dir_all(&dir)
        .await
        .with_context(|| format!("creating state directory {}", dir.display()))?;
    let path = dir.join(name);

    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o666)
        .open(&path)
        .with_context(|| format!("opening lock file {}", path.display()))?;
    // Force permissions regardless of umask (only effective if we own the file or are root)
    let _ = file.set_permissions(std::fs::Permissions::from_mode(0o666));

    // Acquire in spawn_blocking: flock blocks until the lock is free, and these
    // critical sections span multiple subprocess invocations, so don't pin a
    // tokio worker thread while waiting.
    let lock_name = name.to_string();
    tokio::task::spawn_blocking(move || {
        use nix::fcntl::{Flock, FlockArg};
        Flock::lock(file, FlockArg::LockExclusive)
            .map_err(|(_, err)| anyhow::anyhow!("flock failed: {}", err))
    })
    .await
    .context("joining lock acquisition task")?
    .with_context(|| format!("acquiring host network lock {}", lock_name))
}

/// Network manager trait
#[async_trait::async_trait]
pub trait NetworkManager: Send + Sync {
    /// Setup network before VM start
    async fn setup(&mut self) -> Result<NetworkConfig>;

    /// Post-VM-start setup (e.g., start pasta after Firecracker creates namespace)
    /// Called with the PID of the VM process (Firecracker or unshare wrapper).
    /// Default implementation does nothing.
    async fn post_start(&mut self, _vm_pid: u32) -> Result<()> {
        Ok(())
    }

    /// SIGKILL any long-lived helper process this network owns, WITHOUT waiting for it.
    ///
    /// Lets teardown signal the network helper in the same instant as the VMM and the
    /// namespace holder, so the helper's exit overlaps the VMM's address-space reclaim
    /// instead of queueing behind it; [`Self::cleanup`] then reaps a process that is
    /// already dead. Purely an optimization — `cleanup()` still kills and reaps on its
    /// own, so calling this is optional and calling it twice is harmless.
    ///
    /// Default: no-op (bridged and routed have no helper process).
    fn start_kill_processes(&mut self) {}

    /// Cleanup network after VM stop
    async fn cleanup(&mut self) -> Result<()>;

    /// Get the TAP device name
    fn tap_device(&self) -> &str;

    /// Verify port forwarding works end-to-end after VM is running.
    ///
    /// Called after snapshot restore when the guest is active and fc-agent has reconnected.
    /// Verifies that data actually flows through the forwarding path, not just that
    /// the listening socket exists. Default implementation does nothing (bridged DNAT
    /// is kernel-level and works immediately).
    async fn verify_port_forwarding(&self) -> Result<()> {
        Ok(())
    }

    /// Get a reference to Any for downcasting
    fn as_any(&self) -> &dyn std::any::Any;
}

/// Serializes tests whose behavior depends on resolving `ip` through the
/// process-global PATH. Plain `cargo test` runs every test in one process, so
/// a test that points PATH at a fake `ip` races any sibling that spawns the
/// real one by name (nextest's process-per-test isolation never sees this).
/// Both kinds of test hold this lock: the PATH mutator and the by-name
/// spawner. tokio's Mutex because the mutator holds it across an await, and
/// it does not poison when a holder's assertion panics.
#[cfg(test)]
pub(crate) static PATH_IP_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

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
