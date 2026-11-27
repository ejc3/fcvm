//! Each VM's own cgroup (issue #1028).
//!
//! cgroup v2 charges a page to the cgroup of the process that instantiated it, and moving
//! a process later does not move what it was charged. So the fcvm process enters its scope
//! first thing in `podman run`, `podman prepare`, `snapshot run` and `snapshot serve`,
//! before anything is spawned or mapped. The VMM, the holder, pasta, the UFFD server and
//! the FUSE servers then start inside it, and guest memory and the memory server's page
//! cache are charged to the scope, not to the session that launched fcvm.
//!
//! Where the scope lives: for root, `fcvm.slice/fcvm-<pid>-<starttime>.scope` at the root
//! of the cgroup2 hierarchy, created by writing cgroupfs directly. A systemd transient
//! scope was the alternative. `systemd-run --scope` starts a new process and cannot place a
//! running one, and placing this process takes a `StartTransientUnit` call on the system
//! bus, a bus client and a running systemd, which the CI containers and nested guests do
//! not all have. The placement also matters for oomd: a root-level `fcvm.slice` is outside the
//! user slice, so a rule that protects the host by killing a user's session scope does not
//! take the VM with it, which is what happened when the VM was charged to the session.
//! A host whose oomd should be able to pick VMs sets `FCVM_CGROUP_PARENT` to a slice it
//! watches. systemd does not manage or remove a cgroup it did not create. A caller that
//! needs a systemd unit after all (properties set through systemd, oomd rules keyed on
//! units) starts fcvm in one, `systemd-run --scope -p MemorySwapMax=0 env
//! FCVM_CGROUP_PARENT=none fcvm ...`: fcvm then runs from its first instruction in the
//! unit's scope, which is all the entry here exists to achieve.
//!
//! `FCVM_CGROUP_PARENT` names the parent as a path below the cgroup2 root, for root and
//! unprivileged callers alike, or `none`, which keeps the process in the cgroup it started
//! in. Unset, the parent is `fcvm.slice` below the root for root, and inside the caller's
//! delegated cgroup otherwise (below). The bench harness sets `none`: it places fcvm in
//! accounting cgroups of its own and audits that fcvm stays there.
//!
//! The root is the root of the caller's cgroup namespace. A caller confined by a cgroup
//! namespace stays inside it, because the kernel resolves `/sys/fs/cgroup` there. A caller
//! confined only by the cgroup it was started in (a sandbox slice) and running as root could
//! leave that cgroup anyway, and fcvm leaves it by default for the reason above. A caller who
//! wants the VM charged inside its slice names that slice in `FCVM_CGROUP_PARENT`.
//!
//! Only a process that can write the parent can enter. An unprivileged caller with
//! `FCVM_CGROUP_PARENT` unset puts its parent, `fcvm.slice`, inside a cgroup delegated to
//! it ([`delegated_parent`]): the kernel lets a process move only within a subtree whose
//! common root it can write, so that is the only place it can go. The launches that find
//! one:
//!
//! - root, or a caller that can write the cgroup2 root (a cgroup namespace whose root was
//!   handed to it), as above;
//! - a system unit run for the user with `Delegate=yes` and `DelegateSubgroup=` (for
//!   instance `systemd-run --uid=<user> --gid=<group> -p Delegate=yes -p
//!   DelegateSubgroup=payload ...`): systemd marks the unit's cgroup delegated
//!   (`user.delegate=1`), hands it to the user, and runs the payload in a leaf, so the
//!   unit's cgroup holds no process and can enable memory for its children;
//! - a subtree root made and handed to the user (`mkdir`, `+memory`, `chown -R`) outside
//!   any systemd manager's tree: nothing manages it, so the outermost cgroup the user can
//!   write above its own is the delegated one.
//!
//! systemd's rule is "Never create your own cgroups below arbitrary cgroups systemd
//! manages, i.e cgroups you haven't set `Delegate=` in" (systemd.io/CGROUP_DELEGATION),
//! so inside a systemd manager's tree only a cgroup it marked delegated is used. The user
//! manager of systemd 255 to 262 marks none of its units, `Delegate=yes` or not:
//! `cgroup_xattr_apply()` in src/core/cgroup.c returns early with
//! `if (!MANAGER_IS_SYSTEM(u->manager)) return;` before `cgroup_delegate_xattr_apply(u)`.
//! So a `systemd-run --user -p Delegate=yes` launch gets no scope there; systemd commit
//! 70a8c5eb ("core: Apply more cgroups for the user manager", after v262) has the user
//! manager mark its delegated units, and such a launch with `DelegateSubgroup=` is found.
//! `user@<uid>.service` itself is delegated to the user manager, not to its units, and is
//! never used. fcvm does not move processes it did not start: a delegated cgroup that holds
//! processes (no `DelegateSubgroup=`) gets no scope.
//!
//! Anything else keeps its own cgroup as before (an SSH session's scope, a system service,
//! a CI runner), and one line says so. `--no-swap` is refused in that case and whenever no
//! scope with a memory controller could be made, because a flag that promises guest memory
//! will not be swapped must not run a VM that can be.
//!
//! The fcvm process keeps its parent-death signals: moving between cgroups does not touch
//! `pdeath_signal`, so a VM dies with the process that launched fcvm wherever the launcher
//! armed one, as the tests and the bench do. A session that is killed no longer kills a VM
//! by cgroup. A foreground fcvm still dies with its terminal (the hangup's SIGHUP, which ends
//! fcvm and through the per-hop parent-death chain every child it started), and a detached
//! one keeps running, as a detached `podman run` does.
//!
//! The scope is not a `reclaim::Resource`. Reclaim records live per data_dir and per euid,
//! and a cgroup is per kernel: an fcvm with another data_dir, or another user's, would
//! never reclaim the scope of a killed one. The scope's name is its owner record instead.
//!
//! Removal: a cgroup can be removed once it holds no process, and a process cannot remove
//! the cgroup it is in. On a normal exit fcvm moves itself back to the cgroup it came from
//! and removes its scope. A scope that is still populated then, or whose owner was killed,
//! is removed by a sweep of the parent: the scopes whose owner, the (pid, start time) in
//! the name, is gone.
//!
//! A start sweeps only when a sweep is due ([`sweep_if_due`]): at most once per interval
//! and parent, from the start that has the turn (`crate::sweep`, the mechanism the owner
//! records and the routed address leases share). A sweep in every start listed every scope
//! under the parent's exclusive lock, so starts ran one at a time and each took longer the
//! more VMs were running (#2178).
//!
//! The parent directory's `flock` orders creation against removal (`crate::sweep::table`).
//! A scope is empty between its `mkdir` and the write that moves its owner in. A start
//! holds the lock SHARED across that window, so starts do not wait for each other; a sweep
//! removes only under the lock held EXCLUSIVELY, so it never meets a scope in that window.
//! Outside the window a scope holds its owner's processes, and the kernel refuses `rmdir`
//! of a populated cgroup. That covers an owner in another pid namespace, whose pid the
//! sweeper cannot resolve.
//!
//! cgroupfs holds no ordinary file, so the sweep's stamp and lock are on the store, in
//! this user's directory under `<data_dir>/cgroup-sweep/` (made as the owner records'
//! directory is), under names keyed by the parent. Two stores whose root launches share
//! one parent (`fcvm.slice`) each keep a stamp and each sweep it on their own interval. A
//! stamp on a disk outlives the boot whose scopes it speaks of: it names that boot, and
//! the first start of the next boot sweeps.
//!
//! The cases, each with a test below or in `crate::sweep`:
//!
//! | case | what happens |
//! |------|---------------|
//! | a sweep finds a scope between its mkdir and its owner's entry | the owner is alive, so the scope is not listed as dead; listed anyway (an owner in another pid namespace), the sweep waits for the exclusive lock and by then the scope is populated |
//! | a sweeper dies | the kernel releases its locks; what it had not removed waits for the next sweep, one interval later |
//! | two starts find a sweep due together | one has the turn |
//! | the owner's pid was reused | the name carries the start time, and the scope of the earlier process is dead |
//! | a scope's own teardown and a sweep remove it together | whichever is second finds it gone |
//! | an fcvm is SIGKILLed | its scope is removed by the first start under that parent that begins an interval or more after the last sweep began; until then it is an empty cgroup |
//! | the lock stays held (shared, by a stream of starts) for a sweep's whole wait | that sweep removes nothing and the next one tries again |
//! | the stamp cannot be read or written | no sweep from this start, not a sweep in every start |

use std::io;
use std::path::{Component, Path, PathBuf};

use anyhow::{bail, Context, Result};
use tracing::{debug, info, warn};

