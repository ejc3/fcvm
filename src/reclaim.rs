//! Host resources a killed fcvm leaves behind, and how the next fcvm takes them back.
//!
//! fcvm creates host resources that outlive its process unless teardown removes them: a
//! VM's runtime directory (`vm-disks/<vm_id>`, with its disks), the temporaries of an
//! image export. A SIGKILL skips
//! teardown. The kernel ends the child processes (pdeathsig) and releases locks; every
//! other resource stays.
//!
//! An owner record makes each of them reclaimable. Its files live in
//! `<data_dir>/reclaim/<euid>/<b>/`, where `<b>` is the bucket of the owner's id
//! (`state::manager::bucket_of`, 16 buckets), so that owners beginning and completing
//! together do not all take one directory's lock:
//!
//! | step | what the owner does |
//! |------|----------------------|
//! | [`Owner::begin`] | creates `<id>.lock` and holds an exclusive flock on it until it completes or exits |
//! | [`record`] | adds the resource to `<id>.json` (written under a temporary name and renamed) before creating it |
//! | [`complete`] | at the end of teardown, undoes what is still recorded and deletes both files |
//!
//! [`reclaim_abandoned`] is the sweep. It lists every bucket and takes every owner lock it
//! can without waiting, so its cost grows with the owners alive. The commands that need the
//! room a killed process left (setup, snapshot import and export) run it when they
//! start. A VM launch runs it through [`reclaim_abandoned_if_due`]: at most one launch
//! in each [`RECLAIM_EVERY`] sweeps, and the others read one stamp and go on, so a launch
//! lists or locks nothing whose size grows with the VMs running. What a killed owner
//! recorded is therefore reclaimed by the first launch that starts `RECLAIM_EVERY` (1 s) or
//! more after the last sweep began, or by the next of those commands: that launch or
//! command is "the next sweep" wherever a comment says so. A launch that is refused
//! something a killed VM may hold sweeps at once and tries again
//! ([`reclaim_abandoned_on_contention`]): a bridged launch that finds no free subnet. Nothing a launch makes
//! depends on an earlier sweep: names are random or settled against what exists, and an
//! owner that dies just after a sweep listed it was never seen by that sweep either.
//!
//! The sweep takes every owner lock it can
//! without waiting. A lock nobody holds belongs to an owner that exited without completing,
//! so the resources in its record are undone and the record is deleted. The flock is the
//! owner's liveness: the kernel releases it when the process dies however it dies, and a
//! reused PID cannot hold it. A resource that cannot be undone yet stays in the record for
//! the next sweep.
//!
//! Recording comes before creating, so a kill between the two leaves a record of something
//! that does not exist, and undoing it finds nothing. That is safe only for names no other
//! process can create, which holds for a VM's directory, named by its random vm_id, and a
//! temporary, named by a random UUID.
//!
//! A snapshot create's staging directory (`<tag>.creating`) is the exception: every create
//! of one tag stages under that name, so a killed create's record names a directory that a
//! later create of the tag makes again. A create holds the tag's lock exclusively for as
//! long as it uses the directory, so the undo removes it only under that lock, taken
//! without waiting, and leaves it while a create holds the lock
//! ([`Resource::SnapshotStaging`]).
//!
//! Network objects follow the same rule with one change, because their names are not
//! random: a namespace is named from a few characters of the vm_id and settled by
//! `ip netns add` (`network::names`), so it is recorded right after it is created, and a
//! kill between the two leaks it. Every other object of a VM's network is named from the
//! namespace's base or carries an address the VM's veth holds, so no other VM can have it
//! while the namespace exists, and is recorded before it is created. Teardown drops those
//! records before it deletes the veth and the namespace, which is when another VM can
//! take the names (`forget`). Kernel objects are gone after a reboot and their names
//! can be taken again, so a record from another boot is never acted on for them.
//!
//! Records an fcvm before the buckets wrote sit in the owner directory itself, and the
//! sweep reads them there. Such an fcvm does not read the buckets: it leaves what a newer
//! one recorded to a newer one's sweep.
//!
//! Each user reclaims only the records in its own directory, which it creates mode 0700
//! and refuses when anyone else can write it, so root never acts on a record another user
//! wrote. Records stay per data_dir, like the VMs they describe: flock is authoritative
//! within one kernel, and a data_dir is not shared across nesting levels.

use std::collections::HashMap;
use std::fs::File;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, Weak};

use anyhow::{bail, Context, Result};
use nix::fcntl::{Flock, FlockArg};
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

/// A host resource an owner creates and teardown removes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Resource {
    /// A VM's runtime directory. Removed whole.
    RuntimeDir { path: PathBuf },
    /// A snapshot create's staging directory (`<tag>.creating`), which is renamed into
    /// place or removed when the create finishes. Every create of the tag uses this name
    /// while it holds `lock`, the tag's lock, exclusively, so the directory is removed
    /// only under that lock: with a plain `Temp`, the record of a killed create would
    /// remove the directory a live create of the same tag is writing.
    SnapshotStaging { path: PathBuf, lock: PathBuf },
    /// A temporary file or directory that is renamed into place or removed when its
    /// operation finishes.
    Temp { path: PathBuf },
    /// A memory file `snapshot create --memory-dir` wrote outside its snapshot directory.
    /// Removed unless `link`, the published snapshot's `memory.bin`, names it: a create killed
    /// before it published leaves the file to be removed, one killed after keeps it.
    LinkedMemory { path: PathBuf, link: PathBuf },
    /// A named network namespace (`ip netns add`).
    Netns { name: String },
    /// A host network interface. Deleting one end of a veth pair deletes the other.
    Link { name: String },
    /// An iptables or ip6tables rule, as the arguments that append it (`-A`).
    Rule { program: String, args: Vec<String> },
    /// A bridged clone's host route to its guest address through its own veth.
    HostRoute { guest_ip: String, via: String },
    /// The MASQUERADE rules every bridged VM shares, removed once no bridged VM's veth
    /// remains (`portmap::cleanup_global_nat_if_unused`).
    GlobalNat,
    /// A routed VM's proxy NDP entry and, without `--ipv6-prefix`, its MASQUERADE rule,
    /// both keyed by its IPv6 address. Undone only while nobody holds the address's
    /// lease, which every routed VM holds for as long as it uses the address.
    RoutedAddress {
        address: std::net::Ipv6Addr,
        iface: String,
        masquerade: bool,
    },
    /// A kind this build does not know, such as a `mount_target` an fcvm before #1311
    /// recorded for a source directory it recreated. It is dropped from the record, so the
    /// record's other resources are still undone; without it, one unknown entry made the
    /// whole record unreadable and every resource in it stayed.
    #[serde(other)]
    Unknown,
}

impl Resource {
    /// An object of the running kernel, which a reboot discards.
    fn is_kernel_object(&self) -> bool {
        !matches!(
            self,
            Resource::RuntimeDir { .. }
                | Resource::SnapshotStaging { .. }
                | Resource::Temp { .. }
                | Resource::LinkedMemory { .. }
                | Resource::Unknown
        )
    }

    /// Part of a VM's network, which its teardown removes (`forget`).
    pub fn is_network(&self) -> bool {
        self.is_kernel_object()
    }
}

impl std::fmt::Display for Resource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Resource::RuntimeDir { path } => write!(f, "runtime directory {}", path.display()),
            Resource::SnapshotStaging { path, .. } => {
                write!(f, "snapshot staging directory {}", path.display())
            }
            Resource::Unknown => write!(f, "a resource of a kind this build does not know"),
            Resource::Temp { path } => write!(f, "temporary {}", path.display()),
            Resource::LinkedMemory { path, .. } => write!(f, "memory file {}", path.display()),
            Resource::Netns { name } => write!(f, "network namespace {name}"),
            Resource::Link { name } => write!(f, "interface {name}"),
            Resource::Rule { program, args } => write!(f, "{program} {}", args.join(" ")),
            Resource::HostRoute { guest_ip, via } => {
                write!(f, "host route {guest_ip}/32 via {via}")
            }
            Resource::GlobalNat => write!(f, "shared bridged MASQUERADE rules"),
            Resource::RoutedAddress { address, iface, .. } => {
                write!(f, "proxy NDP and MASQUERADE of {address} on {iface}")
            }
        }
    }
}

/// What `<id>.json` holds. The PID is for whoever reads the file; liveness is the lock.
#[derive(Debug, Serialize, Deserialize)]
struct Record {
    pid: u32,
    /// The kernel's boot id when the record was written. Kernel objects of another boot
    /// are gone.
    boot_id: String,
    /// The inode of the user namespace the record was written in; None in a record
    /// written before this field, all of which were written in the initial one.
    #[serde(default)]
    user_namespace: Option<u64>,
    resources: Vec<Resource>,
}

fn boot_id() -> String {
    std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .map(|id| id.trim().to_string())
        .unwrap_or_default()
}

/// The directory this user's owner records live in, under `data_dir`.
pub fn owner_dir() -> Result<PathBuf> {
    owner_dir_in(&crate::paths::data_dir())
}

/// The inode of the initial user namespace (`PROC_USER_INIT_INO`, linux/proc_ns.h).
const INITIAL_USER_NAMESPACE: u64 = 0xEFFF_FFFD;

/// The inode of this process's user namespace, or None when /proc does not say.
fn current_user_namespace() -> Option<u64> {
    std::fs::metadata("/proc/self/ns/user")
        .ok()
        .map(|ns| ns.ino())
}

/// The host uid `euid` maps to in this process's user namespace, from its
/// `/proc/self/uid_map` (`map`): lines of `inside outside count`.
fn host_uid_of(map: &str, euid: u32) -> Option<u32> {
    map.lines().find_map(|line| {
        let mut fields = line.split_whitespace().map(|f| f.parse::<u64>().ok());
        let (inside, outside, count) = (fields.next()??, fields.next()??, fields.next()??);
        let euid = u64::from(euid);
        (inside <= euid && euid < inside + count)
            .then(|| u32::try_from(outside + (euid - inside)).ok())
            .flatten()
    })
}

/// The name of this process's owner directory under `reclaim/`: the euid in the initial
/// user namespace, and in any other the euid with the host uid it maps to
/// (`<euid>-host<uid>`, or `<euid>-userns<inode>` when the map does not say).
///
/// A container's root is uid 0 of its own user namespace, not the host's root, and when
/// the two share a store (CI runners keep /mnt/fcvm-btrfs between jobs) the host root's
/// `reclaim/0` shows in the container as owned by the overflow uid, which `owner_dir_in`
/// refuses. Naming by the mapping rather than by the namespace keeps a replaced
/// container's records reachable to the next container with the same mapping, so the
/// files they name are reclaimed; the kernel objects they name were the old namespace's,
/// and `reclaim_abandoned_in` leaves those alone.
fn owner_dir_name(euid: u32, user_namespace: Option<u64>, host_uid: Option<u32>) -> String {
    match (user_namespace, host_uid) {
        (None, _) => euid.to_string(),
        (Some(namespace), _) if namespace == INITIAL_USER_NAMESPACE => euid.to_string(),
        (Some(_), Some(host)) => format!("{euid}-host{host}"),
        (Some(namespace), None) => format!("{euid}-userns{namespace}"),
    }
}

/// How many directories one owner directory name has: `<name>`, then `<name>.1` and on.
const OWNER_DIR_SLOTS: usize = 64;

/// The owner directory under `shared` for the name `name` and the uid `euid`: the first of
/// `<name>`, `<name>.1`, `<name>.2`, ... that `euid` owns, or else the first that can be made.
/// `owner_of` reads who owns a path, or None when nothing is there.
///
/// The name is the euid with the uid it maps to in the parent user namespace, which is all a
/// process can read of its mapping. In a user namespace nested inside another non-initial one,
/// two containers can both map uid 0 to their parent's uid 0 while their parents map to
/// different host owners. Both get the name `0-host0`, and the directory the first one made
/// shows to the second as owned by the overflow uid, which `owner_dir_in` refuses (#1465). The
/// owner that the filesystem records is what separates them, so a directory another uid owns
/// is passed over. A directory this uid owns is looked for first, in every slot, so that a
/// later process with the same mapping finds the records the earlier one left, whichever slot
/// it was given.
fn pick_owner_dir(
    shared: &Path,
    name: &str,
    euid: u32,
    owner_of: impl Fn(&Path) -> Option<u32>,
) -> Result<PathBuf> {
    let slot = |n: usize| {
        if n == 0 {
            shared.join(name)
        } else {
            shared.join(format!("{name}.{n}"))
        }
    };
    if let Some(mine) = (0..OWNER_DIR_SLOTS)
        .map(slot)
        .find(|path| owner_of(path) == Some(euid))
    {
        return Ok(mine);
    }
    for n in 0..OWNER_DIR_SLOTS {
        let path = slot(n);
        match std::fs::DirBuilder::new().mode(0o700).create(&path) {
            Ok(()) => return Ok(path),
            // Made by another process of this uid in the meantime, or another uid's.
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                if owner_of(&path) == Some(euid) {
                    return Ok(path);
                }
            }
            Err(error) => {
                return Err(error).with_context(|| format!("creating {}", path.display()))
            }
        }
    }
    bail!(
        "every owner directory named {name} in {} belongs to another uid ({OWNER_DIR_SLOTS} \
         tried); remove the ones no fcvm uses",
        shared.display()
    )
}

/// [`owner_dir`] under an explicit data directory.
pub fn owner_dir_in(data_dir: &Path) -> Result<PathBuf> {
    owner_dir_under(data_dir, "reclaim")
}

