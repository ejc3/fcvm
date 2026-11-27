//! The runtime directory: where a data directory's VM states and namespace pool live.
//!
//! Both describe running processes and nothing else: a VM's state names an fcvm process by
//! PID and start time, and that process dies with the host. So they are kept apart from
//! what must outlive a reboot (a VM's disk directory and the owner record that reclaims
//! it, snapshots and their working sets), which stays in the data directory. A start
//! rewrites its state several times, and on btrfs each rewrite is three directory
//! operations that join a transaction of the store's one tree: with a thousand clones
//! starting together those operations queued behind each other and behind the clone
//! disks' own transactions. On tmpfs a rewrite is one rename.
//!
//! # Where
//!
//! `<place>/boot-<boot>`, with `<boot>` 16 characters of the kernel's boot id, and
//! `<place>` the first of:
//!
//! 1. `<base>/<key>` when `FCVM_RUNTIME_DIR`, else `paths.runtime_dir` in the config,
//!    names a base. `<key>` is the first 16 hex digits of the SHA-256 of the data
//!    directory's canonical path, so each data directory (root's, a container's, a
//!    test's, a nesting level's) has its own.
//! 2. `/run/fcvm-<uid>/<key>`, `<uid>` the data directory's owner, when the base
//!    `/run/fcvm-<uid>` exists or the caller is root, who makes it for the owner. `/run`
//!    is a tmpfs only root can write, so the base is root's to make: `sudo fcvm setup`
//!    makes it and installs the tmpfiles.d rule that makes it at every boot.
//! 3. `<data_dir>/runtime` otherwise: on the store, where it needs nothing from root.
//!
//! `/dev/shm` and `$XDG_RUNTIME_DIR` are not used. logind removes a regular user's files
//! in `/dev/shm` when that user's last login session ends (`RemoveIPC`), also while a
//! service of that user is running: measured on Ubuntu 24.04, a file a service of a user
//! without lingering held there was gone 12 s after an unrelated login of that user
//! ended, with the service still running, while the same file under `/run/fcvm-<uid>`
//! stayed. `/run/user/<uid>` exists only while the user has a session or lingers, and is
//! one user's: a data directory's states are shared by root and the store's owner.
//!
//! The boot is part of the path because the store, and a base an operator names, may be
//! on a filesystem that outlives a reboot. A state written before the reboot names a
//! process of the boot before; under another boot's directory it is never read, so it is
//! never taken for a running VM, and the first writer after the reboot removes those
//! directories.
//!
//! # The binding
//!
//! The place is chosen once per boot, by the first command that writes a state, and the
//! store records it: `<data_dir>/state/runtime` names the directory, a random id and the
//! boot, and the directory's `store` file holds the same id and the data directory it
//! serves. Every later command follows the store's record and compares the two, so a
//! base made later in the boot does not move a store whose VMs are running, and two
//! commands cannot keep one store's states in two places.
//!
//! | the store's record | the directory it names | what a command does |
//! |---|---|---|
//! | none, or of another boot | - | a writer chooses the place and binds: makes the directory, an id, the record. A reader sees no VMs |
//! | of this boot | holds the same id | goes on, in that directory |
//! | of this boot | holds no such id, or is not there | refuses: the directory is in another mount namespace (a container with its own `/run`), or was removed under running VMs |
//! | any | holds the id of another data directory | refuses: two data directories with one key |
//!
//! A command whose setting names a base uses that base and is refused when the store is
//! bound elsewhere in this boot.
//!
//! `<data_dir>/state` is where earlier builds kept the states themselves. A VM of such a
//! build that is still running refuses the store, as `StateManager::refuse_other_layouts`
//! does within a state directory; what dead ones left is removed by the first writer. The
//! `layout` file there holds `3`, which a build that keeps its states there refuses.
//!
//! # Ownership
//!
//! `/run/fcvm-<uid>` is made mode 0700 by root for the store's owner, and
//! `<data_dir>/runtime` by the owner or by root for the owner. On every command the
//! directories fcvm made on the way to the states must be real directories, owned by
//! that user or root, that nobody else can write. Root can enter a 0700 directory, which
//! is all the sharing between root and the store's owner needs; what root makes there it
//! hands to the owner. A base the operator names is checked the same way, and may also
//! belong to the user running the command or be a sticky shared directory.

use anyhow::{bail, Context, Result};
use nix::fcntl::{Flock, FlockArg};
use serde::{Deserialize, Serialize};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// The tmpfs where root makes each user's base, `fcvm-<uid>`.
pub const RUN: &str = "/run";
/// Where `fcvm setup` installs the rule that has a user's base under [`RUN`] made at
/// every boot.
pub const TMPFILES_DIR: &str = "/etc/tmpfiles.d";
/// The directory of a data directory that holds its runtime directories when it has no
/// base under [`RUN`].
pub const STORE_PLACE: &str = "runtime";
const BOOT_PREFIX: &str = "boot-";

/// The directory of the data directory that holds the store's markers: the layout number,
/// the binding and its lock. Earlier layouts kept the states here.
const STORE_MARKERS: &str = "state";
const LAYOUT_FILE: &str = "layout";
/// The layout of a store whose states are in a runtime directory. Layout 2 keeps them in
/// `<data_dir>/state/vms/<b>/`, layout 1 in `<data_dir>/state/` itself.
const STORE_LAYOUT: &str = "3";
const BINDING_FILE: &str = "runtime";
const BINDING_LOCK: &str = "runtime.lock";
/// The file in a runtime directory that says which store it serves.
const RECORD_FILE: &str = "store";
const STAGING_PREFIX: &str = ".runtime-staging.";
/// The directories a launch of an older layout makes in the store before or with its
/// state: layout 1's `name/` and `pid/`, written before its `<vm_id>.json`, and layout 2's
/// `vms/`, `names/` and `pids/`.
const OLD_LAUNCH_DIRS: [&str; 5] = ["name", "pid", "vms", "names", "pids"];

/// Which of the three places a runtime directory is in (the module documentation's list).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Place {
    /// Under the base `FCVM_RUNTIME_DIR` or `paths.runtime_dir` names.
    Named,
    /// Under `/run/fcvm-<uid>`.
    Run,
    /// `<data_dir>/runtime`.
    Store,
}

/// A located runtime directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeDir {
    pub dir: PathBuf,
    pub place: Place,
    /// Read from the store's record of this boot: where the states are, not where a first
    /// writer would put them.
    pub followed: bool,
}

impl RuntimeDir {
    /// A base this command's own setting names: it is never exchanged for the one the
    /// store is bound to.
    fn set_by_this_command(&self) -> bool {
        self.place == Place::Named && !self.followed
    }

    /// Whether this is the directory for the rest of the boot: the store's record names
    /// it, or the command's setting does. Otherwise it is where this command would bind
    /// the store if it is the first writer, and another first writer may choose another.
    pub fn settled(&self) -> bool {
        self.followed || self.place == Place::Named
    }
}

/// What a command will do with the states.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// Reads what exists and creates nothing (`fcvm ls`).
    Read,
    /// Creates or changes a state.
    Write,
}

/// What `bind` reads from the host, passed in so tests can stand in for a reboot or
/// another user.
#[derive(Debug, Clone)]
pub struct Host {
    pub boot_id: String,
    pub euid: u32,
}

impl Host {
    pub fn current() -> Result<Self> {
        let boot_id = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
            .context("reading the boot id (/proc/sys/kernel/random/boot_id)")?
            .trim()
            .to_string();
        anyhow::ensure!(!boot_id.is_empty(), "the kernel reports an empty boot id");
        Ok(Self {
            boot_id,
            euid: nix::unistd::geteuid().as_raw(),
        })
    }

    /// The name of this boot's directory in a place: `boot-` and 16 characters of the
    /// boot id.
    fn boot_name(&self) -> String {
        let boot: String = self
            .boot_id
            .chars()
            .filter(char::is_ascii_alphanumeric)
            .take(16)
            .collect();
        format!("{BOOT_PREFIX}{boot}")
    }
}

/// The store's record of its runtime directory (`<data_dir>/state/runtime`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Binding {
    runtime_dir: PathBuf,
    place: Place,
    id: String,
    boot_id: String,
}

/// A runtime directory's record of its store (`<runtime_dir>/store`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Record {
    id: String,
    data_dir: PathBuf,
}

/// Where `data_dir`'s VM states are, or where the first writer of this boot would put
/// them: the base `env` (FCVM_RUNTIME_DIR) or `configured` (`paths.runtime_dir`) names,
/// else the directory the store's record of this boot names, else the store's own place
/// when this boot's directory there is in use, else `<run>/fcvm-<uid>` when that base
/// exists or the caller is root, else the store's own place. An empty setting is no
/// setting. Reads, and makes nothing.
pub fn locate(
    env: Option<std::ffi::OsString>,
    configured: Option<&str>,
    data_dir: &Path,
    run: &Path,
    host: &Host,
) -> RuntimeDir {
    let boot = host.boot_name();
    let named = env
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            configured
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
        });
    if let Some(base) = named {
        return RuntimeDir {
            dir: std::path::absolute(&base)
                .unwrap_or(base)
                .join(key(data_dir))
                .join(boot),
            place: Place::Named,
            followed: false,
        };
    }
    if let Some(binding) = read_binding(data_dir).filter(|binding| binding.boot_id == host.boot_id)
    {
        return RuntimeDir {
            dir: binding.runtime_dir,
            place: binding.place,
            followed: true,
        };
    }
    let on_store = RuntimeDir {
        dir: data_dir.join(STORE_PLACE).join(&boot),
        place: Place::Store,
        followed: false,
    };
    // In use in this boot with the store's record gone: the states are still there.
    if on_store.dir.join(RECORD_FILE).exists() {
        return on_store;
    }
    let owner = std::fs::metadata(data_dir).map_or(host.euid, |metadata| metadata.uid());
    let base = run.join(format!("fcvm-{owner}"));
    let base_exists = std::fs::symlink_metadata(&base).is_ok_and(|metadata| metadata.is_dir());
    if host.euid == 0 || base_exists {
        return RuntimeDir {
            dir: base.join(key(data_dir)).join(boot),
            place: Place::Run,
            followed: false,
        };
    }
    on_store
}

