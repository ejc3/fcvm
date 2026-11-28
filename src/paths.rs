//! Where fcvm keeps what it makes.
//!
//! - The assets directory (`assets_dir`): content-addressed kernels, root filesystems,
//!   initrds and image exports, shared by every data directory.
//! - The data directory (`data_dir`), on the store: what outlives a process and a reboot.
//!   Snapshots, owner records (`crate::reclaim`), and each VM's disk directory
//!   (`vm_disk_dir`, `vm-disks/<bucket>/<vm_id>`), which holds its disks and logs.
//! - The runtime directory (`runtime_dir`, `crate::runtime_dir`), on tmpfs where it can be:
//!   what describes running processes only. VM states, the namespace pool, and each VM's
//!   runtime directory (`vm_runtime_dir`, `<runtime>/vm/<16 hex digits of the id>`), which
//!   holds its sockets and markers. A start creates a dozen entries there; on the store each
//!   create waited for the btrfs transaction commit that clone disk snapshots start.
//!
//! A VM's two directories are recorded in its state (`VmConfig::vm_runtime_dir`,
//! `VmConfig::vm_disk_dir`) and with its owner before either is made.

use std::path::PathBuf;
use std::sync::OnceLock;

/// Global directory for mutable per-instance data (vm-disks, state, snapshots)
static DATA_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Global directory for shared content-addressed assets
static ASSETS_DIR: OnceLock<PathBuf> = OnceLock::new();

/// `paths.runtime_dir` of the config this process loaded, if it names a base.
static RUNTIME_SETTING: OnceLock<Option<String>> = OnceLock::new();

/// Where VM states and the namespace pool live (`crate::runtime_dir`), once that is
/// settled for this process: the store's record of this boot names it, or a setting does.
static RUNTIME_DIR: OnceLock<crate::runtime_dir::RuntimeDir> = OnceLock::new();

/// Until then, the directory [`runtime`] last returned: where this process would bind the
/// store as the first writer of this boot.
static RUNTIME_CANDIDATE: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);

/// Set once this process has found its runtime directory bound to its data directory
/// (`bind_runtime_dir`).
static RUNTIME_BOUND: OnceLock<()> = OnceLock::new();

/// Default base directory when no config file exists
const DEFAULT_BASE_DIR: &str = "/mnt/fcvm-btrfs";

/// Initialize directories from config file.
/// Must be called before any path functions are used.
///
/// For nested VM support, each nesting level uses a different data_dir
/// while sharing the same assets_dir for content-addressed files.
///
/// A config that will not load is an error the command returns, so fcvm exits 1 with the
/// list of places it searched; it is not a panic (exit 101), which a caller that reads the
/// exit status would take for a crash.
pub fn init_from_config() -> anyhow::Result<()> {
    use anyhow::Context;
    let (config, _, _) = crate::setup::rootfs::load_config(None)
        .context("Failed to load config - run 'fcvm setup --generate-config' first")?;
    init_from_loaded_config(&config);
    Ok(())
}

/// Initialize directories from an already loaded config. The FCVM_DATA_DIR environment
/// variable overrides its data_dir, which keeps root, container and rootless runs on one host
/// in separate data directories: a rootless container's root is another uid on the host, so
/// sharing a directory such as `reclaim/0` with host root fails its ownership check.
pub fn init_from_loaded_config(config: &crate::setup::rootfs::Plan) {
    let _ = DATA_DIR.set(resolve_data_dir(&config.paths.data_dir));
    let _ = RUNTIME_SETTING.set(config.paths.runtime_dir.clone());
    crate::confine::set_config_mode(config.confine.mode);
    crate::confine::set_config_allow_seccomp_log(config.confine.allow_seccomp_log);
    let _ = ASSETS_DIR.set(resolve_assets_dir(&config.paths.assets_dir));
}

/// FCVM_DATA_DIR when set, otherwise the configured data_dir, as an absolute path.
fn resolve_data_dir(configured: &str) -> PathBuf {
    resolve_data_dir_from(std::env::var_os("FCVM_DATA_DIR"), configured)
}

fn resolve_data_dir_from(env: Option<std::ffi::OsString>, configured: &str) -> PathBuf {
    match env {
        Some(env) => absolute(PathBuf::from(env), "FCVM_DATA_DIR"),
        None => absolute(PathBuf::from(configured), "paths.data_dir"),
    }
}