/// This user's directory under `<data_dir>/<area>/`, made and checked as the owner records'
/// directory is: mode 0700, owned by this user, refused when anyone else can write it. Root
/// and a user share a data directory but never such a directory.
pub fn owner_dir_under(data_dir: &Path, area: &str) -> Result<PathBuf> {
    std::fs::create_dir_all(data_dir)
        .with_context(|| format!("creating data directory {}", data_dir.display()))?;
    let shared = data_dir.join(area);
    match std::fs::DirBuilder::new().mode(0o1777).create(&shared) {
        // Like /tmp: every user makes a directory of its own here, and the sticky bit
        // keeps users from removing each other's. The umask cleared some of the bits.
        Ok(()) => std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o1777))
            .with_context(|| format!("setting the mode of {}", shared.display()))?,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => {
            return Err(error).with_context(|| format!("creating {}", shared.display()));
        }
    }
    let euid = nix::unistd::geteuid().as_raw();
    let host_uid = std::fs::read_to_string("/proc/self/uid_map")
        .ok()
        .and_then(|map| host_uid_of(&map, euid));
    let name = owner_dir_name(euid, current_user_namespace(), host_uid);
    let mine = pick_owner_dir(&shared, &name, euid, |path| {
        std::fs::symlink_metadata(path)
            .ok()
            .map(|metadata| metadata.uid())
    })?;
    let metadata =
        std::fs::symlink_metadata(&mine).with_context(|| format!("reading {}", mine.display()))?;
    if !metadata.is_dir() || metadata.uid() != euid || metadata.mode() & 0o022 != 0 {
        bail!(
            "refusing this user's directory {}: it must be a directory owned by uid {euid} that \
             no one else can write (found uid {}, mode {:o}{})",
            mine.display(),
            metadata.uid(),
            metadata.mode() & 0o7777,
            if metadata.is_dir() {
                ""
            } else {
                ", not a directory"
            }
        );
    }
    Ok(mine)
}

/// A user's owner directory, held open. Every record is created, read, renamed and removed
/// through this descriptor, never by path: under sudo root works in a data directory a
/// user owns, and that user could otherwise move another directory in place of root's
/// between the check and the use, and make root act on records it did not write.
struct OwnerDir {
    path: PathBuf,
    fd: File,
}

impl OwnerDir {
    /// Open `path` without following a symlink, and refuse it unless it is a directory of
    /// this user's that no one else can write, judged on the descriptor itself.
    fn open(path: &Path) -> Result<Self> {
        use nix::fcntl::OFlag;
        let fd = nix::fcntl::open(
            path,
            OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
            nix::sys::stat::Mode::empty(),
        )
        .with_context(|| format!("opening owner directory {}", path.display()))?;
        Self::checked(path.to_path_buf(), File::from(fd))
    }

    /// The checks of [`OwnerDir::open`], on an open descriptor.
    fn checked(path: PathBuf, fd: File) -> Result<Self> {
        let metadata = fd
            .metadata()
            .with_context(|| format!("reading owner directory {}", path.display()))?;
        let euid = nix::unistd::geteuid().as_raw();
        if metadata.uid() != euid || metadata.mode() & 0o022 != 0 {
            bail!(
                "refusing owner records in {}: it must be owned by uid {euid} and writable by \
                 no one else (found uid {}, mode {:o})",
                path.display(),
                metadata.uid(),
                metadata.mode() & 0o7777
            );
        }
        Ok(Self { path, fd })
    }

    /// The bucket directory `name` of this owner directory, opened through this descriptor
    /// and held to the rules of the owner directory itself, or `None` when there is none.
    fn open_bucket(&self, name: &str) -> Result<Option<Self>> {
        use nix::fcntl::OFlag;
        let path = self.path.join(name);
        match nix::fcntl::openat(
            &self.fd,
            name,
            OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
            nix::sys::stat::Mode::empty(),
        ) {
            Ok(fd) => Self::checked(path, File::from(fd)).map(Some),
            Err(nix::errno::Errno::ENOENT) => Ok(None),
            Err(error) => {
                Err(error).with_context(|| format!("opening owner bucket {}", path.display()))
            }
        }
    }

    /// The bucket directory that holds the files of the owner `id`, made if it is missing.
    fn bucket_of(&self, id: &str) -> Result<Self> {
        let name = crate::state::manager::bucket_of(id);
        match nix::sys::stat::mkdirat(
            &self.fd,
            name.as_str(),
            nix::sys::stat::Mode::from_bits_truncate(0o700),
        ) {
            Ok(()) | Err(nix::errno::Errno::EEXIST) => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("creating owner bucket {}", self.display(&name)))
            }
        }
        self.open_bucket(&name)?
            .with_context(|| format!("the owner bucket {} is gone", self.display(&name)))
    }

    /// Every directory of this owner directory that can hold records: the buckets that
    /// exist, and the owner directory itself, where an fcvm before the buckets kept them.
    fn record_dirs(self) -> Result<Vec<Self>> {
        let mut dirs = Vec::new();
        for name in BUCKETS.chars().map(String::from) {
            if let Some(bucket) = self.open_bucket(&name)? {
                dirs.push(bucket);
            }
        }
        dirs.push(self);
        Ok(dirs)
    }

    fn display(&self, name: &str) -> String {
        self.path.join(name).display().to_string()
    }

    fn open_file(&self, name: &str, flags: nix::fcntl::OFlag) -> nix::Result<File> {
        use nix::fcntl::OFlag;
        nix::fcntl::openat(
            &self.fd,
            name,
            flags | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
            nix::sys::stat::Mode::from_bits_truncate(0o600),
        )
        .map(File::from)
    }

    /// The identity of `name` in the directory, or `None` when there is nothing there.
    fn identity(&self, name: &str) -> Result<Option<(u64, u64)>> {
        match nix::sys::stat::fstatat(&self.fd, name, nix::fcntl::AtFlags::AT_SYMLINK_NOFOLLOW) {
            Ok(stat) => Ok(Some((stat.st_dev, stat.st_ino))),
            Err(nix::errno::Errno::ENOENT) => Ok(None),
            Err(error) => Err(error).with_context(|| format!("reading {}", self.display(name))),
        }
    }

    fn remove(&self, name: &str) -> Result<()> {
        match nix::unistd::unlinkat(&self.fd, name, nix::unistd::UnlinkatFlags::NoRemoveDir) {
            Ok(()) | Err(nix::errno::Errno::ENOENT) => Ok(()),
            Err(error) => Err(error).with_context(|| format!("removing {}", self.display(name))),
        }
    }

    fn read(&self, name: &str) -> Result<Option<Vec<u8>>> {
        use std::io::Read;
        let mut file = match self.open_file(name, nix::fcntl::OFlag::O_RDONLY) {
            Ok(file) => file,
            Err(nix::errno::Errno::ENOENT) => return Ok(None),
            Err(error) => {
                return Err(error).with_context(|| format!("opening {}", self.display(name)))
            }
        };
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)
            .with_context(|| format!("reading {}", self.display(name)))?;
        Ok(Some(bytes))
    }

    fn names(&self) -> Result<Vec<String>> {
        use nix::fcntl::OFlag;
        let listing = nix::dir::Dir::openat(
            &self.fd,
            ".",
            OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC,
            nix::sys::stat::Mode::empty(),
        )
        .with_context(|| format!("listing {}", self.path.display()))?;
        let mut names = Vec::new();
        for entry in listing {
            let entry = entry.with_context(|| format!("listing {}", self.path.display()))?;
            if let Ok(name) = entry.file_name().to_str() {
                if name != "." && name != ".." {
                    names.push(name.to_string());
                }
            }
        }
        Ok(names)
    }
}

/// The names of an owner directory's buckets: every value of `state::manager::bucket_of`.
const BUCKETS: &str = "0123456789abcdef";

/// The lock of the launch that is sweeping an owner directory, and the stamp of the last
/// sweep that began: the boot it ran in and when, as `<boot id> <CLOCK_BOOTTIME ns>`. The
/// time is the kernel's count since boot, which no clock change moves, and the boot id says
/// which boot's count it is. No owner id
/// begins with a dot, and neither name ends as a record's files do, so no sweep, of this
/// build or of one that kept its records in the owner directory, takes them for an owner's.
const SWEEP_LOCK: &str = ".sweep-lock";
const SWEEP_STAMP: &str = ".sweep-stamp";

/// How often a VM launch sweeps ([`reclaim_abandoned_if_due`]). A sweep lists every bucket
/// and tries every owner's lock, so its cost grows with the VMs running; one a second is one
/// launch in hundreds during a ramp, and bounds how long a killed VM's directory, namespace
/// and rules outlive it on a host that keeps launching.
pub const RECLAIM_EVERY: std::time::Duration = std::time::Duration::from_secs(1);

/// `FCVM_RECLAIM_EVERY_MS` replaces [`RECLAIM_EVERY`] when it holds a number of
/// milliseconds. `0` makes every launch sweep before it does anything else, whatever
/// another launch is doing, which is what a launch did before the interval existed.
pub const RECLAIM_EVERY_ENV: &str = "FCVM_RECLAIM_EVERY_MS";

/// The interval in force, from the value of [`RECLAIM_EVERY_ENV`] if any.
fn reclaim_every(value: Option<&str>) -> std::time::Duration {
    value
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(std::time::Duration::from_millis)
        .unwrap_or(RECLAIM_EVERY)
}

fn lock_name(id: &str) -> String {
    format!("{id}.lock")
}

fn record_name(id: &str) -> String {
    format!("{id}.json")
}

fn record_temp_name(id: &str) -> String {
    format!("{id}.json.tmp")
}

/// Where the owner `id`'s record is, in the owner directory `dir`.
#[doc(hidden)]
pub fn record_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(crate::state::manager::bucket_of(id))
        .join(record_name(id))
}

/// Every record and lock file in the owner directory `dir`, in its buckets and in the
/// directory itself, sorted. For tests and tools that look at what owners left.
#[doc(hidden)]
pub fn record_files(dir: &Path) -> Vec<PathBuf> {
    let is_record_file = |path: &PathBuf| {
        path.file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| !name.starts_with('.'))
            && path.is_file()
    };
    let list = |dir: &Path| -> Vec<PathBuf> {
        std::fs::read_dir(dir)
            .map(|entries| entries.flatten().map(|entry| entry.path()).collect())
            .unwrap_or_default()
    };
    let mut files: Vec<PathBuf> = list(dir).into_iter().filter(is_record_file).collect();
    for bucket in BUCKETS.chars() {
        files.extend(
            list(&dir.join(bucket.to_string()))
                .into_iter()
                .filter(is_record_file),
        );
    }
    files.sort();
    files
}

/// The file names of [`record_files`], sorted.
#[doc(hidden)]
pub fn record_file_names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = record_files(dir)
        .iter()
        .filter_map(|path| Some(path.file_name()?.to_string_lossy().into_owned()))
        .collect();
    names.sort();
    names
}

/// Lock the owner `id`'s lock file exclusively, creating it if needed. `None` when `wait`
/// is false and someone holds it. A lock file is unlinked by whoever completes or reclaims
/// its owner while still holding it, so the lock is retaken until the inode held is the
/// one at the name.
fn lock_owner_file(dir: &OwnerDir, id: &str, wait: bool) -> Result<Option<Flock<File>>> {
    use nix::fcntl::OFlag;
    let name = lock_name(id);
    loop {
        let file = dir
            .open_file(&name, OFlag::O_CREAT | OFlag::O_WRONLY)
            .with_context(|| format!("opening {}", dir.display(&name)))?;
        let arg = if wait {
            FlockArg::LockExclusive
        } else {
            FlockArg::LockExclusiveNonblock
        };
        let guard = match Flock::lock(file, arg) {
            Ok(guard) => guard,
            Err((_, nix::errno::Errno::EWOULDBLOCK)) if !wait => return Ok(None),
            Err((_, error)) => {
                return Err(error).with_context(|| format!("locking {}", dir.display(&name)))
            }
        };
        let held = guard
            .metadata()
            .with_context(|| format!("reading the identity of {}", dir.display(&name)))?;
        if dir.identity(&name)? == Some((held.dev(), held.ino())) {
            return Ok(Some(guard));
        }
    }
}

/// Close this process's descriptor of an owner lock without unlocking it. An explicit
/// unlock would release the lock for every descriptor of it, including the ones helpers
/// inherited (`Holder`), as a process that dies does not.
fn release(lock: Flock<File>) {
    let lock = std::mem::ManuallyDrop::new(lock);
    // SAFETY: the wrapper is never dropped, so the file is read out of it exactly once.
    let file: File = unsafe { std::ptr::read(&**lock) };
    drop(file);
}

fn write_record(dir: &OwnerDir, id: &str, resources: &[Resource]) -> Result<()> {
    use nix::fcntl::OFlag;
    use std::io::Write;
    let record = Record {
        pid: std::process::id(),
        boot_id: boot_id(),
        user_namespace: current_user_namespace(),
        resources: resources.to_vec(),
    };
    let temp = record_temp_name(id);
    let mut file = dir
        .open_file(&temp, OFlag::O_CREAT | OFlag::O_WRONLY | OFlag::O_TRUNC)
        .with_context(|| format!("creating {}", dir.display(&temp)))?;
    file.write_all(&serde_json::to_vec(&record)?)
        .with_context(|| format!("writing {}", dir.display(&temp)))?;
    drop(file);
    nix::fcntl::renameat(&dir.fd, temp.as_str(), &dir.fd, record_name(id).as_str())
        .with_context(|| format!("publishing the owner record of {id}"))
}

/// Delete an owner's files. The caller holds `lock`, and the lock file goes last so that
/// no one can take a new lock on the id while its record still exists.
fn delete_owner_files(dir: &OwnerDir, id: &str, lock: Flock<File>) -> Result<()> {
    dir.remove(&record_name(id))
        .and_then(|()| dir.remove(&record_temp_name(id)))
        .and_then(|()| dir.remove(&lock_name(id)))
        .with_context(|| format!("deleting the owner record of {id}"))?;
    release(lock);
    Ok(())
}

struct OwnerState {
    /// Held until the owner completes or is dropped.
    lock: Option<Flock<File>>,
    resources: Vec<Resource>,
    completed: bool,
}

struct Inner {
    id: String,
    dir: OwnerDir,
    state: Mutex<OwnerState>,
}

/// Owners of this process, by id, for code that records a resource deep inside a VM's
/// setup and has only the vm_id to hand.
static OWNERS: LazyLock<Mutex<HashMap<String, Weak<Inner>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// The live owner of a set of resources. Dropping it without [`Owner::complete`] (an early
/// return, a panic) releases its lock and keeps its record, so the next sweep reclaims
/// what it recorded.
pub struct Owner {
    inner: Arc<Inner>,
}

impl Owner {
    /// Begin owning resources under `id`, in this user's owner directory.
    pub fn begin(id: &str) -> Result<Owner> {
        Self::begin_in(&owner_dir()?, id)
    }