/// Where the cgroup2 hierarchy is mounted.
pub const CGROUP2_ROOT: &str = "/sys/fs/cgroup";
/// Environment variable naming the scopes' parent below the root, or `none`.
pub const PARENT_ENV: &str = "FCVM_CGROUP_PARENT";
/// The default parent.
pub const DEFAULT_PARENT: &str = "fcvm.slice";

/// Where this process's VM scope goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Placement {
    /// `FCVM_CGROUP_PARENT` unset: for root, or a caller that can write the cgroup2 root,
    /// [`DEFAULT_PARENT`] below that root; for anyone else, [`DEFAULT_PARENT`] inside the
    /// cgroup delegated to the caller ([`delegated_parent`]).
    Default,
    /// Under this parent, a path relative to the cgroup2 root.
    Under(PathBuf),
    /// Nowhere: the process stays in the cgroup it started in.
    Stay,
}

impl Placement {
    /// Read the placement from the value of `FCVM_CGROUP_PARENT`.
    pub fn from_env_value(value: Option<&str>) -> Result<Self> {
        let Some(value) = value else {
            return Ok(Self::Default);
        };
        if value == "none" {
            return Ok(Self::Stay);
        }
        let mut parent = PathBuf::new();
        for component in Path::new(value).components() {
            match component {
                Component::RootDir => {}
                Component::Normal(part) => parent.push(part),
                _ => bail!(
                    "{PARENT_ENV}={value:?}: name a cgroup below the cgroup2 root without \
                     '.' or '..', or 'none'"
                ),
            }
        }
        if parent.as_os_str().is_empty() {
            bail!(
                "{PARENT_ENV}={value:?} names the cgroup2 root, which cannot hold a scope; \
                 name a cgroup below it, or 'none'"
            );
        }
        Ok(Self::Under(parent))
    }
}

/// The scope's name: its owner's pid and start time, which together name one process.
pub fn scope_name(pid: u32, start_time: u64) -> String {
    format!("fcvm-{pid}-{start_time}.scope")
}

/// Who a scope in the parent belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScopeOwner {
    /// `fcvm-<pid>-<starttime>.scope`.
    Process { pid: u32, start_time: u64 },
    /// `fcvm-<pid>.scope`, made by the `--no-swap` of earlier releases for the Firecracker
    /// pid after the restore, and never removed. The name holds no start time, and no fcvm
    /// makes one any more, so none is between its creation and its owner's entry: it is
    /// removed whenever it is empty, which the kernel decides.
    Legacy,
}

fn scope_owner(name: &str) -> Option<ScopeOwner> {
    let middle = name.strip_prefix("fcvm-")?.strip_suffix(".scope")?;
    fn number<T: std::str::FromStr>(s: &str) -> Option<T> {
        if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        s.parse().ok()
    }
    match middle.split_once('-') {
        Some((pid, start_time)) => Some(ScopeOwner::Process {
            pid: number(pid)?,
            start_time: number(start_time)?,
        }),
        None => number::<u32>(middle).map(|_| ScopeOwner::Legacy),
    }
}

/// What a sweep did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SweepCount {
    /// Scopes whose owner was gone, removed.
    pub removed: usize,
    /// Scopes whose owner was gone but that still held processes (or children).
    pub populated: usize,
}

/// The scopes in `parent` whose owner is gone. `start_time_of` gives a live process's
/// start time, or `None` when no process has that pid. Reads only: no lock is needed to
/// look.
fn dead_scopes(parent: &Path, start_time_of: &dyn Fn(u32) -> Option<u64>) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(parent) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|entry| {
            let name = entry.file_name();
            match name.to_str().and_then(scope_owner) {
                Some(ScopeOwner::Process { pid, start_time }) => {
                    start_time_of(pid) != Some(start_time)
                }
                // A pid alone cannot tell a reused pid from the owner, and an rmdir of a
                // scope that still holds Firecracker fails, so the kernel is the only check.
                Some(ScopeOwner::Legacy) => true,
                None => false,
            }
        })
        .map(|entry| entry.path())
        .collect()
}

/// Remove `scopes`, each one's owner gone. The caller holds the parent's lock exclusively
/// (`crate::sweep::sweep_table`), so none of them is between its creation and its owner's
/// entry: a start holds the lock shared across that window. A scope that holds a process
/// after all is refused by the kernel.
fn remove_scopes(scopes: &[PathBuf]) -> SweepCount {
    let mut count = SweepCount::default();
    for path in scopes {
        match std::fs::remove_dir(path) {
            Ok(()) => {
                count.removed += 1;
                debug!(scope = %path.display(), "removed the scope of a process that is gone");
            }
            // Its own teardown, or another sweep, removed it first.
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            // EBUSY on cgroupfs (still has processes or child cgroups); ENOTEMPTY elsewhere.
            Err(e) => {
                count.populated += 1;
                debug!(scope = %path.display(), error = %e, "kept the scope of a process that is gone");
            }
        }
    }
    count
}

/// Remove the scopes in `parent` whose owner is gone, when a sweep is due: at most once
/// per interval and parent, by the start that has the turn (`crate::sweep::CGROUP_SCOPES`).
///
/// Every start used to sweep, under the parent's lock held exclusively: a listing of
/// every scope and a /proc read for each, so a start's time under the lock grew with the
/// VMs running and starts ran one at a time. With 2000 clones entering scopes of one
/// parent, 512 at a time, that was 73 starts a second with 15 of 96 cores busy, against
/// 620 or more with the clones in cgroups made beforehand (#2178).
///
/// Looking needs no lock. Removing takes the lock exclusively, and only when something
/// was found, so in the steady state a sweep holds nothing. A sweep that cannot get the
/// lock within `crate::sweep::TABLE_WAIT`, or that dies part way, leaves the rest to the
/// next one. A stamp that cannot be read or written (the store refuses it) means no sweep
/// from this start, not a sweep in every start.
fn sweep_if_due(
    hierarchy: &Hierarchy,
    parent: &Path,
    start_time_of: &dyn Fn(u32) -> Option<u64>,
) -> SweepCount {
    use crate::sweep::TableSweep;
    let swept = hierarchy.stamps.dir().and_then(|stamps| {
        let dir = crate::sweep::open_dir(&stamps)?;
        let key = sweep_key(parent);
        crate::sweep::sweep_table(
            &crate::sweep::Place::keyed(&dir, &stamps, &key),
            hierarchy.sweep_every,
            parent,
            crate::sweep::TABLE_WAIT,
            || dead_scopes(parent, start_time_of),
            |dead| remove_scopes(&dead),
        )
    });
    match swept {
        Ok(TableSweep::Swept(count)) => count,
        Ok(TableSweep::NotDue | TableSweep::NothingFound) => SweepCount::default(),
        Ok(TableSweep::TableBusy) => {
            debug!(parent = %parent.display(), "the scopes' parent stayed locked; the next sweep removes what this one found");
            SweepCount::default()
        }
        Err(error) => {
            debug!(parent = %parent.display(), error = %format!("{error:#}"), "no sweep of the scopes' parent from this start");
            SweepCount::default()
        }
    }
}

/// The key of `parent`'s sweep among the sweeps kept in one directory.
fn sweep_key(parent: &Path) -> String {
    use sha2::Digest;
    let digest = sha2::Sha256::digest(parent.as_os_str().as_encoded_bytes());
    digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Where the sweeps of scopes' parents keep their stamps and locks.
#[derive(Debug, Clone)]
pub enum Stamps {
    /// In this user's directory under `<data_dir>/cgroup-sweep/`, made when first asked
    /// for: an fcvm that enters no scope makes nothing on the store.
    OnTheStore,
    /// In this directory.
    At(PathBuf),
}

impl Stamps {
    fn dir(&self) -> Result<PathBuf> {
        match self {
            Self::OnTheStore => {
                crate::reclaim::owner_dir_under(&crate::paths::data_dir(), "cgroup-sweep")
            }
            Self::At(dir) => Ok(dir.clone()),
        }
    }
}

/// Write one value to an existing cgroup interface file.
fn write_interface(path: &Path, value: &str) -> io::Result<()> {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(path)?;
    file.write_all(value.as_bytes())
}

fn lists_memory(path: &Path) -> bool {
    std::fs::read_to_string(path)
        .map(|s| s.split_whitespace().any(|c| c == "memory"))
        .unwrap_or(false)
}

/// Make the memory controller available to cgroups created in `parent`, enabling it on
/// every level below the root that does not have it. The root's own `subtree_control` is
/// not changed: that is the host's decision, and on a systemd host it already lists memory.
fn enable_memory_below(root: &Path, parent: &Path) -> io::Result<()> {
    if !lists_memory(&root.join("cgroup.subtree_control")) {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!(
                "the memory controller is not enabled in {}/cgroup.subtree_control",
                root.display()
            ),
        ));
    }
    let relative = parent.strip_prefix(root).unwrap_or(parent);
    let mut level = root.to_path_buf();
    for component in relative.components() {
        level.push(component);
        let control = level.join("cgroup.subtree_control");
        if !lists_memory(&control) {
            write_interface(&control, "+memory")?;
        }
    }
    Ok(())
}

