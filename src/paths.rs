use std::path::PathBuf;

/// Base directory for all fcvm data
/// Uses btrfs mount for CoW reflink support
pub fn base_dir() -> PathBuf {
    PathBuf::from("/mnt/fcvm-btrfs")
}

/// Directory for kernel images
pub fn kernel_dir() -> PathBuf {
    base_dir().join("kernels")
}

/// Directory for rootfs images
pub fn rootfs_dir() -> PathBuf {
    base_dir().join("rootfs")
}

/// Path to base rootfs image
pub fn base_rootfs() -> PathBuf {
    rootfs_dir().join("base.ext4")
}

/// Directory for VM state files
pub fn state_dir() -> PathBuf {
    base_dir().join("state")
}

/// Directory for VM runtime data (disks, sockets, logs)
pub fn vm_runtime_dir(vm_id: &str) -> PathBuf {
    base_dir().join("vm-disks").join(vm_id)
}

/// Directory for snapshot data
pub fn snapshot_dir() -> PathBuf {
    base_dir().join("snapshots")
}
