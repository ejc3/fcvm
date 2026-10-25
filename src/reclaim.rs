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
//! too, and its undo removes it only if it holds nothing but empty directories and nobody
//! leases it (`storage::runtime_dir`).
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
    /// while it holds nothing but empty directories and no restore leases it, so a live
    /// VM's directory is never touched.
    MountTarget { path: PathBuf },
    /// A temporary file or directory that is renamed into place or removed when its
    /// operation finishes.
    Temp { path: PathBuf },
}

impl std::fmt::Display for Resource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Resource::RuntimeDir { path } => write!(f, "runtime directory {}", path.display()),
            Resource::MountTarget { path } => write!(f, "mount target {}", path.display()),
            Resource::Temp { path } => write!(f, "temporary {}", path.display()),
        }
    }
}

/// What `<id>.json` holds. The PID is for whoever reads the file; liveness is the lock.
#[derive(Debug, Serialize, Deserialize)]
struct Record {
    pid: u32,
    resources: Vec<Resource>,
}

/// The directory this user's owner records live in, under `data_dir`.
pub fn owner_dir() -> Result<PathBuf> {
    owner_dir_in(&crate::paths::data_dir())
}

/// [`owner_dir`] under an explicit data directory.
pub fn owner_dir_in(data_dir: &Path) -> Result<PathBuf> {
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

fn write_record(dir: &Path, id: &str, resources: &[Resource]) -> Result<()> {
    let record = Record {
        pid: std::process::id(),
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
    drop(lock);
    Ok(())
}

struct OwnerState {
    /// Held until the owner completes or is dropped.
    lock: Option<Flock<File>>,
    resources: Vec<Resource>,
    /// Files kept open for the owner's lifetime, such as the shared lease a restore holds
    /// on a mount target it made (`storage::runtime_dir`).
    pins: Vec<File>,
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
                pins: Vec::new(),
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

    /// Undo whatever is still recorded and delete the record (see [`complete`]).
    pub async fn complete(self) -> Result<()> {
        self.inner.complete().await
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
        state.pins.clear();
        state.lock.take();
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
        if state.resources.contains(&resource) {
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
        let (resources, pins) = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if state.completed {
                return Ok(());
            }
            (
                std::mem::take(&mut state.resources),
                std::mem::take(&mut state.pins),
            )
        };
        // A pin is a lease on a resource below, which its undo must be able to take.
        drop(pins);
        let remaining = undo_all(&self.id, resources).await;
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.completed = true;
        let lock = state
            .lock
            .take()
            .context("a completing owner holds its lock")?;
        if remaining.is_empty() {
            return delete_owner_files(&self.dir, &self.id, lock);
        }
        let kept = remaining
            .iter()
            .map(|(resource, why)| format!("{resource}: {why}"))
            .collect::<Vec<_>>()
            .join("; ");
        let resources: Vec<Resource> = remaining.into_iter().map(|(r, _)| r).collect();
        write_record(&self.dir, &self.id, &resources)?;
        drop(lock);
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

/// Keep `file` open until the owner under `id` completes or exits.
pub fn pin(id: &str, file: File) -> Result<()> {
    let inner = owner(id)?;
    let mut state = inner.state.lock().unwrap_or_else(|e| e.into_inner());
    if state.completed {
        bail!("the owner {id} has completed; nothing can be pinned to it");
    }
    state.pins.push(file);
    Ok(())
}

/// End the owner registered under `id`: release its pins, undo every resource still
/// recorded, and delete its record. Teardown has removed most of them already, and undoing
/// a resource that is gone does nothing. A resource that cannot be undone stays recorded
/// for the next launch and fails this call.
pub async fn complete(id: &str) -> Result<()> {
    owner(id)?.complete().await
}

/// Why an undo left a resource in place.
enum Undo {
    /// The resource is gone, or is not this owner's to remove.
    Done,
    /// Still there; a later launch tries again.
    Kept(String),
}

/// Undo `resources` newest first, returning the ones that stay.
async fn undo_all(id: &str, resources: Vec<Resource>) -> Vec<(Resource, String)> {
    let mut remaining = Vec::new();
    for resource in resources.into_iter().rev() {
        match undo(&resource).await {
            Undo::Done => debug!(owner = %id, %resource, "undone"),
            Undo::Kept(why) => remaining.push((resource, why)),
        }
    }
    remaining.reverse();
    remaining
}

async fn undo(resource: &Resource) -> Undo {
    match resource {
        Resource::RuntimeDir { path } => undo_runtime_dir(path).await,
        Resource::MountTarget { path } => undo_mount_target(path).await,
        Resource::Temp { path } => undo_temp(path).await,
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
            return Undo::Kept("a restore leases it".to_string())
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
        // Every process that leases a mount target records it first, so a live one is
        // responsible for it now.
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return Undo::Done,
        Err(error) => return Undo::Kept(error.to_string()),
    };
    let path = path.to_path_buf();
    let removed = tokio::task::spawn_blocking(move || {
        if !crate::storage::runtime_dir::holds_only_empty_dirs(&path)? {
            // A VM's own directory, not a mountpoint a restore made.
            return Ok(());
        }
        crate::storage::runtime_dir::remove_empty_dirs(&path)
    })
    .await;
    match removed {
        Ok(Ok(())) => Undo::Done,
        Ok(Err(error)) if error.kind() == std::io::ErrorKind::NotFound => Undo::Done,
        Ok(Err(error)) => Undo::Kept(error.to_string()),
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
        let remaining = undo_all(&id, record.resources).await;
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
            warn!(owner = %id, pid = record.pid, %resource, reason = %why, "cannot reclaim yet");
        }
        let resources: Vec<Resource> = remaining.into_iter().map(|(r, _)| r).collect();
        if let Err(error) = write_record(dir, &id, &resources) {
            warn!(owner = %id, error = %format!("{error:#}"), "cannot rewrite owner record");
        }
        drop(lock);
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
        assert!(
            runtime_dir.exists() && temp.exists(),
            "the resources are gone before any pass ran"
        );

        assert_eq!(
            reclaim_abandoned_in(&dir).await,
            Reclaimed {
                owners: 1,
                resources: 2,
                remaining: 0
            }
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

    /// A mount target is removed only when it is an unleased tree of empty directories.
    #[tokio::test]
    async fn a_mount_target_is_removed_only_when_it_is_an_empty_unleased_tree() {
        let (root, dir) = test_dir();
        let made = root.path().join("vm-disks/vm-made");
        std::fs::create_dir_all(made.join("disks")).unwrap();
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

        assert_eq!(
            reclaim_abandoned_in(&dir).await,
            Reclaimed {
                owners: 1,
                resources: 3,
                remaining: 0
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
}
