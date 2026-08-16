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
/// For nested VM support, each nesting level uses a different data_dir
/// while sharing the same assets_dir for content-addressed files.
///
/// The FCVM_DATA_DIR environment variable overrides config data_dir,
/// allowing test isolation between root/container/rootless modes.
pub fn init_from_config() {
    // Check for env var override first (test isolation)
    if let Ok(data_dir) = std::env::var("FCVM_DATA_DIR") {
        let (config, _, _) = crate::setup::rootfs::load_config(None)
            .expect("Failed to load config - run 'fcvm setup --generate-config' first");
        let _ = DATA_DIR.set(PathBuf::from(data_dir));
        let _ = ASSETS_DIR.set(PathBuf::from(&config.paths.assets_dir));
        return;
    }

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
/// Configure via `paths.data_dir` in rootfs-config.toml for nested VM nesting.
/// FCVM_DATA_DIR environment variable overrides config value.
pub fn data_dir() -> PathBuf {
    DATA_DIR
        .get_or_init(|| {
            // Check env var first (test isolation)
            if let Ok(data_dir) = std::env::var("FCVM_DATA_DIR") {
                return PathBuf::from(data_dir);
            }
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

/// Assets directory *only if* it has already been initialised.
///
/// Unlike [`assets_dir`] this never loads the config (and so never panics when
/// no config exists). Used by best-effort on-disk caches that must degrade to a
/// process-local cache rather than take down a caller that never needed paths.
pub fn assets_dir_if_initialized() -> Option<PathBuf> {
    ASSETS_DIR.get().cloned()
}

// === Content-addressed assets (use assets_dir) ===
//
// The asset store subdirectories listed here are also enumerated in
// scripts/normalize-store-ownership.sh (the root-owned-entry heal); keep the
// two lists in sync when adding a store.

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

// NOTE: Podman cache snapshots now use snapshot_dir() with cache_key as name.
// This unifies snapshot storage - cached snapshots are regular snapshots.

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

/// `sockaddr_un.sun_path` is 108 bytes including the NUL, so a path of 107
/// bytes is the most that can ever be bound.
const SUN_PATH_MAX: usize = 107;

/// The longest socket file placed in a VM's runtime directory.
///
/// Checked rather than assumed: if a longer name is added later, the budget
/// below silently stops covering it, so this constant and the check move
/// together.
const LONGEST_SOCKET_NAME: &str = "firecracker.socket";

/// Fail early when this VM's data directory leaves no room for its sockets.
///
/// A data directory only a few characters too deep pushes the VM's sockets past
/// `sun_path`, `bind` returns ENAMETOOLONG, and what the operator sees is a
/// downstream symptom with no mention of paths at all. Observed 2026-08-16: a
/// nested data directory keyed by a full uuid produced a 128 byte socket path,
/// and the failure surfaced as
///   "VolumeServer 0 failed to signal ready: channel closed"
/// which names neither the path nor the limit. Worse, the same class of failure
/// appears to have been misdiagnosed in 55ef6350 as "FUSE doesn't support Unix
/// sockets", and the workaround for that non-problem disabled reflinks for
/// every nested VM for seven months (issue #810).
pub fn check_socket_path_budget(vm_id: &str) -> anyhow::Result<()> {
    check_socket_path_budget_under(&data_dir(), vm_id)
}

/// [`check_socket_path_budget`] against an explicit data directory.
///
/// Split out so the rule is testable: the real one reads a process-global
/// `OnceLock`, and a check that cannot be tested is how the limit gets
/// forgotten again.
pub fn check_socket_path_budget_under(
    data_dir: &std::path::Path,
    vm_id: &str,
) -> anyhow::Result<()> {
    let longest = data_dir
        .join("vm-disks")
        .join(vm_id)
        .join(LONGEST_SOCKET_NAME);
    let length = longest.as_os_str().len();
    anyhow::ensure!(
        length <= SUN_PATH_MAX,
        "the data directory is too deep for this VM's Unix sockets: {length} bytes, \
         limit {SUN_PATH_MAX} (sockaddr_un.sun_path)\n  {}\n\
         Shorten the data directory (FCVM_DATA_DIR or paths.data_dir). This fails \
         here on purpose: bind() would otherwise return ENAMETOOLONG and surface \
         as an unrelated-looking readiness or channel error.",
        longest.display()
    );
    Ok(())
}

/// Directory for snapshot data
pub fn snapshot_dir() -> PathBuf {
    data_dir().join("snapshots")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// A 32-hex vm_id, the real shape.
    const VM_ID: &str = "vm-1036ea9415214256aff023d3e28646a9";

    /// The exact directory that broke the nested L2 launch on 2026-08-16.
    #[test]
    fn a_data_dir_that_overflows_sun_path_is_rejected() {
        let too_deep =
            Path::new("/mnt/fcvm-btrfs/nested-data/9db69791-596f-4f92-9717-44208140b129");
        let error = check_socket_path_budget_under(too_deep, VM_ID)
            .expect_err("a 128 byte socket path must be refused, not bound");
        let text = error.to_string();
        assert!(
            text.contains("sun_path") && text.contains("128"),
            "the error must name the limit AND the measured length, or the operator \
             is left with the same unattributable symptom this replaces: {text}"
        );
    }

    /// The shortened form must pass, or the guard would block the fix.
    #[test]
    fn the_shortened_nested_data_dir_fits() {
        let short = Path::new("/mnt/fcvm-btrfs/nd/9db69791");
        check_socket_path_budget_under(short, VM_ID)
            .expect("91 bytes must be accepted; if this fails the budget is miscomputed");
    }

    /// The default layout must have room, or every VM would fail to start.
    #[test]
    fn the_default_data_dir_fits() {
        check_socket_path_budget_under(Path::new("/mnt/fcvm-btrfs"), VM_ID)
            .expect("the default data dir must leave room for VM sockets");
    }

    /// Exactly at the limit is allowed; one byte over is not. Pins the
    /// boundary, so an off-by-one cannot creep in unnoticed.
    #[test]
    fn the_boundary_is_exact() {
        let suffix_len = format!("/vm-disks/{VM_ID}/{LONGEST_SOCKET_NAME}").len();
        let at_limit = "/".repeat(SUN_PATH_MAX - suffix_len);
        assert_eq!(
            at_limit.len() + suffix_len,
            SUN_PATH_MAX,
            "test set-up is wrong"
        );
        check_socket_path_budget_under(Path::new(&at_limit), VM_ID)
            .expect("a path of exactly SUN_PATH_MAX bytes must be accepted");

        let over = format!("{at_limit}x");
        check_socket_path_budget_under(Path::new(&over), VM_ID)
            .expect_err("one byte over SUN_PATH_MAX must be refused");
    }
}
