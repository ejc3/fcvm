pub mod bridged;
pub mod namespace;
pub mod portmap;
pub mod slirp;
pub mod types;
pub mod veth;

pub use bridged::BridgedNetwork;
pub use slirp::SlirpNetwork;
pub use types::*;

// Backwards compatibility alias
#[deprecated(since = "0.4.0", note = "Renamed to BridgedNetwork for clarity")]
pub type RootlessNetwork = BridgedNetwork;

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
