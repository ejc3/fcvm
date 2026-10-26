//! Host resources a killed fcvm leaves behind, and how the next fcvm takes them back.
//!
//! fcvm creates host resources that outlive its process unless teardown removes them: a
//! VM's runtime directory (`vm-disks/<vm_id>`, with its disks), the mountpoint a restore
//! makes over a source VM's directory, the temporaries of an image export. A SIGKILL skips
//! teardown. The kernel ends the child processes (pdeathsig) and releases locks; every
//! other resource stays.
//!
//! An owner record makes each of them reclaimable. Its files live in
//! `<data_dir>/reclaim/<euid>/`:
//!
//! | step | what the owner does |
//! |------|----------------------|
//! | [`Owner::begin`] | creates `<id>.lock` and holds an exclusive flock on it until it completes or exits |
//! | [`record`] | adds the resource to `<id>.json` (written under a temporary name and renamed) before creating it |
//! | [`complete`] | at the end of teardown, undoes what is still recorded and deletes both files |
//!
//! [`reclaim_abandoned`] runs when a VM launch starts. It takes every owner lock it can
//! without waiting. A lock nobody holds belongs to an owner that exited without completing,
//! so the resources in its record are undone and the record is deleted. The flock is the
//! owner's liveness: the kernel releases it when the process dies however it dies, and a
//! reused PID cannot hold it. A resource that cannot be undone yet stays in the record for
//! the next launch.
//!
//! Recording comes before creating, so a kill between the two leaves a record of something
//! that does not exist, and undoing it finds nothing. That is safe only for names no other
//! process can create, which holds for every resource here: a VM's directory is named by
//! its random vm_id and a temporary by a random UUID. A mountpoint can be another process's
//! too, and its undo removes it only if it is empty and nobody
//! leases it (`storage::runtime_dir`).
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
    /// A VM's runtime directory. Removed whole, once no restore leases it.
    RuntimeDir { path: PathBuf },
    /// A source VM's directory that a restore binds its own directory over. Removed only
    /// while it is empty and no restore leases it, so a live
    /// VM's directory is never touched.
    MountTarget { path: PathBuf },
    /// A temporary file or directory that is renamed into place or removed when its
    /// operation finishes.
    Temp { path: PathBuf },
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
}

