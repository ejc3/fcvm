pub mod kernel;
pub mod rootfs;

pub use kernel::{ensure_kernel, get_kernel_path, get_kernel_url_hash, install_host_kernel};
pub use rootfs::{
    ensure_fc_agent_initrd, ensure_rootfs, get_kernel_profile, KernelProfile,
};
