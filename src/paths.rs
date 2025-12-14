use std::path::PathBuf;
use std::sync::OnceLock;
use tracing::info;

/// Global base directory for writable data, set once at startup
static DATA_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Default base directory (btrfs mount for CoW support)
const DEFAULT_BASE_DIR: &str = "/mnt/fcvm-btrfs";

/// User data directory for rootless mode (user-writable)
fn user_data_dir() -> PathBuf {
    // Use ~/.local/share/fcvm for user-specific data
    if let Some(home) = std::env::var_os("HOME") {
        PathBuf::from(home).join(".local/share/fcvm")
    } else {
        // Last resort: /tmp/fcvm-{uid}
        let uid = unsafe { libc::getuid() };
        PathBuf::from(format!("/tmp/fcvm-{}", uid))
    }
}

/// Check if directory exists and is writable by current user
fn is_writable(path: &PathBuf) -> bool {
    if !path.exists() {
        return false;
    }
    // Check write permission using access()
    use std::os::unix::ffi::OsStrExt;
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok();
    if let Some(path_cstr) = c_path {
        unsafe { libc::access(path_cstr.as_ptr(), libc::W_OK) == 0 }
    } else {
        false
    }
}

/// Initialize base directory from CLI argument or environment variable.
/// Must be called before any path functions are used.
/// If not called, base_dir() will use the default or FCVM_BASE_DIR env var.
///
/// Auto-fallback for rootless: If no explicit path is given and the default
/// directory is not writable, writable data (vm-disks, state) goes to ~/.local/share/fcvm
/// while kernel/rootfs are still read from the default system location.
pub fn init_base_dir(path: Option<&str>) {
    let dir = match path {
        Some(p) => PathBuf::from(shellexpand::tilde(p).as_ref()),
        None => {
            // Check environment variable first
            if let Ok(configured) = std::env::var("FCVM_BASE_DIR") {
                PathBuf::from(shellexpand::tilde(&configured).as_ref())
            } else {
                // Try default, fall back to user directory if not writable
                let default = PathBuf::from(DEFAULT_BASE_DIR);
                if is_writable(&default) {
                    default
                } else {
                    let fallback = user_data_dir();
                    info!(
                        target: "paths",
                        "Default base dir {} not writable, using {} for VM data",
                        DEFAULT_BASE_DIR,
                        fallback.display()
                    );
                    fallback
                }
            }
        }
    };
    // Ignore if already set (e.g., in tests)
    let _ = DATA_DIR.set(dir);
}

/// Base directory for fcvm data.
/// Defaults to `/mnt/fcvm-btrfs` but can be overridden with `--base-dir` or `FCVM_BASE_DIR`.
/// If the default is not writable, automatically falls back to ~/.local/share/fcvm for
/// writable data, while kernel/rootfs are read from the system location.
pub fn base_dir() -> PathBuf {
    DATA_DIR
        .get_or_init(|| {
            // Check environment variable first
            if let Ok(configured) = std::env::var("FCVM_BASE_DIR") {
                return PathBuf::from(shellexpand::tilde(&configured).as_ref());
            }
            // Try default, fall back to user directory if not writable
            let default = PathBuf::from(DEFAULT_BASE_DIR);
            if is_writable(&default) {
                default
            } else {
                user_data_dir()
            }
        })
        .clone()
}

/// Directory for kernel images.
/// Falls back to system location if kernel not found in user data directory.
pub fn kernel_dir() -> PathBuf {
    let user_dir = base_dir().join("kernels");
    // Check if kernel FILE exists in user dir (not just the directory)
    if user_dir.join("vmlinux.bin").exists() {
        return user_dir;
    }
    // Fall back to system location if kernel exists there
    let system_dir = PathBuf::from(DEFAULT_BASE_DIR).join("kernels");
    if system_dir.join("vmlinux.bin").exists() {
        return system_dir;
    }
    // Return user dir (will be created if needed)
    user_dir
}

/// Directory for rootfs images.
/// Falls back to system location if rootfs not found in user data directory.
pub fn rootfs_dir() -> PathBuf {
    let user_dir = base_dir().join("rootfs");
    // Check if rootfs FILE exists in user dir (not just the directory)
    if user_dir.join("base.ext4").exists() {
        return user_dir;
    }
    // Fall back to system location if rootfs exists there
    let system_dir = PathBuf::from(DEFAULT_BASE_DIR).join("rootfs");
    if system_dir.join("base.ext4").exists() {
        return system_dir;
    }
    // Return user dir (will be created if needed)
    user_dir
}

/// Path to base rootfs image.
/// Falls back to system location if not found in user data directory.
pub fn base_rootfs() -> PathBuf {
    let user_path = base_dir().join("rootfs").join("base.ext4");
    if user_path.exists() {
        return user_path;
    }
    // Fall back to system location
    let system_path = PathBuf::from(DEFAULT_BASE_DIR).join("rootfs").join("base.ext4");
    if system_path.exists() {
        return system_path;
    }
    // Return user path (setup will create it)
    user_path
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