/// The cgroup2 hierarchy fcvm writes, and how it makes a cgroup there. Tests point it at a
/// directory tree whose `mkdir` adds the interface files the kernel would.
pub struct Hierarchy {
    pub root: PathBuf,
    pub mkdir: fn(&Path) -> io::Result<()>,
    /// Where the sweeps of the parents keep their stamps and locks ([`sweep_if_due`]).
    pub stamps: Stamps,
    /// How often a start sweeps a parent.
    pub sweep_every: std::time::Duration,
    /// Whether the caller is root, which may write every cgroup.
    pub caller_is_root: bool,
    /// Whether the caller can make a cgroup in this one and move a process into it.
    pub writable: fn(&Path) -> bool,
    /// Whether a service manager marked this cgroup as delegated (`user.delegate` or
    /// `trusted.delegate` set to `1`).
    pub delegated: fn(&Path) -> bool,
}

/// Whether this process can make a cgroup in `level` and write its `cgroup.procs`.
fn writable_by_this_process(level: &Path) -> bool {
    let access = |path: &Path| {
        std::ffi::CString::new(path.as_os_str().as_encoded_bytes())
            // SAFETY: access on a NUL-terminated path.
            .is_ok_and(|path| unsafe { libc::access(path.as_ptr(), libc::W_OK) } == 0)
    };
    access(level) && access(&level.join("cgroup.procs"))
}

/// Whether a service manager marked `level` as delegated. systemd sets both xattrs to `1`
/// on a cgroup it delegates; an unprivileged caller can read only `user.delegate`.
fn marked_delegated(level: &Path) -> bool {
    let Ok(path) = std::ffi::CString::new(level.as_os_str().as_encoded_bytes()) else {
        return false;
    };
    [c"user.delegate", c"trusted.delegate"].iter().any(|name| {
        let mut value = [0u8; 8];
        // SAFETY: getxattr into a buffer of the length passed, on NUL-terminated strings.
        let len = unsafe {
            libc::getxattr(
                path.as_ptr(),
                name.as_ptr(),
                value.as_mut_ptr().cast(),
                value.len(),
            )
        };
        len == 1 && value[0] == b'1'
    })
}

impl Hierarchy {
    /// The host's hierarchy at [`CGROUP2_ROOT`], swept at
    /// `crate::sweep::CGROUP_SCOPES`'s interval, with its stamps on the store.
    pub fn system() -> Self {
        Self {
            root: PathBuf::from(CGROUP2_ROOT),
            mkdir: |path| std::fs::create_dir(path),
            stamps: Stamps::OnTheStore,
            sweep_every: crate::sweep::CGROUP_SCOPES.interval(),
            caller_is_root: nix::unistd::geteuid().is_root(),
            writable: writable_by_this_process,
            delegated: marked_delegated,
        }
    }
}

/// This process's VM scope, entered.
#[derive(Debug)]
pub struct VmScope {
    path: PathBuf,
    origin: Option<PathBuf>,
    pid: u32,
}

impl VmScope {
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Move back to the cgroup this process came from and remove the scope. A scope that
    /// still holds a process, or whose origin is gone, stays for the next sweep.
    pub fn leave(self) {
        let Some(origin) = &self.origin else {
            return;
        };
        if let Err(e) = write_interface(&origin.join("cgroup.procs"), &self.pid.to_string()) {
            debug!(
                origin = %origin.display(), scope = %self.path.display(), error = %e,
                "could not move back to the cgroup fcvm started in; the next sweep removes the scope"
            );
            return;
        }
        match std::fs::remove_dir(&self.path) {
            Ok(()) => debug!(scope = %self.path.display(), "removed the VM's cgroup"),
            Err(e) => debug!(
                scope = %self.path.display(), error = %e,
                "the VM's cgroup still holds a process; the next sweep removes it"
            ),
        }
    }
}

/// Why no scope was entered.
#[derive(Debug)]
pub struct NotEntered {
    pub reason: String,
    /// The caller lacks permission: the expected case for an unprivileged caller.
    pub unprivileged: bool,
}

/// The errors a caller without write access to the hierarchy gets.
fn is_permission_error(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(libc::EACCES | libc::EPERM | libc::EROFS)
    )
}

fn holds_processes(cgroup: &Path) -> bool {
    std::fs::read_to_string(cgroup.join("cgroup.procs"))
        .map(|procs| !procs.trim().is_empty())
        .unwrap_or(false)
}

/// The cgroup an unprivileged caller in `origin` (a path below the root) may make its
/// scopes' parent in, or why there is none.
///
/// The kernel lets a process move only to a cgroup whose `cgroup.procs` it can write, and
/// only when it can also write the `cgroup.procs` of the nearest cgroup that holds both
/// where it is and where it goes. So an unprivileged caller can move only inside a subtree
/// it already runs in, and the search walks up from `origin`:
///
/// - The walk stops below the first cgroup holding an `init.scope`: the root of a systemd
///   manager (`user@<uid>.service` for the user manager). systemd's rule is "Never create
///   your own cgroups below arbitrary cgroups systemd manages, i.e cgroups you haven't set
///   `Delegate=` in" (systemd.io/CGROUP_DELEGATION), and `user@<uid>.service` is delegated
///   to the user manager, not to its units.
/// - The innermost cgroup on the way that a manager marked delegated is the caller's, when
///   the caller can write it.
/// - With no manager root on the way (a subtree made by hand and handed to the user, a
///   container's root without systemd), nothing manages the cgroups, and the outermost one
///   the caller can write, every level from `origin` up to it writable, is the caller's.
///   It must be above `origin`: a scope's parent cannot have a memory controller below a
///   cgroup that holds processes.
///
/// The cgroup found must hold no process unless it already enables memory for its
/// children, for the same reason. A delegated unit gets that from `DelegateSubgroup=`,
/// which puts the payload in a leaf; fcvm does not move processes it did not start.
fn delegated_parent(h: &Hierarchy, origin: Option<&Path>) -> Result<PathBuf, String> {
    let Some(origin) = origin else {
        return Err("this process's cgroup is unknown".to_string());
    };
    let origin = h.root.join(origin.strip_prefix("/").unwrap_or(origin));
    let mut levels = Vec::new();
    let mut manager = None;
    let mut level = origin.clone();
    while level != h.root && level.starts_with(&h.root) {
        if level.join("init.scope").is_dir() {
            manager = Some(level);
            break;
        }
        levels.push(level.clone());
        level.pop();
    }
    let room = |cgroup: &Path| -> Result<PathBuf, String> {
        if holds_processes(cgroup) && !lists_memory(&cgroup.join("cgroup.subtree_control")) {
            return Err(format!(
                "{} holds processes, so no cgroup below it can have a memory controller; \
                 start fcvm in a unit with DelegateSubgroup=, which runs it in a leaf",
                cgroup.display()
            ));
        }
        Ok(cgroup.to_path_buf())
    };
    if let Some(delegated) = levels.iter().find(|level| (h.delegated)(level)) {
        if !(h.writable)(delegated) {
            return Err(format!(
                "{} is delegated, but not to this user",
                delegated.display()
            ));
        }
        return room(delegated);
    }
    if let Some(manager) = manager {
        return Err(format!(
            "no cgroup between {} and the systemd manager's own {} is delegated to this user",
            origin.display(),
            manager.display()
        ));
    }
    let outermost = levels.iter().take_while(|level| (h.writable)(level)).last();
    match outermost {
        Some(outermost) if *outermost != origin => room(outermost),
        _ => Err(format!(
            "no cgroup above {} is writable by this user",
            origin.display()
        )),
    }
}