/// The directory `data_dir`'s VM states are in, in this boot: what its record names, when
/// that directory holds the record's id. `None` when the store is bound to none in this
/// boot, so has no VMs. For tests, benchmarks and tools that look at a data directory
/// other than their own process's; a process's own is `crate::paths::runtime_dir`. Nothing
/// else may build a path to a state, a name slot or the pool's socket.
pub fn bound_to(data_dir: &Path) -> Result<Option<PathBuf>> {
    bound_in(data_dir, &Host::current()?)
}

fn bound_in(data_dir: &Path, host: &Host) -> Result<Option<PathBuf>> {
    let Some(binding) = read_binding(data_dir).filter(|binding| binding.boot_id == host.boot_id)
    else {
        return Ok(None);
    };
    let holds_the_id =
        read_record(&binding.runtime_dir)?.is_some_and(|record| record.id == binding.id);
    Ok(holds_the_id.then_some(binding.runtime_dir))
}

/// The store's record of its runtime directory, if it is there and whole.
fn read_binding(data_dir: &Path) -> Option<Binding> {
    let bytes = std::fs::read(data_dir.join(STORE_MARKERS).join(BINDING_FILE)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// The name of `data_dir`'s runtime directory: 16 hex digits of the SHA-256 of its
/// canonical path, so two spellings of one directory (through a symlink, with `..`) get
/// one name.
pub fn key(data_dir: &Path) -> String {
    use sha2::Digest;
    use std::os::unix::ffi::OsStrExt;
    let canonical = canonical(data_dir);
    sha2::Sha256::digest(canonical.as_os_str().as_bytes())[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// `path` with its symlinks resolved. A path that does not exist yet is resolved as far as
/// it exists, with the rest appended, so the answer is the same before and after it is
/// made.
fn canonical(path: &Path) -> PathBuf {
    let mut existing = path;
    let mut rest = Vec::new();
    loop {
        if let Ok(resolved) = std::fs::canonicalize(existing) {
            return rest
                .iter()
                .rev()
                .fold(resolved, |path, part| path.join(part));
        }
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_owned());
                existing = parent;
            }
            _ => return path.to_path_buf(),
        }
    }
}

/// What `<data_dir>/state` holds.
#[derive(Default)]
struct StoreScan {
    layout: Option<String>,
    binding: Option<Binding>,
    /// Layout 1: `<vm_id>.json` and its lock and temporary files, `name/`, `pid/`.
    layout_1: Vec<PathBuf>,
    /// Layout 2: `vms/`, `names/`, `pids/`, the sweep's lock and stamp, staging directories.
    layout_2: Vec<PathBuf>,
}

impl StoreScan {
    fn has_old_entries(&self) -> bool {
        !(self.layout_1.is_empty() && self.layout_2.is_empty())
    }
}

fn scan_store(store: &Path) -> Result<StoreScan> {
    let mut scan = StoreScan::default();
    let entries = match std::fs::read_dir(store) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(scan),
        Err(error) => return Err(error).with_context(|| format!("reading {}", store.display())),
    };
    for entry in entries.flatten() {
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        let path = entry.path();
        if name == LAYOUT_FILE {
            scan.layout = Some(
                std::fs::read_to_string(&path)
                    .with_context(|| format!("reading the layout marker {}", path.display()))?
                    .trim_end_matches('\n')
                    .to_string(),
            );
        } else if name == BINDING_FILE {
            let bytes =
                std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
            scan.binding = Some(serde_json::from_slice(&bytes).with_context(|| {
                format!(
                    "{} is empty or damaged. It records where this data directory's VM \
                     states are. When no fcvm VM of this data directory is running, remove \
                     that file and run this command again: fcvm writes it anew",
                    path.display()
                )
            })?);
        } else if name.ends_with(".json")
            || name.ends_with(".json.lock")
            || name.ends_with(".json.tmp")
            || name == "name"
            || name == "pid"
        {
            scan.layout_1.push(path);
        } else if matches!(
            name.as_str(),
            "vms" | "names" | "pids" | "sweep.lock" | "sweep.stamp"
        ) || name.starts_with(".index-staging.")
        {
            scan.layout_2.push(path);
        }
    }
    Ok(scan)
}

fn read_record(runtime: &Path) -> Result<Option<Record>> {
    let path = runtime.join(RECORD_FILE);
    match std::fs::read(&path) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes).with_context(|| {
            format!(
                "{} is empty or damaged. It says which data directory this runtime \
                 directory serves. When no fcvm VM is running from it, remove the directory \
                 {} and run this command again",
                path.display(),
                runtime.display()
            )
        })?)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        // A directory this user cannot enter holds no record this user can use.
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => Ok(None),
        Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
    }
}

/// The running VMs whose states are the files `states`, as `'name' (pid N)`.
///
/// Fails closed: a state whose owner's liveness cannot be read counts as running, and so
/// does a state file that cannot be read (no permission, an I/O error) or does not parse
/// and was written since this boot. Only one that is gone, names a dead process, names no
/// process, or is cut short from before this boot (what a host crash leaves) is not
/// running. Removing an older build's state on the strength of a read that failed would
/// take the name and the address of a VM that may be up.
fn running(states: impl Iterator<Item = PathBuf>) -> Vec<String> {
    let mut running = Vec::new();
    for path in states {
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                running.push(format!(
                    "the VM of {} (unreadable: {error})",
                    path.display()
                ));
                continue;
            }
        };
        let Ok(state) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
            if !crate::state::written_before_this_boot(&path) {
                running.push(format!(
                    "the VM of {} (it does not parse and was written since this boot)",
                    path.display()
                ));
            }
            continue;
        };
        let pid = state
            .get("pid")
            .and_then(|pid| pid.as_u64())
            .and_then(|pid| u32::try_from(pid).ok());
        let start = state.get("pid_start_time").and_then(|start| start.as_u64());
        if let Some(pid) = pid.filter(|pid| crate::state::process_is_live(*pid, start)) {
            let name = state
                .get("name")
                .and_then(|name| name.as_str())
                .unwrap_or("(unnamed)");
            running.push(format!("'{name}' (pid {pid})"));
        }
    }
    running.sort();
    running
}

/// The state files of a layout 2 state directory: `<dir>/vms/<b>/<vm_id>.json`. A
/// bucket that cannot be listed is returned itself, so that `running` reports it as
/// unreadable and nothing in it is taken for dead.
fn bucketed_states(state_dir: &Path) -> Vec<PathBuf> {
    let mut states = Vec::new();
    let vms = state_dir.join("vms");
    let buckets = match std::fs::read_dir(&vms) {
        Ok(buckets) => buckets,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return states,
        Err(_) => return vec![vms],
    };
    for bucket in buckets.flatten() {
        let Ok(entries) = std::fs::read_dir(bucket.path()) else {
            states.push(bucket.path());
            continue;
        };
        states.extend(entries.flatten().map(|entry| entry.path()).filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        }));
    }
    states
}

/// Fail while a VM an older fcvm started is running with its state in `store`.
fn refuse_running_old_vms(store: &Path, scan: &StoreScan) -> Result<()> {
    if !scan.has_old_entries() {
        return Ok(());
    }
    let layout_1 = scan
        .layout_1
        .iter()
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .cloned();
    let running = running(layout_1.chain(bucketed_states(store)));
    if running.is_empty() {
        return Ok(());
    }
    bail!(
        "{} holds the state of {} VM(s) started by an older fcvm that are running or cannot \
         be shown to have stopped, which this fcvm does not read or write: {}. Stop them \
         with the fcvm binary that started them (and remove a state file that cannot be \
         read once its VM is known to be gone), then run this command again",
        store.display(),
        running.len(),
        running.join(", ")
    )
}

/// Fail when the store is bound, in this boot, to a runtime directory this process does
/// not see.
fn refuse_unseen_binding(
    data_dir: &Path,
    store: &Path,
    scan: &StoreScan,
    record: Option<&Record>,
    runtime: &RuntimeDir,
    host: &Host,
) -> Result<()> {
    let Some(binding) = &scan.binding else {
        return Ok(());
    };
    if record.is_some_and(|record| record.id == binding.id) || binding.boot_id != host.boot_id {
        return Ok(());
    }
    let found = match record {
        Some(record) => format!("holds another id ({})", record.id),
        None if runtime.dir.exists() => "holds no id".to_string(),
        None => "does not exist".to_string(),
    };
    bail!(
        "the data directory {data} keeps its VM states in the runtime directory {bound} (id \
         {id}), made since this host booted, and this process does not see that directory: \
         {mine} {found}. VMs of this data directory may be running where it is visible, so \
         nothing is started or listed from here. A container or a unit with its own /run \
         needs the directory bound in (podman run -v {bound}:{bound}), or FCVM_RUNTIME_DIR \
         set to a directory every user of this data directory sees. If no fcvm VM of this \
         data directory is running, remove {record} and run this command again",
        data = data_dir.display(),
        bound = binding.runtime_dir.display(),
        id = binding.id,
        mine = runtime.dir.display(),
        record = store.join(BINDING_FILE).display(),
    )
}