    /// [`Owner::begin`] in an explicit directory.
    pub fn begin_in(dir: &Path, id: &str) -> Result<Owner> {
        anyhow::ensure!(
            !id.is_empty() && !id.starts_with('.') && !id.contains('/'),
            "invalid owner id {id:?}"
        );
        let mut owners = OWNERS.lock().unwrap_or_else(|e| e.into_inner());
        if owners.get(id).and_then(Weak::upgrade).is_some() {
            bail!("resources under {id} already have an owner in this process");
        }
        let owner_dir = OwnerDir::open(dir)?.bucket_of(id)?;
        let lock = lock_owner_file(&owner_dir, id, true)?
            .context("an owner lock taken with waiting returned none")?;
        if owner_dir.identity(&record_name(id))?.is_some() {
            bail!(
                "an owner record for {id} already exists in {}; ids are never reused",
                dir.display()
            );
        }
        let inner = Arc::new(Inner {
            id: id.to_string(),
            dir: owner_dir,
            state: Mutex::new(OwnerState {
                lock: Some(lock),
                resources: Vec::new(),
                completed: false,
            }),
        });
        owners.insert(id.to_string(), Arc::downgrade(&inner));
        Ok(Owner { inner })
    }

    /// Record `resource` before creating it.
    pub fn record(&self, resource: Resource) -> Result<()> {
        self.inner.record(resource)
    }

    /// Delete the record without undoing anything: the caller removed or published every
    /// resource it recorded.
    pub fn discard(self) -> Result<()> {
        let mut state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
        state.completed = true;
        state.resources.clear();
        let lock = state
            .lock
            .take()
            .context("a discarded owner holds its lock")?;
        delete_owner_files(&self.inner.dir, &self.inner.id, lock)
    }

    /// Undo whatever is still recorded and delete the record (see [`complete`]).
    pub async fn complete(self) -> Result<()> {
        self.inner.complete().await
    }
}

impl Owner {
    /// A handle on this owner's lock that a child process can inherit (`Holder::hold_in`).
    /// The lock is one open file description, so it stays held while any process holds a
    /// descriptor of it: a helper that writes this owner's resources keeps the owner live
    /// until it exits, and nothing reclaims what it is still writing.
    pub fn holder(&self) -> Result<Holder> {
        self.inner.holder()
    }
}

impl Inner {
    fn holder(&self) -> Result<Holder> {
        use std::os::fd::AsFd;
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let lock = state
            .lock
            .as_ref()
            .context("a completed owner has no lock to hold")?;
        let fd = lock
            .as_fd()
            .try_clone_to_owned()
            .context("duplicating the owner lock")?;
        Ok(Holder(std::sync::Arc::new(fd)))
    }
}

/// A descriptor of an owner's lock (`Owner::holder`).
#[derive(Clone)]
pub struct Holder(std::sync::Arc<std::os::fd::OwnedFd>);

impl Holder {
    /// Let the process `command` starts inherit the lock, so the owner stays live until
    /// that process exits, whatever happens to this one.
    pub fn hold_in(&self, command: &mut tokio::process::Command) {
        use std::os::fd::AsRawFd;
        let fd = self.0.as_raw_fd();
        let keep = self.0.clone();
        // SAFETY: fcntl is async-signal-safe and touches only the child's descriptor table.
        unsafe {
            command.pre_exec(move || {
                let _ = &keep;
                if libc::fcntl(fd, libc::F_SETFD, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
}

impl Drop for Owner {
    fn drop(&mut self) {
        let mut owners = OWNERS.lock().unwrap_or_else(|e| e.into_inner());
        if owners
            .get(&self.inner.id)
            .is_some_and(|weak| weak.as_ptr() == Arc::as_ptr(&self.inner))
        {
            owners.remove(&self.inner.id);
        }
        drop(owners);
        let mut state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
        if !state.completed && !state.resources.is_empty() {
            debug!(
                owner = %self.inner.id,
                resources = state.resources.len(),
                "owner ended without completing; the next sweep reclaims what it recorded"
            );
        }
        if let Some(lock) = state.lock.take() {
            release(lock);
        }
    }
}

impl Inner {
    fn record(&self, resource: Resource) -> Result<()> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.completed {
            bail!(
                "the owner {} has completed; cannot record {resource}",
                self.id
            );
        }
        // Two published ports can append one rule twice (the same guest port), and each
        // copy needs its own delete.
        if !matches!(resource, Resource::Rule { .. }) && state.resources.contains(&resource) {
            return Ok(());
        }
        state.resources.push(resource);
        if let Err(error) = write_record(&self.dir, &self.id, &state.resources) {
            state.resources.pop();
            return Err(error);
        }
        Ok(())
    }

    async fn complete(&self) -> Result<()> {
        let resources = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if state.completed {
                return Ok(());
            }
            std::mem::take(&mut state.resources)
        };
        let remaining = undo_all(&self.id, resources, true, |list| {
            write_record(&self.dir, &self.id, list)
        })
        .await;
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.completed = true;
        let lock = state
            .lock
            .take()
            .context("a completing owner holds its lock")?;
        if remaining.is_empty() {
            return delete_owner_files(&self.dir, &self.id, lock);
        }
        if remaining.iter().all(|(_, why)| why.starts_with(IN_USE)) {
            // Another VM still holds the lease of a routed address this one recorded. The
            // record stays, and the first launch after the lease ends removes it.
            let resources: Vec<Resource> = remaining.into_iter().map(|(r, _)| r).collect();
            write_record(&self.dir, &self.id, &resources)?;
            release(lock);
            return Ok(());
        }
        let kept = remaining
            .iter()
            .map(|(resource, why)| format!("{resource}: {why}"))
            .collect::<Vec<_>>()
            .join("; ");
        let resources: Vec<Resource> = remaining.into_iter().map(|(r, _)| r).collect();
        write_record(&self.dir, &self.id, &resources)?;
        release(lock);
        bail!(
            "{} resource(s) of {} remain for the next sweep to reclaim: {kept}",
            resources.len(),
            self.id
        )
    }
}

fn owner(id: &str) -> Result<Arc<Inner>> {
    OWNERS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(id)
        .and_then(Weak::upgrade)
        .with_context(|| format!("no owner records resources for {id} in this process"))
}

/// Record `resource` for the owner registered under `id`, before creating it. Fails when
/// no owner is registered: a resource that is created without a record leaks on SIGKILL.
pub fn record(id: &str, resource: Resource) -> Result<()> {
    owner(id)?.record(resource)
}

/// A handle on the lock of the owner registered under `id` (`Owner::holder`), for a helper
/// that writes one of its resources.
pub fn holder(id: &str) -> Result<Holder> {
    owner(id)?.holder()
}

/// Drop the records of the owner under `id` that `matches` selects, without undoing
/// them: teardown is about to remove those objects itself, after which their names can
/// belong to another VM. Nothing is recorded without an owner, so none is not an error.
pub fn forget(id: &str, matches: impl Fn(&Resource) -> bool) -> Result<()> {
    let Ok(inner) = owner(id) else {
        return Ok(());
    };
    let mut state = inner.state.lock().unwrap_or_else(|e| e.into_inner());
    if state.completed {
        return Ok(());
    }
    let kept: Vec<Resource> = state
        .resources
        .iter()
        .filter(|resource| !matches(resource))
        .cloned()
        .collect();
    if kept.len() == state.resources.len() {
        return Ok(());
    }
    // On disk first: until the write lands the objects are still recorded there, and
    // the caller must not delete them.
    write_record(&inner.dir, &inner.id, &kept)?;
    state.resources = kept;
    Ok(())
}

/// End the owner registered under `id`: undo every resource still
/// recorded, and delete its record. Teardown has removed most of them already, and undoing
/// a resource that is gone does nothing. A resource that cannot be undone stays recorded
/// for the next sweep and fails this call.
pub async fn complete(id: &str) -> Result<()> {
    owner(id)?.complete().await
}

/// Why an undo left a resource in place.
pub(crate) enum Undo {
    /// The resource is gone, or is not this owner's to remove.
    Done,
    /// Still there; a later launch tries again.
    Kept(String),
    /// Still there because a live process uses it: a lease or a lock on it is held. Not a
    /// failure; the record stays until a launch finds it unused.
    InUse(String),
}

/// How the reason of an [`Undo::InUse`] resource starts, in what `undo_all` returns.
const IN_USE: &str = "in use: ";

/// Undo `resources` newest first, returning the ones that stay. `same_boot` is false for
/// a record written before the last reboot, whose kernel objects are gone, or in another
/// user namespace, whose kernel objects are not this process's to name.
///
/// A kernel object's name can be another VM's once the object is deleted, so its record
/// is rewritten without it (`persist`) before it is deleted: a kill in between leaks the
/// object instead of leaving a record that would delete a namesake. When one cannot be
/// deleted, the older kernel objects stay as well, because they are what keeps its names
/// and addresses this owner's (the namespace and the veth outlive the rules that name
/// them).
async fn undo_all(
    id: &str,
    resources: Vec<Resource>,
    same_boot: bool,
    persist: impl Fn(&[Resource]) -> Result<()>,
) -> Vec<(Resource, String)> {
    let mut pending = resources;
    // Newest first.
    let mut remaining: Vec<(Resource, String)> = Vec::new();
    let mut hold_kernel: Option<String> = None;
    let record = |pending: &[Resource], remaining: &[(Resource, String)]| -> Vec<Resource> {
        pending
            .iter()
            .cloned()
            .chain(remaining.iter().rev().map(|(r, _)| r.clone()))
            .collect()
    };
    while let Some(resource) = pending.pop() {
        if !resource.is_kernel_object() {
            match undo(&resource).await {
                Undo::Done => debug!(owner = %id, %resource, "undone"),
                Undo::Kept(why) => remaining.push((resource, why)),
                Undo::InUse(why) => remaining.push((resource, format!("{IN_USE}{why}"))),
            }
            continue;
        }
        if !same_boot {
            debug!(owner = %id, %resource, "recorded before the last boot or in another user namespace; left alone");
            continue;
        }
        if let Some(why) = &hold_kernel {
            remaining.push((resource, why.clone()));
            continue;
        }
        if let Err(error) = persist(&record(&pending, &remaining)) {
            let why = format!("its record could not be retired first: {error:#}");
            hold_kernel = Some(why.clone());
            remaining.push((resource, why));
            continue;
        }
        let left = match undo(&resource).await {
            Undo::Done => {
                debug!(owner = %id, %resource, "undone");
                continue;
            }
            Undo::Kept(why) => why,
            Undo::InUse(why) => format!("{IN_USE}{why}"),
        };
        hold_kernel = Some(format!("{IN_USE}{resource} stays"));
        remaining.push((resource, left));
        // Still this owner's: record it again.
        let _ = persist(&record(&pending, &remaining));
    }
    remaining.reverse();
    remaining
}

async fn undo(resource: &Resource) -> Undo {
    match resource {
        Resource::RuntimeDir { path } => undo_runtime_dir(path).await,
        Resource::SnapshotStaging { path, lock } => undo_snapshot_staging(path, lock).await,
        Resource::Unknown => Undo::Done,
        Resource::Temp { path } => undo_temp(path).await,
        Resource::LinkedMemory { path, link } => undo_linked_memory(path, link),
        Resource::Netns { name } => {
            undo_result(crate::network::namespace::delete_namespace(name).await)
        }
        Resource::Link { name } => undo_result(crate::network::veth::delete_veth_pair(name).await),
        Resource::Rule { program, args } => undo_rule(program, args).await,
        Resource::HostRoute { guest_ip, via } => {
            undo_result(crate::network::veth::delete_host_route_to_guest(guest_ip, via).await)
        }
        Resource::GlobalNat => {
            undo_result(crate::network::portmap::cleanup_global_nat_if_unused().await)
        }
        Resource::RoutedAddress {
            address,
            iface,
            masquerade,
        } => crate::network::routed::release_address(*address, iface, *masquerade).await,
    }
}

fn undo_result(result: Result<()>) -> Undo {
    match result {
        Ok(()) => Undo::Done,
        Err(error) => Undo::Kept(format!("{error:#}")),
    }
}

/// Delete a rule recorded by the arguments that appended it.
async fn undo_rule(program: &str, args: &[String]) -> Undo {
    let mut delete = args.to_vec();
    let Some(append) = delete.iter().position(|arg| arg == "-A") else {
        return Undo::Kept(format!("{program} rule {args:?} has no -A"));
    };
    delete[append] = "-D".to_string();
    match crate::utils::helper_command(program)
        .args(&delete)
        .output()
        .await
    {
        Ok(output) if output.status.success() => Undo::Done,
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            // The rule is not there: never added, or removed already.
            if stderr.contains("Bad rule")
                || stderr.contains("does a matching rule exist")
                || stderr.contains("No chain")
                || stderr.contains("does not exist")
            {
                Undo::Done
            } else {
                Undo::Kept(stderr.trim().to_string())
            }
        }
        Err(error) => Undo::Kept(error.to_string()),
    }
}

async fn undo_runtime_dir(path: &Path) -> Undo {
    remove_tree(path).await
}

/// Remove a directory tree. A directory without owner write (an image layer's
/// `/proc`, a `--disk-dir` copy that kept its modes) blocks the removal of what it holds,
/// and the tree is being deleted, so its modes go first.
async fn remove_tree(path: &Path) -> Undo {
    let path = path.to_path_buf();
    let removed = tokio::task::spawn_blocking(move || {
        crate::storage::disk_dir::make_removable(&path);
        std::fs::remove_dir_all(&path)
    })
    .await;
    match removed {
        Ok(Ok(())) => Undo::Done,
        Ok(Err(error)) if error.kind() == std::io::ErrorKind::NotFound => Undo::Done,
        Ok(Err(error)) => Undo::Kept(error.to_string()),
        Err(error) => Undo::Kept(error.to_string()),
    }
}

/// Remove `path`, a memory file of a create that did not finish, unless the published
/// snapshot's `link` names it. The check and the removal hold the snapshot's generation lock
/// (`<snapshot>.lock`), so a create replacing that snapshot meanwhile, whose rename leaves no
/// link for a moment, cannot make the check miss a published file; a busy snapshot is left
/// for a later reclaim. With the snapshots directory itself gone no link can name the file,
/// and it is removed.
fn undo_linked_memory(path: &Path, link: &Path) -> Undo {
    undo_linked_memory_with(path, link, &|link| std::fs::read_link(link))
}

/// [`undo_linked_memory`] with the link's read passed in, for the tests. A link that cannot be
/// read for another reason than being absent or not a link (a permission or I/O error) cannot
/// rule out that it names the file, which is then kept.
fn undo_linked_memory_with(
    path: &Path,
    link: &Path,
    read_link: &dyn Fn(&Path) -> std::io::Result<PathBuf>,
) -> Undo {
    let remove = || match std::fs::remove_file(path) {
        Ok(()) => Undo::Done,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Undo::Done,
        Err(error) => Undo::Kept(error.to_string()),
    };
    // a relative link (a relative FCVM_DATA_DIR) resolves against whatever directory this
    // process runs in, so it cannot say whether the published snapshot names the file
    if link.is_relative() {
        return Undo::Kept(format!("{} is a relative path", link.display()));
    }
    let Some(snapshot) = link.parent() else {
        return remove();
    };
    let lock_path = crate::commands::common::snapshot_sibling(snapshot, "lock");
    let lock = match std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
    {
        Ok(lock) => lock,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return remove(),
        Err(error) => return Undo::Kept(format!("opening {}: {error}", lock_path.display())),
    };
    if let Err(error) = fs2::FileExt::try_lock_exclusive(&lock) {
        return Undo::Kept(format!("{} is held: {error}", lock_path.display()));
    }
    match read_link(link) {
        Ok(target) if target == path => Undo::Done,
        Ok(_) => remove(),
        // absent, or a regular file: a snapshot published without --memory-dir took the tag
        Err(error)
            if error.kind() == std::io::ErrorKind::NotFound
                || error.raw_os_error() == Some(libc::EINVAL) =>
        {
            remove()
        }
        Err(error) => Undo::Kept(format!("reading {}: {error}", link.display())),
    }
}

async fn undo_temp(path: &Path) -> Undo {
    let removed = match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) if metadata.is_dir() => return remove_tree(path).await,
        Ok(_) => tokio::fs::remove_file(path).await,
        Err(error) => Err(error),
    };
    match removed {
        Ok(()) => Undo::Done,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Undo::Done,
        Err(error) => Undo::Kept(error.to_string()),
    }
}