/// Enter `pid`'s scope under `placement`. `origin` is the cgroup the process is in, as a
/// path below the root. Sweeps the parent first, under its lock.
pub fn enter(
    hierarchy: &Hierarchy,
    placement: &Placement,
    pid: u32,
    start_time: u64,
    origin: Option<&Path>,
    no_swap: bool,
    start_time_of: &dyn Fn(u32) -> Option<u64>,
) -> Result<VmScope, NotEntered> {
    let not_entered = |reason: String, error: Option<&io::Error>| NotEntered {
        reason,
        unprivileged: error.is_some_and(is_permission_error),
    };
    if *placement == Placement::Stay {
        return Err(not_entered(format!("{PARENT_ENV}=none"), None));
    }
    let root = &hierarchy.root;
    if !root.join("cgroup.controllers").exists() {
        return Err(not_entered(
            format!("no cgroup2 hierarchy is mounted at {}", root.display()),
            None,
        ));
    }
    // The cgroup the parent is made in, and the parent's path below it.
    let (base, parent_relative) = match placement {
        Placement::Under(relative) => (root.clone(), relative.clone()),
        // A caller that can write the root (a cgroup namespace whose root was handed to it)
        // places as root does, as it did before delegated parents were looked for.
        Placement::Default if hierarchy.caller_is_root || (hierarchy.writable)(root) => {
            (root.clone(), PathBuf::from(DEFAULT_PARENT))
        }
        Placement::Default => match delegated_parent(hierarchy, origin) {
            Ok(delegated) => (delegated, PathBuf::from(DEFAULT_PARENT)),
            Err(reason) => {
                return Err(NotEntered {
                    reason,
                    unprivileged: true,
                })
            }
        },
        Placement::Stay => unreachable!("returned above"),
    };
    let parent = base.join(&parent_relative);
    let mut level = base;
    for component in parent_relative.components() {
        level.push(component);
        match (hierarchy.mkdir)(&level) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => {
                return Err(not_entered(
                    format!("creating {}: {e}", level.display()),
                    Some(&e),
                ))
            }
        }
    }
    let path_c = std::ffi::CString::new(parent.as_os_str().as_encoded_bytes())
        .map_err(|e| not_entered(format!("{}: {e}", parent.display()), None))?;
    // SAFETY: access on a NUL-terminated path.
    if unsafe { libc::access(path_c.as_ptr(), libc::W_OK) } != 0 {
        let e = io::Error::last_os_error();
        return Err(not_entered(
            format!("{} is not writable: {e}", parent.display()),
            Some(&e),
        ));
    }
    let swept = sweep_if_due(hierarchy, &parent, start_time_of);
    if swept.removed > 0 || swept.populated > 0 {
        info!(
            parent = %parent.display(), removed = swept.removed, still_populated = swept.populated,
            "swept the cgroups of VMs whose fcvm is gone"
        );
    }

    let memory = enable_memory_below(root, &parent).map_err(|e| e.to_string());
    if no_swap {
        if let Err(reason) = &memory {
            return Err(not_entered(
                format!("--no-swap needs the memory controller: {reason}"),
                None,
            ));
        }
    }
    // Shared, from before the scope exists until this process is inside it: a sweep, which
    // removes under the exclusive lock, never meets the scope empty. Starts do not wait
    // for each other.
    let lock = crate::sweep::table::shared(&parent).map_err(|e| {
        let e = io::Error::from(e);
        not_entered(format!("locking {}: {e}", parent.display()), Some(&e))
    })?;
    let path = parent.join(scope_name(pid, start_time));
    (hierarchy.mkdir)(&path)
        .map_err(|e| not_entered(format!("creating {}: {e}", path.display()), Some(&e)))?;
    let configured = (|| -> Result<(), NotEntered> {
        if no_swap {
            let swap_max = path.join("memory.swap.max");
            write_interface(&swap_max, "0").map_err(|e| {
                not_entered(
                    format!("writing 0 to {}: {e}", swap_max.display()),
                    Some(&e),
                )
            })?;
        }
        let procs = path.join("cgroup.procs");
        write_interface(&procs, &pid.to_string()).map_err(|e| {
            not_entered(
                format!("writing {pid} to {}: {e}", procs.display()),
                Some(&e),
            )
        })
    })();
    if let Err(e) = configured {
        let _ = std::fs::remove_dir(&path);
        return Err(e);
    }
    drop(lock);

    if let Err(reason) = &memory {
        info!(
            scope = %path.display(), %reason,
            "entered the VM's cgroup without a memory controller of its own; its memory is \
             charged to the nearest ancestor that has one"
        );
    } else {
        info!(scope = %path.display(), no_swap, "entered the VM's cgroup");
    }
    Ok(VmScope {
        path,
        origin: origin.map(|o| root.join(o.strip_prefix("/").unwrap_or(o))),
        pid,
    })
}

/// Whether `cmd` runs in a VM scope, and with what `--no-swap`: `podman run`, `podman
/// prepare`, `snapshot run` and `snapshot serve`, the commands that start a VMM or a memory
/// server. Other commands stay where they are.
pub fn scope_request(cmd: &crate::cli::Commands) -> Option<bool> {
    use crate::cli::{Commands, PodmanCommands, SnapshotCommands, VmCommands};
    match cmd {
        Commands::Podman(args) => Some(match &args.cmd {
            PodmanCommands::Run(run) => run.no_swap,
            PodmanCommands::Prepare(prepare) => prepare.run.no_swap,
        }),
        Commands::Snapshot(args) => match &args.cmd {
            SnapshotCommands::Serve(serve) => Some(serve.no_swap),
            SnapshotCommands::Run(run) => Some(run.no_swap),
            SnapshotCommands::Create(_)
            | SnapshotCommands::Ls
            | SnapshotCommands::Export(_)
            | SnapshotCommands::Import(_) => None,
        },
        Commands::Vm(args) => match &args.cmd {
            VmCommands::Run(_) => Some(false),
            VmCommands::NsHelper(_) => None,
        },
        _ => None,
    }
}

/// The cgroup2 path of this process, from `/proc/self/cgroup` (`0::<path>`).
fn own_cgroup() -> Option<PathBuf> {
    let text = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    text.lines()
        .find_map(|line| line.strip_prefix("0::"))
        .map(PathBuf::from)
}

/// Enter this process's VM scope, first thing in a command that starts a VM or a memory
/// server. `Ok(None)` when no scope could be entered and none is required; an error when
/// `--no-swap` was asked for and cannot be honoured, or `FCVM_CGROUP_PARENT` is invalid.
pub fn enter_this_process(no_swap: bool) -> Result<Option<VmScope>> {
    let value = std::env::var(PARENT_ENV).ok();
    let placement = Placement::from_env_value(value.as_deref())?;
    let pid = std::process::id();
    let start_time = crate::utils::process_start_time(pid)
        .context("reading this process's start time from /proc/self/stat")?;
    let origin = own_cgroup();
    match enter(
        &Hierarchy::system(),
        &placement,
        pid,
        start_time,
        origin.as_deref(),
        no_swap,
        &crate::utils::process_start_time,
    ) {
        Ok(scope) => Ok(Some(scope)),
        Err(not) if no_swap => bail!(
            "--no-swap needs the VM's own cgroup with memory.swap.max set to 0, and none \
             could be made ({}). Run fcvm as root on a cgroup2 host, with {PARENT_ENV} unset \
             or naming a cgroup where the memory controller can be enabled, or unprivileged \
             in a cgroup delegated to its user, with {PARENT_ENV} unset",
            not.reason
        ),
        Err(not) if not.unprivileged => {
            info!(
                reason = %not.reason,
                "no cgroup of the VM's own (needs root, or a cgroup delegated to this user); \
                 the VM's memory is charged to the cgroup fcvm was started in"
            );
            Ok(None)
        }
        Err(not) if placement == Placement::Stay => {
            debug!(reason = %not.reason, "staying in the cgroup fcvm was started in");
            Ok(None)
        }
        Err(not) => {
            warn!(
                reason = %not.reason,
                "no cgroup of the VM's own; the VM's memory is charged to the cgroup fcvm was \
                 started in"
            );
            Ok(None)
        }
    }
}

/// Refuse `--no-swap` unless this process already runs in a cgroup whose
/// `memory.swap.max` is 0. The fcvm binary enters such a scope before the command starts
/// ([`enter_this_process`]); a library caller of a command that starts a VM or a memory
/// server with `no_swap` set must do the same, or the VM would start swappable.
pub fn require_swap_disabled(no_swap: bool) -> Result<()> {
    if !no_swap {
        return Ok(());
    }
    let proc_cgroup = std::fs::read_to_string("/proc/self/cgroup").unwrap_or_default();
    swap_disabled_in(Path::new(CGROUP2_ROOT), &proc_cgroup)
}

