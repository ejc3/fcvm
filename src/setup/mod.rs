pub mod kernel;
pub mod rootfs;

pub use kernel::ensure_kernel;
pub use rootfs::ensure_rootfs;
