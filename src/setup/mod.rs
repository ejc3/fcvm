pub mod kernel;
pub mod rootfs;

pub use kernel::ensure_kernel;
pub use rootfs::{ensure_fc_agent_initrd, ensure_rootfs};