/// Fail when `path` is not a real directory owned by one of `owners` that nobody else can
/// write. Read without following a link at `path`.
fn check_dir(path: &Path, owners: &[u32]) -> Result<()> {
    let metadata =
        std::fs::symlink_metadata(path).with_context(|| format!("reading {}", path.display()))?;
    if !metadata.is_dir() || !owners.contains(&metadata.uid()) || metadata.mode() & 0o022 != 0 {
        bail!(
            "refusing the runtime directory {}: it must be a directory owned by uid {} that \
             no one else can write (found uid {}, mode {:o}{}). Remove it if no fcvm uses \
             it, or set FCVM_RUNTIME_DIR (paths.runtime_dir) to a directory that is",
            path.display(),
            owners
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(" or "),
            metadata.uid(),
            metadata.mode() & 0o7777,
            if metadata.is_dir() {
                ""
            } else {
                ", not a directory"
            }
        );
    }
    Ok(())
}

/// The owner and group of the data directory.
fn store_owner(data_dir: &Path) -> Result<(u32, u32)> {
    let metadata = std::fs::metadata(data_dir)
        .with_context(|| format!("reading the data directory {}", data_dir.display()))?;
    Ok((metadata.uid(), metadata.gid()))
}

/// The ownership check of a runtime directory that exists, on every command: this boot's
/// directory, the place it is in, and for a place under a base the base.
///
/// A base under `/run` is this store's owner's (`fcvm-<uid>`), so it is held to the same
/// rule as the directories in it. A base the operator names may also be a shared one where
/// anyone can make an entry and nobody can rename or remove another's (the sticky bit):
/// there the place's own check is what keeps a stranger's directory out. The store's place
/// is in the data directory, which is not fcvm's to judge.
fn check_runtime_dir(runtime: &RuntimeDir, owner: u32, host: &Host) -> Result<()> {
    let owners: &[u32] = if runtime.place == Place::Named {
        &[owner, 0, host.euid]
    } else {
        &[owner, 0]
    };
    let place = place_dir(runtime)?;
    if let Some(base) = place.parent().filter(|_| runtime.place != Place::Store) {
        let sticky = runtime.place == Place::Named
            && std::fs::symlink_metadata(base)
                .is_ok_and(|metadata| metadata.is_dir() && metadata.mode() & 0o1000 != 0);
        if !sticky {
            check_dir(base, owners)?;
        }
    }
    check_dir(place, owners)?;
    check_dir(&runtime.dir, owners)
}

/// The place a runtime directory is in: the parent of its boot's directory.
fn place_dir(runtime: &RuntimeDir) -> Result<&Path> {
    runtime.dir.parent().with_context(|| {
        format!(
            "the runtime directory {} has no parent",
            runtime.dir.display()
        )
    })
}

/// Give the directory `dir`, just made by root, to `owner`. Through a descriptor opened
/// without following links, so a link planted at the path cannot have something else
/// given away.
fn hand_to(dir: &Path, owner: (u32, u32)) -> Result<()> {
    use nix::fcntl::OFlag;
    let fd = nix::fcntl::open(
        dir,
        OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
        nix::sys::stat::Mode::empty(),
    )
    .with_context(|| format!("opening {}", dir.display()))?;
    nix::unistd::fchown(
        &fd,
        Some(nix::unistd::Uid::from_raw(owner.0)),
        Some(nix::unistd::Gid::from_raw(owner.1)),
    )
    .with_context(|| format!("giving {} to uid {}", dir.display(), owner.0))
}

/// Make the directory `dir` with `mode` if it is missing, and when root makes it for
/// another user's store, give it to that user.
fn make_dir(dir: &Path, mode: u32, owner: (u32, u32), host: &Host) -> Result<()> {
    match std::fs::DirBuilder::new().mode(mode).create(dir) {
        Ok(()) => {
            // The umask cleared some of the bits.
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(mode))
                .with_context(|| format!("setting the mode of {}", dir.display()))?;
            if host.euid == 0 && owner.0 != 0 {
                hand_to(dir, owner)?;
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error).with_context(|| format!("making {}", dir.display())),
    }
}

/// Make this boot's runtime directory, its place and for a place under a base the base,
/// or check the ones that are there.
fn make_runtime_dir(runtime: &RuntimeDir, owner: (u32, u32), host: &Host) -> Result<()> {
    let place = place_dir(runtime)?;
    match (runtime.place, place.parent()) {
        (Place::Named, Some(base)) => {
            // The operator's choice, on whatever filesystem it is. One that is missing is
            // made closed to everyone else, like a base under /run.
            if let Some(parent) = base.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("making {}", parent.display()))?;
            }
            make_dir(base, 0o700, owner, host)?;
        }
        (Place::Run, Some(base)) => {
            only_the_owner_or_root(runtime, owner, host)?;
            // Root makes the base for the store's owner. Anyone else finds it made, by
            // root at setup or at boot, or is told who makes it.
            make_dir(base, 0o700, owner, host).with_context(|| {
                format!(
                    "{} is root's to make: run `sudo fcvm setup --runtime-dir`, which also \
                     has it made at every boot",
                    base.display()
                )
            })?;
        }
        _ => only_the_owner_or_root(runtime, owner, host)?,
    }
    make_dir(place, 0o755, owner, host)?;
    make_dir(&runtime.dir, 0o755, owner, host)?;
    check_runtime_dir(runtime, owner.0, host)
}

/// A place that is not an operator's choice is made by the store's owner or by root for
/// the owner. A directory a third user made there would be refused by every later
/// command, the owner's included.
fn only_the_owner_or_root(runtime: &RuntimeDir, owner: (u32, u32), host: &Host) -> Result<()> {
    if host.euid != owner.0 && host.euid != 0 {
        bail!(
            "the data directory belongs to uid {}, and only that user or root can make its \
             runtime directory {} (this is uid {}). Run as that user, or set \
             FCVM_RUNTIME_DIR to a directory of your own",
            owner.0,
            runtime.dir.display(),
            host.euid
        );
    }
    Ok(())
}

/// An exclusive flock on the place directory `place`, taken only while it is still the
/// directory at that path: `None` when it is gone or another directory is there now, or,
/// with `wait` false, when another process holds the lock.
fn lock_place(place: &Path, wait: bool) -> Result<Option<Flock<std::fs::File>>> {
    use nix::fcntl::OFlag;
    let fd = match nix::fcntl::open(
        place,
        OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
        nix::sys::stat::Mode::empty(),
    ) {
        Ok(fd) => fd,
        Err(nix::errno::Errno::ENOENT) => return Ok(None),
        Err(errno) => return Err(errno).with_context(|| format!("opening {}", place.display())),
    };
    let file = std::fs::File::from(fd);
    let how = if wait {
        FlockArg::LockExclusive
    } else {
        FlockArg::LockExclusiveNonblock
    };
    let held = match Flock::lock(file, how) {
        Ok(held) => held,
        Err((_, nix::errno::Errno::EWOULDBLOCK)) if !wait => return Ok(None),
        Err((_, errno)) => {
            return Err(errno).with_context(|| format!("locking {}", place.display()))
        }
    };
    let locked = held
        .metadata()
        .with_context(|| format!("reading {}", place.display()))?;
    let same = std::fs::symlink_metadata(place)
        .is_ok_and(|now| now.dev() == locked.dev() && now.ino() == locked.ino());
    Ok(same.then_some(held))
}

/// Hold the lock on `runtime`'s place while the binding fills it: the orphan sweep takes
/// the same lock to remove a place, so it never removes one a binding is filling, and one
/// it removed while this waited is made again before the lock is held.
fn hold_place(
    runtime: &RuntimeDir,
    owner: (u32, u32),
    host: &Host,
) -> Result<Flock<std::fs::File>> {
    let place = place_dir(runtime)?;
    loop {
        if let Some(held) = lock_place(place, true)? {
            return Ok(held);
        }
        make_runtime_dir(runtime, owner, host)?;
    }
}

/// Remove the directories of other boots in `place`. A state in one names a process of a
/// boot that is over; on tmpfs there is none to remove.
fn remove_other_boots(place: &Path, this_boot: &std::ffi::OsStr) {
    let Ok(entries) = std::fs::read_dir(place) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let another_boot = name != this_boot
            && name.to_string_lossy().starts_with(BOOT_PREFIX)
            && entry.file_type().is_ok_and(|kind| kind.is_dir());
        if !another_boot {
            continue;
        }
        match std::fs::remove_dir_all(entry.path()) {
            Ok(()) => tracing::info!(
                dir = %entry.path().display(),
                "removed the VM states of an earlier boot"
            ),
            Err(error) => tracing::warn!(
                dir = %entry.path().display(),
                %error,
                "cannot remove the VM states of an earlier boot; nothing reads them"
            ),
        }
    }
}

