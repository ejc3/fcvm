pub mod kernel;
pub mod rootfs;

pub use kernel::{ensure_kernel, ensure_profile_kernel, install_host_kernel};
pub use rootfs::{
    detect_kernel_profile, ensure_fc_agent_initrd, ensure_rootfs, get_active_kernel_profile,
    get_kernel_profile, get_profile_kernel_path, KernelProfile,
};