/// Remove a snapshot staging directory that no create is using. A create holds the tag's
/// lock exclusively from before it makes the directory until it has renamed or removed it,
/// so the lock is taken here without waiting and held across the removal: a create that
/// holds it keeps its directory, and a create that starts meanwhile waits for the removal
/// to finish before it stages.
async fn undo_snapshot_staging(path: &Path, lock: &Path) -> Undo {
    match tokio::fs::symlink_metadata(path).await {
        Ok(_) => {}
        // Renamed into place, or removed by a later create of the tag. The lock is not
        // opened: that would make a lock file for a tag that may be gone.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Undo::Done,
        Err(error) => return Undo::Kept(format!("cannot stat {}: {error}", path.display())),
    }
    // Never truncated or unlinked: every holder locks this one inode.
    let held = match std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock)
    {
        Ok(file) => file,
        Err(error) => return Undo::Kept(format!("cannot open {}: {error}", lock.display())),
    };
    match fs2::FileExt::try_lock_exclusive(&held) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
            return Undo::InUse(format!(
                "{} is locked by a snapshot command that is running",
                lock.display()
            ))
        }
        Err(error) => return Undo::Kept(format!("cannot lock {}: {error}", lock.display())),
    }
    let removed = crate::storage::disk::remove_snapshot_dir(path).await;
    drop(held);
    match removed {
        Ok(()) => Undo::Done,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Undo::Done,
        Err(error) => Undo::Kept(error.to_string()),
    }
}

/// The record of a snapshot staging directory, for the create that makes it
/// ([`record_snapshot_staging_in`]).
///
/// Dropping it deletes the record when the directory is gone, which is how every create
/// leaves it: renamed into place, or removed on a failure. The create still holds the tag's
/// lock then, so no other create can have made the directory again. A directory that is
/// still there (an early return that removed nothing, a cancelled create) keeps its record,
/// and the next sweep removes both. A SIGKILLed create drops nothing, and the next sweep
/// does the same.
pub struct SnapshotStagingRecord {
    path: PathBuf,
    owner: Option<Owner>,
}

/// Record the staging directory `path` of a snapshot whose tag lock is `lock`, before
/// making it, in the owner directory `owners`. The caller holds `lock` exclusively, and
/// keeps the returned record until it has renamed `path` into place or removed it.
pub fn record_snapshot_staging_in(
    owners: &Path,
    path: &Path,
    lock: &Path,
) -> Result<SnapshotStagingRecord> {
    // An owner of its own: `snapshot create` runs in a process that owns no VM.
    let owner = Owner::begin_in(
        owners,
        &format!("snapshot-staging-{}", uuid::Uuid::new_v4().simple()),
    )?;
    owner.record(Resource::SnapshotStaging {
        path: path.to_path_buf(),
        lock: lock.to_path_buf(),
    })?;
    Ok(SnapshotStagingRecord {
        path: path.to_path_buf(),
        owner: Some(owner),
    })
}

impl Drop for SnapshotStagingRecord {
    fn drop(&mut self) {
        let Some(owner) = self.owner.take() else {
            return;
        };
        match std::fs::symlink_metadata(&self.path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if let Err(error) = owner.discard() {
                    warn!(path = %self.path.display(), error = %format!("{error:#}"),
                        "deleting a snapshot staging directory's record");
                }
            }
            // Still there, or unreadable: the record stays for the next sweep.
            _ => drop(owner),
        }
    }
}

/// What one [`reclaim_abandoned_in`] pass did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Reclaimed {
    /// Records of owners that exited without completing, now deleted.
    pub owners: usize,
    /// Resources undone.
    pub resources: usize,
    /// Resources that stay recorded for a later pass.
    pub remaining: usize,
}

/// Reclaim what every owner that exited without completing recorded, in this user's
/// owner directory: the sweep. Called when a command that needs the room starts, and by a
/// VM launch through [`reclaim_abandoned_if_due`]. Never waits on a live owner, and never
/// fails its caller: what it cannot undo is logged and left for the next sweep.
pub async fn reclaim_abandoned() -> Reclaimed {
    collect_pmem_temps().await;
    let dir = match owner_dir() {
        Ok(dir) => dir,
        Err(error) => {
            warn!(error = %format!("{error:#}"), "cannot reclaim resources of exited fcvm processes");
            return Reclaimed::default();
        }
    };
    reclaim_abandoned_in(&dir).await
}

/// The pmem store keeps its own record of who is using `tmp/`: its lock.
async fn collect_pmem_temps() {
    let data_dir = crate::paths::data_dir();
    match tokio::task::spawn_blocking(move || {
        crate::storage::pmem_store::collect_abandoned_temps(&data_dir)
    })
    .await
    {
        Ok(Ok(0)) => {}
        Ok(Ok(removed)) => info!(removed, "removed temps of killed pmem ingests"),
        Ok(Err(error)) => warn!(error = %format!("{error:#}"), "cannot collect pmem store temps"),
        Err(error) => warn!(%error, "cannot collect pmem store temps"),
    }
}

/// [`reclaim_abandoned`] for a VM launch: the sweep, unless one began in this boot less
/// than [`RECLAIM_EVERY`] ago or another launch is sweeping now. A launch that does not
/// sweep opens and reads one stamp, whatever the number of VMs running.
pub async fn reclaim_abandoned_if_due() -> Reclaimed {
    let dir = match owner_dir() {
        Ok(dir) => dir,
        Err(error) => {
            warn!(error = %format!("{error:#}"), "cannot reclaim resources of exited fcvm processes");
            return Reclaimed::default();
        }
    };
    let every = reclaim_every(std::env::var(RECLAIM_EVERY_ENV).ok().as_deref());
    reclaim_abandoned_if_due_in(&dir, every).await
}

/// [`reclaim_abandoned_if_due`] in an explicit owner directory, with an explicit interval.
/// The pmem store's temps are collected with the sweep, as [`reclaim_abandoned`] does.
pub async fn reclaim_abandoned_if_due_in(path: &Path, every: std::time::Duration) -> Reclaimed {
    if every.is_zero() {
        // Every launch sweeps, beside whichever launch is sweeping.
        collect_pmem_temps().await;
        return reclaim_abandoned_in(path).await;
    }
    let _sweeping = match take_sweep(path, every) {
        Ok(Some(lock)) => Some(lock),
        // Swept lately, or being swept by another launch.
        Ok(None) => return Reclaimed::default(),
        Err(error) => {
            warn!(dir = %path.display(), error = %format!("{error:#}"), "cannot tell whether a sweep is due; sweeping");
            None
        }
    };
    collect_pmem_temps().await;
    reclaim_abandoned_in(path).await
}

/// Nanoseconds since boot, suspend included (`CLOCK_BOOTTIME`).
fn boottime_ns() -> u64 {
    let mut now = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: the kernel writes one timespec through a valid pointer.
    unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut now) };
    (now.tv_sec as u64) * 1_000_000_000 + now.tv_nsec as u64
}

/// What a sweep that begins at `now` writes in the stamp.
fn stamp_text(boot: &str, now: u64) -> String {
    format!("{boot} {now}")
}

/// Whether `stamp` says a sweep began in the boot `boot` less than `every` before `now`.
/// Anything else makes a sweep due: another boot's stamp, one that does not parse, and one
/// later than `now`, which a process in another time namespace can write.
fn stamp_is_fresh(stamp: &str, boot: &str, now: u64, every: std::time::Duration) -> bool {
    let Some((stamp_boot, began)) = stamp.trim().split_once(' ') else {
        return false;
    };
    let Ok(began) = began.parse::<u64>() else {
        return false;
    };
    stamp_boot == boot && began <= now && u128::from(now - began) < every.as_nanos()
}

/// Whether the sweep of the owner directory `dir` that last began did so in this boot and
/// less than `every` ago.
fn swept_lately(dir: &OwnerDir, every: std::time::Duration) -> Result<bool> {
    Ok(dir
        .read(SWEEP_STAMP)?
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .is_some_and(|stamp| stamp_is_fresh(&stamp, &boot_id(), boottime_ns(), every)))
}

/// Write the stamp of a sweep that begins now. It is written under a temporary name and
/// renamed, so a launch never reads half of one.
fn write_stamp(dir: &OwnerDir, now: u64) -> Result<()> {
    use nix::fcntl::OFlag;
    use std::io::Write;
    let temp = format!("{SWEEP_STAMP}.{}", std::process::id());
    let mut file = dir
        .open_file(&temp, OFlag::O_CREAT | OFlag::O_WRONLY | OFlag::O_TRUNC)
        .with_context(|| format!("creating {}", dir.display(&temp)))?;
    file.write_all(stamp_text(&boot_id(), now).as_bytes())
        .with_context(|| format!("writing {}", dir.display(&temp)))?;
    drop(file);
    nix::fcntl::renameat(&dir.fd, temp.as_str(), &dir.fd, SWEEP_STAMP)
        .with_context(|| format!("publishing {}", dir.display(SWEEP_STAMP)))
}

/// Set the stamp of the owner directory `path` to a sweep that began `ago` before now, for
/// tests of what a launch does with a fresh stamp and with an old one.
#[doc(hidden)]
pub fn set_sweep_stamp(path: &Path, ago: std::time::Duration) -> Result<()> {
    let ago = u64::try_from(ago.as_nanos()).unwrap_or(u64::MAX);
    write_stamp(&OwnerDir::open(path)?, boottime_ns().saturating_sub(ago))
}

/// Try the sweep lock of `dir` without waiting.
fn try_sweep_lock(dir: &OwnerDir) -> Result<Option<Flock<File>>> {
    use nix::fcntl::OFlag;
    let lock = dir
        .open_file(SWEEP_LOCK, OFlag::O_CREAT | OFlag::O_WRONLY)
        .with_context(|| format!("opening {}", dir.display(SWEEP_LOCK)))?;
    match Flock::lock(lock, FlockArg::LockExclusiveNonblock) {
        Ok(lock) => Ok(Some(lock)),
        Err((_, nix::errno::Errno::EWOULDBLOCK)) => Ok(None),
        Err((_, error)) => {
            Err(error).with_context(|| format!("locking {}", dir.display(SWEEP_LOCK)))
        }
    }
}

/// The right to sweep the owner directory at `path` now: `None` when a sweep began less
/// than `every` ago or another process holds the sweep lock. Otherwise the stamp is
/// rewritten and the lock returned, to be held for the length of the sweep, so that the
/// launches that arrive while it runs find either the lock held or the stamp new. The
/// stamp is written when the sweep begins, not when it ends: a sweeper killed at once has
/// reclaimed nothing and leaves its stamp, so what was there waits one more interval, and
/// its lock goes with its process.
fn take_sweep(path: &Path, every: std::time::Duration) -> Result<Option<Flock<File>>> {
    let dir = OwnerDir::open(path)?;
    if swept_lately(&dir, every)? {
        return Ok(None);
    }
    let Some(lock) = try_sweep_lock(&dir)? else {
        return Ok(None);
    };
    // The launch that held the lock before this one may have stamped and swept already.
    if swept_lately(&dir, every)? {
        return Ok(None);
    }
    write_stamp(&dir, boottime_ns())?;
    Ok(Some(lock))
}

/// How long a launch that cannot go on without a sweep waits for the launch that is
/// sweeping ([`reclaim_abandoned_on_contention`]).
const CONTENTION_SWEEP_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// The sweep, for a launch that has just been refused something a killed VM may still
/// hold (a bridged launch that found no free subnet), whatever the stamp says. It waits for
/// a sweep in progress, for [`CONTENTION_SWEEP_WAIT`] at most, because that sweep may be
/// freeing what the launch needs and two sweeps at once undo the same records no faster;
/// then it sweeps itself, since the other listed the records before this launch was refused.
pub async fn reclaim_abandoned_on_contention() -> Reclaimed {
    let dir = match owner_dir() {
        Ok(dir) => dir,
        Err(error) => {
            warn!(error = %format!("{error:#}"), "cannot reclaim resources of exited fcvm processes");
            return Reclaimed::default();
        }
    };
    reclaim_abandoned_on_contention_in(&dir, CONTENTION_SWEEP_WAIT).await
}