/// The configured assets_dir, as an absolute path.
fn resolve_assets_dir(configured: &str) -> PathBuf {
    absolute(PathBuf::from(configured), "paths.assets_dir")
}

/// `path` as an absolute path, a relative one taken against the working directory.
///
/// Every directory the path functions return goes through here once, when its `OnceLock` is
/// set, so a run names one directory whatever its working directory is later. Reclaim
/// records, state files and snapshot configs store paths under these directories as given:
/// with a relative FCVM_DATA_DIR, a record of `data/snapshots/<tag>.creating` meant another
/// directory, or none, to a reclaim pass that ran from another working directory, and the
/// staging directory it recorded was never removed (#1918). `source` names the setting in
/// the panic, which, like a config that will not load, leaves the run nothing to work in.
fn absolute(path: PathBuf, source: &str) -> PathBuf {
    std::path::absolute(&path).unwrap_or_else(|error| {
        panic!(
            "{source} {:?} cannot be made an absolute path: {error}. Give it as an absolute \
             path",
            path
        )
    })
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
    let _ = DATA_DIR.set(absolute(data_dir.into(), "data_dir"));
    let _ = ASSETS_DIR.set(absolute(assets_dir.into(), "assets_dir"));
}

/// Directory for mutable per-instance data that outlives its processes (vm-disks,
/// snapshots, owner records). VM states are in [`runtime_dir`].
/// Configure via `paths.data_dir` in rootfs-config.toml for nested VM nesting.
/// FCVM_DATA_DIR environment variable overrides config value. Always absolute: a relative
/// value is taken against the working directory once, when it is first read.
pub fn data_dir() -> PathBuf {
    DATA_DIR
        .get_or_init(|| {
            if let Some(data_dir) = std::env::var_os("FCVM_DATA_DIR") {
                return absolute(PathBuf::from(data_dir), "FCVM_DATA_DIR");
            }
            let (config, _, _) =
                crate::setup::rootfs::load_config(None).expect("Failed to load config");
            resolve_data_dir(&config.paths.data_dir)
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
            resolve_assets_dir(&config.paths.assets_dir)
        })
        .clone()
}

/// The assets directory `config` names, as an absolute path, whatever paths this process
/// started with. `fcvm fleet` starts with the default paths ([`init_with_defaults`]) and
/// runs `fcvm` commands that read the config, so a lookup it shares with those commands
/// takes the directory from the config.
pub fn configured_assets_dir(config: &crate::setup::rootfs::Plan) -> PathBuf {
    resolve_assets_dir(&config.paths.assets_dir)
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

/// Make `dir`, and any missing parent, looking each up before making it.
///
/// A mkdir of a directory that exists fails with EEXIST, but the kernel takes the lock of
/// the directory it would be made in, for writing, before it finds that out, and on btrfs
/// the holder can wait out a transaction commit. `std::fs::create_dir_all` makes that mkdir
/// first and looks only after it fails, so every start that called it on a store directory
/// queued on that directory's parent. This looks first: a directory that exists costs one
/// lookup and takes no lock for writing. A missing one is made, parents first, and a mkdir
/// that loses a race to another process (EEXIST) is success when a directory is there.
pub fn ensure_dir(dir: impl AsRef<std::path::Path>) -> std::io::Result<()> {
    let dir = dir.as_ref();
    match std::fs::metadata(dir) {
        Ok(metadata) if metadata.is_dir() => return Ok(()),
        Ok(_) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                format!("{} exists and is not a directory", dir.display()),
            ))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    if let Some(parent) = dir.parent().filter(|parent| !parent.as_os_str().is_empty()) {
        ensure_dir(parent)?;
    }
    match std::fs::create_dir(dir) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists && dir.is_dir() => Ok(()),
        Err(error) => Err(error),
    }
}

/// [`ensure_dir`] on a blocking thread, for async callers.
pub async fn ensure_dir_async(dir: impl AsRef<std::path::Path>) -> std::io::Result<()> {
    let dir = dir.as_ref().to_path_buf();
    tokio::task::spawn_blocking(move || ensure_dir(dir))
        .await
        .map_err(std::io::Error::other)?
}

// === Mutable per-instance data (use data_dir) ===

