use std::path::PathBuf;
use std::sync::OnceLock;

/// Global base directory for writable data, set once at startup
static DATA_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Default base directory (btrfs mount for CoW support)
const DEFAULT_BASE_DIR: &str = "/mnt/fcvm-btrfs";

/// Initialize base directory from CLI argument or environment variable.
/// Must be called before any path functions are used.
/// If not called, base_dir() will use the default or FCVM_BASE_DIR env var.
pub fn init_base_dir(path: Option<&str>) {
    let dir = match path {
        Some(p) => PathBuf::from(shellexpand::tilde(p).as_ref()),
        None => {
            // Check environment variable first
            if let Ok(configured) = std::env::var("FCVM_BASE_DIR") {
                PathBuf::from(shellexpand::tilde(&configured).as_ref())
            } else {
                PathBuf::from(DEFAULT_BASE_DIR)
            }
        }
    };
    // Ignore if already set (e.g., in tests)
    let _ = DATA_DIR.set(dir);
}

/// Base directory for fcvm data.
/// Defaults to `/mnt/fcvm-btrfs` but can be overridden with `--base-dir` or `FCVM_BASE_DIR`.
pub fn base_dir() -> PathBuf {
    DATA_DIR
        .get_or_init(|| {
            // Check environment variable first
            if let Ok(configured) = std::env::var("FCVM_BASE_DIR") {
                return PathBuf::from(shellexpand::tilde(&configured).as_ref());
            }
            PathBuf::from(DEFAULT_BASE_DIR)
        })
        .clone()
}

/// Directory for kernel images (vmlinux-*.bin files).
pub fn kernel_dir() -> PathBuf {
    base_dir().join("kernels")
}

/// Directory for rootfs images (layer2-*.raw files).
pub fn rootfs_dir() -> PathBuf {
    base_dir().join("rootfs")
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