/// [`reclaim_abandoned_on_contention`] in an explicit owner directory, with an explicit wait.
pub async fn reclaim_abandoned_on_contention_in(
    path: &Path,
    wait: std::time::Duration,
) -> Reclaimed {
    let deadline = tokio::time::Instant::now() + wait;
    let _sweeping = loop {
        let taken = OwnerDir::open(path).and_then(|dir| {
            let lock = try_sweep_lock(&dir)?;
            if lock.is_some() {
                write_stamp(&dir, boottime_ns())?;
            }
            Ok(lock)
        });
        match taken {
            Ok(Some(lock)) => break Some(lock),
            Ok(None) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            // Still held after the wait: sweep beside it, as two launches always could.
            Ok(None) => break None,
            Err(error) => {
                warn!(dir = %path.display(), error = %format!("{error:#}"), "cannot take the sweep lock; sweeping without it");
                break None;
            }
        }
    };
    reclaim_abandoned_in(path).await
}

/// [`reclaim_abandoned`] in an explicit owner directory: every bucket of it, and the
/// directory itself.
pub async fn reclaim_abandoned_in(path: &Path) -> Reclaimed {
    let mut done = Reclaimed::default();
    let dirs = match OwnerDir::open(path).and_then(OwnerDir::record_dirs) {
        Ok(dirs) => dirs,
        Err(error) => {
            warn!(dir = %path.display(), error = %format!("{error:#}"), "cannot read owner records");
            return done;
        }
    };
    let started = std::time::Instant::now();
    let mut owners_seen = 0;
    for dir in &dirs {
        owners_seen += reclaim_dir(dir, &mut done).await;
    }
    debug!(
        dir = %path.display(),
        owners_seen,
        directories = dirs.len(),
        elapsed_us = started.elapsed().as_micros() as u64,
        "swept owner records"
    );
    if done != Reclaimed::default() {
        info!(
            owners = done.owners,
            resources = done.resources,
            remaining = done.remaining,
            "reclaimed resources of fcvm processes that exited without teardown"
        );
    }
    done
}