/// The runtime directory, for what describes running processes only: where the store's
/// record of this boot says it is, else where this process would put it as the boot's
/// first writer (`crate::runtime_dir::locate`). With a data directory set without a config
/// (`init_with_paths`, `init_with_defaults`) only FCVM_RUNTIME_DIR can name its base.
///
/// Settled once: by the store's record, by a setting, or by this process's own
/// [`bind_runtime_dir`]. Until then every call locates again, because another first
/// writer may choose another place between two calls.
pub fn runtime() -> crate::runtime_dir::RuntimeDir {
    use crate::runtime_dir::Host;
    if let Some(runtime) = RUNTIME_DIR.get() {
        return runtime.clone();
    }
    let configured = RUNTIME_SETTING.get_or_init(|| match DATA_DIR.get() {
        Some(_) => None,
        None => {
            let (config, _, _) =
                crate::setup::rootfs::load_config(None).expect("Failed to load config");
            let _ = DATA_DIR.set(resolve_data_dir(&config.paths.data_dir));
            config.paths.runtime_dir
        }
    });
    // A host whose boot id cannot be read binds nothing: `bind_runtime_dir` returns that
    // error before anything is made or read here.
    let host = Host::current().unwrap_or_else(|_| Host {
        boot_id: "unknown".to_string(),
        euid: nix::unistd::geteuid().as_raw(),
    });
    let located = crate::runtime_dir::locate(
        std::env::var_os("FCVM_RUNTIME_DIR"),
        configured.as_deref(),
        &data_dir(),
        std::path::Path::new(crate::runtime_dir::RUN),
        &host,
    );
    if located.settled() {
        return RUNTIME_DIR.get_or_init(|| located).clone();
    }
    *RUNTIME_CANDIDATE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(located.dir.clone());
    located
}

/// [`runtime`]'s directory. For a path into it, call [`bind_runtime_dir`] first: before
/// the store is bound in this boot this is only where it would be.
pub fn runtime_dir() -> PathBuf {
    runtime().dir
}

/// Directory for VM state files, in the runtime directory.
pub fn state_dir() -> PathBuf {
    runtime_dir().join("state")
}

/// Whether `dir` is this process's own state directory: the one in the runtime directory
/// its data directory is bound to, or would be bound to by this process. False until a
/// path function has located it, and for the directories tests and other tools give a
/// `StateManager` directly.
pub fn is_store_state_dir(dir: &std::path::Path) -> bool {
    RUNTIME_DIR
        .get()
        .is_some_and(|runtime| runtime.dir.join("state") == dir)
        || RUNTIME_CANDIDATE
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
            .is_some_and(|candidate| candidate.join("state") == dir)
}

/// Check, and for a writer make, the binding between the data directory and the runtime
/// directory (`crate::runtime_dir::bind`), once per process once it holds. From then on
/// [`runtime_dir`] is the directory the store is bound to, which is not the one it
/// returned before when another first writer of this boot chose another place.
/// `Ok(false)` tells a reader that the store has no runtime directory in this boot, so no
/// VMs.
pub fn bind_runtime_dir(access: crate::runtime_dir::Access) -> anyhow::Result<bool> {
    if RUNTIME_BOUND.get().is_some() {
        return Ok(true);
    }
    let host = crate::runtime_dir::Host::current()?;
    let Some(bound) = crate::runtime_dir::bind(&data_dir(), &runtime(), access, &host)? else {
        return Ok(false);
    };
    let _ = RUNTIME_DIR.set(bound);
    let _ = RUNTIME_BOUND.set(());
    Ok(true)
}

/// The directory the VMs' disk directories are in, one bucket down (`vm_disk_dir`).
pub fn vm_disks_dir() -> PathBuf {
    data_dir().join("vm-disks")
}

/// The bucket of `vm-disks` that VM `vm_id`'s disk directory is in: the first byte of
/// sha256(vm_id) as two hex digits, so 256 buckets. A pure function of the id: a snapshot
/// records its VM's paths, and a restore finds the VM's directory again from the id.
///
/// Every start makes its disk directory, and a mkdir holds its parent directory's lock
/// for writing across btrfs_mkdir, which joins the running transaction and so can wait out
/// a commit (every clone's disk is a subvolume snapshot, which ends in one). With one
/// `vm-disks` directory that lock was what every start queued on: 968 of 1411 ms of lock
/// wait a clone at 512 starts in flight, fcvm's host kernel, states off btrfs (2026-11-27).
pub fn vm_bucket(vm_id: &str) -> String {
    crate::store_buckets::name(vm_id)
}

/// Whether `name` is the name of a bucket of `vm-disks` (two lowercase hex digits). A VM id
/// is never one (`vm-` and 32 hex digits).
pub fn is_vm_bucket(name: &str) -> bool {
    crate::store_buckets::is_name(name)
}

