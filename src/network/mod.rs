pub mod namespace;
pub mod portmap;
pub mod rootless;
pub mod types;
pub mod veth;

pub use rootless::RootlessNetwork;
pub use types::*;

use anyhow::Result;

/// Network manager trait
#[async_trait::async_trait]
pub trait NetworkManager: Send + Sync {
    /// Setup network before VM start
    async fn setup(&mut self) -> Result<NetworkConfig>;

    /// Cleanup network after VM stop
    async fn cleanup(&mut self) -> Result<()>;

    /// Get the TAP device name
    fn tap_device(&self) -> &str;
}