/// One directory of records in a sweep: a bucket, or the owner directory itself. Returns
/// how many owners it held, alive or not.
async fn reclaim_dir(dir: &OwnerDir, done: &mut Reclaimed) -> usize {
    let names = match dir.names() {
        Ok(names) => names,
        Err(error) => {
            warn!(dir = %dir.path.display(), error = %format!("{error:#}"), "cannot read owner records");
            return 0;
        }
    };
    let mut ids = std::collections::BTreeSet::new();
    for name in &names {
        if let Some(id) = name
            .strip_suffix(".lock")
            .or_else(|| name.strip_suffix(".json"))
        {
            ids.insert(id.to_string());
        }
    }
    let owners_seen = ids.len();
    for id in ids {
        let lock = match lock_owner_file(dir, &id, false) {
            Ok(Some(lock)) => lock,
            // Its owner is alive.
            Ok(None) => continue,
            Err(error) => {
                warn!(owner = %id, error = %format!("{error:#}"), "cannot lock owner record");
                continue;
            }
        };
        let record = match dir.read(&record_name(&id)) {
            Ok(Some(bytes)) => bytes,
            // An owner that recorded nothing, or one that is between creating its lock
            // and taking it (it finds the file gone and makes a new one).
            Ok(None) => {
                if let Err(error) = delete_owner_files(dir, &id, lock) {
                    warn!(owner = %id, error = %format!("{error:#}"), "cannot delete an empty owner lock");
                }
                continue;
            }
            Err(error) => {
                warn!(owner = %id, %error, "cannot read owner record");
                continue;
            }
        };
        let record: Record = match serde_json::from_slice(&record) {
            Ok(record) => record,
            Err(error) => {
                warn!(owner = %id, %error, "cannot parse owner record; leaving it");
                continue;
            }
        };
        let total = record.resources.len();
        // Kernel objects are this process's to undo only when they were made in this
        // boot and in this user namespace's view of the host (a replaced container's
        // records share this directory; their objects went with it).
        let same_namespace = record.user_namespace.unwrap_or(INITIAL_USER_NAMESPACE)
            == current_user_namespace().unwrap_or(INITIAL_USER_NAMESPACE);
        let same_boot = record.boot_id == boot_id() && same_namespace;
        let remaining = undo_all(&id, record.resources, same_boot, |list| {
            write_record(dir, &id, list)
        })
        .await;
        done.resources += total - remaining.len();
        if remaining.is_empty() {
            match delete_owner_files(dir, &id, lock) {
                Ok(()) => done.owners += 1,
                Err(error) => {
                    warn!(owner = %id, error = %format!("{error:#}"), "cannot delete owner record")
                }
            }
            continue;
        }
        done.remaining += remaining.len();
        for (resource, why) in &remaining {
            if why.starts_with(IN_USE) {
                debug!(owner = %id, pid = record.pid, %resource, reason = %why, "still in use");
            } else {
                warn!(owner = %id, pid = record.pid, %resource, reason = %why, "cannot reclaim yet");
            }
        }
        let resources: Vec<Resource> = remaining.into_iter().map(|(r, _)| r).collect();
        if let Err(error) = write_record(dir, &id, &resources) {
            warn!(owner = %id, error = %format!("{error:#}"), "cannot rewrite owner record");
        }
        release(lock);
    }
    owners_seen
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A memory file of a create that did not finish is removed; one the published snapshot's
    /// memory.bin names (a create killed after publishing, before it discarded its record) is
    /// kept, and so the snapshot stays restorable. While another create holds the snapshot's
    /// generation lock (it may be between its two renames, with no link), nothing is removed;
    /// with the snapshots directory gone, the file is removed.
    #[test]
    fn a_linked_memory_file_is_kept_only_when_the_published_link_names_it() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("fcvm-memory-a.bin");
        std::fs::write(&file, b"m").unwrap();
        let snapshot = dir.path().join("snap");
        std::fs::create_dir(&snapshot).unwrap();
        let link = snapshot.join("memory.bin");
        std::os::unix::fs::symlink(&file, &link).unwrap();
        assert!(matches!(undo_linked_memory(&file, &link), Undo::Done));
        assert!(
            file.exists(),
            "the published snapshot's memory file was removed"
        );
        std::fs::remove_file(&link).unwrap();
        let held = std::fs::File::create(dir.path().join("snap.lock")).unwrap();
        fs2::FileExt::lock_exclusive(&held).unwrap();
        assert!(matches!(undo_linked_memory(&file, &link), Undo::Kept(_)));
        assert!(file.exists(), "removed while a create held the snapshot");
        drop(held);
        std::os::unix::fs::symlink(dir.path().join("fcvm-memory-b.bin"), &link).unwrap();
        assert!(matches!(undo_linked_memory(&file, &link), Undo::Done));
        assert!(!file.exists(), "an unpublished memory file was kept");
        assert!(
            matches!(undo_linked_memory(&file, &link), Undo::Done),
            "a gone file is done"
        );
        // a link that cannot be read keeps the file: it may name it
        std::fs::write(&file, b"m").unwrap();
        let unreadable = |_: &Path| Err(std::io::Error::from_raw_os_error(libc::EACCES));
        assert!(matches!(
            undo_linked_memory_with(&file, &link, &unreadable),
            Undo::Kept(_)
        ));
        assert!(file.exists(), "removed although the link could not be read");
        // a regular memory.bin (a later snapshot without --memory-dir) names no file of ours
        std::fs::remove_file(&link).unwrap();
        std::fs::write(&link, b"store memory").unwrap();
        assert!(matches!(undo_linked_memory(&file, &link), Undo::Done));
        assert!(
            !file.exists(),
            "kept although the snapshot's memory is on the store"
        );
        std::fs::write(&file, b"m").unwrap();
        // a relative link (a relative FCVM_DATA_DIR) names nothing reliably from another
        // working directory, so the file is kept rather than judged against the wrong path
        assert!(matches!(
            undo_linked_memory(&file, Path::new("data/snapshots/snap/memory.bin")),
            Undo::Kept(_)
        ));
        assert!(file.exists(), "removed through a relative link");
        let gone = dir
            .path()
            .join("no-snapshots")
            .join("snap")
            .join("memory.bin");
        assert!(matches!(undo_linked_memory(&file, &gone), Undo::Done));
        assert!(!file.exists(), "kept although no snapshot can name it");
    }

    use std::io::BufRead;

    const CHILD_DIR: &str = "FCVM_RECLAIM_TEST_CHILD_DIR";
    const CHILD_READY: &str = "reclaim-child: resources created";

    fn test_dir() -> (tempfile::TempDir, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let dir = owner_dir_in(root.path()).unwrap();
        (root, dir)
    }

    /// The names of the record and lock files in an owner directory, in whichever of its
    /// directories they are.
    fn files_in(dir: &Path) -> Vec<String> {
        record_file_names(dir)
    }

    /// Put `record` where the owner `id`'s record goes, with no owner having begun.
    fn write_by_hand(dir: &Path, id: &str, record: &Record) {
        let path = record_path(dir, id);
        std::fs::DirBuilder::new()
            .mode(0o700)
            .recursive(true)
            .create(path.parent().unwrap())
            .unwrap();
        std::fs::write(path, serde_json::to_vec(record).unwrap()).unwrap();
    }

    /// The resources the child test process creates under `root`.
    fn child_resources(root: &Path) -> (PathBuf, PathBuf) {
        (
            root.join("vm-disks").join("vm-child"),
            root.join("image-cache").join("archive.docker.tar.1234.tmp"),
        )
    }

    /// The child half of `a_sigkilled_owners_resources_are_reclaimed`: record, create,
    /// report, and wait to be killed.
    fn run_child(root: &Path) -> ! {
        let dir = owner_dir_in(root).unwrap();
        let (runtime_dir, temp) = child_resources(root);
        let owner = Owner::begin_in(&dir, "vm-child").unwrap();
        owner
            .record(Resource::RuntimeDir {
                path: runtime_dir.clone(),
            })
            .unwrap();
        std::fs::create_dir_all(runtime_dir.join("disks")).unwrap();
        std::fs::write(runtime_dir.join("disks/rootfs.raw"), b"disk").unwrap();
        owner.record(Resource::Temp { path: temp.clone() }).unwrap();
        std::fs::create_dir_all(temp.parent().unwrap()).unwrap();
        std::fs::write(&temp, b"partial export").unwrap();
        // A setup store build (#1182): its uniquely named temp.
        let store_temp =
            crate::setup::StoreTemp::new_in(&dir, &root.join("kernels/vmlinux.bin")).unwrap();
        std::fs::create_dir_all(root.join("kernels")).unwrap();
        std::fs::write(store_temp.path(), b"partial kernel").unwrap();
        // A snapshot create: it holds the tag's lock and has staged the memory image.
        let (_, staging, lock) = snapshot_paths(root);
        let tag_lock = std::fs::File::create(&lock).unwrap();
        fs2::FileExt::lock_exclusive(&tag_lock).unwrap();
        let _staging = record_snapshot_staging_in(&dir, &staging, &lock).unwrap();
        std::fs::create_dir(&staging).unwrap();
        std::fs::write(staging.join("memory.bin"), b"partial memory").unwrap();
        println!("{CHILD_READY}");
        loop {
            std::thread::sleep(std::time::Duration::from_secs(60));
        }
    }

    /// An owner killed with SIGKILL runs no teardown. The next pass reclaims what it
    /// recorded, and leaves it alone while the owner lives.
    #[tokio::test]
    async fn a_sigkilled_owners_resources_are_reclaimed() {
        if let Some(root) = std::env::var_os(CHILD_DIR) {
            run_child(Path::new(&root));
        }
        let root = tempfile::tempdir().unwrap();
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "reclaim::tests::a_sigkilled_owners_resources_are_reclaimed",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHILD_DIR, root.path())
            .stdout(std::process::Stdio::piped());
        // The child dies with this test, so a failed assertion cannot leave it running.
        unsafe {
            use std::os::unix::process::CommandExt;
            command.pre_exec(|| {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn().unwrap();
        let stdout = child.stdout.take().unwrap();
        let ready = std::io::BufReader::new(stdout)
            .lines()
            .map_while(|line| line.ok())
            .any(|line| line.contains(CHILD_READY));
        assert!(ready, "the child exited before creating its resources");

        let dir = owner_dir_in(root.path()).unwrap();
        let (runtime_dir, temp) = child_resources(root.path());
        let (_, staging, _) = snapshot_paths(root.path());
        assert_eq!(
            reclaim_abandoned_in(&dir).await,
            Reclaimed::default(),
            "a pass reclaimed resources of a live owner"
        );
        assert!(runtime_dir.join("disks/rootfs.raw").exists() && temp.exists());
        assert!(staging.join("memory.bin").exists());

        nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(child.id() as i32),
            nix::sys::signal::Signal::SIGKILL,
        )
        .unwrap();
        child.wait().unwrap();
        let store_temps = || -> Vec<PathBuf> {
            std::fs::read_dir(root.path().join("kernels"))
                .map(|entries| entries.map(|e| e.unwrap().path()).collect())
                .unwrap_or_default()
        };
        assert!(
            runtime_dir.exists() && temp.exists() && store_temps().len() == 1 && staging.exists(),
            "the resources are gone before any pass ran"
        );

        // The kill released the tag's lock, so the staging directory goes too.
        assert_eq!(
            reclaim_abandoned_in(&dir).await,
            Reclaimed {
                owners: 3,
                resources: 4,
                remaining: 0
            }
        );
        assert!(
            !staging.exists(),
            "the killed create's staging directory is left"
        );
        assert!(
            store_temps().is_empty(),
            "the store temp is left: {:?}",
            store_temps()
        );
        assert!(!runtime_dir.exists(), "the runtime directory is left");
        assert!(!temp.exists(), "the temporary is left");
        assert!(
            files_in(&dir).is_empty(),
            "owner files left: {:?}",
            files_in(&dir)
        );
    }

    #[tokio::test]
    async fn complete_removes_what_teardown_left_and_the_record() {
        let (root, dir) = test_dir();
        let temp = root.path().join("left.tmp");
        let gone = root.path().join("vm-disks/vm-gone");
        let owner = Owner::begin_in(&dir, "vm-complete").unwrap();
        owner
            .record(Resource::RuntimeDir { path: gone.clone() })
            .unwrap();
        owner.record(Resource::Temp { path: temp.clone() }).unwrap();
        std::fs::write(&temp, b"x").unwrap();
        assert_eq!(
            files_in(&dir),
            ["vm-complete.json", "vm-complete.lock"],
            "the record is not on disk before the resource is created"
        );
        complete("vm-complete").await.unwrap();
        assert!(!temp.exists());
        assert!(files_in(&dir).is_empty());
        assert!(
            record("vm-complete", Resource::Temp { path: temp }).is_err(),
            "a completed owner accepted a record"
        );
        drop(owner);
    }

    /// A record that holds a resource of a kind this build does not know (here a
    /// `mount_target`, which fcvm wrote before #1311) is still reclaimed: the unknown entry
    /// is dropped and the runtime directory beside it is removed.
    #[tokio::test]
    async fn a_record_with_an_unknown_resource_kind_is_still_reclaimed() {
        let (root, dir) = test_dir();
        let runtime_dir = root.path().join("vm-disks/vm-old");
        std::fs::create_dir_all(runtime_dir.join("disks")).unwrap();
        let owner = Owner::begin_in(&dir, "vm-old").unwrap();
        owner
            .record(Resource::RuntimeDir {
                path: runtime_dir.clone(),
            })
            .unwrap();
        drop(owner);
        let path = record_path(&dir, "vm-old");
        let mut record: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        record["resources"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({"kind": "mount_target", "path": "/nonexistent/vm-src"}));
        std::fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();

        assert_eq!(reclaim_abandoned_in(&dir).await.owners, 1);
        assert!(
            !runtime_dir.exists(),
            "the runtime directory beside it was left"
        );
        assert!(files_in(&dir).is_empty());
    }

    #[test]
    fn record_without_an_owner_fails() {
        let error = record(
            "vm-nobody",
            Resource::Temp {
                path: PathBuf::from("/nonexistent"),
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("vm-nobody"), "{error}");
    }

    #[test]
    fn an_owner_directory_others_can_write_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let dir = owner_dir_in(root.path()).unwrap();
        assert_eq!(
            std::fs::metadata(root.path().join("reclaim"))
                .unwrap()
                .mode()
                & 0o7777,
            0o1777
        );
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777)).unwrap();
        let error = owner_dir_in(root.path()).unwrap_err().to_string();
        assert!(error.contains("no one else can write"), "{error}");
    }

    /// The initial user namespace keeps the directory name it had; any other carries the
    /// namespace.
    #[test]
    fn the_owner_directory_name_carries_a_user_namespace_other_than_the_initial_one() {
        assert_eq!(
            owner_dir_name(0, Some(INITIAL_USER_NAMESPACE), Some(0)),
            "0"
        );
        assert_eq!(owner_dir_name(1000, None, None), "1000");
        assert_eq!(
            owner_dir_name(0, Some(4026532301), Some(1000)),
            "0-host1000"
        );
        assert_eq!(
            owner_dir_name(0, Some(4026532301), None),
            "0-userns4026532301"
        );
        assert_eq!(
            host_uid_of("         0       1000          1\n", 0),
            Some(1000)
        );
        assert_eq!(host_uid_of("0 1000 1\n1 100000 65536\n", 5), Some(100004));
        assert_eq!(host_uid_of("0 1000 1\n", 7), None);
        assert_eq!(host_uid_of("0 0 4294967295\n", 1000), Some(1000));
    }

    /// Set in the child of `a_namespaced_root_has_an_owner_directory_of_its_own`: the
    /// data directory it makes its owner directory in.
    #[cfg(feature = "privileged-tests")]
    const NAMESPACED_ROOT_CHILD: &str = "FCVM_RECLAIM_NAMESPACED_ROOT_DATA_DIR";

    /// A container's root is uid 0 of a user namespace, not the host's root, and both use
    /// one store when the store is shared (CI runners keep /mnt/fcvm-btrfs between jobs).
    /// The host's root made `reclaim/0`; in the container that directory shows as owned by
    /// the overflow uid, and every launch and `fcvm setup` there failed with "refusing owner
    /// records in .../reclaim/0 ... (found uid 65534, mode 700)". Each must get a directory
    /// of its own. The child runs this test again as root of a new user namespace whose
    /// root is an unprivileged host uid (`setpriv` + `unshare -r`).
    #[cfg(feature = "privileged-tests")]
    #[test]
    fn a_namespaced_root_has_an_owner_directory_of_its_own() {
        if let Some(data_dir) = std::env::var_os(NAMESPACED_ROOT_CHILD) {
            let mine = owner_dir_in(Path::new(&data_dir)).expect("owner directory");
            Owner::begin_in(&mine, "namespaced-root").expect("an owner in it");
            println!("OWNER_DIR={}", mine.display());
            return;
        }
        assert!(
            nix::unistd::geteuid().is_root(),
            "privileged-tests run as root"
        );
        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        let host_root = owner_dir_in(root.path()).unwrap();
        // The child runs as nobody, who may not be able to reach this build's binary.
        let exe = root.path().join("test-binary");
        std::fs::copy(std::env::current_exe().unwrap(), &exe).unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        let child = || {
            let output = std::process::Command::new("setpriv")
                .args(["--reuid=65534", "--regid=65534", "--clear-groups"])
                .args(["unshare", "-U", "-r"])
                .arg(&exe)
                .args([
                    "--exact",
                    "reclaim::tests::a_namespaced_root_has_an_owner_directory_of_its_own",
                    "--nocapture",
                ])
                .env(NAMESPACED_ROOT_CHILD, root.path())
                .output()
                .expect("spawning the namespaced child");
            let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
            assert!(
                output.status.success(),
                "the namespaced root could not use the shared store: {}\n{stdout}\n{}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
            PathBuf::from(
                stdout
                    .lines()
                    .find_map(|line| line.strip_prefix("OWNER_DIR="))
                    .expect("the child names its owner directory"),
            )
        };
        let theirs = child();
        assert_ne!(theirs, host_root, "one directory for two roots");
        // A container replaced by another with the same uid mapping (a new user namespace)
        // finds the records its predecessor left, so what they name on disk is reclaimed.
        assert_eq!(
            child(),
            theirs,
            "a new user namespace with the same mapping lost its predecessor's records"
        );
        // The host's root still uses its own directory.
        assert_eq!(owner_dir_in(root.path()).unwrap(), host_root);
    }

    /// Two owners whose names collide get directories of their own, told apart by who the
    /// filesystem says owns each, and a later process of either finds its own again in
    /// whichever slot it is.
    #[test]
    fn an_owner_directory_another_uid_owns_is_passed_over() {
        let shared = tempfile::tempdir().unwrap();
        let me = nix::unistd::geteuid().as_raw();
        let other = me.wrapping_add(1);
        let theirs: std::collections::HashSet<PathBuf> = ["0-host0", "0-host0.2"]
            .map(|name| shared.path().join(name))
            .into();
        for path in &theirs {
            std::fs::create_dir(path).unwrap();
        }
        let owner_of = |path: &Path| {
            let found = std::fs::symlink_metadata(path).ok()?;
            Some(if theirs.contains(path) {
                other
            } else {
                found.uid()
            })
        };

        let mine = pick_owner_dir(shared.path(), "0-host0", me, owner_of).unwrap();
        assert_eq!(mine, shared.path().join("0-host0.1"));
        assert_eq!(
            pick_owner_dir(shared.path(), "0-host0", me, owner_of).unwrap(),
            mine
        );

        // Ours in a later slot than a free one: found, not a new one made.
        std::fs::remove_dir(&mine).unwrap();
        std::fs::create_dir(shared.path().join("0-host0.3")).unwrap();
        assert_eq!(
            pick_owner_dir(shared.path(), "0-host0", me, owner_of).unwrap(),
            shared.path().join("0-host0.3")
        );
        assert!(
            !mine.exists(),
            "a new directory was made while ours existed"
        );
    }

    /// Set in the children of `nested_namespaces_with_one_mapping_get_their_own_directories`:
    /// the data directory they make their owner directory in.
    #[cfg(feature = "privileged-tests")]
    const NESTED_ROOT_CHILD: &str = "FCVM_RECLAIM_NESTED_ROOT_DATA_DIR";

    /// #1465: two containers, each root of a user namespace nested in another non-initial one,
    /// both map uid 0 to their parent's uid 0 while the parents map to different host uids. Both
    /// get the name `0-host0`. The second used to refuse the first's directory, which it sees as
    /// owned by the overflow uid, and fail every launch. Each child runs this test again as
    /// root of `unshare -r` inside `unshare -r` under a different unprivileged host uid.
    #[cfg(feature = "privileged-tests")]
    #[test]
    fn nested_namespaces_with_one_mapping_get_their_own_directories() {
        if let Some(data_dir) = std::env::var_os(NESTED_ROOT_CHILD) {
            let mine = owner_dir_in(Path::new(&data_dir)).expect("owner directory");
            Owner::begin_in(&mine, &format!("nested-root-{}", std::process::id()))
                .expect("an owner in it");
            println!("OWNER_DIR={}", mine.display());
            return;
        }
        assert!(
            nix::unistd::geteuid().is_root(),
            "privileged-tests run as root"
        );
        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        owner_dir_in(root.path()).unwrap();
        let exe = root.path().join("test-binary");
        std::fs::copy(std::env::current_exe().unwrap(), &exe).unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        let child = |host_uid: u32| {
            let output = std::process::Command::new("setpriv")
                .arg(format!("--reuid={host_uid}"))
                .arg(format!("--regid={host_uid}"))
                .arg("--clear-groups")
                .args(["unshare", "-U", "-r", "unshare", "-U", "-r"])
                .arg(&exe)
                .args([
                    "--exact",
                    "reclaim::tests::nested_namespaces_with_one_mapping_get_their_own_directories",
                    "--nocapture",
                ])
                .env(NESTED_ROOT_CHILD, root.path())
                .output()
                .expect("spawning the nested child");
            let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
            assert!(
                output.status.success(),
                "a nested root as host uid {host_uid} could not use the shared store: {}\n{stdout}\n{}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
            PathBuf::from(
                stdout
                    .lines()
                    .find_map(|line| line.strip_prefix("OWNER_DIR="))
                    .expect("the child names its owner directory"),
            )
        };
        let first = child(65534);
        let second = child(65533);
        assert_ne!(first, second, "one directory for two host owners");
        assert_eq!(child(65534), first, "the first owner lost its directory");
        assert_eq!(child(65533), second, "the second owner lost its directory");
    }

    /// A snapshot's directory under `root`, with its staging directory and its tag lock as
    /// `commands::common::snapshot_sibling` names them.
    fn snapshot_paths(root: &Path) -> (PathBuf, PathBuf, PathBuf) {
        let snapshot = root.join("snapshots").join("app.v1");
        std::fs::create_dir_all(snapshot.parent().unwrap()).unwrap();
        let sibling = |suffix| crate::commands::common::snapshot_sibling(&snapshot, suffix);
        (snapshot.clone(), sibling("creating"), sibling("lock"))
    }

    /// What a create that was killed after it staged leaves: the staging directory with a
    /// memory file in it, and its record, with no owner alive.
    fn killed_create(dir: &Path, staging: &Path, lock: &Path) {
        let record = record_snapshot_staging_in(dir, staging, lock).unwrap();
        assert_eq!(
            files_in(dir).len(),
            2,
            "the record is not on disk before the staging directory is made"
        );
        std::fs::create_dir(staging).unwrap();
        std::fs::write(staging.join("memory.bin"), b"memory").unwrap();
        // The directory is still there, so the record stays, as after a SIGKILL.
        drop(record);
        assert_eq!(
            files_in(dir).len(),
            2,
            "the record of a left directory is gone"
        );
    }

    /// Every create of a tag stages in `<tag>.creating`. The record of a killed create names
    /// the directory a later create of the tag is writing, and a pass leaves that directory
    /// while the later create holds the tag's lock.
    ///
    /// RED WITHOUT THE LOCK: the pass removed the live create's staging directory, as it
    /// would with the directory recorded as a `Temp`.
    #[tokio::test]
    async fn a_staging_directory_stays_while_a_create_holds_its_tag_lock() {
        let (root, dir) = test_dir();
        let (snapshot, staging, lock) = snapshot_paths(root.path());
        killed_create(&dir, &staging, &lock);
        // A later create of the tag: it holds the lock and writes the same directory.
        let live_create = crate::commands::common::acquire_snapshot_dir_lock(&snapshot, true)
            .await
            .unwrap();
        let resource = Resource::SnapshotStaging {
            path: staging.clone(),
            lock: lock.clone(),
        };
        assert!(
            matches!(undo(&resource).await, Undo::InUse(_)),
            "the undo did not report a staging directory whose tag lock is held as in use"
        );
        assert_eq!(
            reclaim_abandoned_in(&dir).await,
            Reclaimed {
                owners: 0,
                resources: 0,
                remaining: 1
            },
            "a pass did not keep the record of a live create's staging directory"
        );
        assert!(
            staging.join("memory.bin").exists(),
            "a pass removed the staging directory a live create holds the tag lock for"
        );
        let names = files_in(&dir);
        let name = names
            .iter()
            .find(|name| name.ends_with(".json"))
            .unwrap_or_else(|| panic!("the record did not stay: {names:?}"));
        let record: Record =
            serde_json::from_slice(&std::fs::read(dir.join(name)).unwrap()).unwrap();
        assert_eq!(record.resources, [resource]);
        drop(live_create);
    }

    /// With nobody holding the tag's lock, the pass removes a killed create's staging
    /// directory and its record. The lock file stays: its inode is what every holder locks.
    ///
    /// RED WITHOUT THE UNDO: the directory stayed, as it does in a build that does not
    /// know the resource kind.
    #[tokio::test]
    async fn a_staging_directory_nobody_holds_the_tag_lock_for_is_removed() {
        let (root, dir) = test_dir();
        let (snapshot, staging, lock) = snapshot_paths(root.path());
        // The killed create held the lock, and its death released it.
        let killed = crate::commands::common::acquire_snapshot_dir_lock(&snapshot, true)
            .await
            .unwrap();
        killed_create(&dir, &staging, &lock);
        drop(killed);
        assert_eq!(
            reclaim_abandoned_in(&dir).await,
            Reclaimed {
                owners: 1,
                resources: 1,
                remaining: 0
            }
        );
        assert!(!staging.exists(), "the staging directory is left");
        assert!(lock.exists(), "the tag's lock file was unlinked");
        assert!(
            files_in(&dir).is_empty(),
            "owner files left: {:?}",
            files_in(&dir)
        );
    }

    /// A create that renamed its staging directory into place, or removed it, leaves no
    /// record. A record whose directory a later create already cleared is dropped without
    /// making a lock file for the tag.
    #[tokio::test]
    async fn a_staging_record_goes_with_its_directory() {
        let (root, dir) = test_dir();
        let (snapshot, staging, lock) = snapshot_paths(root.path());
        // Renamed into place.
        let record = record_snapshot_staging_in(&dir, &staging, &lock).unwrap();
        std::fs::create_dir(&staging).unwrap();
        std::fs::rename(&staging, &snapshot).unwrap();
        drop(record);
        assert!(
            files_in(&dir).is_empty(),
            "a published snapshot left its staging record: {:?}",
            files_in(&dir)
        );
        // Removed on a failure.
        let record = record_snapshot_staging_in(&dir, &staging, &lock).unwrap();
        std::fs::create_dir(&staging).unwrap();
        std::fs::remove_dir(&staging).unwrap();
        drop(record);
        assert!(
            files_in(&dir).is_empty(),
            "a failed create that removed its staging directory left its record: {:?}",
            files_in(&dir)
        );
        // Left by a killed create, then cleared by the next create of the tag.
        killed_create(&dir, &staging, &lock);
        std::fs::remove_dir_all(&staging).unwrap();
        assert_eq!(
            reclaim_abandoned_in(&dir).await,
            Reclaimed {
                owners: 1,
                resources: 1,
                remaining: 0
            }
        );
        assert!(files_in(&dir).is_empty());
        assert!(!lock.exists(), "the pass made a lock file for a tag");
    }

    /// The body of the function whose definition starts with `signature` in `source`.
    fn body_of<'a>(source: &'a str, signature: &str) -> &'a str {
        let start = source
            .find(signature)
            .unwrap_or_else(|| panic!("{signature} is gone"));
        let open = start + source[start..].find('{').expect("a body");
        let mut depth = 0;
        for (at, c) in source[open..].char_indices() {
            match c {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        return &source[open..open + at + 1];
                    }
                }
                _ => {}
            }
        }
        panic!("{signature} has no end")
    }

    /// #1316: every helper a reclaim pass runs to undo a kernel object dies with the pass. A
    /// pass killed while its `iptables -D` was stopped left the delete to run later, after the
    /// names and addresses could belong to a new VM. Each function `undo` reaches spawns its
    /// commands with `crate::utils::helper_command`, and `undo` reaches no other. The same
    /// holds for a snapshot staging directory, whose name the next create of the tag takes:
    /// its removal spawns only `btrfs subvolume delete`, through `delete_subvolume`.
    ///
    /// RED BEFORE THE FIX: every one of them spawned with `Command::new`.
    #[test]
    fn every_undo_helper_dies_with_the_pass_that_runs_it() {
        let reclaim = include_str!("reclaim.rs");
        let undo = body_of(reclaim, "async fn undo(resource: &Resource) -> Undo {");
        let disk = include_str!("storage/disk.rs");
        assert!(
            body_of(
                reclaim,
                "async fn undo_snapshot_staging(path: &Path, lock: &Path) -> Undo {"
            )
            .contains("crate::storage::disk::remove_snapshot_dir(path)"),
            "a snapshot staging directory is not removed by remove_snapshot_dir"
        );
        let remove = body_of(disk, "pub async fn remove_snapshot_dir(");
        assert!(
            remove.contains("delete_subvolume(") && !remove.contains("Command"),
            "remove_snapshot_dir spawns a helper of its own: check it and list it here"
        );
        let checked: [(&str, &str, &[&str]); 6] = [
            ("reclaim", reclaim, &["async fn undo_rule("]),
            ("storage::disk", disk, &["async fn delete_subvolume("]),
            (
                "network::namespace",
                include_str!("network/namespace.rs"),
                &["pub async fn delete_namespace("],
            ),
            (
                "network::veth",
                include_str!("network/veth.rs"),
                &[
                    "pub async fn delete_veth_pair(",
                    "pub async fn delete_host_route_to_guest(",
                ],
            ),
            (
                "network::portmap",
                include_str!("network/portmap.rs"),
                &["pub async fn cleanup_global_nat_if_unused("],
            ),
            (
                "network::routed",
                include_str!("network/routed.rs"),
                &["async fn remove_address_objects("],
            ),
        ];
        let mut reached: Vec<&str> = undo
            .match_indices("crate::network::")
            .map(|(at, _)| {
                let call = &undo[at..];
                &call[..call.find('(').expect("a call")]
            })
            .collect();
        reached.sort();
        assert_eq!(
            reached,
            [
                "crate::network::namespace::delete_namespace",
                "crate::network::portmap::cleanup_global_nat_if_unused",
                "crate::network::routed::release_address",
                "crate::network::veth::delete_host_route_to_guest",
                "crate::network::veth::delete_veth_pair",
            ],
            "undo reaches other network functions: check their helpers and list them here"
        );
        // release_address leases the address and calls remove_address_objects.
        assert!(body_of(
            include_str!("network/routed.rs"),
            "async fn release_address_in("
        )
        .contains("remove_address_objects("));
        for (module, source, functions) in checked {
            for function in functions {
                let body = body_of(source, function);
                assert!(
                    body.contains("crate::utils::helper_command("),
                    "{module} {function} spawns no helper_command"
                );
                assert!(
                    !body.contains("Command::new("),
                    "{module} {function} spawns a helper that outlives the pass"
                );
            }
        }
    }

    /// A helper that writes an owner's resources inherits its lock, so a pass leaves them
    /// while the helper runs, even after the owner itself is gone.
    #[tokio::test]
    async fn a_helper_holding_the_lock_keeps_its_owners_resources() {
        let (root, dir) = test_dir();
        let temp = root.path().join("being-written.tmp");
        std::fs::write(&temp, b"x").unwrap();
        let owner = Owner::begin_in(&dir, "image-temp-helper").unwrap();
        owner.record(Resource::Temp { path: temp.clone() }).unwrap();
        let mut command = tokio::process::Command::new("sleep");
        command.arg("600").kill_on_drop(true);
        owner.holder().unwrap().hold_in(&mut command);
        let mut helper = command.spawn().unwrap();
        // This process's copy of the descriptor goes with the command.
        drop(command);
        // The owner's process dies; its helper does not.
        drop(owner);
        assert_eq!(
            reclaim_abandoned_in(&dir).await,
            Reclaimed::default(),
            "a pass reclaimed what a running helper is writing"
        );
        assert!(temp.exists());
        helper.kill().await.unwrap();
        assert_eq!(reclaim_abandoned_in(&dir).await.resources, 1);
        assert!(!temp.exists());
    }

    /// The first launch with a new data directory creates it, as it did before owners.
    #[test]
    fn the_owner_directory_is_made_in_a_data_directory_that_does_not_exist_yet() {
        let root = tempfile::tempdir().unwrap();
        let dir = owner_dir_in(&root.path().join("new/data")).unwrap();
        assert!(dir.is_dir());
    }

    /// Teardown drops a network object's record before it deletes the object, after
    /// which another VM can take its name.
    #[tokio::test]
    async fn forget_drops_records_without_undoing_them() {
        let (root, dir) = test_dir();
        let kept = root.path().join("kept.tmp");
        std::fs::write(&kept, b"x").unwrap();
        let owner = Owner::begin_in(&dir, "vm-forget").unwrap();
        owner
            .record(Resource::Netns {
                name: "fcvm-vm-forget".into(),
            })
            .unwrap();
        owner.record(Resource::Temp { path: kept.clone() }).unwrap();
        forget("vm-forget", Resource::is_network).unwrap();
        drop(owner);
        let record: Record =
            serde_json::from_slice(&std::fs::read(record_path(&dir, "vm-forget")).unwrap())
                .unwrap();
        assert_eq!(record.resources, [Resource::Temp { path: kept.clone() }]);
        assert!(kept.exists(), "forget undid a resource");
        forget("vm-without-owner", |_| true).unwrap();
    }

    /// A reboot discards kernel objects, and a later VM can hold their names, so a record
    /// from another boot is never acted on for them.
    #[tokio::test]
    async fn kernel_objects_recorded_before_the_last_boot_are_left_alone() {
        let (root, dir) = test_dir();
        let temp = root.path().join("before-reboot.tmp");
        std::fs::write(&temp, b"x").unwrap();
        let record = Record {
            pid: 1,
            boot_id: "a-boot-before-this-one".into(),
            user_namespace: current_user_namespace(),
            resources: vec![
                // Its undo cannot run, so a pass that acted on it would keep it.
                Resource::Rule {
                    program: "/nonexistent/iptables".into(),
                    args: vec!["-A".into(), "FORWARD".into()],
                },
                Resource::Temp { path: temp.clone() },
            ],
        };
        write_by_hand(&dir, "vm-before-reboot", &record);
        assert_eq!(
            reclaim_abandoned_in(&dir).await,
            Reclaimed {
                owners: 1,
                resources: 2,
                remaining: 0
            }
        );
        assert!(!temp.exists());
        assert!(files_in(&dir).is_empty());
    }

    /// Kernel objects are named in the namespaces of the process that made them. A
    /// record another user namespace wrote (a container since replaced, sharing this
    /// store) names objects that are gone or are another VM's now, so a pass leaves them
    /// alone and still undoes the record's files.
    #[tokio::test]
    async fn kernel_objects_recorded_in_another_user_namespace_are_left_alone() {
        let (root, dir) = test_dir();
        let temp = root.path().join("other-namespace.tmp");
        std::fs::write(&temp, b"x").unwrap();
        let record = Record {
            pid: 1,
            boot_id: boot_id(),
            user_namespace: Some(current_user_namespace().unwrap() + 1),
            resources: vec![
                // Its undo cannot run, so a pass that acted on it would keep it.
                Resource::Rule {
                    program: "/nonexistent/iptables".into(),
                    args: vec!["-A".into(), "FORWARD".into()],
                },
                Resource::Temp { path: temp.clone() },
            ],
        };
        write_by_hand(&dir, "vm-other-namespace", &record);
        assert_eq!(
            reclaim_abandoned_in(&dir).await,
            Reclaimed {
                owners: 1,
                resources: 2,
                remaining: 0
            }
        );
        assert!(!temp.exists());
    }

    fn rule(program: &str, chain: &str) -> Resource {
        Resource::Rule {
            program: program.into(),
            args: vec!["-A".into(), chain.into()],
        }
    }

    /// A kernel object's record is rewritten without it before it is deleted, so a pass
    /// killed in between leaks it instead of later deleting a namesake.
    #[tokio::test]
    async fn a_kernel_object_is_retired_from_the_record_before_it_is_deleted() {
        let temp = PathBuf::from("/nonexistent/reclaim-test.tmp");
        let persisted = std::sync::Mutex::new(Vec::new());
        let remaining = undo_all(
            "vm-retire",
            vec![
                Resource::Temp { path: temp.clone() },
                rule("true", "FORWARD"),
            ],
            true,
            |list| {
                persisted.lock().unwrap().push(list.to_vec());
                Ok(())
            },
        )
        .await;
        assert!(remaining.is_empty());
        assert_eq!(
            persisted.into_inner().unwrap(),
            [vec![Resource::Temp { path: temp }]],
            "the rule was not retired before its delete"
        );
    }

    /// A kernel object that cannot be removed keeps the older ones that hold its names.
    #[tokio::test]
    async fn an_object_that_cannot_be_removed_keeps_the_ones_recorded_before_it() {
        let veth = Resource::Link {
            name: "veth0-vm-reclm0".into(),
        };
        let stuck = rule("/nonexistent/iptables", "FORWARD");
        let remaining = undo_all("vm-stuck", vec![veth.clone(), stuck.clone()], true, |_| {
            Ok(())
        })
        .await;
        let remaining: Vec<Resource> = remaining.into_iter().map(|(r, _)| r).collect();
        assert_eq!(remaining, [veth, stuck]);
    }

    /// Two published ports with one guest port append the same rule twice; each copy is
    /// recorded, so each gets a delete.
    #[tokio::test]
    async fn a_rule_recorded_twice_is_kept_twice() {
        let (_root, dir) = test_dir();
        let owner = Owner::begin_in(&dir, "vm-twice").unwrap();
        let twice = rule("iptables", "FORWARD");
        owner.record(twice.clone()).unwrap();
        owner.record(twice.clone()).unwrap();
        let record: Record =
            serde_json::from_slice(&std::fs::read(record_path(&dir, "vm-twice")).unwrap()).unwrap();
        assert_eq!(record.resources, [twice.clone(), twice]);
        drop(owner);
    }

    /// A forget whose write fails changes nothing, so the caller keeps the objects.
    #[tokio::test]
    async fn a_failed_forget_keeps_the_records() {
        let (_root, dir) = test_dir();
        let owner = Owner::begin_in(&dir, "vm-forget-fails").unwrap();
        let netns = Resource::Netns {
            name: "fcvm-vm-ffails".into(),
        };
        owner.record(netns.clone()).unwrap();
        // A directory in the record's place makes the rename onto it fail, as root too.
        let record = record_path(&dir, "vm-forget-fails");
        std::fs::remove_file(&record).unwrap();
        std::fs::create_dir_all(record.join("blocker")).unwrap();
        assert!(forget("vm-forget-fails", Resource::is_network).is_err());
        let state = owner.inner.state.lock().unwrap();
        assert_eq!(state.resources, [netns]);
        drop(state);
        drop(owner);
    }

    /// An owner's files are in the bucket of its id and nowhere else, and the bucket is a
    /// directory of this user's that no one else can write.
    #[tokio::test]
    async fn an_owners_files_are_in_the_bucket_of_its_id() {
        let (root, dir) = test_dir();
        let owner = Owner::begin_in(&dir, "vm-bucketed").unwrap();
        owner
            .record(Resource::Temp {
                path: root.path().join("t.tmp"),
            })
            .unwrap();
        let bucket = dir.join(crate::state::manager::bucket_of("vm-bucketed"));
        assert_eq!(
            record_files(&dir),
            [
                bucket.join("vm-bucketed.json"),
                bucket.join("vm-bucketed.lock")
            ]
        );
        let metadata = std::fs::metadata(&bucket).unwrap();
        assert_eq!(metadata.mode() & 0o777, 0o700);
        assert_eq!(metadata.uid(), nix::unistd::geteuid().as_raw());
        owner.complete().await.unwrap();
        assert!(record_files(&dir).is_empty());
    }

    /// The sweep reclaims a record in any bucket, and one in the owner directory itself,
    /// where an fcvm before the buckets kept its records. The sweep's own lock and stamp
    /// are not taken for an owner's files, by this sweep or by that fcvm's.
    #[tokio::test]
    async fn the_sweep_reads_every_bucket_and_the_owner_directory_itself() {
        let (root, dir) = test_dir();
        let mut temps = Vec::new();
        for n in 0..64 {
            let temp = root.path().join(format!("{n}.tmp"));
            std::fs::write(&temp, b"x").unwrap();
            let owner = Owner::begin_in(&dir, &format!("vm-{n}")).unwrap();
            owner.record(Resource::Temp { path: temp.clone() }).unwrap();
            drop(owner);
            temps.push(temp);
        }
        let buckets: std::collections::BTreeSet<_> = record_files(&dir)
            .iter()
            .map(|path| path.parent().unwrap().to_path_buf())
            .collect();
        assert!(
            buckets.len() > 8,
            "64 ids fell in {} buckets",
            buckets.len()
        );
        // A record of an older fcvm, beside the buckets.
        let old = root.path().join("old.tmp");
        std::fs::write(&old, b"x").unwrap();
        let record = Record {
            pid: 1,
            boot_id: boot_id(),
            user_namespace: current_user_namespace(),
            resources: vec![Resource::Temp { path: old.clone() }],
        };
        std::fs::write(
            dir.join("vm-flat.json"),
            serde_json::to_vec(&record).unwrap(),
        )
        .unwrap();
        std::fs::write(dir.join("vm-flat.lock"), b"").unwrap();

        // A launch's sweep, which leaves its lock and stamp in the owner directory.
        let swept = reclaim_abandoned_if_due_in(&dir, std::time::Duration::from_secs(3600)).await;
        assert_eq!(
            (swept.owners, swept.resources, swept.remaining),
            (65, 65, 0)
        );
        assert!(temps.iter().all(|temp| !temp.exists()) && !old.exists());
        assert!(record_files(&dir).is_empty());
        let mut left: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.len() > 1)
            .collect();
        left.sort();
        assert_eq!(left, [SWEEP_LOCK, SWEEP_STAMP]);
        for name in [SWEEP_LOCK, SWEEP_STAMP] {
            assert!(!name.ends_with(".lock") && !name.ends_with(".json"));
        }
        // An unconditional sweep leaves them where they are.
        assert_eq!(reclaim_abandoned_in(&dir).await, Reclaimed::default());
        assert!(dir.join(SWEEP_LOCK).exists() && dir.join(SWEEP_STAMP).exists());
    }

    /// A stamp is fresh only when it is this boot's, parses, and names a time at or before
    /// now and less than the interval ago.
    #[test]
    fn a_stamp_is_fresh_only_within_the_interval_of_this_boot() {
        let second = std::time::Duration::from_secs(1);
        let now = 50_000_000_000u64;
        let fresh = |stamp: &str| stamp_is_fresh(stamp, "boot-a", now, second);
        assert!(fresh(&stamp_text("boot-a", now)));
        assert!(fresh(&stamp_text("boot-a", now - 999_999_999)));
        assert!(
            !fresh(&stamp_text("boot-a", now - 1_000_000_000)),
            "the interval has passed"
        );
        assert!(!fresh(&stamp_text("boot-a", 0)));
        assert!(
            !fresh(&stamp_text("boot-a", now + 1)),
            "a stamp later than now"
        );
        assert!(!fresh(&stamp_text("boot-b", now)), "another boot's count");
        for unreadable in ["", "boot-a", "boot-a soon", "boot-a -5", "boot-a 1 2"] {
            assert!(!fresh(unreadable), "{unreadable:?}");
        }
        assert_eq!(RECLAIM_EVERY, second);
        // The clock a stamp is taken from does not go back.
        let (a, b) = (boottime_ns(), boottime_ns());
        assert!(a > 0 && b >= a);
    }

    /// A launch sweeps when no sweep began in this boot within the interval, and otherwise
    /// reclaims nothing and leaves the records for the sweep that is due next. A stamp of
    /// another boot, a stamp later than now, an unreadable stamp and a missing one all make
    /// a sweep due.
    #[tokio::test]
    async fn a_launch_sweeps_only_when_a_sweep_is_due() {
        let (root, dir) = test_dir();
        let hour = std::time::Duration::from_secs(3600);
        let abandon = |id: &str| {
            let temp = root.path().join(format!("{id}.tmp"));
            std::fs::write(&temp, b"x").unwrap();
            let owner = Owner::begin_in(&dir, id).unwrap();
            owner.record(Resource::Temp { path: temp.clone() }).unwrap();
            drop(owner);
            temp
        };
        let stamp = dir.join(SWEEP_STAMP);

        // No stamp: due. The sweep stamps this boot and a time not after now.
        let first = abandon("vm-first");
        assert_eq!(reclaim_abandoned_if_due_in(&dir, hour).await.owners, 1);
        assert!(!first.exists());
        let written = std::fs::read_to_string(&stamp).unwrap();
        let (boot, began) = written.split_once(' ').unwrap();
        assert_eq!(boot, boot_id());
        assert!(began.parse::<u64>().unwrap() <= boottime_ns());

        // Swept within the interval: not due, and the record stays whole.
        let second = abandon("vm-second");
        assert_eq!(
            reclaim_abandoned_if_due_in(&dir, hour).await,
            Reclaimed::default()
        );
        assert!(second.exists() && record_path(&dir, "vm-second").exists());
        set_sweep_stamp(&dir, hour - std::time::Duration::from_secs(60)).unwrap();
        assert_eq!(
            reclaim_abandoned_if_due_in(&dir, hour).await,
            Reclaimed::default()
        );

        // The interval has passed: due.
        set_sweep_stamp(&dir, hour).unwrap();
        assert_eq!(reclaim_abandoned_if_due_in(&dir, hour).await.owners, 1);
        assert!(!second.exists());

        // Another boot's stamp, one later than now, and one that does not parse: due.
        for (id, text) in [
            ("vm-third", stamp_text("another-boot", boottime_ns())),
            (
                "vm-fourth",
                stamp_text(&boot_id(), boottime_ns() + 3_600_000_000_000),
            ),
            ("vm-fifth", "not a stamp".to_string()),
        ] {
            let temp = abandon(id);
            std::fs::write(&stamp, text).unwrap();
            assert_eq!(
                reclaim_abandoned_if_due_in(&dir, hour).await.owners,
                1,
                "{id}"
            );
            assert!(!temp.exists());
        }
    }

    /// While one launch holds the sweep lock, another launch does not sweep and does not
    /// wait, although no sweep has stamped; a sweeper that is killed gives the lock back.
    #[tokio::test]
    async fn a_launch_does_not_sweep_or_wait_while_another_sweeps() {
        use std::os::unix::process::CommandExt;
        let (root, dir) = test_dir();
        let temp = root.path().join("left.tmp");
        std::fs::write(&temp, b"x").unwrap();
        let owner = Owner::begin_in(&dir, "vm-left").unwrap();
        owner.record(Resource::Temp { path: temp.clone() }).unwrap();
        drop(owner);

        let mut sweeper = std::process::Command::new("flock")
            .args(["-x", "-o"])
            .arg(dir.join(SWEEP_LOCK))
            .args(["sh", "-c", "echo held; exec sleep 60"])
            .stdout(std::process::Stdio::piped())
            .process_group(0)
            .spawn()
            .unwrap();
        let mut line = String::new();
        std::io::BufReader::new(sweeper.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        assert_eq!(line.trim(), "held");
        // Any interval finds a sweep due: nothing has stamped.
        let hour = std::time::Duration::from_secs(3600);
        assert_eq!(
            reclaim_abandoned_if_due_in(&dir, hour).await,
            Reclaimed::default()
        );
        assert!(temp.exists(), "swept beside the launch that holds the lock");
        assert!(!dir.join(SWEEP_STAMP).exists(), "stamped without sweeping");

        nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(sweeper.id() as i32),
            nix::sys::signal::Signal::SIGKILL,
        )
        .unwrap();
        sweeper.wait().unwrap();
        assert_eq!(reclaim_abandoned_if_due_in(&dir, hour).await.owners, 1);
        assert!(!temp.exists());

        // With the interval set to nothing a launch sweeps whatever the lock and the stamp.
        let again = root.path().join("again.tmp");
        std::fs::write(&again, b"x").unwrap();
        let owner = Owner::begin_in(&dir, "vm-again").unwrap();
        owner
            .record(Resource::Temp {
                path: again.clone(),
            })
            .unwrap();
        drop(owner);
        let _held = take_sweep(&dir, std::time::Duration::from_nanos(1))
            .unwrap()
            .expect("the sweep lock");
        assert_eq!(
            reclaim_abandoned_if_due_in(&dir, hour).await,
            Reclaimed::default()
        );
        let zero = std::time::Duration::ZERO;
        assert_eq!(reclaim_abandoned_if_due_in(&dir, zero).await.owners, 1);
        assert!(!again.exists());
        assert_eq!(reclaim_every(None), RECLAIM_EVERY);
        assert_eq!(reclaim_every(Some(" 0\n")), zero);
        assert_eq!(reclaim_every(Some("250")).as_millis(), 250);
        assert_eq!(reclaim_every(Some("soon")), RECLAIM_EVERY);
    }

    /// A launch refused something a killed VM may hold sweeps whatever the stamp says. It
    /// waits for a launch that is sweeping, then sweeps; when that launch is still at it
    /// after the wait, it sweeps beside it.
    #[tokio::test]
    async fn a_refused_launch_sweeps_at_once_and_waits_only_so_long() {
        let (root, dir) = test_dir();
        let abandon = |id: &str| {
            let temp = root.path().join(format!("{id}.tmp"));
            std::fs::write(&temp, b"x").unwrap();
            let owner = Owner::begin_in(&dir, id).unwrap();
            owner.record(Resource::Temp { path: temp.clone() }).unwrap();
            drop(owner);
            temp
        };
        let hour = std::time::Duration::from_secs(3600);
        // A fresh stamp keeps an ordinary launch from sweeping, not a refused one.
        set_sweep_stamp(&dir, std::time::Duration::ZERO).unwrap();
        let first = abandon("vm-first");
        assert_eq!(
            reclaim_abandoned_if_due_in(&dir, hour).await,
            Reclaimed::default()
        );
        assert_eq!(
            reclaim_abandoned_on_contention_in(&dir, hour).await.owners,
            1
        );
        assert!(!first.exists());

        // Another launch holds the sweep lock and lets go of it 100 ms in: the refused
        // launch waits for it, well inside its hour.
        let second = abandon("vm-second");
        let held = try_sweep_lock(&OwnerDir::open(&dir).unwrap())
            .unwrap()
            .expect("the sweep lock");
        let releaser = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            drop(held);
        });
        let started = std::time::Instant::now();
        assert_eq!(
            reclaim_abandoned_on_contention_in(&dir, hour).await.owners,
            1
        );
        assert!(started.elapsed() >= std::time::Duration::from_millis(90));
        assert!(!second.exists());
        releaser.await.unwrap();

        // The lock is held for longer than the wait: the launch sweeps beside it.
        let third = abandon("vm-third");
        let _held = try_sweep_lock(&OwnerDir::open(&dir).unwrap())
            .unwrap()
            .expect("the sweep lock");
        let wait = std::time::Duration::from_millis(50);
        let started = std::time::Instant::now();
        assert_eq!(
            reclaim_abandoned_on_contention_in(&dir, wait).await.owners,
            1
        );
        assert!(started.elapsed() >= wait);
        assert!(!third.exists());
    }

    /// The launch commands sweep through the gate, and the commands that need the room a
    /// killed process left sweep whenever they start.
    #[test]
    fn launches_sweep_when_due_and_the_rest_always() {
        let gated = "crate::reclaim::reclaim_abandoned_if_due().await";
        let always = "crate::reclaim::reclaim_abandoned().await";
        for (name, source, gated_calls, always_calls) in [
            ("podman", include_str!("commands/podman/mod.rs"), 1, 0),
            ("snapshot", include_str!("commands/snapshot.rs"), 2, 0),
            ("vm", include_str!("commands/vm.rs"), 1, 0),
            ("setup", include_str!("commands/setup.rs"), 0, 2),
            ("import", include_str!("commands/snapshot_import.rs"), 0, 1),
        ] {
            let shipped = &source[..source
                .find("\n#[cfg(test)]\nmod tests {")
                .unwrap_or(source.len())];
            assert_eq!(
                (
                    shipped.matches(gated).count(),
                    shipped.matches(always).count()
                ),
                (gated_calls, always_calls),
                "{name}: sweeps through the gate, and sweeps always"
            );
        }
        let export = include_str!("commands/snapshot_export.rs");
        assert!(export.contains("crate::reclaim::reclaim_abandoned_in(&owner_dir).await"));
    }

    /// Records are read through the owner directory's own descriptor, opened without
    /// following a symlink, so a directory put in its place (a symlink to one another
    /// user wrote) is refused instead of read.
    #[tokio::test]
    async fn an_owner_directory_replaced_by_a_symlink_is_refused() {
        let (root, _dir) = test_dir();
        let victim = root.path().join("victim");
        std::fs::write(&victim, b"keep").unwrap();
        let forged = root.path().join("forged");
        std::fs::create_dir(&forged).unwrap();
        let record = Record {
            pid: 1,
            boot_id: boot_id(),
            user_namespace: current_user_namespace(),
            resources: vec![Resource::Temp {
                path: victim.clone(),
            }],
        };
        std::fs::write(forged.join("x.json"), serde_json::to_vec(&record).unwrap()).unwrap();
        let link = root.path().join("swapped");
        std::os::unix::fs::symlink(&forged, &link).unwrap();
        assert_eq!(reclaim_abandoned_in(&link).await, Reclaimed::default());
        assert!(
            victim.exists(),
            "a record read through a symlink removed its path"
        );
    }
}