/// VM `vm_id`'s disk directory, `vm-disks/<bucket>/<vm_id>` on the store: what needs the
/// store's filesystem (reflinks, subvolume snapshots) or can grow without bound. It holds
/// `disks/` (the root disk, extra disks, `--disk-dir` images), `disk-dir-staging/`, a Cloud
/// Hypervisor restore's `ch-restore/`, `firecracker.log` and a prepare's container output.
pub fn vm_disk_dir(vm_id: &str) -> PathBuf {
    vm_disk_dir_under(&data_dir(), vm_id)
}

/// [`vm_disk_dir`] under an explicit data directory.
pub fn vm_disk_dir_under(data_dir: &std::path::Path, vm_id: &str) -> PathBuf {
    data_dir.join("vm-disks").join(vm_bucket(vm_id)).join(vm_id)
}

/// VM `vm_id`'s runtime directory, `<runtime>/vm/<v>` in [`runtime_dir`]: its Unix sockets
/// (`firecracker.sock`, `vsock.sock` and every `vsock.sock_<port>`, the memory server's and
/// the RDP relay's), `pasta.pid`, the container's ready and exit markers and the memory
/// server's failure note. Each of those is a directory entry a start creates, and on the
/// store each create waited for the running btrfs transaction commit, which every clone's
/// subvolume snapshot starts: 0.6 to 1.0 s a clone. On tmpfs nothing waits.
///
/// `<v>` is the first 16 hex digits of the id after `vm-` (an id is `vm-` and 32 hex
/// digits), which keeps the sockets short; the start makes the directory exclusively, so
/// two VMs that share those digits cannot share it.
///
/// For a path that names the directory the store is bound to, call
/// [`bind_runtime_dir`] first, as every start does.
pub fn vm_runtime_dir(vm_id: &str) -> PathBuf {
    vm_runtime_dir_under(&runtime_dir(), vm_id)
}

/// [`vm_runtime_dir`] under an explicit runtime directory.
pub fn vm_runtime_dir_under(runtime_dir: &std::path::Path, vm_id: &str) -> PathBuf {
    runtime_dir
        .join(crate::runtime_dir::VM_DIR)
        .join(vm_runtime_name(vm_id))
}

/// The name of VM `vm_id`'s runtime directory: the first 16 characters of the id after
/// `vm-`, or the whole of a shorter id.
fn vm_runtime_name(vm_id: &str) -> &str {
    let id = vm_id.strip_prefix("vm-").unwrap_or(vm_id);
    id.get(..16).unwrap_or(id)
}

/// Both directories of one VM ([`vm_disk_dir`], [`vm_runtime_dir`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VmDirs {
    /// On tmpfs: sockets, markers, the failure note.
    pub runtime: PathBuf,
    /// On the store: disks and logs.
    pub disk: PathBuf,
}

impl VmDirs {
    /// Where a start of VM `vm_id` puts its directories. Binds the runtime directory for
    /// writing first (`bind_runtime_dir`), so the runtime path is the one the store is bound
    /// to: the start records both paths with its owner (`crate::reclaim`) before it makes
    /// either.
    pub fn for_start(vm_id: &str) -> anyhow::Result<Self> {
        use anyhow::Context;
        bind_runtime_dir(crate::runtime_dir::Access::Write)
            .context("binding the runtime directory for the VM's runtime directory")?;
        Ok(Self {
            runtime: vm_runtime_dir(vm_id),
            disk: vm_disk_dir(vm_id),
        })
    }
}

/// Make VM `vm_id`'s disk directory in its bucket of `vm-disks` ([`crate::store_buckets`]:
/// the first start of a store makes all the buckets, with `vm-disks`' owner and permission
/// bits, so root's and a user's VMs share them when they share a data directory). One that
/// exists already is used: a `--disk-dir` launch makes it before it stages the directory.
pub async fn create_vm_disk_dir(vm_id: &str) -> anyhow::Result<PathBuf> {
    use anyhow::Context;
    let dir = vm_disk_dir(vm_id);
    let bucket = vm_bucket(vm_id);
    let vm_disks = vm_disks_dir_of(&dir);
    tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        crate::store_buckets::ensure(&vm_disks, &bucket)
            .map(|_| ())
            .with_context(|| format!("making the bucket {bucket} of {}", vm_disks.display()))
    })
    .await
    .context("the bucket task")??;
    tokio::fs::create_dir(&dir)
        .await
        .or_else(|error| match error.kind() {
            std::io::ErrorKind::AlreadyExists => Ok(()),
            _ => Err(error),
        })
        .with_context(|| format!("creating {}", dir.display()))?;
    Ok(dir)
}