/// Write `content` at `path` whole: under a name of its own, world-readable whatever the
/// umask, synced, renamed, and the directory synced. These files are written once per
/// boot at most, and one cut short by a host crash would have every command refuse the
/// store until someone removed it.
fn publish(path: &Path, content: &[u8]) -> Result<()> {
    use std::io::Write;
    let dir = path.parent().context("a marker has a directory")?;
    let temp = dir.join(format!("{STAGING_PREFIX}{}", uuid::Uuid::new_v4()));
    let written = (|| {
        let mut file = std::fs::File::create(&temp)?;
        file.write_all(content)?;
        file.set_permissions(std::fs::Permissions::from_mode(0o644))?;
        file.sync_all()?;
        std::fs::rename(&temp, path)?;
        std::fs::File::open(dir)?.sync_all()
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    written.with_context(|| format!("writing {}", path.display()))
}

/// Remove what earlier layouts left in `store` with no VM running.
///
/// Layout 1's files go only under layout 1's own `names.lock`, taken without waiting: a
/// launch of that build holds it from before it writes its name entry until its state is
/// in place, so nothing of a launch in progress is removed. Layout 2 has no such lock; a
/// launch of it that began before the `layout` marker changed writes a state that the
/// next command finds and refuses.
fn remove_old_entries(store: &Path) -> Result<()> {
    let scan = scan_store(store)?;
    if !scan.has_old_entries() {
        return Ok(());
    }
    let mut _names = None;
    if !scan.layout_1.is_empty() {
        let path = store.join("names.lock");
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o666)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        _names = Some(Flock::lock(file, FlockArg::LockExclusiveNonblock).map_err(
            |(_, error)| {
                anyhow::anyhow!(
                    "an older fcvm holds {} ({error}): it is starting a VM in this data \
                     directory, or is stopped while doing so. Run this command again when \
                     it has finished",
                    path.display()
                )
            },
        )?);
    }
    let scan = scan_store(store)?;
    refuse_running_old_vms(store, &scan)?;
    for path in scan.layout_1.iter().chain(&scan.layout_2) {
        let removed = match std::fs::symlink_metadata(path) {
            Ok(metadata) if metadata.is_dir() => std::fs::remove_dir_all(path),
            Ok(_) => std::fs::remove_file(path),
            Err(error) => Err(error),
        };
        match removed {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                return Err(error)
                    .with_context(|| format!("removing {} of an older layout", path.display()))
            }
            _ => {}
        }
    }
    Ok(())
}

/// What every command but the first writer after a boot finds: the store marked with this
/// layout and bound, in this boot, to the directory this process located, and none of the
/// directories an older layout's launch writes its name or state into. Read by name, three
/// small files and five lookups, no directory listing: every start of every VM of the
/// store comes through here, and on btrfs a listing takes its directory's lock
/// exclusively. A launch of an older build that checked the store before the `layout`
/// marker changed can still write after the binding removed its layout's directories; it
/// makes one of those directories again, so the command takes the full check, which
/// refuses the store while that VM runs.
fn already_bound(store: &Path, runtime: &Path, served: &Path, host: &Host) -> bool {
    if OLD_LAUNCH_DIRS
        .iter()
        .any(|name| std::fs::symlink_metadata(store.join(name)).is_ok())
    {
        return false;
    }
    let read = |path: PathBuf| std::fs::read(path).ok();
    let layout = read(store.join(LAYOUT_FILE));
    let binding = read(store.join(BINDING_FILE))
        .and_then(|bytes| serde_json::from_slice::<Binding>(&bytes).ok());
    let record = read(runtime.join(RECORD_FILE))
        .and_then(|bytes| serde_json::from_slice::<Record>(&bytes).ok());
    layout.as_deref() == Some(format!("{STORE_LAYOUT}\n").as_bytes())
        && binding.zip(record).is_some_and(|(binding, record)| {
            binding.boot_id == host.boot_id
                && binding.runtime_dir == runtime
                && binding.id == record.id
                && record.data_dir == served
        })
}

/// Whether the store's record of this boot and the runtime directory's agree and nothing
/// of an older layout is in the store, from a full scan of the store.
fn bound(scan: &StoreScan, record: Option<&Record>, host: &Host) -> bool {
    scan.layout.as_deref() == Some(STORE_LAYOUT)
        && !scan.has_old_entries()
        && scan
            .binding
            .as_ref()
            .zip(record)
            .is_some_and(|(binding, record)| {
                binding.boot_id == host.boot_id && binding.id == record.id
            })
}

/// Check, and for a writer make, the binding between `data_dir` and its runtime directory.
///
/// `located` is what [`locate`] returned. `Ok(Some(dir))`: `dir` holds this store's states,
/// and is the store's record's directory when another first writer of this boot chose
/// another place than `located` between the two calls. `Ok(None)`, for a reader only: the
/// store has no runtime directory in this boot, so it has no VMs. An error refuses the
/// command; the table in the module documentation says when.
pub fn bind(
    data_dir: &Path,
    located: &RuntimeDir,
    access: Access,
    host: &Host,
) -> Result<Option<RuntimeDir>> {
    let store = data_dir.join(STORE_MARKERS);
    let served = canonical(data_dir);
    // The directory to go on in: the one the store's record of this boot names, unless
    // this command's own setting names one.
    let settle = |scan: &StoreScan| -> RuntimeDir {
        match scan
            .binding
            .as_ref()
            .filter(|binding| binding.boot_id == host.boot_id)
        {
            Some(binding) if !located.set_by_this_command() => RuntimeDir {
                dir: binding.runtime_dir.clone(),
                place: binding.place,
                followed: true,
            },
            _ => located.clone(),
        }
    };
    let found = |runtime: RuntimeDir| RuntimeDir {
        followed: !runtime.set_by_this_command(),
        ..runtime
    };
    let check = |scan: &StoreScan, record: Option<&Record>, runtime: &RuntimeDir| -> Result<()> {
        if let Some(record) = record.filter(|record| record.data_dir != served) {
            bail!(
                "the runtime directory {} holds the VM states of another data directory, \
                 {}, and cannot also serve {}. When no fcvm VM is running from it, remove \
                 that directory and run this command again",
                runtime.dir.display(),
                record.data_dir.display(),
                served.display()
            );
        }
        refuse_unseen_binding(data_dir, &store, scan, record, runtime, host)?;
        refuse_running_old_vms(&store, scan)
    };

    if already_bound(&store, &located.dir, &served, host) {
        check_runtime_dir(located, store_owner(data_dir)?.0, host)?;
        return Ok(Some(found(located.clone())));
    }
    let scan = scan_store(&store)?;
    let runtime = settle(&scan);
    let record = read_record(&runtime.dir)?;
    check(&scan, record.as_ref(), &runtime)?;
    if access == Access::Read {
        let agree = scan
            .binding
            .as_ref()
            .zip(record.as_ref())
            .is_some_and(|(binding, record)| {
                binding.boot_id == host.boot_id && binding.id == record.id
            });
        return Ok(agree.then(|| found(runtime)));
    }

    std::fs::create_dir_all(data_dir)
        .with_context(|| format!("making the data directory {}", data_dir.display()))?;
    let owner = store_owner(data_dir)?;
    make_dir(&store, 0o755, owner, host)?;
    let lock_path = store.join(BINDING_LOCK);
    let lock_file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o666)
        .open(&lock_path)
        .or_else(|_| std::fs::File::open(&lock_path))
        .with_context(|| format!("opening {}", lock_path.display()))?;
    // Whatever the umask, so root and the store's owner can each open one the other made.
    // It fails for the one who does not own the file, which changes nothing.
    let _ = lock_file.set_permissions(std::fs::Permissions::from_mode(0o666));
    let _lock = Flock::lock(lock_file, FlockArg::LockExclusive)
        .map_err(|(_, error)| error)
        .with_context(|| format!("locking {}", lock_path.display()))?;

    // What another writer did while this one waited for the lock: it may have bound the
    // store, and to another place than this command located.
    let scan = scan_store(&store)?;
    let runtime = settle(&scan);
    let record = read_record(&runtime.dir)?;
    if bound(&scan, record.as_ref(), host) {
        check_runtime_dir(&runtime, owner.0, host)?;
        return Ok(Some(found(runtime)));
    }
    check(&scan, record.as_ref(), &runtime)?;

    // The marker first: a build that keeps its states here refuses the store from now on,
    // so what is removed next is not written again behind this one's back.
    if scan.layout.as_deref() != Some(STORE_LAYOUT) {
        publish(
            &store.join(LAYOUT_FILE),
            format!("{STORE_LAYOUT}\n").as_bytes(),
        )?;
    }
    remove_old_entries(&store)?;

    make_runtime_dir(&runtime, owner, host)?;
    let _place = hold_place(&runtime, owner, host)?;
    // No process of an earlier boot is left to own a state: its directories go, here and
    // in the store's own place when this boot's states are kept elsewhere.
    let this_boot = runtime.dir.file_name().unwrap_or_default();
    remove_other_boots(place_dir(&runtime)?, this_boot);
    let on_store = data_dir.join(STORE_PLACE);
    if runtime.place != Place::Store && check_dir(&on_store, &[owner.0, 0]).is_ok() {
        remove_other_boots(&on_store, this_boot);
        // Gone when that left it empty.
        let _ = std::fs::remove_dir(&on_store);
    }
    let id = match record {
        Some(record) => record.id,
        None => {
            let id = uuid::Uuid::new_v4().to_string();
            let record = Record {
                id: id.clone(),
                data_dir: served.clone(),
            };
            publish(
                &runtime.dir.join(RECORD_FILE),
                &serde_json::to_vec_pretty(&record)?,
            )?;
            id
        }
    };
    make_dir(&runtime.dir.join("state"), 0o755, owner, host)?;
    let binding = Binding {
        runtime_dir: runtime.dir.clone(),
        place: runtime.place,
        id,
        boot_id: host.boot_id.clone(),
    };
    publish(
        &store.join(BINDING_FILE),
        &serde_json::to_vec_pretty(&binding)?,
    )?;
    tracing::info!(
        data_dir = %data_dir.display(),
        runtime_dir = %runtime.dir.display(),
        place = ?runtime.place,
        id = %binding.id,
        "bound the data directory to its runtime directory"
    );
    if runtime.place == Place::Store {
        tracing::info!(
            "VM states of {} are kept on the store until the next boot, because {RUN}/fcvm-{} \
             does not exist. Starts are faster with the states on tmpfs: `sudo fcvm setup \
             --runtime-dir` makes that directory and has it made at every boot",
            data_dir.display(),
            owner.0
        );
    }
    Ok(Some(found(runtime)))
}

