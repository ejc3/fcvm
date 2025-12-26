use std::path::PathBuf;
use std::sync::OnceLock;

/// Global directory for mutable per-instance data (vm-disks, state, snapshots)
static DATA_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Global directory for shared content-addressed assets
static ASSETS_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Default base directory when no config file exists
const DEFAULT_BASE_DIR: &str = "/mnt/fcvm-btrfs";

/// Initialize directories from config file.
/// Must be called before any path functions are used.
///
/// For inception support, each nesting level uses a different data_dir
/// while sharing the same assets_dir for content-addressed files.
pub fn init_from_config() {
    let (config, _, _) = crate::setup::rootfs::load_config(None)
        .expect("Failed to load config - run 'fcvm setup --generate-config' first");
    let _ = DATA_DIR.set(PathBuf::from(&config.paths.data_dir));
    let _ = ASSETS_DIR.set(PathBuf::from(&config.paths.assets_dir));
}

/// Initialize directories with default values (no config file required).
/// Used for commands like --generate-config that don't need an existing config.
pub fn init_with_defaults() {
    let _ = DATA_DIR.set(PathBuf::from(DEFAULT_BASE_DIR));
    let _ = ASSETS_DIR.set(PathBuf::from(DEFAULT_BASE_DIR));
}

/// Initialize directories with explicit paths (for testing).
/// This allows tests to use custom directories without requiring a config file.
pub fn init_with_paths(data_dir: impl Into<PathBuf>, assets_dir: impl Into<PathBuf>) {
    let _ = DATA_DIR.set(data_dir.into());
    let _ = ASSETS_DIR.set(assets_dir.into());
}

/// Directory for mutable per-instance data (vm-disks, state, snapshots).
/// Configure via `paths.data_dir` in rootfs-config.toml for inception nesting.
pub fn data_dir() -> PathBuf {
    DATA_DIR
        .get_or_init(|| {
            let (config, _, _) =
                crate::setup::rootfs::load_config(None).expect("Failed to load config");
            PathBuf::from(&config.paths.data_dir)
        })
        .clone()
}

/// Directory for shared content-addressed assets (kernels, rootfs, initrd, image-cache).
/// Configure via `paths.assets_dir` in rootfs-config.toml.
pub fn assets_dir() -> PathBuf {
    ASSETS_DIR
        .get_or_init(|| {
            let (config, _, _) =
                crate::setup::rootfs::load_config(None).expect("Failed to load config");
            PathBuf::from(&config.paths.assets_dir)
        })
        .clone()
}

// === Content-addressed assets (use assets_dir) ===

/// Directory for kernel images (vmlinux-*.bin files).
pub fn kernel_dir() -> PathBuf {
    assets_dir().join("kernels")
}

/// Directory for rootfs images (layer2-*.raw files).
pub fn rootfs_dir() -> PathBuf {
    assets_dir().join("rootfs")
}

/// Directory for initrd images (fc-agent-*.initrd files).
pub fn initrd_dir() -> PathBuf {
    assets_dir().join("initrd")
}

/// Directory for container image cache (sha256:* directories).
pub fn image_cache_dir() -> PathBuf {
    assets_dir().join("image-cache")
}

/// Directory for downloaded files (ubuntu cloud image, etc).
pub fn cache_dir() -> PathBuf {
    assets_dir().join("cache")
}

// === Mutable per-instance data (use data_dir) ===

/// Directory for VM state files
pub fn state_dir() -> PathBuf {
    data_dir().join("state")
}

/// Directory for VM runtime data (disks, sockets, logs)
pub fn vm_runtime_dir(vm_id: &str) -> PathBuf {
    data_dir().join("vm-disks").join(vm_id)
}

/// Directory for snapshot data
pub fn snapshot_dir() -> PathBuf {
    data_dir().join("snapshots")
}