/// Make both of VM `vm_id`'s directories: its disk directory ([`create_vm_disk_dir`]), and
/// its runtime directory in the runtime directory's `vm/` (made for the store's owner when
/// missing, `crate::runtime_dir::make_vm_dir`). The runtime directory is made exclusively:
/// one that exists belongs to another VM whose id starts with the same 16 digits, and the
/// start fails rather than share it.
pub async fn create_vm_runtime_dir(vm_id: &str) -> anyhow::Result<VmDirs> {
    use anyhow::Context;
    let dirs = VmDirs::for_start(vm_id)?;
    create_vm_disk_dir(vm_id).await?;
    let runtime = runtime_dir();
    let data = data_dir();
    tokio::task::spawn_blocking(move || crate::runtime_dir::make_vm_dir(&data, &runtime))
        .await
        .context("the runtime directory task")??;
    tokio::fs::create_dir(&dirs.runtime)
        .await
        .with_context(|| {
            format!(
                "creating the VM's runtime directory {} (one that exists is another VM's)",
                dirs.runtime.display()
            )
        })?;
    Ok(dirs)
}

/// The `vm-disks` directory a disk directory `<vm-disks>/<bucket>/<vm_id>` is in.
fn vm_disks_dir_of(dir: &std::path::Path) -> PathBuf {
    dir.parent()
        .and_then(std::path::Path::parent)
        .expect("a disk directory is in a bucket of vm-disks")
        .to_path_buf()
}

/// `sockaddr_un.sun_path` is 108 bytes including the NUL, so a path of 107
/// bytes is the most that can ever be bound.
pub const SUN_PATH_MAX: usize = 107;

/// The longest socket file placed in a VM's runtime directory.
///
/// Checked rather than assumed: if a longer name is added later, the budget
/// below silently stops covering it, so this constant and the check move
/// together.
const LONGEST_SOCKET_NAME: &str = "vsock.sock_inode_tables";

/// Fail early when this VM's runtime directory leaves no room for its sockets.
///
/// A directory only a few characters too deep pushes the VM's sockets past
/// `sun_path`, `bind` returns ENAMETOOLONG, and what the operator sees is a
/// downstream symptom with no mention of paths at all. Observed 2026-08-16: a
/// nested data directory keyed by a full uuid produced a 128 byte socket path,
/// and the failure surfaced as
///   "VolumeServer 0 failed to signal ready: channel closed"
/// which names neither the path nor the limit. Worse, the same class of failure
/// appears to have been misdiagnosed in 55ef6350 as "FUSE doesn't support Unix
/// sockets", and the workaround for that non-problem disabled reflinks for
/// every nested VM for seven months (issue #810).
///
/// The sockets are in the VM's runtime directory ([`vm_runtime_dir`]), so the setting to
/// shorten is the runtime directory's base, or the data directory when the runtime
/// directory is in the store's own place. Call [`bind_runtime_dir`] first.
pub fn check_socket_path_budget(vm_id: &str) -> anyhow::Result<()> {
    check_socket_path_budget_under(&runtime(), vm_id)
}

