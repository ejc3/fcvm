use std::path::PathBuf;
use std::sync::OnceLock;

/// Global base directory, set once at startup
static BASE_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Initialize base directory from CLI argument or environment variable.
/// Must be called before any path functions are used.
/// If not called, base_dir() will use the default or FCVM_BASE_DIR env var.
pub fn init_base_dir(path: Option<&str>) {
    let dir = match path {
        Some(p) => PathBuf::from(shellexpand::tilde(p).as_ref()),
        None => {
            let default = "/mnt/fcvm-btrfs".to_string();
            let configured = std::env::var("FCVM_BASE_DIR").unwrap_or(default);
            PathBuf::from(shellexpand::tilde(&configured).as_ref())
        }
    };
    // Ignore if already set (e.g., in tests)
    let _ = BASE_DIR.set(dir);
}

/// Base directory for all fcvm data.
/// Defaults to `/mnt/fcvm-btrfs` but can be overridden with `--base-dir` or `FCVM_BASE_DIR`.
pub fn base_dir() -> PathBuf {
    BASE_DIR
        .get_or_init(|| {
            let default = "/mnt/fcvm-btrfs".to_string();
            let configured = std::env::var("FCVM_BASE_DIR").unwrap_or(default);
            PathBuf::from(shellexpand::tilde(&configured).as_ref())
        })
        .clone()
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