fn swap_disabled_in(root: &Path, proc_cgroup: &str) -> Result<()> {
    let Some(own) = proc_cgroup
        .lines()
        .find_map(|line| line.strip_prefix("0::"))
    else {
        bail!("--no-swap: this process is in no cgroup2 cgroup");
    };
    let swap_max = root
        .join(own.trim_start_matches('/'))
        .join("memory.swap.max");
    match std::fs::read_to_string(&swap_max) {
        Ok(value) if value.trim() == "0" => Ok(()),
        Ok(value) => bail!(
            "--no-swap: {} is {}, not 0. The fcvm binary enters a cgroup with it set to 0 \
             before the command starts (fcvm::cgroup::enter_this_process); a library caller \
             must too",
            swap_max.display(),
            value.trim()
        ),
        Err(e) => bail!("--no-swap: reading {}: {e}", swap_max.display()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sweep::Sweep;

    /// A `mkdir` that adds the interface files cgroupfs gives a new cgroup. Controller files
    /// appear only when the parent enables the controller, as in the kernel. A parent marked
    /// read-only ([`readonly`]) refuses, as one owned by another user does.
    fn fake_mkdir(path: &Path) -> io::Result<()> {
        if !fake_writable(path.parent().unwrap()) {
            return Err(io::Error::from_raw_os_error(libc::EACCES));
        }
        std::fs::create_dir(path)?;
        for file in [
            "cgroup.procs",
            "cgroup.subtree_control",
            "cgroup.controllers",
        ] {
            std::fs::write(path.join(file), "")?;
        }
        if fake_enabled(&path.parent().unwrap().join("cgroup.subtree_control")) {
            std::fs::write(path.join("memory.swap.max"), "max\n")?;
            std::fs::write(path.join("memory.current"), "0\n")?;
        }
        Ok(())
    }

    /// The kernel lists `memory` in `cgroup.subtree_control` after `+memory` is written; a
    /// plain file holds what was written.
    fn fake_enabled(path: &Path) -> bool {
        std::fs::read_to_string(path).is_ok_and(|s| s.contains("memory"))
    }

    fn fake_root(memory: bool) -> (tempfile::TempDir, Hierarchy) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("cgroup.controllers"), "cpu memory pids\n").unwrap();
        let control = if memory {
            "cpu memory pids\n"
        } else {
            "cpu pids\n"
        };
        std::fs::write(dir.path().join("cgroup.subtree_control"), control).unwrap();
        std::fs::write(dir.path().join("cgroup.procs"), "").unwrap();
        let stamps = dir.path().join("stamps");
        std::fs::create_dir(&stamps).unwrap();
        let hierarchy = Hierarchy {
            root: dir.path().to_path_buf(),
            mkdir: fake_mkdir,
            stamps: Stamps::At(stamps),
            sweep_every: crate::sweep::CGROUP_SCOPES.every,
            caller_is_root: true,
            writable: fake_writable,
            delegated: fake_delegated,
        };
        (dir, hierarchy)
    }

    /// A cgroup the caller cannot write: another user's, or root's for an unprivileged caller.
    fn readonly(cgroup: &Path) {
        std::fs::write(cgroup.join(".fake-readonly"), "").unwrap();
    }

    fn fake_writable(cgroup: &Path) -> bool {
        !cgroup.join(".fake-readonly").exists()
    }

    /// A cgroup a service manager marked as delegated (`user.delegate=1`).
    fn delegate(cgroup: &Path) {
        std::fs::write(cgroup.join(".fake-delegated"), "").unwrap();
    }

    fn fake_delegated(cgroup: &Path) -> bool {
        cgroup.join(".fake-delegated").exists()
    }

    /// The cgroup `relative` below the fake root, each missing level made as the kernel
    /// makes one, whatever the caller may write.
    fn cgroup(h: &Hierarchy, relative: &str) -> PathBuf {
        let mut level = h.root.clone();
        for part in Path::new(relative).components() {
            level.push(part);
            if !level.exists() {
                std::fs::create_dir(&level).unwrap();
                for file in [
                    "cgroup.procs",
                    "cgroup.subtree_control",
                    "cgroup.controllers",
                ] {
                    std::fs::write(level.join(file), "").unwrap();
                }
            }
        }
        level
    }

    fn enable_memory(cgroup: &Path) {
        std::fs::write(cgroup.join("cgroup.subtree_control"), "memory\n").unwrap();
    }

    /// The systemd user manager of uid 1000 in [`systemd_host`].
    const USER_MANAGER: &str = "user.slice/user-1000.slice/user@1000.service";

    /// A systemd host as an unprivileged caller (uid 1000) sees it: the root and the slices
    /// are root's; `user@1000.service` is the user's, marked delegated by the system manager
    /// and holding the user manager in its `init.scope`; memory is enabled down to it.
    fn systemd_host() -> (tempfile::TempDir, Hierarchy) {
        let (dir, mut h) = fake_root(true);
        h.caller_is_root = false;
        cgroup(&h, "init.scope");
        for slice in ["system.slice", "user.slice", "user.slice/user-1000.slice"] {
            let slice = cgroup(&h, slice);
            enable_memory(&slice);
            readonly(&slice);
        }
        let manager = cgroup(&h, USER_MANAGER);
        enable_memory(&manager);
        delegate(&manager);
        cgroup(&h, &format!("{USER_MANAGER}/init.scope"));
        enable_memory(&cgroup(&h, &format!("{USER_MANAGER}/app.slice")));
        readonly(&h.root);
        (dir, h)
    }

    /// A system unit run for the user with `Delegate=yes` and `DelegateSubgroup=payload`
    /// (`systemd-run --uid=1000 -p Delegate=yes -p DelegateSubgroup=payload`): the unit's
    /// cgroup is the user's, marked delegated, and holds no process; the payload runs in its
    /// leaf. An unprivileged fcvm started there gets a scope with a memory controller inside
    /// the unit, and creates nothing outside it.
    #[test]
    fn an_unprivileged_start_in_a_delegated_unit_gets_a_scope_with_memory() {
        let (_dir, h) = systemd_host();
        let unit = cgroup(&h, "system.slice/plan.service");
        delegate(&unit);
        let payload = cgroup(&h, "system.slice/plan.service/payload");
        std::fs::write(payload.join("cgroup.procs"), "100\n4242\n").unwrap();
        let scope = enter(
            &h,
            &default_placement(),
            4242,
            77,
            Some(Path::new("/system.slice/plan.service/payload")),
            false,
            &nobody,
        )
        .unwrap();
        assert_eq!(scope.path(), unit.join("fcvm.slice/fcvm-4242-77.scope"));
        assert!(fake_enabled(&unit.join("cgroup.subtree_control")));
        assert!(fake_enabled(
            &unit.join("fcvm.slice/cgroup.subtree_control")
        ));
        assert!(scope.path().join("memory.swap.max").exists());
        assert!(!h.root.join("fcvm.slice").exists());
        assert_eq!(scope.origin.as_deref(), Some(payload.as_path()));
    }

    /// Every directory below the fake root named `fcvm.slice`: where a start made a parent.
    fn parents_made(h: &Hierarchy) -> Vec<PathBuf> {
        fn walk(dir: &Path, found: &mut Vec<PathBuf>) {
            for entry in std::fs::read_dir(dir).unwrap().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    if entry.file_name() == DEFAULT_PARENT {
                        found.push(path.clone());
                    }
                    walk(&path, found);
                }
            }
        }
        let mut found = Vec::new();
        walk(&h.root, &mut found);
        found
    }

    /// Enter as pid 4242 from `origin` with the default placement, and expect no scope:
    /// the unprivileged case, with nothing made anywhere.
    fn refused(h: &Hierarchy, origin: &str, no_swap: bool) -> String {
        let error = enter(
            h,
            &default_placement(),
            4242,
            77,
            Some(Path::new(origin)),
            no_swap,
            &nobody,
        )
        .unwrap_err();
        assert!(error.unprivileged, "{}", error.reason);
        assert_eq!(parents_made(h), Vec::<PathBuf>::new(), "{}", error.reason);
        error.reason
    }

    /// A user manager unit with `Delegate=yes` and `DelegateSubgroup=`, on a systemd whose
    /// user manager marks delegated units (after systemd commit 70a8c5eb, "core: Apply more
    /// cgroups for the user manager"): its scopes go inside the unit, not beside it.
    #[test]
    fn a_marked_unit_of_the_user_manager_holds_the_scopes() {
        let (_dir, h) = systemd_host();
        let unit = cgroup(&h, &format!("{USER_MANAGER}/app.slice/run-u5.service"));
        delegate(&unit);
        let payload = cgroup(
            &h,
            &format!("{USER_MANAGER}/app.slice/run-u5.service/payload"),
        );
        let origin = format!("/{USER_MANAGER}/app.slice/run-u5.service/payload");
        let scope = enter(
            &h,
            &default_placement(),
            4242,
            77,
            Some(Path::new(&origin)),
            false,
            &nobody,
        )
        .unwrap();
        assert_eq!(scope.path(), unit.join("fcvm.slice/fcvm-4242-77.scope"));
        assert!(fake_enabled(
            &unit.join("fcvm.slice/cgroup.subtree_control")
        ));
        assert_eq!(scope.origin.as_deref(), Some(payload.as_path()));
        assert_eq!(parents_made(&h), [unit.join("fcvm.slice")]);
    }

    /// systemd 255 to 262 mark no unit of the user manager as delegated, `Delegate=yes` or
    /// not (`cgroup_xattr_apply()` returns for a user manager before it sets the xattrs),
    /// so fcvm cannot tell a delegated one from one it must not write. The only mark on the
    /// way is the user manager's own `user@<uid>.service`, which is delegated to that
    /// manager: no scope, and nothing made beside `app.slice`.
    #[test]
    fn an_unmarked_unit_of_the_user_manager_gets_no_scope() {
        let (_dir, h) = systemd_host();
        let unit = cgroup(&h, &format!("{USER_MANAGER}/app.slice/plan.scope"));
        std::fs::write(unit.join("cgroup.procs"), "100\n4242\n").unwrap();
        let reason = refused(&h, &format!("/{USER_MANAGER}/app.slice/plan.scope"), false);
        assert!(reason.contains("systemd manager"), "{reason}");
    }

    /// A delegated unit without `DelegateSubgroup=` runs its payload in the unit's own
    /// cgroup, where the kernel refuses a memory controller for children. fcvm does not move
    /// the processes it did not start; it enters no scope and names the fix.
    #[test]
    fn a_delegated_unit_that_holds_processes_gets_no_scope() {
        let (_dir, h) = systemd_host();
        let unit = cgroup(&h, "system.slice/plan.service");
        delegate(&unit);
        std::fs::write(unit.join("cgroup.procs"), "100\n4242\n").unwrap();
        let reason = refused(&h, "/system.slice/plan.service", false);
        assert!(reason.contains("DelegateSubgroup="), "{reason}");
    }

    /// A unit marked delegated whose cgroup the caller cannot write (a scope run as the
    /// user by `systemd-run --uid= --scope`, whose cgroup stays root's) is not the caller's.
    #[test]
    fn a_delegated_unit_of_another_user_gets_no_scope() {
        let (_dir, h) = systemd_host();
        let unit = cgroup(&h, "system.slice/plan.scope");
        delegate(&unit);
        readonly(&unit);
        let reason = refused(&h, "/system.slice/plan.scope", false);
        assert!(reason.contains("not to this user"), "{reason}");
    }

    /// An SSH login's session scope is made for logind by the system manager and is root's:
    /// today's behaviour, no scope, with or without `--no-swap`.
    #[test]
    fn a_login_session_gets_no_scope() {
        let (_dir, h) = systemd_host();
        let session = cgroup(&h, "user.slice/user-1000.slice/session-3.scope");
        readonly(&session);
        refused(&h, "/user.slice/user-1000.slice/session-3.scope", false);
        refused(&h, "/user.slice/user-1000.slice/session-3.scope", true);
    }

    /// A subtree root made and handed to the user (`mkdir`, `+memory`, `chown -R`), outside
    /// any systemd manager: nothing manages it, so the outermost cgroup the user can write
    /// holds the parent, and the user's shell can stay in a sibling leaf.
    #[test]
    fn a_subtree_handed_to_the_user_holds_the_scopes() {
        let (_dir, h) = systemd_host();
        let handed = cgroup(&h, "sr");
        enable_memory(&handed);
        let shell = cgroup(&h, "sr/h");
        std::fs::write(shell.join("cgroup.procs"), "100\n4242\n").unwrap();
        let scope = enter(
            &h,
            &default_placement(),
            4242,
            77,
            Some(Path::new("/sr/h")),
            true,
            &nobody,
        )
        .unwrap();
        assert_eq!(scope.path(), handed.join("fcvm.slice/fcvm-4242-77.scope"));
        assert_eq!(
            std::fs::read_to_string(scope.path().join("memory.swap.max")).unwrap(),
            "0"
        );
    }

    /// A user who can write only its own cgroup, not the one above it, has nowhere to put a
    /// parent: one below its own cgroup could not have a memory controller.
    #[test]
    fn a_writable_cgroup_of_its_own_alone_gets_no_scope() {
        let (_dir, h) = systemd_host();
        readonly(&cgroup(&h, "ws"));
        cgroup(&h, "ws/h");
        refused(&h, "/ws/h", false);
    }

    /// Root keeps `fcvm.slice` at the cgroup2 root wherever it starts, a delegated unit
    /// included, and an explicit parent keeps naming a cgroup below the root for everyone.
    #[test]
    fn root_and_an_explicit_parent_place_as_before() {
        let (_dir, h) = fake_root(true);
        let unit = cgroup(&h, "system.slice/plan.service");
        delegate(&unit);
        cgroup(&h, "system.slice/plan.service/payload");
        let scope = enter(
            &h,
            &default_placement(),
            4242,
            77,
            Some(Path::new("/system.slice/plan.service/payload")),
            false,
            &nobody,
        )
        .unwrap();
        assert_eq!(scope.path(), h.root.join("fcvm.slice/fcvm-4242-77.scope"));

        let (_dir, h) = systemd_host();
        let unit = cgroup(&h, "system.slice/plan.service");
        delegate(&unit);
        cgroup(&h, "system.slice/plan.service/payload");
        let error = enter(
            &h,
            &Placement::from_env_value(Some("vms.slice")).unwrap(),
            4242,
            77,
            Some(Path::new("/system.slice/plan.service/payload")),
            false,
            &nobody,
        )
        .unwrap_err();
        assert!(error.unprivileged, "{}", error.reason);
        assert!(!h.root.join("vms.slice").exists());
        assert_eq!(parents_made(&h), Vec::<PathBuf>::new());
    }

    /// A caller that is not root but can write the cgroup2 root (a cgroup namespace whose
    /// root was delegated to it, mounted at `/sys/fs/cgroup`) places as before delegated
    /// parents were looked for: `fcvm.slice` at that root, not nothing.
    #[test]
    fn a_writable_namespace_root_places_as_before() {
        let (_dir, mut h) = fake_root(true);
        h.caller_is_root = false;
        delegate(&h.root);
        cgroup(&h, "payload");
        let scope = enter(
            &h,
            &default_placement(),
            4242,
            77,
            Some(Path::new("/payload")),
            true,
            &nobody,
        )
        .unwrap();
        assert_eq!(scope.path(), h.root.join("fcvm.slice/fcvm-4242-77.scope"));
        assert_eq!(
            std::fs::read_to_string(scope.path().join("memory.swap.max")).unwrap(),
            "0"
        );
    }

    /// A subtree outside any manager whose middle level the user cannot write
    /// (`/delegated` and `/delegated/branch/payload` the user's, `branch` root's): unmarked,
    /// fcvm does what it did before, no scope and nothing made; marked delegated, the scope
    /// goes in the marked cgroup, where the kernel lets the user move (it can write the
    /// common ancestor, `/delegated`).
    #[test]
    fn a_root_owned_level_between_two_of_the_users() {
        let (_dir, h) = systemd_host();
        let delegated = cgroup(&h, "delegated");
        enable_memory(&delegated);
        let branch = cgroup(&h, "delegated/branch");
        enable_memory(&branch);
        readonly(&branch);
        cgroup(&h, "delegated/branch/payload");
        refused(&h, "/delegated/branch/payload", false);

        delegate(&delegated);
        let scope = enter(
            &h,
            &default_placement(),
            4242,
            77,
            Some(Path::new("/delegated/branch/payload")),
            false,
            &nobody,
        )
        .unwrap();
        assert_eq!(
            scope.path(),
            delegated.join("fcvm.slice/fcvm-4242-77.scope")
        );
    }

    /// `--no-swap` in a delegated unit: the user's scope gets `memory.swap.max` 0. Leaving
    /// moves fcvm back to the payload's leaf and removes the scope.
    #[test]
    fn no_swap_and_leaving_in_a_delegated_unit() {
        let (_dir, h) = systemd_host();
        let unit = cgroup(&h, "system.slice/plan.service");
        delegate(&unit);
        let payload = cgroup(&h, "system.slice/plan.service/payload");
        let scope = enter(
            &h,
            &default_placement(),
            4242,
            77,
            Some(Path::new("/system.slice/plan.service/payload")),
            true,
            &nobody,
        )
        .unwrap();
        let path = scope.path().to_path_buf();
        assert_eq!(
            std::fs::read_to_string(path.join("memory.swap.max")).unwrap(),
            "0"
        );
        for file in std::fs::read_dir(&path).unwrap() {
            std::fs::remove_file(file.unwrap().path()).unwrap();
        }
        scope.leave();
        assert!(!path.exists());
        assert_eq!(
            std::fs::read_to_string(payload.join("cgroup.procs")).unwrap(),
            "4242"
        );
    }

    /// The sweep runs under the parent found in the delegated unit: a dead owner's scope
    /// there goes, a live one stays.
    #[test]
    fn the_sweep_covers_the_parent_in_a_delegated_unit() {
        let (_dir, h) = systemd_host();
        let unit = cgroup(&h, "system.slice/plan.service");
        delegate(&unit);
        cgroup(&h, "system.slice/plan.service/payload");
        let parent = unit.join(DEFAULT_PARENT);
        std::fs::create_dir(&parent).unwrap();
        enable_memory(&parent);
        std::fs::create_dir(parent.join("fcvm-200-9.scope")).unwrap();
        std::fs::create_dir(parent.join("fcvm-50001-7.scope")).unwrap();
        enter(
            &h,
            &default_placement(),
            50_000,
            7,
            Some(Path::new("/system.slice/plan.service/payload")),
            false,
            &alive_from_50000,
        )
        .unwrap();
        assert!(!parent.join("fcvm-200-9.scope").exists());
        assert!(parent.join("fcvm-50001-7.scope").exists());
        assert!(parent.join("fcvm-50000-7.scope").exists());
    }

    fn default_placement() -> Placement {
        Placement::from_env_value(None).unwrap()
    }

    fn nobody(_: u32) -> Option<u64> {
        None
    }

    #[test]
    fn the_scope_is_named_by_pid_and_start_time() {
        assert_eq!(scope_name(4242, 987654), "fcvm-4242-987654.scope");
        assert_eq!(
            scope_owner("fcvm-4242-987654.scope"),
            Some(ScopeOwner::Process {
                pid: 4242,
                start_time: 987654
            })
        );
        assert_eq!(scope_owner("fcvm-4242.scope"), Some(ScopeOwner::Legacy));
        for other in [
            "fcvm.slice",
            "fcvm-.scope",
            "fcvm-1-.scope",
            "fcvm--1.scope",
            "fcvm-1-2-3.scope",
            "fcvm-x-1.scope",
            "fcvm-1-2.scope.bak",
            "session-5.scope",
        ] {
            assert_eq!(scope_owner(other), None, "{other}");
        }
    }

    #[test]
    fn the_parent_comes_from_fcvm_cgroup_parent() {
        assert_eq!(default_placement(), Placement::Default);
        assert_eq!(
            Placement::from_env_value(Some("none")).unwrap(),
            Placement::Stay
        );
        assert_eq!(
            Placement::from_env_value(Some("/system.slice/vms.slice")).unwrap(),
            Placement::Under(PathBuf::from("system.slice/vms.slice"))
        );
        for bad in ["", "/", "../x", "a/../b", "./x"] {
            assert!(Placement::from_env_value(Some(bad)).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn entry_creates_the_scope_moves_the_process_and_records_where_it_came_from() {
        let (_dir, h) = fake_root(true);
        let scope = enter(
            &h,
            &default_placement(),
            4242,
            77,
            Some(Path::new("/user.slice/session-1.scope")),
            false,
            &nobody,
        )
        .unwrap();
        let expected = h.root.join("fcvm.slice/fcvm-4242-77.scope");
        assert_eq!(scope.path(), expected);
        assert_eq!(
            std::fs::read_to_string(expected.join("cgroup.procs")).unwrap(),
            "4242"
        );
        // The memory controller is enabled for the scope, and swap is left alone.
        assert!(fake_enabled(
            &h.root.join("fcvm.slice/cgroup.subtree_control")
        ));
        assert_eq!(
            std::fs::read_to_string(expected.join("memory.swap.max")).unwrap(),
            "max\n"
        );
        assert_eq!(
            scope.origin.as_deref(),
            Some(h.root.join("user.slice/session-1.scope").as_path())
        );
    }

    #[test]
    fn no_swap_sets_swap_max_to_zero_before_the_process_enters() {
        let (_dir, h) = fake_root(true);
        let scope = enter(&h, &default_placement(), 4242, 77, None, true, &nobody).unwrap();
        assert_eq!(
            std::fs::read_to_string(scope.path().join("memory.swap.max")).unwrap(),
            "0"
        );
    }

    /// A command asked for `--no-swap` runs only inside a cgroup with swap off, so a library
    /// caller that never entered a scope is refused instead of starting a swappable VM.
    #[test]
    fn no_swap_is_refused_outside_a_cgroup_with_swap_off() {
        let root = tempfile::tempdir().unwrap();
        let scope = root.path().join("fcvm.slice/fcvm-1-2.scope");
        std::fs::create_dir_all(&scope).unwrap();
        let proc_cgroup = "0::/fcvm.slice/fcvm-1-2.scope\n";
        std::fs::write(scope.join("memory.swap.max"), "0\n").unwrap();
        swap_disabled_in(root.path(), proc_cgroup).expect("swap off");
        std::fs::write(scope.join("memory.swap.max"), "max\n").unwrap();
        assert!(swap_disabled_in(root.path(), proc_cgroup).is_err());
        assert!(swap_disabled_in(root.path(), "0::/user.slice\n").is_err());
        assert!(swap_disabled_in(root.path(), "").is_err());
        require_swap_disabled(false).expect("nothing to check without --no-swap");

        // Every command that starts a VM or a memory server checks it first.
        let code = |source: &str, function: &str| -> bool {
            let body = &source[source.find(function).unwrap()..];
            let body = &body[..body.find("\n}\n").unwrap()];
            body.contains("crate::cgroup::require_swap_disabled(")
        };
        let podman = include_str!("commands/podman/mod.rs");
        let snapshot = include_str!("commands/snapshot.rs");
        // podman run, podman prepare and the library's start_vm and prepare_vm.
        assert!(code(podman, "async fn prepare_vm_for_lifecycle("));
        assert!(code(snapshot, "pub async fn cmd_snapshot_run("));
        assert!(code(snapshot, "async fn cmd_snapshot_serve("));
    }

    #[test]
    fn no_swap_without_a_memory_controller_is_refused_and_leaves_no_scope() {
        let (_dir, h) = fake_root(false);
        let error = enter(&h, &default_placement(), 4242, 77, None, true, &nobody).unwrap_err();
        assert!(
            error.reason.contains("memory controller"),
            "{}",
            error.reason
        );
        assert!(!h.root.join("fcvm.slice/fcvm-4242-77.scope").exists());
        // Without --no-swap the scope is entered all the same.
        enter(&h, &default_placement(), 4242, 77, None, false, &nobody).unwrap();
    }

    #[test]
    fn staying_or_a_host_without_cgroup2_enters_nothing() {
        let (_dir, h) = fake_root(true);
        let error = enter(&h, &Placement::Stay, 1, 1, None, false, &nobody).unwrap_err();
        assert!(!error.unprivileged);
        assert!(!h.root.join("fcvm.slice").exists());

        std::fs::remove_file(h.root.join("cgroup.controllers")).unwrap();
        let error = enter(&h, &default_placement(), 1, 1, None, false, &nobody).unwrap_err();
        assert!(error.reason.contains("no cgroup2"), "{}", error.reason);
    }

    #[test]
    fn a_custom_parent_gets_the_memory_controller_on_every_level() {
        let (_dir, h) = fake_root(true);
        let placement = Placement::from_env_value(Some("vms.slice/batch.slice")).unwrap();
        let scope = enter(&h, &placement, 9, 3, None, true, &nobody).unwrap();
        assert_eq!(
            scope.path(),
            h.root.join("vms.slice/batch.slice/fcvm-9-3.scope")
        );
        assert!(fake_enabled(
            &h.root.join("vms.slice/cgroup.subtree_control")
        ));
        assert!(fake_enabled(
            &h.root.join("vms.slice/batch.slice/cgroup.subtree_control")
        ));
    }

    /// The sweep removes a scope only when no process has its owner's (pid, start time):
    /// a live owner keeps it, and a reused pid with another start time does not.
    #[test]
    fn the_sweep_removes_scopes_whose_owner_is_gone_and_keeps_live_ones() {
        let (_dir, h) = fake_root(true);
        let parent = h.root.join("fcvm.slice");
        std::fs::create_dir(&parent).unwrap();
        for name in [
            "fcvm-100-5.scope", // live: pid 100 started at 5
            "fcvm-100-4.scope", // pid 100 reused: an earlier process with that pid
            "fcvm-200-9.scope", // pid 200 gone
            "fcvm-300-1.scope", // pid 300 gone, but the cgroup still has a process
            "fcvm-400.scope",   // an earlier release's --no-swap scope, empty
            "fcvm-100.scope",   // an earlier release's scope, empty, its pid now reused
            "fcvm-500.scope",   // an earlier release's scope that still holds Firecracker
            "other.scope",      // not ours
        ] {
            std::fs::create_dir(parent.join(name)).unwrap();
        }
        // A non-empty directory stands for a populated cgroup: rmdir fails on both.
        std::fs::write(parent.join("fcvm-300-1.scope/held"), "").unwrap();
        std::fs::write(parent.join("fcvm-500.scope/held"), "").unwrap();
        let start_time_of = |pid: u32| (pid == 100).then_some(5);

        let count = remove_scopes(&dead_scopes(&parent, &start_time_of));
        assert_eq!(
            count,
            SweepCount {
                removed: 4,
                populated: 2
            }
        );
        let mut left: Vec<String> = std::fs::read_dir(&parent)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        assert_eq!(
            left,
            [
                "fcvm-100-5.scope",
                "fcvm-300-1.scope",
                "fcvm-500.scope",
                "other.scope"
            ]
        );
    }

    #[test]
    fn entry_sweeps_before_it_creates_its_own_scope() {
        let (_dir, h) = fake_root(true);
        let parent = h.root.join("fcvm.slice");
        std::fs::create_dir(&parent).unwrap();
        std::fs::write(parent.join("cgroup.subtree_control"), "memory\n").unwrap();
        std::fs::create_dir(parent.join("fcvm-200-9.scope")).unwrap();
        enter(&h, &default_placement(), 4242, 77, None, false, &nobody).unwrap();
        assert!(!parent.join("fcvm-200-9.scope").exists());
        assert!(parent.join("fcvm-4242-77.scope").exists());
    }

    /// A copy of `h` for another thread.
    fn same(h: &Hierarchy) -> Hierarchy {
        Hierarchy {
            root: h.root.clone(),
            mkdir: h.mkdir,
            stamps: h.stamps.clone(),
            sweep_every: h.sweep_every,
            caller_is_root: h.caller_is_root,
            writable: h.writable,
            delegated: h.delegated,
        }
    }

    fn slice(h: &Hierarchy) -> PathBuf {
        let parent = h.root.join("fcvm.slice");
        std::fs::create_dir(&parent).unwrap();
        std::fs::write(parent.join("cgroup.subtree_control"), "memory\n").unwrap();
        parent
    }

    /// Two sweeps of the same dead scopes, and a teardown racing a sweep: whichever comes
    /// second finds the scope gone and counts nothing.
    #[test]
    fn a_scope_removed_twice_is_removed_once() {
        let (_dir, h) = fake_root(true);
        let parent = slice(&h);
        std::fs::create_dir(parent.join("fcvm-200-9.scope")).unwrap();
        let dead = dead_scopes(&parent, &nobody);
        assert_eq!(remove_scopes(&dead).removed, 1);
        assert_eq!(remove_scopes(&dead), SweepCount::default());
    }

    /// A stamp that cannot be written means no sweep from that start, not a sweep in
    /// every start: a sweep per start is what ran starts one at a time.
    #[test]
    fn a_start_that_cannot_write_the_stamp_does_not_sweep() {
        let (_dir, mut h) = fake_root(true);
        h.stamps = Stamps::At(h.root.join("no-such-directory"));
        let parent = slice(&h);
        std::fs::create_dir(parent.join("fcvm-200-9.scope")).unwrap();
        enter(&h, &default_placement(), 4242, 77, None, false, &nobody).unwrap();
        assert!(parent.join("fcvm-200-9.scope").exists());
    }

    /// The scopes' parent as a caller of the sweep (`crate::sweep::testing`): a parent in
    /// a fake hierarchy, scopes of dead and of live owners, and what an entry does.
    struct ScopesOfAParent {
        _dir: tempfile::TempDir,
        h: Hierarchy,
        parent: PathBuf,
        entered: std::sync::atomic::AtomicU32,
    }

    /// The processes of [`ScopesOfAParent`] that are alive: pids from 50000, started at 7.
    fn alive_from_50000(pid: u32) -> Option<u64> {
        (pid >= 50_000).then_some(7)
    }

    impl crate::sweep::testing::Caller for ScopesOfAParent {
        const SWEEP: &'static Sweep = &crate::sweep::CGROUP_SCOPES;

        fn new() -> Self {
            let (dir, h) = fake_root(true);
            let parent = slice(&h);
            Self {
                _dir: dir,
                h,
                parent,
                entered: std::sync::atomic::AtomicU32::new(60_000),
            }
        }

        fn abandon(&self, n: usize) -> PathBuf {
            let scope = self.parent.join(scope_name(9_000 + n as u32, 1));
            std::fs::create_dir(&scope).unwrap();
            scope
        }

        fn live(&self, n: usize) -> (PathBuf, Box<dyn std::any::Any>) {
            let scope = self.parent.join(scope_name(50_000 + n as u32, 7));
            std::fs::create_dir(&scope).unwrap();
            (scope, Box::new(()))
        }

        fn start(&self, every: std::time::Duration) {
            let pid = self
                .entered
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let h = Hierarchy {
                sweep_every: every,
                ..same(&self.h)
            };
            enter(
                &h,
                &default_placement(),
                pid,
                7,
                None,
                false,
                &alive_from_50000,
            )
            .unwrap();
        }

        fn with_place<R>(&self, f: impl FnOnce(&crate::sweep::Place) -> R) -> R {
            let stamps = self.h.stamps.dir().unwrap();
            let dir = crate::sweep::open_dir(&stamps).unwrap();
            f(&crate::sweep::Place::keyed(
                &dir,
                &stamps,
                &sweep_key(&self.parent),
            ))
        }

        fn listed(&self) -> Vec<PathBuf> {
            vec![self.parent.clone()]
        }

        fn table(&self) -> Option<PathBuf> {
            Some(self.parent.clone())
        }
    }

    crate::sweep::testing::caller_matrix!(ScopesOfAParent);
    crate::sweep::testing::table_matrix!(ScopesOfAParent);

    /// The sweeps of two parents kept in one directory have a stamp and a lock each, under
    /// names that are a file's and not a path's.
    #[test]
    fn each_parent_has_its_own_stamp() {
        let (_dir, h) = fake_root(true);
        let (a, b) = (h.root.join("fcvm.slice"), h.root.join("ring/fcvm.slice"));
        assert_ne!(sweep_key(&a), sweep_key(&b));
        assert_eq!(sweep_key(&a), sweep_key(&h.root.join("fcvm.slice")));
        assert!(sweep_key(&b).bytes().all(|byte| byte.is_ascii_hexdigit()));
        let parent = slice(&h);
        enter(&h, &default_placement(), 4242, 77, None, false, &nobody).unwrap();
        let mut names: Vec<String> = std::fs::read_dir(h.stamps.dir().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        let key = sweep_key(&parent);
        assert_eq!(
            names,
            [format!(".sweep-lock-{key}"), format!(".sweep-stamp-{key}")]
        );
    }

    #[test]
    fn leaving_moves_back_and_removes_the_scope() {
        let (_dir, h) = fake_root(true);
        std::fs::create_dir(h.root.join("user.slice")).unwrap();
        std::fs::write(h.root.join("user.slice/cgroup.procs"), "").unwrap();
        let scope = enter(
            &h,
            &default_placement(),
            4242,
            77,
            Some(Path::new("/user.slice")),
            false,
            &nobody,
        )
        .unwrap();
        let path = scope.path().to_path_buf();
        // cgroupfs removes a cgroup's interface files with it; a plain directory does not.
        for file in std::fs::read_dir(&path).unwrap() {
            std::fs::remove_file(file.unwrap().path()).unwrap();
        }
        scope.leave();
        assert!(!path.exists());
        assert_eq!(
            std::fs::read_to_string(h.root.join("user.slice/cgroup.procs")).unwrap(),
            "4242"
        );
    }

    #[test]
    fn the_commands_that_start_a_vm_or_a_memory_server_enter_a_scope() {
        use clap::Parser;
        let request = |argv: &[&str]| {
            let cli = crate::cli::Cli::try_parse_from(
                std::iter::once("fcvm").chain(argv.iter().copied()),
            )
            .unwrap();
            scope_request(&cli.cmd)
        };
        assert_eq!(
            request(&["podman", "run", "--name", "a", "alpine"]),
            Some(false)
        );
        assert_eq!(
            request(&["podman", "run", "--name", "a", "--no-swap", "alpine"]),
            Some(true)
        );
        assert_eq!(
            request(&["podman", "prepare", "--name", "a", "--no-swap", "alpine"]),
            Some(true)
        );
        assert_eq!(request(&["snapshot", "serve", "s"]), Some(false));
        assert_eq!(
            request(&["snapshot", "serve", "s", "--no-swap"]),
            Some(true)
        );
        assert_eq!(
            request(&["snapshot", "run", "--snapshot", "s", "--no-swap"]),
            Some(true)
        );
        assert_eq!(request(&["snapshot", "create", "--pid", "1"]), None);
        assert_eq!(request(&["ls"]), None);
    }

    #[test]
    fn permission_errors_are_the_unprivileged_case() {
        for errno in [libc::EACCES, libc::EPERM, libc::EROFS] {
            assert!(is_permission_error(&io::Error::from_raw_os_error(errno)));
        }
        for errno in [libc::EBUSY, libc::ENOENT, libc::EINVAL] {
            assert!(!is_permission_error(&io::Error::from_raw_os_error(errno)));
        }
    }
}