/// What `sudo fcvm setup` does for the owner of `data_dir`, so that the owner's VM states
/// are on tmpfs with no further root step: make `<run>/fcvm-<uid>` now, mode 0700 and the
/// owner's, and install the tmpfiles.d rule that makes it at every boot. Returns the base
/// and the rule's file; removing that file undoes it from the next boot on.
pub fn install_base(
    data_dir: &Path,
    run: &Path,
    tmpfiles_dir: &Path,
    host: &Host,
) -> Result<(PathBuf, PathBuf)> {
    let owner = store_owner(data_dir)?;
    let base = run.join(format!("fcvm-{}", owner.0));
    if host.euid != 0 {
        bail!(
            "{} is root's to make (this is uid {}): run `sudo fcvm setup --runtime-dir`",
            base.display(),
            host.euid
        );
    }
    make_dir(&base, 0o700, owner, host)?;
    check_dir(&base, &[owner.0, 0])?;
    std::fs::create_dir_all(tmpfiles_dir)
        .with_context(|| format!("making {}", tmpfiles_dir.display()))?;
    let rule = tmpfiles_dir.join(format!("fcvm-{}.conf", owner.0));
    let content = format!(
        "# Installed by `fcvm setup`: the tmpfs directory fcvm keeps the VM states of uid {uid}'s\n\
         # data directories in, made at every boot. Remove this file to undo.\n\
         d {base} 0700 {uid} {gid} -\n",
        uid = owner.0,
        gid = owner.1,
        base = base.display(),
    );
    publish(&rule, content.as_bytes())?;
    Ok((base, rule))
}

/// Remove the runtime directories of data directories that no longer exist, among the
/// ones beside `runtime`'s place in its base. Tests make data directories under temporary
/// paths, and each leaves a runtime directory. One is removed only when the data
/// directory every record in it names is gone and no state of this boot in it has a live
/// owner. The store's own place has nothing beside it: it goes with its data directory.
///
/// A place is removed under its lock (the one a binding holds while it fills its place),
/// after a second look: the data directory may have been made again at the same path and
/// bound to the same place since the first.
pub fn remove_orphans(runtime: &RuntimeDir) {
    remove_orphans_with(runtime, |_| {});
}

/// [`remove_orphans`], with `between` run after a place is judged orphaned and before it
/// is locked, for tests to interleave a binding there.
fn remove_orphans_with(runtime: &RuntimeDir, between: impl Fn(&Path)) {
    if runtime.place == Place::Store {
        return;
    }
    let (Some(place), Some(this_boot)) = (runtime.dir.parent(), runtime.dir.file_name()) else {
        return;
    };
    let Some(entries) = place.parent().and_then(|base| std::fs::read_dir(base).ok()) else {
        return;
    };
    for entry in entries.flatten() {
        let dir = entry.path();
        if dir == place || !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        if !orphaned(&dir, this_boot) {
            continue;
        }
        between(&dir);
        let Ok(Some(_held)) = lock_place(&dir, false) else {
            continue;
        };
        if !orphaned(&dir, this_boot) {
            continue;
        }
        match std::fs::remove_dir_all(&dir) {
            Ok(()) => tracing::debug!(
                dir = %dir.display(),
                "removed the runtime directory of a data directory that is gone"
            ),
            Err(error) => {
                tracing::debug!(dir = %dir.display(), %error, "cannot remove a runtime directory")
            }
        }
    }
}