impl Resource {
    /// An object of the running kernel, which a reboot discards.
    fn is_kernel_object(&self) -> bool {
        !matches!(
            self,
            Resource::RuntimeDir { .. } | Resource::MountTarget { .. } | Resource::Temp { .. }
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
            Resource::MountTarget { path } => write!(f, "mount target {}", path.display()),
            Resource::Temp { path } => write!(f, "temporary {}", path.display()),
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

/// [`owner_dir`] under an explicit data directory.
pub fn owner_dir_in(data_dir: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(data_dir)
        .with_context(|| format!("creating data directory {}", data_dir.display()))?;
    let shared = data_dir.join("reclaim");
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
    let mine = shared.join(euid.to_string());
    match std::fs::DirBuilder::new().mode(0o700).create(&mine) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error).with_context(|| format!("creating {}", mine.display())),
    }
    let metadata =
        std::fs::symlink_metadata(&mine).with_context(|| format!("reading {}", mine.display()))?;
    if !metadata.is_dir() || metadata.uid() != euid || metadata.mode() & 0o022 != 0 {
        bail!(
            "refusing owner records in {}: it must be a directory owned by uid {euid} that \
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

fn lock_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.lock"))
}

fn record_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.json"))
}

fn record_temp_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.json.tmp"))
}

fn remove_if_exists(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

/// Lock `path` exclusively, creating it if needed. `None` when `wait` is false and someone
/// holds it. A lock file is unlinked by whoever completes or reclaims its owner while still
/// holding it, so the lock is retaken until the inode held is the one at the path.
fn lock_owner_file(path: &Path, wait: bool) -> Result<Option<Flock<File>>> {
    loop {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)
            .with_context(|| format!("opening {}", path.display()))?;
        let arg = if wait {
            FlockArg::LockExclusive
        } else {
            FlockArg::LockExclusiveNonblock
        };
        let guard = match Flock::lock(file, arg) {
            Ok(guard) => guard,
            Err((_, nix::errno::Errno::EWOULDBLOCK)) if !wait => return Ok(None),
            Err((_, error)) => {
                return Err(error).with_context(|| format!("locking {}", path.display()))
            }
        };
        let held = guard
            .metadata()
            .with_context(|| format!("reading the identity of {}", path.display()))?;
        match std::fs::metadata(path) {
            Ok(current) if (current.dev(), current.ino()) == (held.dev(), held.ino()) => {
                return Ok(Some(guard))
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
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

fn write_record(dir: &Path, id: &str, resources: &[Resource]) -> Result<()> {
    let record = Record {
        pid: std::process::id(),
        boot_id: boot_id(),
        resources: resources.to_vec(),
    };
    let temp = record_temp_path(dir, id);
    std::fs::write(&temp, serde_json::to_vec(&record)?)
        .with_context(|| format!("writing {}", temp.display()))?;
    std::fs::rename(&temp, record_path(dir, id))
        .with_context(|| format!("publishing the owner record of {id}"))
}

/// Delete an owner's files. The caller holds `lock`, and the lock file goes last so that
/// no one can take a new lock on the id while its record still exists.
fn delete_owner_files(dir: &Path, id: &str, lock: Flock<File>) -> Result<()> {
    remove_if_exists(&record_path(dir, id))
        .and_then(|()| remove_if_exists(&record_temp_path(dir, id)))
        .and_then(|()| remove_if_exists(&lock_path(dir, id)))
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
    dir: PathBuf,
    state: Mutex<OwnerState>,
}

/// Owners of this process, by id, for code that records a resource deep inside a VM's
/// setup and has only the vm_id to hand.
static OWNERS: LazyLock<Mutex<HashMap<String, Weak<Inner>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// The live owner of a set of resources. Dropping it without [`Owner::complete`] (an early
/// return, a panic) releases its lock and keeps its record, so the next launch reclaims
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
        let lock = lock_owner_file(&lock_path(dir, id), true)?
            .context("an owner lock taken with waiting returned none")?;
        if record_path(dir, id).exists() {
            bail!(
                "an owner record for {id} already exists in {}; ids are never reused",
                dir.display()
            );
        }
        let inner = Arc::new(Inner {
            id: id.to_string(),
            dir: dir.to_path_buf(),
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
        use std::os::fd::AsFd;
        let state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
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
                "owner ended without completing; the next launch reclaims what it recorded"
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
            // This process, or another, still leases them (a clone keeps the mountpoint
            // its VMM's vsock path goes through until it exits). The record stays, and
            // the first launch after the lease ends removes them.
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
            "{} resource(s) of {} remain for the next launch to reclaim: {kept}",
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
/// for the next launch and fails this call.
pub async fn complete(id: &str) -> Result<()> {
    owner(id)?.complete().await
}

/// Why an undo left a resource in place.
pub(crate) enum Undo {
    /// The resource is gone, or is not this owner's to remove.
    Done,
    /// Still there; a later launch tries again.
    Kept(String),
    /// Still there because a live process uses it: a lease on it is held. Not a failure;
    /// the record stays until a launch finds it unused.
    InUse(String),
}

/// How the reason of an [`Undo::InUse`] resource starts, in what `undo_all` returns.
const IN_USE: &str = "in use: ";

/// Undo `resources` newest first, returning the ones that stay. `same_boot` is false for
/// a record written before the last reboot, whose kernel objects are gone.
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
            debug!(owner = %id, %resource, "recorded before the last boot; gone with it");
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
        Resource::MountTarget { path } => undo_mount_target(path).await,
        Resource::Temp { path } => undo_temp(path).await,
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
    match tokio::process::Command::new(program)
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

/// Open `path` and lock it exclusively without waiting. `Ok(None)` when it does not exist
/// or another inode replaced it after the open; `Err` with `WouldBlock` when it is locked.
fn lock_directory(path: &Path) -> std::io::Result<Option<File>> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    // Fully qualified: std::fs::File has inherent try_lock_* methods with another error
    // type, and inherent methods win over trait methods.
    fs2::FileExt::try_lock_exclusive(&file)?;
    let held = file.metadata()?;
    match std::fs::symlink_metadata(path) {
        Ok(current) if (current.dev(), current.ino()) == (held.dev(), held.ino()) => Ok(Some(file)),
        Ok(_) => Ok(None),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

async fn undo_runtime_dir(path: &Path) -> Undo {
    let _lock = match lock_directory(path) {
        Ok(Some(file)) => file,
        Ok(None) => return Undo::Done,
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
            return Undo::InUse("a restore leases it".to_string())
        }
        Err(error) => return Undo::Kept(error.to_string()),
    };
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

async fn undo_mount_target(path: &Path) -> Undo {
    let _lock = match lock_directory(path) {
        Ok(Some(file)) => file,
        Ok(None) => return Undo::Done,
        // A restore leases it: removed by whichever lease ends last, or by a later
        // launch once none does (a clone that exits without dropping its lease).
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
            return Undo::InUse("a restore leases it".to_string())
        }
        Err(error) => return Undo::Kept(error.to_string()),
    };
    // Empty, as `storage::runtime_dir` judges a mountpoint a restore made: a live VM's
    // directory holds its disks.
    let removed = match std::fs::read_dir(path).map(|mut entries| entries.next().is_none()) {
        Ok(true) => std::fs::remove_dir(path),
        Ok(false) => Ok(()),
        Err(error) => Err(error),
    };
    match removed {
        Ok(()) => Undo::Done,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Undo::Done,
        Err(error) => Undo::Kept(error.to_string()),
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
/// owner directory. Called when a VM launch starts. Never waits on a live owner, and never
/// fails the launch: what it cannot undo is logged and left for the next one.
pub async fn reclaim_abandoned() -> Reclaimed {
    // The pmem store keeps its own record of who is using `tmp/`: its lock.
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
    let dir = match owner_dir() {
        Ok(dir) => dir,
        Err(error) => {
            warn!(error = %format!("{error:#}"), "cannot reclaim resources of exited fcvm processes");
            return Reclaimed::default();
        }
    };
    reclaim_abandoned_in(&dir).await
}

/// [`reclaim_abandoned`] in an explicit owner directory.
pub async fn reclaim_abandoned_in(dir: &Path) -> Reclaimed {
    let mut done = Reclaimed::default();
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) => {
            warn!(dir = %dir.display(), %error, "cannot read owner records");
            return done;
        }
    };
    let mut ids = std::collections::BTreeSet::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if let Some(id) = name
            .strip_suffix(".lock")
            .or_else(|| name.strip_suffix(".json"))
        {
            ids.insert(id.to_string());
        }
    }
    for id in ids {
        let lock = match lock_owner_file(&lock_path(dir, &id), false) {
            Ok(Some(lock)) => lock,
            // Its owner is alive.
            Ok(None) => continue,
            Err(error) => {
                warn!(owner = %id, error = %format!("{error:#}"), "cannot lock owner record");
                continue;
            }
        };
        let record = match std::fs::read(record_path(dir, &id)) {
            Ok(bytes) => bytes,
            // An owner that recorded nothing, or one that is between creating its lock
            // and taking it (it finds the file gone and makes a new one).
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
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
        let same_boot = record.boot_id == boot_id();
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufRead;

    const CHILD_DIR: &str = "FCVM_RECLAIM_TEST_CHILD_DIR";
    const CHILD_READY: &str = "reclaim-child: resources created";

    fn test_dir() -> (tempfile::TempDir, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let dir = owner_dir_in(root.path()).unwrap();
        (root, dir)
    }

    fn files_in(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
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
        assert_eq!(
            reclaim_abandoned_in(&dir).await,
            Reclaimed::default(),
            "a pass reclaimed resources of a live owner"
        );
        assert!(runtime_dir.join("disks/rootfs.raw").exists() && temp.exists());

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
            runtime_dir.exists() && temp.exists() && store_temps().len() == 1,
            "the resources are gone before any pass ran"
        );

        assert_eq!(
            reclaim_abandoned_in(&dir).await,
            Reclaimed {
                owners: 2,
                resources: 3,
                remaining: 0
            }
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

    /// The runtime directory of a dead owner can still be leased by a restore of its
    /// snapshot. It stays recorded until the lease ends.
    #[tokio::test]
    async fn a_leased_runtime_dir_waits_for_a_later_pass() {
        let (root, dir) = test_dir();
        let runtime_dir = root.path().join("vm-disks/vm-leased");
        std::fs::create_dir_all(runtime_dir.join("disks")).unwrap();
        std::fs::write(runtime_dir.join("disks/rootfs.raw"), b"disk").unwrap();
        let owner = Owner::begin_in(&dir, "vm-leased").unwrap();
        owner
            .record(Resource::RuntimeDir {
                path: runtime_dir.clone(),
            })
            .unwrap();
        drop(owner);

        let lease =
            crate::storage::runtime_dir::lease_mount_targets(std::slice::from_ref(&runtime_dir))
                .await
                .unwrap();
        assert_eq!(
            reclaim_abandoned_in(&dir).await,
            Reclaimed {
                owners: 0,
                resources: 0,
                remaining: 1
            }
        );
        assert!(runtime_dir.join("disks/rootfs.raw").exists());
        drop(lease);
        assert_eq!(
            reclaim_abandoned_in(&dir).await,
            Reclaimed {
                owners: 1,
                resources: 1,
                remaining: 0
            }
        );
        assert!(!runtime_dir.exists());
        assert!(files_in(&dir).is_empty());
    }

    /// A mount target is removed only when it is empty and unleased.
    #[tokio::test]
    async fn a_mount_target_is_removed_only_when_it_is_an_empty_unleased_tree() {
        let (root, dir) = test_dir();
        let made = root.path().join("vm-disks/vm-made");
        std::fs::create_dir_all(&made).unwrap();
        let live = root.path().join("vm-disks/vm-live");
        std::fs::create_dir_all(live.join("disks")).unwrap();
        std::fs::write(live.join("disks/rootfs.raw"), b"disk").unwrap();
        let held = root.path().join("vm-disks/vm-held");
        std::fs::create_dir_all(&held).unwrap();

        let owner = Owner::begin_in(&dir, "vm-clone").unwrap();
        for path in [&made, &live, &held] {
            owner
                .record(Resource::MountTarget { path: path.clone() })
                .unwrap();
        }
        drop(owner);
        let other_clone = std::fs::File::open(&held).unwrap();
        fs2::FileExt::try_lock_shared(&other_clone).unwrap();

        // The held one stays recorded until no restore leases it.
        assert_eq!(
            reclaim_abandoned_in(&dir).await,
            Reclaimed {
                owners: 0,
                resources: 2,
                remaining: 1
            }
        );
        assert!(!made.exists(), "an unleased empty mountpoint was left");
        assert!(
            live.join("disks/rootfs.raw").exists(),
            "a VM's own directory was removed"
        );
        assert!(
            held.exists(),
            "a mountpoint another clone holds was removed"
        );
        drop(other_clone);
        assert_eq!(reclaim_abandoned_in(&dir).await.owners, 1);
        assert!(
            !held.exists(),
            "the mountpoint was left once no lease held it"
        );
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
            serde_json::from_slice(&std::fs::read(dir.join("vm-forget.json")).unwrap()).unwrap();
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
            resources: vec![
                // Its undo cannot run, so a pass that acted on it would keep it.
                Resource::Rule {
                    program: "/nonexistent/iptables".into(),
                    args: vec!["-A".into(), "FORWARD".into()],
                },
                Resource::Temp { path: temp.clone() },
            ],
        };
        std::fs::write(
            record_path(&dir, "vm-before-reboot"),
            serde_json::to_vec(&record).unwrap(),
        )
        .unwrap();
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
            serde_json::from_slice(&std::fs::read(dir.join("vm-twice.json")).unwrap()).unwrap();
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
        let record = dir.join("vm-forget-fails.json");
        std::fs::remove_file(&record).unwrap();
        std::fs::create_dir_all(record.join("blocker")).unwrap();
        assert!(forget("vm-forget-fails", Resource::is_network).is_err());
        let state = owner.inner.state.lock().unwrap();
        assert_eq!(state.resources, [netns]);
        drop(state);
        drop(owner);
    }

    /// A clone whose VMM goes through the redirect keeps its lease on the mountpoint past
    /// its teardown. Completing its owner then leaves the record, so a clone that exits
    /// without dropping the lease still has its mountpoint reclaimed.
    #[tokio::test]
    async fn completing_an_owner_keeps_a_mount_target_it_still_leases() {
        let (root, dir) = test_dir();
        let made = root.path().join("vm-disks/vm-gone-source");
        std::fs::create_dir_all(&made).unwrap();
        let owner = Owner::begin_in(&dir, "vm-clone-leasing").unwrap();
        owner
            .record(Resource::MountTarget { path: made.clone() })
            .unwrap();
        let own_lease = std::fs::File::open(&made).unwrap();
        fs2::FileExt::try_lock_shared(&own_lease).unwrap();
        owner.complete().await.unwrap();
        assert!(
            dir.join("vm-clone-leasing.json").exists(),
            "completion dropped the record of a mountpoint still leased"
        );
        drop(own_lease);
        assert_eq!(reclaim_abandoned_in(&dir).await.owners, 1);
        assert!(!made.exists());
    }
}