/// [`check_socket_path_budget`] against an explicit runtime directory.
///
/// Split out so the rule is testable: the real one reads a process-global
/// `OnceLock`, and a check that cannot be tested is how the limit gets
/// forgotten again.
pub fn check_socket_path_budget_under(
    runtime: &crate::runtime_dir::RuntimeDir,
    vm_id: &str,
) -> anyhow::Result<()> {
    use crate::runtime_dir::Place;
    let longest = vm_runtime_dir_under(&runtime.dir, vm_id).join(LONGEST_SOCKET_NAME);
    let length = longest.as_os_str().len();
    let shorten = match runtime.place {
        Place::Named => {
            "Shorten the runtime directory's base (FCVM_RUNTIME_DIR or paths.runtime_dir)"
        }
        Place::Store => {
            "Shorten the data directory (FCVM_DATA_DIR or paths.data_dir), which holds the \
             runtime directory, or name a short base for the runtime directory \
             (FCVM_RUNTIME_DIR or paths.runtime_dir)"
        }
        Place::Run => {
            "Name a short base for the runtime directory (FCVM_RUNTIME_DIR or paths.runtime_dir)"
        }
    };
    anyhow::ensure!(
        length <= SUN_PATH_MAX,
        "the runtime directory is too deep for this VM's Unix sockets: {length} bytes, \
         limit {SUN_PATH_MAX} (sockaddr_un.sun_path)\n  {}\n\
         {shorten}. This fails here on purpose: bind() would otherwise return \
         ENAMETOOLONG and surface as an unrelated-looking readiness or channel error.",
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
    use crate::runtime_dir::{Place, RuntimeDir};
    use std::path::Path;

    /// A 32-hex vm_id, the real shape.
    const VM_ID: &str = "vm-1036ea9415214256aff023d3e28646a9";

    /// The runtime directory a data directory has in the store's own place
    /// (`<data_dir>/runtime/boot-<16>`).
    fn on_store(data_dir: &Path) -> RuntimeDir {
        RuntimeDir {
            dir: data_dir
                .join(crate::runtime_dir::STORE_PLACE)
                .join("boot-0123456789abcdef"),
            place: Place::Store,
            followed: false,
        }
    }

    /// The exact data directory that broke the nested L2 launch on 2026-08-16, holding its
    /// runtime directory. The error names the data directory as the setting to shorten.
    #[test]
    fn a_runtime_dir_that_overflows_sun_path_is_rejected() {
        let too_deep =
            Path::new("/mnt/fcvm-btrfs/nested-data/9db69791-596f-4f92-9717-44208140b129");
        let runtime = on_store(too_deep);
        let error = check_socket_path_budget_under(&runtime, VM_ID)
            .expect_err("a socket path past sun_path must be refused, not bound");
        let text = error.to_string();
        let length = format!(
            "{}/vm/1036ea9415214256/{LONGEST_SOCKET_NAME}",
            runtime.dir.display()
        )
        .len();
        assert!(length > SUN_PATH_MAX, "test set-up is wrong: {length}");
        assert!(
            text.contains("sun_path") && text.contains(&format!("{length} bytes")),
            "the error must name the limit AND the measured length, or the operator \
             is left with the same unattributable symptom this replaces: {text}"
        );
        assert!(
            text.contains("FCVM_DATA_DIR or paths.data_dir"),
            "a runtime directory in the store's place is shortened through the data \
             directory: {text}"
        );
    }

    /// A runtime directory under a named base is shortened through that base, and the
    /// error says so.
    #[test]
    fn a_named_base_that_overflows_names_the_runtime_dir_setting() {
        let base = format!("/{}", "b".repeat(80));
        let runtime = RuntimeDir {
            dir: Path::new(&base)
                .join("0123456789abcdef")
                .join("boot-0123456789abcdef"),
            place: Place::Named,
            followed: false,
        };
        let text = check_socket_path_budget_under(&runtime, VM_ID)
            .expect_err("past sun_path")
            .to_string();
        assert!(
            text.contains("FCVM_RUNTIME_DIR or paths.runtime_dir")
                && !text.contains("FCVM_DATA_DIR"),
            "{text}"
        );
    }

    /// The shortened nested data directory still fits with its runtime directory in it.
    #[test]
    fn the_shortened_nested_data_dir_fits() {
        check_socket_path_budget_under(&on_store(Path::new("/mnt/fcvm-btrfs/nd/9db69791")), VM_ID)
            .expect("the shortened nested data directory must be accepted");
    }

    /// The default layouts must have room, or every VM would fail to start: the store's
    /// place in the default data directory, and `/run/fcvm-<uid>` for the widest uid.
    #[test]
    fn the_default_runtime_dirs_fit() {
        check_socket_path_budget_under(&on_store(Path::new("/mnt/fcvm-btrfs")), VM_ID)
            .expect("the default data dir must leave room for VM sockets");
        let run = RuntimeDir {
            dir: Path::new("/run/fcvm-4294967295/0123456789abcdef/boot-0123456789abcdef")
                .to_path_buf(),
            place: Place::Run,
            followed: false,
        };
        check_socket_path_budget_under(&run, VM_ID)
            .expect("/run/fcvm-<uid> must leave room for VM sockets");
    }

    /// Every socket fcvm binds in a VM's runtime directory must fit the name the budget reserves.
    /// The portable-volume inode table socket (`vsock.sock_inode_tables`) is the longest.
    #[test]
    fn the_budget_covers_the_inode_table_socket() {
        let name = crate::volume::inode_table_socket_path(Path::new("vsock.sock"));
        assert!(
            name.as_os_str().len() <= LONGEST_SOCKET_NAME.len(),
            "{} is longer than the {LONGEST_SOCKET_NAME} the budget reserves",
            name.display()
        );
    }

    /// Exactly at the limit is allowed; one byte over is not. Pins the
    /// boundary, so an off-by-one cannot creep in unnoticed.
    #[test]
    fn the_boundary_is_exact() {
        let suffix_len = format!("/vm/1036ea9415214256/{LONGEST_SOCKET_NAME}").len();
        let named = |dir: String| RuntimeDir {
            dir: PathBuf::from(dir),
            place: Place::Named,
            followed: false,
        };
        let at_limit = "/".repeat(SUN_PATH_MAX - suffix_len);
        assert_eq!(
            at_limit.len() + suffix_len,
            SUN_PATH_MAX,
            "test set-up is wrong"
        );
        check_socket_path_budget_under(&named(at_limit.clone()), VM_ID)
            .expect("a path of exactly SUN_PATH_MAX bytes must be accepted");

        let over = format!("{at_limit}x");
        check_socket_path_budget_under(&named(over), VM_ID)
            .expect_err("one byte over SUN_PATH_MAX must be refused");
    }

    /// A VM's disk directory is in the bucket its id gives, two hex digits, the same for
    /// the same id every time (a restore finds a snapshot's source directory from the id).
    #[test]
    fn a_disk_directory_is_in_the_bucket_of_its_id() {
        let dir = vm_disk_dir_under(Path::new("/srv/store"), VM_ID);
        let bucket = vm_bucket(VM_ID);
        assert_eq!(
            dir,
            Path::new("/srv/store/vm-disks").join(&bucket).join(VM_ID)
        );
        assert!(is_vm_bucket(&bucket), "{bucket}");
        assert_eq!(vm_bucket(VM_ID), bucket);
        let buckets: std::collections::BTreeSet<String> = (0..4096)
            .map(|index| vm_bucket(&format!("vm-{index:032x}")))
            .collect();
        assert_eq!(
            buckets.len(),
            256,
            "4096 ids reach every one of the 256 buckets"
        );
        for name in [VM_ID, "a", "abc", "AB", "0g", ".ab"] {
            assert!(!is_vm_bucket(name), "{name} is not a bucket's name");
        }
    }

    /// A VM's runtime directory is `<runtime>/vm/` and the first 16 hex digits of its id, a
    /// plain directory with no bucket, and never the `vm` directory itself.
    #[test]
    fn a_runtime_directory_is_named_by_16_digits_of_its_id() {
        let runtime = Path::new("/run/fcvm-1000/0123456789abcdef/boot-0123456789abcdef");
        assert_eq!(
            vm_runtime_dir_under(runtime, VM_ID),
            runtime.join("vm").join("1036ea9415214256")
        );
        // Shorter ids, as tests give, keep what they have.
        assert_eq!(
            vm_runtime_dir_under(runtime, "vm-abc123"),
            runtime.join("vm").join("abc123")
        );
        assert_ne!(
            vm_runtime_dir_under(runtime, VM_ID),
            vm_disk_dir_under(Path::new("/srv/store"), VM_ID)
        );
        // An empty id names the directory every VM's runtime directory is in, so nothing
        // may derive a directory to remove from one.
        assert_eq!(vm_runtime_dir_under(runtime, ""), runtime.join("vm"));
    }
}

#[cfg(test)]
mod ensure_dir_tests {
    use super::*;

    /// A directory that exists is looked up and nothing is made; a missing one is made with
    /// its missing parents; a file in the way is an error.
    #[test]
    fn ensure_dir_makes_only_what_is_missing() {
        let root = tempfile::tempdir().unwrap();
        let deep = root.path().join("a/b/c");
        ensure_dir(&deep).unwrap();
        assert!(deep.is_dir());
        ensure_dir(&deep).unwrap();
        let file = root.path().join("f");
        std::fs::write(&file, b"").unwrap();
        assert!(ensure_dir(&file).is_err());
    }

    /// The modules a start, a restore and their teardown run through make directories only
    /// with [`ensure_dir`]: `create_dir_all` makes the mkdir before it looks, and a mkdir of
    /// a directory that exists takes its parent's lock for writing. Test modules are not
    /// checked (everything from a file's `#[cfg(test)] mod` on).
    #[test]
    fn the_start_path_makes_directories_only_through_ensure_dir() {
        let sources = [
            (
                "commands/podman/image.rs",
                include_str!("commands/podman/image.rs"),
            ),
            (
                "commands/podman/mod.rs",
                include_str!("commands/podman/mod.rs"),
            ),
            (
                "commands/podman/snapshot.rs",
                include_str!("commands/podman/snapshot.rs"),
            ),
            (
                "commands/podman/vm_config.rs",
                include_str!("commands/podman/vm_config.rs"),
            ),
            ("commands/snapshot.rs", include_str!("commands/snapshot.rs")),
            ("commands/common.rs", include_str!("commands/common.rs")),
            ("commands/vm.rs", include_str!("commands/vm.rs")),
            ("storage/disk.rs", include_str!("storage/disk.rs")),
            ("storage/disk_dir.rs", include_str!("storage/disk_dir.rs")),
            (
                "storage/hugepage_spares.rs",
                include_str!("storage/hugepage_spares.rs"),
            ),
            (
                "storage/pmem_store.rs",
                include_str!("storage/pmem_store.rs"),
            ),
            ("storage/snapshot.rs", include_str!("storage/snapshot.rs")),
            ("network/mod.rs", include_str!("network/mod.rs")),
            ("network/routed.rs", include_str!("network/routed.rs")),
            ("network/pasta.rs", include_str!("network/pasta.rs")),
            ("store_buckets.rs", include_str!("store_buckets.rs")),
            ("reclaim.rs", include_str!("reclaim.rs")),
            ("state/manager.rs", include_str!("state/manager.rs")),
            ("runtime_dir.rs", include_str!("runtime_dir.rs")),
            ("uffd/server.rs", include_str!("uffd/server.rs")),
            ("utils.rs", include_str!("utils.rs")),
        ];
        let mut found = Vec::new();
        for (file, source) in sources {
            let code = source
                .find("#[cfg(test)]\nmod ")
                .map_or(source, |at| &source[..at]);
            for (number, line) in code.lines().enumerate() {
                if line.contains("create_dir_all(") && !line.trim_start().starts_with("//") {
                    found.push(format!("src/{file}:{}: {}", number + 1, line.trim()));
                }
            }
        }
        assert!(
            found.is_empty(),
            "create_dir_all on the start path (use paths::ensure_dir):\n{}",
            found.join("\n")
        );
    }
}

#[cfg(test)]
mod data_dir_override_tests {
    use super::*;

    #[test]
    fn fcvm_data_dir_overrides_the_configured_data_dir() {
        assert_eq!(
            resolve_data_dir_from(Some("/mnt/store/container".into()), "/mnt/store"),
            PathBuf::from("/mnt/store/container")
        );
        assert_eq!(
            resolve_data_dir_from(None, "/mnt/store"),
            PathBuf::from("/mnt/store")
        );
    }

    /// A relative FCVM_DATA_DIR or paths value is taken against the working directory, so
    /// what a run records under its data directory names that directory from any other
    /// working directory too (#1918).
    #[test]
    fn a_relative_data_or_assets_dir_is_made_absolute() {
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(
            resolve_data_dir_from(Some("relative/data".into()), "/mnt/store"),
            cwd.join("relative/data")
        );
        assert_eq!(
            resolve_data_dir_from(None, "relative/configured"),
            cwd.join("relative/configured")
        );
        assert_eq!(
            resolve_assets_dir("relative/assets"),
            cwd.join("relative/assets")
        );
    }

    /// `fcvm setup` must take its data dir through the same override as every other
    /// command. When it read the configured data_dir directly, setup in CI's rootless
    /// container (FCVM_DATA_DIR=<store>/container) wrote root's owner records into the host's
    /// `<store>/reclaim/0` as host uid 1000, and the next host-root job on that runner
    /// refused the directory (and the reverse, as uid 65534).
    #[test]
    fn setup_initializes_paths_through_the_data_dir_override() {
        let setup = include_str!("commands/setup.rs");
        assert!(
            setup.contains("paths::init_from_loaded_config(&config)"),
            "fcvm setup does not initialize its paths through init_from_loaded_config"
        );
        assert!(
            !setup.contains("init_with_paths(&config.paths.data_dir"),
            "fcvm setup reads the configured data_dir past FCVM_DATA_DIR"
        );
    }
}