/// Whether the place `dir` serves only data directories that are gone and holds no state
/// of this boot with a live owner.
fn orphaned(dir: &Path, this_boot: &std::ffi::OsStr) -> bool {
    let records: Vec<Record> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|boot| boot.file_name().to_string_lossy().starts_with(BOOT_PREFIX))
        .filter_map(|boot| read_record(&boot.path()).ok().flatten())
        .collect();
    let gone = !records.is_empty()
        && records.iter().all(|record| {
            matches!(
                std::fs::symlink_metadata(&record.data_dir),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound
            )
        });
    gone && running(bucketed_states(&dir.join(this_boot).join("state")).into_iter()).is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(boot: &str) -> Host {
        Host {
            boot_id: boot.to_string(),
            euid: nix::unistd::geteuid().as_raw(),
        }
    }

    /// A user who is neither root nor, on any test host, the owner of a test's directory:
    /// for what `locate` decides, which reads and makes nothing.
    fn some_user(boot: &str) -> Host {
        Host {
            euid: 54321,
            ..host(boot)
        }
    }

    /// A data directory and a stand-in for `/run`, both temporary. `new` makes the user's
    /// base in it, as root does at setup and at boot; `without_a_base` does not.
    struct Fixture {
        _root: tempfile::TempDir,
        data: PathBuf,
        run: PathBuf,
    }

    impl Fixture {
        fn without_a_base() -> Self {
            let root = tempfile::tempdir().unwrap();
            let data = root.path().join("data");
            let run = root.path().join("run");
            std::fs::create_dir_all(&data).unwrap();
            std::fs::create_dir_all(&run).unwrap();
            Self {
                _root: root,
                data,
                run,
            }
        }

        fn new() -> Self {
            let fixture = Self::without_a_base();
            fixture.make_base();
            fixture
        }

        fn base(&self) -> PathBuf {
            let owner = std::fs::metadata(&self.data).unwrap().uid();
            self.run.join(format!("fcvm-{owner}"))
        }

        fn make_base(&self) {
            std::fs::create_dir(self.base()).unwrap();
            std::fs::set_permissions(self.base(), std::fs::Permissions::from_mode(0o700)).unwrap();
        }

        fn locate(&self, host: &Host) -> RuntimeDir {
            locate(None, None, &self.data, &self.run, host)
        }

        /// Where boot `boot-1` keeps the states of a store with a base: under the base.
        fn runtime(&self) -> RuntimeDir {
            self.locate(&host("boot-1"))
        }

        /// The store's own place in the boot of `host`, as `locate` gives it to a user
        /// with no base.
        fn on_store(&self, host: &Host) -> RuntimeDir {
            RuntimeDir {
                dir: self.data.join(STORE_PLACE).join(host.boot_name()),
                place: Place::Store,
                followed: false,
            }
        }

        fn store(&self) -> PathBuf {
            self.data.join(STORE_MARKERS)
        }

        fn binding(&self) -> Binding {
            serde_json::from_slice(&std::fs::read(self.store().join(BINDING_FILE)).unwrap())
                .unwrap()
        }
    }

    fn write_state(path: &Path, name: &str, pid: u32) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let start = crate::utils::process_start_time(pid);
        std::fs::write(
            path,
            serde_json::json!({"name": name, "pid": pid, "pid_start_time": start}).to_string(),
        )
        .unwrap();
    }

    /// A PID no process has: this test's own child, reaped.
    fn dead_pid() -> u32 {
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        pid
    }

    #[test]
    fn the_environment_wins_over_the_config_and_both_over_the_default() {
        let fixture = Fixture::new();
        let boot = host("boot-1");
        let from_env = locate(
            Some("/run/a".into()),
            Some("/run/b"),
            &fixture.data,
            &fixture.run,
            &boot,
        );
        let name = key(&fixture.data);
        assert_eq!(
            from_env,
            RuntimeDir {
                dir: Path::new("/run/a").join(&name).join("boot-boot1"),
                place: Place::Named,
                followed: false,
            }
        );
        let from_config = locate(None, Some("/run/b"), &fixture.data, &fixture.run, &boot);
        assert_eq!(
            from_config.dir,
            Path::new("/run/b").join(&name).join("boot-boot1")
        );
        // Two data directories under one named base do not meet.
        let other = locate(
            Some("/run/a".into()),
            None,
            Path::new("/elsewhere"),
            &fixture.run,
            &boot,
        );
        assert_ne!(other.dir, from_env.dir);
        // An empty value is no value, in either place.
        let default = locate(
            Some("".into()),
            Some(""),
            &fixture.data,
            &fixture.run,
            &boot,
        );
        assert_eq!(
            default,
            RuntimeDir {
                dir: fixture.base().join(&name).join("boot-boot1"),
                place: Place::Run,
                followed: false,
            }
        );
    }

    /// The default for a user with no prior root step is the store: nothing of theirs is
    /// in a directory a logout can empty. Root, and a user whose base root has made, get
    /// the tmpfs.
    #[test]
    fn a_user_without_a_base_is_given_the_store_and_root_or_a_base_gives_run() {
        let fixture = Fixture::without_a_base();
        let user = some_user("boot-1");
        let name = key(&fixture.data);
        assert_eq!(
            fixture.locate(&user),
            RuntimeDir {
                dir: fixture.data.join("runtime").join("boot-boot1"),
                place: Place::Store,
                followed: false,
            }
        );
        let root = Host {
            euid: 0,
            ..host("boot-1")
        };
        let under_run = RuntimeDir {
            dir: fixture.base().join(&name).join("boot-boot1"),
            place: Place::Run,
            followed: false,
        };
        assert_eq!(fixture.locate(&root), under_run);
        assert!(!fixture.base().exists(), "locating makes nothing");
        fixture.make_base();
        assert_eq!(fixture.locate(&user), under_run);
        // A file of that name is not a base.
        let other = Fixture::without_a_base();
        std::fs::write(other.base(), "").unwrap();
        assert_eq!(other.locate(&user).place, Place::Store);
    }

    /// Only root makes a base under /run, and anyone else is told the command.
    #[test]
    fn a_user_cannot_install_the_base_and_is_told_who_does() {
        let fixture = Fixture::without_a_base();
        let rules = fixture.run.join("tmpfiles.d");
        let error = install_base(&fixture.data, &fixture.run, &rules, &some_user("boot-1"))
            .expect_err("a base under /run is root's to make");
        assert!(
            format!("{error:#}").contains("sudo fcvm setup --runtime-dir"),
            "{error:#}"
        );
        assert!(!fixture.base().exists() && !rules.exists());
    }

    #[test]
    fn one_data_directory_has_one_key_however_it_is_spelled() {
        let fixture = Fixture::new();
        let link = fixture.data.parent().unwrap().join("link");
        std::os::unix::fs::symlink(&fixture.data, &link).unwrap();
        assert_eq!(key(&fixture.data), key(&link));
        assert_eq!(
            key(&fixture.data),
            key(&fixture.data.join("..").join("data"))
        );
        let other = fixture.data.parent().unwrap().join("other");
        assert_ne!(key(&fixture.data), key(&other));
        // The same before and after the directory is made.
        let before = key(&other);
        std::fs::create_dir(&other).unwrap();
        assert_eq!(before, key(&other));
        assert_eq!(key(&fixture.data).len(), 16);
    }

    #[test]
    fn a_writer_binds_and_every_later_command_finds_the_binding() {
        let fixture = Fixture::new();
        let boot = host("boot-1");
        let runtime = fixture.runtime();
        // A reader before any writer: no VMs, and nothing made.
        assert_eq!(
            bind(&fixture.data, &runtime, Access::Read, &boot).unwrap(),
            None
        );
        assert!(!fixture.store().exists() && !runtime.dir.exists());

        let bound = bind(&fixture.data, &runtime, Access::Write, &boot)
            .unwrap()
            .unwrap();
        assert_eq!(
            bound,
            RuntimeDir {
                followed: true,
                ..runtime.clone()
            }
        );
        assert_eq!(
            std::fs::read_to_string(fixture.store().join(LAYOUT_FILE)).unwrap(),
            "3\n"
        );
        let binding = fixture.binding();
        let record = read_record(&runtime.dir).unwrap().unwrap();
        assert_eq!(binding.id, record.id);
        assert_eq!(
            (binding.runtime_dir.clone(), binding.place),
            (runtime.dir.clone(), Place::Run)
        );
        assert_eq!(record.data_dir, canonical(&fixture.data));
        assert!(runtime.dir.join("state").is_dir());

        // Every later command is led to the same directory by the store's record, and
        // finds the same id.
        let later = fixture.locate(&boot);
        assert_eq!(later, bound);
        for access in [Access::Write, Access::Read] {
            assert_eq!(
                bind(&fixture.data, &later, access, &boot).unwrap(),
                Some(bound.clone())
            );
        }
        assert_eq!(read_record(&runtime.dir).unwrap().unwrap().id, binding.id);
        // What a test or a tool is told about this data directory: the same directory in
        // this boot, none in another, none once the directory no longer holds the id.
        assert_eq!(
            bound_in(&fixture.data, &boot).unwrap(),
            Some(runtime.dir.clone())
        );
        assert_eq!(bound_in(&fixture.data, &host("boot-2")).unwrap(), None);
        std::fs::remove_file(runtime.dir.join(RECORD_FILE)).unwrap();
        assert_eq!(bound_in(&fixture.data, &boot).unwrap(), None);
    }

    /// A user with no base: the states are in the data directory, in this boot's
    /// directory, and every later command is led there.
    #[test]
    fn a_start_with_no_base_binds_the_store_to_its_own_place() {
        let fixture = Fixture::without_a_base();
        let boot = host("boot-1");
        let runtime = fixture.on_store(&boot);
        let bound = bind(&fixture.data, &runtime, Access::Write, &boot)
            .unwrap()
            .unwrap();
        assert_eq!(bound.dir, fixture.data.join("runtime").join("boot-boot1"));
        assert_eq!(fixture.binding().place, Place::Store);
        assert!(bound.dir.join("state").is_dir());
        assert!(!fixture.base().exists());
        assert_eq!(fixture.locate(&some_user("boot-1")), bound);
        assert_eq!(
            bind(&fixture.data, &bound, Access::Read, &boot).unwrap(),
            Some(bound)
        );
    }

    /// The place is chosen once per boot. A base root makes while the store's VMs run on
    /// the store does not move their states: commands that came after it would list none
    /// of them. The next boot's first writer uses the base.
    #[test]
    fn a_base_made_later_does_not_move_a_bound_store() {
        let fixture = Fixture::without_a_base();
        let boot = host("boot-1");
        let bound = bind(
            &fixture.data,
            &fixture.on_store(&boot),
            Access::Write,
            &boot,
        )
        .unwrap()
        .unwrap();
        fixture.make_base();
        for who in [
            some_user("boot-1"),
            Host {
                euid: 0,
                ..boot.clone()
            },
        ] {
            assert_eq!(fixture.locate(&who), bound, "euid {}", who.euid);
        }
        assert_eq!(
            bind(&fixture.data, &fixture.locate(&boot), Access::Write, &boot).unwrap(),
            Some(bound.clone())
        );
        // The store's record removed by hand, as a refusal's advice says: this boot's
        // directory on the store still holds the states.
        std::fs::remove_file(fixture.store().join(BINDING_FILE)).unwrap();
        assert_eq!(fixture.locate(&some_user("boot-1")).dir, bound.dir);

        assert_eq!(fixture.locate(&some_user("boot-2")).place, Place::Run);
    }

    /// Two first writers of a boot that located different places (root and a user with no
    /// base, at the same moment) end in one: the second follows the store's record.
    #[test]
    fn a_second_first_writer_follows_the_store_to_the_place_the_first_chose() {
        let fixture = Fixture::new();
        let boot = host("boot-1");
        // What a user located before root made the base.
        let stale = fixture.on_store(&boot);
        let chosen = bind(&fixture.data, &fixture.runtime(), Access::Write, &boot)
            .unwrap()
            .unwrap();
        assert_eq!(chosen.place, Place::Run);
        for access in [Access::Read, Access::Write] {
            assert_eq!(
                bind(&fixture.data, &stale, access, &boot).unwrap(),
                Some(chosen.clone()),
                "{access:?}"
            );
        }
        assert!(
            !fixture.data.join("runtime").exists(),
            "nothing is made in the place that was not chosen"
        );
    }

    /// A state written before a reboot names a process of that boot. On the store, and
    /// under a base on a disk, its file is still there afterwards: it is in another
    /// boot's directory, which no command of this boot reads, and the first writer
    /// removes it. The state here names a process that is running (this test), which a
    /// check of the PID alone would take for the VM.
    #[test]
    fn a_state_of_an_earlier_boot_is_never_read_and_the_first_writer_removes_it() {
        let fixture = Fixture::without_a_base();
        let before = host("boot-1");
        let old = bind(
            &fixture.data,
            &fixture.on_store(&before),
            Access::Write,
            &before,
        )
        .unwrap()
        .unwrap();
        let state = old.dir.join("state/vms/0/vm-before.json");
        write_state(&state, "before-the-reboot", std::process::id());

        let after = host("boot-2");
        let located = fixture.locate(&some_user("boot-2"));
        assert_ne!(located.dir, old.dir);
        assert!(!located.followed);
        assert_eq!(
            bind(&fixture.data, &located, Access::Read, &after).unwrap(),
            None,
            "a reader after the reboot sees no VMs"
        );
        assert!(state.exists(), "a reader removes nothing");

        let new = bind(&fixture.data, &located, Access::Write, &after)
            .unwrap()
            .unwrap();
        assert_eq!(new.dir, located.dir);
        assert!(bucketed_states(&new.dir.join("state")).is_empty());
        assert!(!old.dir.exists(), "the earlier boot's states are removed");

        // The same when this boot's states go under a base: the store's own place is
        // emptied of the earlier boot and removed.
        let moved = Fixture::without_a_base();
        let old = bind(
            &moved.data,
            &moved.on_store(&before),
            Access::Write,
            &before,
        )
        .unwrap()
        .unwrap();
        write_state(
            &old.dir.join("state/vms/0/vm-before.json"),
            "before-the-reboot",
            std::process::id(),
        );
        moved.make_base();
        let located = moved.locate(&after);
        assert_eq!(located.place, Place::Run);
        bind(&moved.data, &located, Access::Write, &after)
            .unwrap()
            .unwrap();
        assert!(!moved.data.join("runtime").exists());
    }

    /// Once bound, a command reads three files by name and lists no directory of the
    /// store: every start comes through here, and a listing locks its directory.
    #[test]
    fn a_bound_store_is_checked_without_listing_it() {
        use nix::sys::inotify::{AddWatchFlags, InitFlags, Inotify};
        let fixture = Fixture::new();
        let boot = host("boot-1");
        let runtime = fixture.runtime();
        bind(&fixture.data, &runtime, Access::Write, &boot).unwrap();
        let inotify = Inotify::init(InitFlags::IN_NONBLOCK).unwrap();
        for dir in [fixture.store(), runtime.dir.clone()] {
            inotify
                .add_watch(
                    &dir,
                    AddWatchFlags::IN_OPEN | AddWatchFlags::IN_ONLYDIR | AddWatchFlags::IN_CREATE,
                )
                .unwrap();
        }
        for access in [Access::Write, Access::Read] {
            let located = fixture.locate(&boot);
            assert!(bind(&fixture.data, &located, access, &boot)
                .unwrap()
                .is_some());
        }
        let mut listed = Vec::new();
        while let Ok(events) = inotify.read_events() {
            if events.is_empty() {
                break;
            }
            // An event with no name is on the watched directory itself: it was opened.
            listed.extend(events.into_iter().filter(|event| {
                event.name.is_none() || event.mask.contains(AddWatchFlags::IN_CREATE)
            }));
        }
        assert!(
            listed.is_empty(),
            "a bound store's check opened a directory or created a file: {listed:?}"
        );
    }

    /// A container with its own /run, or a runtime directory removed under running VMs:
    /// the store names a directory made in this boot that this process does not have.
    /// Starting or listing from an empty one would hand out taken names. A command whose
    /// setting names another base is refused the same way.
    #[test]
    fn a_binding_of_this_boot_that_is_not_seen_is_refused() {
        let fixture = Fixture::new();
        let boot = host("boot-1");
        let runtime = fixture.runtime();
        bind(&fixture.data, &runtime, Access::Write, &boot).unwrap();

        let elsewhere = locate(
            Some(fixture.run.join("named").into()),
            None,
            &fixture.data,
            &fixture.run,
            &boot,
        );
        for access in [Access::Read, Access::Write] {
            let error = bind(&fixture.data, &elsewhere, access, &boot)
                .expect_err("a store bound in this boot to an unseen directory must refuse");
            let text = format!("{error:#}");
            assert!(
                text.contains(&runtime.dir.display().to_string())
                    && text.contains("FCVM_RUNTIME_DIR")
                    && text.contains(&fixture.store().join(BINDING_FILE).display().to_string()),
                "the refusal names the bound directory, the setting and the way out: {text}"
            );
        }
        assert!(
            !fixture.run.join("named").exists(),
            "a refused command makes no runtime directory"
        );

        // Another mount namespace, or the directory removed and made again in the same
        // boot: the path the store names holds no id.
        std::fs::remove_dir_all(&runtime.dir).unwrap();
        let located = fixture.locate(&boot);
        assert_eq!(located.dir, runtime.dir);
        for access in [Access::Read, Access::Write] {
            assert!(bind(&fixture.data, &located, access, &boot).is_err());
        }
    }

    /// After a reboot no VM survived: the first writer binds anew, and a reader before it
    /// sees no VMs and writes nothing.
    #[test]
    fn a_binding_of_another_boot_is_replaced_by_the_first_writer() {
        let fixture = Fixture::new();
        let runtime = fixture.runtime();
        bind(&fixture.data, &runtime, Access::Write, &host("boot-1")).unwrap();
        let before = read_record(&runtime.dir).unwrap().unwrap().id;

        let boot = host("boot-2");
        let located = fixture.locate(&boot);
        assert_ne!(located.dir, runtime.dir);
        let marker = std::fs::read(fixture.store().join(BINDING_FILE)).unwrap();
        assert_eq!(
            bind(&fixture.data, &located, Access::Read, &boot).unwrap(),
            None
        );
        assert_eq!(
            std::fs::read(fixture.store().join(BINDING_FILE)).unwrap(),
            marker,
            "a reader rewrites nothing"
        );
        assert!(!located.dir.exists());

        assert!(bind(&fixture.data, &located, Access::Write, &boot)
            .unwrap()
            .is_some());
        let after = read_record(&located.dir).unwrap().unwrap().id;
        assert_ne!(before, after);
        let binding = fixture.binding();
        assert_eq!((binding.id, binding.boot_id), (after, "boot-2".to_string()));
    }

    #[test]
    fn one_runtime_directory_does_not_serve_two_data_directories() {
        let fixture = Fixture::new();
        let boot = host("boot-1");
        let shared = RuntimeDir {
            dir: fixture.run.join("shared").join("place").join("boot-boot1"),
            place: Place::Named,
            followed: false,
        };
        bind(&fixture.data, &shared, Access::Write, &boot).unwrap();
        let second = fixture.data.parent().unwrap().join("second");
        std::fs::create_dir(&second).unwrap();
        for access in [Access::Read, Access::Write] {
            let error = bind(&second, &shared, access, &boot).unwrap_err();
            assert!(
                format!("{error:#}").contains("another data directory"),
                "{error:#}"
            );
        }
        assert!(!second.join(STORE_MARKERS).join(BINDING_FILE).exists());
    }

    #[test]
    fn a_running_vm_of_an_older_layout_refuses_the_store() {
        for layout in [1, 2] {
            let fixture = Fixture::new();
            let state = match layout {
                1 => fixture.store().join("vm-old.json"),
                _ => fixture.store().join("vms").join("a").join("vm-old.json"),
            };
            write_state(&state, "old-vm", std::process::id());
            for access in [Access::Read, Access::Write] {
                let error = bind(&fixture.data, &fixture.runtime(), access, &host("boot-1"))
                    .expect_err("a running VM of an older layout must refuse the store");
                let text = format!("{error:#}");
                assert!(
                    text.contains("'old-vm'") && text.contains("older fcvm"),
                    "layout {layout}: {text}"
                );
            }
            assert!(state.exists(), "a refused command removes nothing");
            assert!(!fixture.store().join(BINDING_FILE).exists());
        }
    }

    /// A launch of an older build that checked the store before the binding changed its
    /// layout writes its name and state after the binding removed that layout's
    /// directories. A bound store's fast path must not hide it: the store is refused
    /// while that VM runs, for a reader and a writer alike.
    #[test]
    fn a_late_launch_of_an_older_layout_refuses_a_bound_store() {
        for layout in [1, 2] {
            let fixture = Fixture::new();
            let boot = host("boot-1");
            bind(&fixture.data, &fixture.runtime(), Access::Write, &boot).unwrap();
            let state = match layout {
                1 => {
                    std::fs::create_dir_all(fixture.store().join("name")).unwrap();
                    fixture.store().join("vm-late.json")
                }
                _ => fixture.store().join("vms").join("a").join("vm-late.json"),
            };
            write_state(&state, "late-vm", std::process::id());
            for access in [Access::Read, Access::Write] {
                let located = fixture.locate(&boot);
                let error = bind(&fixture.data, &located, access, &boot)
                    .expect_err("a late launch of an older layout must refuse the bound store");
                let text = format!("{error:#}");
                assert!(
                    text.contains("'late-vm'") && text.contains("older fcvm"),
                    "layout {layout}: {text}"
                );
            }
            assert!(state.exists(), "a refused command removes nothing");
        }
    }

    /// An older build's state that cannot be read, or that does not parse and is from
    /// this boot, may be a running VM's: the store is refused and nothing is removed. One
    /// cut short before this boot is what a host crash left, and goes.
    #[test]
    fn an_old_state_that_cannot_be_judged_refuses_the_store() {
        for layout in [1, 2] {
            let fixture = Fixture::new();
            let boot = host("boot-1");
            let dir = match layout {
                1 => fixture.store(),
                _ => fixture.store().join("vms").join("a"),
            };
            std::fs::create_dir_all(&dir).unwrap();
            // A directory where the state should be: reading it fails for every user.
            let unreadable = dir.join("vm-unreadable.json");
            std::fs::create_dir(&unreadable).unwrap();
            let error = bind(&fixture.data, &fixture.runtime(), Access::Write, &boot)
                .expect_err("an unreadable old state must refuse the store");
            assert!(format!("{error:#}").contains("unreadable"), "{error:#}");
            assert!(unreadable.exists());
            std::fs::remove_dir(&unreadable).unwrap();

            let torn = dir.join("vm-torn.json");
            std::fs::write(&torn, "{\"pid\": 1").unwrap();
            let error = bind(&fixture.data, &fixture.runtime(), Access::Write, &boot)
                .expect_err("an old state cut short in this boot must refuse the store");
            assert!(format!("{error:#}").contains("does not parse"), "{error:#}");
            assert!(torn.exists());

            // The same file from before this boot is a host crash's leftover.
            std::fs::File::options()
                .write(true)
                .open(&torn)
                .unwrap()
                .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1))
                .unwrap();
            assert!(
                bind(&fixture.data, &fixture.runtime(), Access::Write, &boot)
                    .unwrap()
                    .is_some()
            );
            assert!(!torn.exists(), "layout {layout}");
        }
    }

    /// Every directory and marker is made with its own mode, whatever the umask: under
    /// 077 a directory made with the default mode shut out whichever of root and the
    /// store's owner did not make it.
    #[test]
    fn modes_do_not_come_from_the_umask() {
        let fixture = Fixture::without_a_base();
        let boot = host("boot-1");
        let named = fixture.run.join("named");
        let mode = |path: &Path| std::fs::metadata(path).unwrap().mode() & 0o7777;
        for runtime in [
            locate(
                Some(named.clone().into()),
                None,
                &fixture.data,
                &fixture.run,
                &boot,
            ),
            fixture.on_store(&boot),
        ] {
            let old = nix::sys::stat::umask(nix::sys::stat::Mode::from_bits_truncate(0o077));
            let bound = bind(&fixture.data, &runtime, Access::Write, &boot);
            nix::sys::stat::umask(old);
            bound.unwrap();
            assert_eq!(mode(runtime.dir.parent().unwrap()), 0o755, "{runtime:?}");
            assert_eq!(mode(&runtime.dir), 0o755, "{runtime:?}");
            assert_eq!(mode(&runtime.dir.join("state")), 0o755);
            assert_eq!(mode(&runtime.dir.join(RECORD_FILE)), 0o644);
            std::fs::remove_file(fixture.store().join(BINDING_FILE)).unwrap();
        }
        assert_eq!(mode(&named), 0o700, "a base fcvm makes is closed");
        assert_eq!(mode(&fixture.store()), 0o755);
        assert_eq!(mode(&fixture.store().join(LAYOUT_FILE)), 0o644);
        assert_eq!(mode(&fixture.store().join(BINDING_LOCK)), 0o666);
    }

    #[test]
    fn what_dead_vms_of_an_older_layout_left_is_removed_by_a_writer_only() {
        let fixture = Fixture::new();
        let boot = host("boot-1");
        let store = fixture.store();
        let dead = dead_pid();
        write_state(&store.join("vm-one.json"), "one", dead);
        std::fs::write(store.join("vm-one.json.lock"), "").unwrap();
        std::fs::create_dir(store.join("name")).unwrap();
        write_state(
            &store.join("vms").join("3").join("vm-two.json"),
            "two",
            dead,
        );
        std::fs::create_dir_all(store.join("names").join("3")).unwrap();
        std::fs::create_dir_all(store.join("pids").join("3")).unwrap();
        std::fs::write(store.join("sweep.lock"), "").unwrap();
        std::fs::write(store.join(LAYOUT_FILE), "2\n").unwrap();
        std::fs::write(store.join("names.lock"), "").unwrap();

        let runtime = fixture.runtime();
        assert_eq!(
            bind(&fixture.data, &runtime, Access::Read, &boot).unwrap(),
            None
        );
        assert!(
            store.join("vm-one.json").exists() && store.join("vms").exists(),
            "a reader removes nothing"
        );

        assert!(bind(&fixture.data, &runtime, Access::Write, &boot)
            .unwrap()
            .is_some());
        let mut left: Vec<String> = std::fs::read_dir(&store)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        assert_eq!(left, ["layout", "names.lock", "runtime", "runtime.lock"]);
        // What a layout 2 build reads before it writes: a number that is not its own.
        assert_eq!(
            std::fs::read_to_string(store.join(LAYOUT_FILE)).unwrap(),
            "3\n"
        );
    }

    /// A layout 1 launch holds `names.lock` while it writes its name entry and state.
    /// Its files are not removed under it.
    #[test]
    fn nothing_of_an_older_launch_in_progress_is_removed() {
        let fixture = Fixture::new();
        let store = fixture.store();
        std::fs::create_dir_all(store.join("name")).unwrap();
        let names = std::fs::File::create(store.join("names.lock")).unwrap();
        let _held = Flock::lock(names, FlockArg::LockExclusive).unwrap();
        let error = bind(
            &fixture.data,
            &fixture.runtime(),
            Access::Write,
            &host("boot-1"),
        )
        .expect_err("a held names.lock means a launch in progress");
        assert!(format!("{error:#}").contains("names.lock"), "{error:#}");
        assert!(store.join("name").exists());
    }

    #[test]
    fn a_runtime_directory_someone_else_can_write_is_refused() {
        let fixture = Fixture::new();
        let boot = host("boot-1");
        let runtime = fixture.runtime();
        bind(&fixture.data, &runtime, Access::Write, &boot).unwrap();
        let place = runtime.dir.parent().unwrap().to_path_buf();
        for dir in [&fixture.base(), &place, &runtime.dir] {
            let mode = std::fs::metadata(dir).unwrap().mode() & 0o7777;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(mode | 0o002)).unwrap();
            for access in [Access::Read, Access::Write] {
                let error = bind(&fixture.data, &runtime, access, &boot).unwrap_err();
                assert!(
                    format!("{error:#}").contains("no one else can write"),
                    "{error:#}"
                );
            }
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(mode)).unwrap();
            bind(&fixture.data, &runtime, Access::Read, &boot).unwrap();
        }

        // A link where the directory should be, to a directory that passes every check.
        let real = fixture.run.join("real");
        std::fs::rename(&runtime.dir, &real).unwrap();
        std::os::unix::fs::symlink(&real, &runtime.dir).unwrap();
        let error = bind(&fixture.data, &runtime, Access::Read, &boot).unwrap_err();
        assert!(
            format!("{error:#}").contains("not a directory"),
            "{error:#}"
        );

        // A user who is neither the store's owner nor root makes nothing for the store,
        // under a base or in the data directory.
        let stranger = Host {
            euid: boot.euid + 1,
            ..boot.clone()
        };
        let fresh = Fixture::new();
        for runtime in [fresh.runtime(), fresh.on_store(&boot)] {
            let error = bind(&fresh.data, &runtime, Access::Write, &stranger).unwrap_err();
            assert!(
                format!("{error:#}").contains("only that user or root"),
                "{error:#}"
            );
            assert!(!runtime.dir.parent().unwrap().exists());
        }
    }

    /// The sweep judges a place orphaned, then the data directory is made again at the
    /// same path and a launch binds it to the same place before the sweep removes it. The
    /// sweep must leave the new binding: its record, its state directory and its states.
    #[test]
    fn a_place_bound_again_after_the_sweep_judged_it_orphaned_is_kept() {
        let fixture = Fixture::new();
        let boot = host("boot-1");
        let kept = fixture.runtime();
        bind(&fixture.data, &kept, Access::Write, &boot).unwrap();
        let data = fixture.data.parent().unwrap().join("again");
        std::fs::create_dir(&data).unwrap();
        let runtime = locate(None, None, &data, &fixture.run, &boot);
        assert_eq!(runtime.place, Place::Run);
        bind(&data, &runtime, Access::Write, &boot).unwrap();
        std::fs::remove_dir_all(&data).unwrap();

        let rebound = std::cell::Cell::new(false);
        remove_orphans_with(&kept, |dir| {
            if dir != place_dir(&runtime).unwrap() {
                return;
            }
            std::fs::create_dir(&data).unwrap();
            bind(&data, &runtime, Access::Write, &boot).unwrap();
            write_state(
                &runtime.dir.join("state/vms/0/vm-new.json"),
                "new",
                std::process::id(),
            );
            rebound.set(true);
        });
        assert!(rebound.get(), "the sweep judged the place orphaned first");
        assert!(
            runtime.dir.join(RECORD_FILE).exists()
                && runtime.dir.join("state/vms/0/vm-new.json").exists(),
            "the sweep removed a place bound again after it looked"
        );
        assert!(bind(&data, &runtime, Access::Read, &boot)
            .unwrap()
            .is_some());
    }

    #[test]
    fn the_runtime_directory_of_a_data_directory_that_is_gone_is_removed() {
        let fixture = Fixture::new();
        let boot = host("boot-1");
        let kept = fixture.runtime();
        bind(&fixture.data, &kept, Access::Write, &boot).unwrap();

        let make = |name: &str| {
            let data = fixture.data.parent().unwrap().join(name);
            std::fs::create_dir(&data).unwrap();
            let runtime = locate(None, None, &data, &fixture.run, &boot);
            assert_eq!(runtime.place, Place::Run);
            bind(&data, &runtime, Access::Write, &boot).unwrap();
            (data, runtime)
        };
        let (gone, gone_runtime) = make("gone");
        let (busy, busy_runtime) = make("busy");
        let (_present, present_runtime) = make("present");
        write_state(
            &busy_runtime.dir.join("state/vms/0/vm-live.json"),
            "live",
            std::process::id(),
        );
        write_state(
            &gone_runtime.dir.join("state/vms/0/vm-dead.json"),
            "dead",
            dead_pid(),
        );
        std::fs::remove_dir_all(&gone).unwrap();
        std::fs::remove_dir_all(&busy).unwrap();

        remove_orphans(&kept);
        assert!(!gone_runtime.dir.parent().unwrap().exists());
        assert!(
            busy_runtime.dir.exists(),
            "a state with a live owner keeps its directory"
        );
        assert!(present_runtime.dir.exists() && kept.dir.exists());

        // The store's own place has no neighbours to judge: its parent is the data
        // directory.
        remove_orphans(&fixture.on_store(&boot));
        assert!(fixture.data.exists() && fixture.store().exists());
    }
}
