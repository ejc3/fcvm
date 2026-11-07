//! Each VM's own cgroup (issue #1028).
//!
//! cgroup v2 charges a page to the cgroup of the process that instantiated it, and moving
//! a process later does not move what it was charged. So the fcvm process enters its scope
//! first thing in `podman run`, `podman prepare`, `snapshot run` and `snapshot serve`,
//! before anything is spawned or mapped. The VMM, the holder, pasta, the UFFD server and
//! the FUSE servers then start inside it, and guest memory and the memory server's page
//! cache are charged to the scope, not to the session that launched fcvm.
//!
//! Where the scope lives: `fcvm.slice/fcvm-<pid>-<starttime>.scope` at the root of the
//! cgroup2 hierarchy, created by writing cgroupfs directly. A systemd transient scope was
//! the alternative. `systemd-run --scope` starts a new process and cannot place a running
//! one, and placing this process takes a `StartTransientUnit` call on the system bus, a
//! bus client and a running systemd, which the CI containers and nested guests do not all
//! have. The placement also matters for oomd: a root-level `fcvm.slice` is outside the
//! user slice, so a rule that protects the host by killing a user's session scope does not
//! take the VM with it, which is what happened when the VM was charged to the session.
//! A host whose oomd should be able to pick VMs sets `FCVM_CGROUP_PARENT` to a slice it
//! watches. systemd does not manage or remove a cgroup it did not create. A caller that
//! needs a systemd unit after all (properties set through systemd, oomd rules keyed on
//! units) starts fcvm in one, `systemd-run --scope -p MemorySwapMax=0 env
//! FCVM_CGROUP_PARENT=none fcvm ...`: fcvm then runs from its first instruction in the
//! unit's scope, which is all the entry here exists to achieve.
//!
//! `FCVM_CGROUP_PARENT` names the parent as a path below the cgroup2 root (default
//! `fcvm.slice`), or `none`, which keeps the process in the cgroup it started in. The bench
//! harness sets `none`: it places fcvm in accounting cgroups of its own and audits that
//! fcvm stays there.
//!
//! The root is the root of the caller's cgroup namespace. A caller confined by a cgroup
//! namespace stays inside it, because the kernel resolves `/sys/fs/cgroup` there. A caller
//! confined only by the cgroup it was started in (a sandbox slice) and running as root could
//! leave that cgroup anyway, and fcvm leaves it by default for the reason above. A caller who
//! wants the VM charged inside its slice names that slice in `FCVM_CGROUP_PARENT`.
//!
//! Only a process that can write the parent can enter. An unprivileged caller cannot, and
//! keeps its own cgroup as before; one line says so. `--no-swap` is refused in that case and
//! whenever no scope with a memory controller could be made, because a flag that promises
//! guest memory will not be swapped must not run a VM that can be.
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
//! is removed by the next entry's sweep: every entry removes the scopes whose owner, the
//! (pid, start time) in the name, is gone.
//!
//! The sweep and the creation run under an exclusive `flock` on the parent directory. A
//! scope is empty between its `mkdir` and the write that moves its owner in, and the owner
//! holds the lock across that window, so no sweep can remove it there. Outside the window a
//! scope holds its owner's processes, and the kernel refuses `rmdir` of a populated cgroup.
//! That covers an owner in another pid namespace, whose pid this process cannot resolve.

use std::io;
use std::os::fd::AsRawFd;
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
    /// Under this parent, a path relative to the cgroup2 root.
    Under(PathBuf),
    /// Nowhere: the process stays in the cgroup it started in.
    Stay,
}

impl Placement {
    /// Read the placement from the value of `FCVM_CGROUP_PARENT`.
    pub fn from_env_value(value: Option<&str>) -> Result<Self> {
        let Some(value) = value else {
            return Ok(Self::Under(PathBuf::from(DEFAULT_PARENT)));
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

/// Remove the scopes in `parent` whose owner is gone. `start_time_of` gives a live
/// process's start time, or `None` when no process has that pid.
///
/// The caller holds the parent's lock ([`lock_parent`]), so no scope here is between its
/// creation and its owner's entry except the caller's own, which it has not made yet.
fn sweep(parent: &Path, start_time_of: &dyn Fn(u32) -> Option<u64>) -> SweepCount {
    let mut count = SweepCount::default();
    let Ok(entries) = std::fs::read_dir(parent) else {
        return count;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(owner) = name.to_str().and_then(scope_owner) else {
            continue;
        };
        let owner_alive = match owner {
            ScopeOwner::Process { pid, start_time } => start_time_of(pid) == Some(start_time),
            // A pid alone cannot tell a reused pid from the owner, and an rmdir of a scope
            // that still holds Firecracker fails, so the kernel is the only check.
            ScopeOwner::Legacy => false,
        };
        if owner_alive {
            continue;
        }
        let path = entry.path();
        match std::fs::remove_dir(&path) {
            Ok(()) => {
                count.removed += 1;
                debug!(scope = %path.display(), "removed the scope of a process that is gone");
            }
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

/// Hold an exclusive `flock` on the parent directory. Released when the file is dropped.
fn lock_parent(parent: &Path) -> io::Result<std::fs::File> {
    let dir = std::fs::File::open(parent)?;
    loop {
        // SAFETY: flock on a descriptor this function owns.
        if unsafe { libc::flock(dir.as_raw_fd(), libc::LOCK_EX) } == 0 {
            return Ok(dir);
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
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
}

impl Hierarchy {
    /// The host's hierarchy at [`CGROUP2_ROOT`].
    pub fn system() -> Self {
        Self {
            root: PathBuf::from(CGROUP2_ROOT),
            mkdir: |path| std::fs::create_dir(path),
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
    /// still holds a process, or whose origin is gone, stays for the next entry's sweep.
    pub fn leave(self) {
        let Some(origin) = &self.origin else {
            return;
        };
        if let Err(e) = write_interface(&origin.join("cgroup.procs"), &self.pid.to_string()) {
            debug!(
                origin = %origin.display(), scope = %self.path.display(), error = %e,
                "could not move back to the cgroup fcvm started in; the next fcvm sweeps the scope"
            );
            return;
        }
        match std::fs::remove_dir(&self.path) {
            Ok(()) => debug!(scope = %self.path.display(), "removed the VM's cgroup"),
            Err(e) => debug!(
                scope = %self.path.display(), error = %e,
                "the VM's cgroup still holds a process; the next fcvm sweeps it"
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
    let Placement::Under(parent_relative) = placement else {
        return Err(not_entered(format!("{PARENT_ENV}=none"), None));
    };
    let root = &hierarchy.root;
    if !root.join("cgroup.controllers").exists() {
        return Err(not_entered(
            format!("no cgroup2 hierarchy is mounted at {}", root.display()),
            None,
        ));
    }
    let parent = root.join(parent_relative);
    let mut level = root.clone();
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
    let lock = lock_parent(&parent)
        .map_err(|e| not_entered(format!("locking {}: {e}", parent.display()), Some(&e)))?;

    let swept = sweep(&parent, start_time_of);
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
    use crate::cli::{Commands, PodmanCommands, SnapshotCommands};
    match cmd {
        Commands::Podman(args) => Some(match &args.cmd {
            PodmanCommands::Run(run) => run.no_swap,
            PodmanCommands::Prepare(prepare) => prepare.run.no_swap,
        }),
        Commands::Snapshot(args) => match &args.cmd {
            SnapshotCommands::Serve(serve) => Some(serve.no_swap),
            SnapshotCommands::Run(run) => Some(run.no_swap),
            SnapshotCommands::Create(_) | SnapshotCommands::Ls => None,
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
             or naming a cgroup where the memory controller can be enabled",
            not.reason
        ),
        Err(not) if not.unprivileged => {
            info!(
                reason = %not.reason,
                "no cgroup of the VM's own (needs root); the VM's memory is charged to the \
                 cgroup fcvm was started in"
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

    /// A `mkdir` that adds the interface files cgroupfs gives a new cgroup. Controller files
    /// appear only when the parent enables the controller, as in the kernel.
    fn fake_mkdir(path: &Path) -> io::Result<()> {
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
        let hierarchy = Hierarchy {
            root: dir.path().to_path_buf(),
            mkdir: fake_mkdir,
        };
        (dir, hierarchy)
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
        assert_eq!(
            default_placement(),
            Placement::Under(PathBuf::from("fcvm.slice"))
        );
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

        let count = sweep(&parent, &start_time_of);
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

    /// An entry holds the parent's lock from its sweep until its process is inside its
    /// scope, so a concurrent entry's sweep cannot remove a scope in the window where it is
    /// empty. Here the test holds the lock and the entry must wait for it.
    #[test]
    fn entry_waits_for_the_parent_lock() {
        let (_dir, h) = fake_root(true);
        let parent = h.root.join("fcvm.slice");
        std::fs::create_dir(&parent).unwrap();
        std::fs::write(parent.join("cgroup.subtree_control"), "memory\n").unwrap();
        let held = lock_parent(&parent).unwrap();
        let root = h.root.clone();
        let entry = std::thread::spawn(move || {
            let h = Hierarchy {
                root,
                mkdir: fake_mkdir,
            };
            enter(&h, &default_placement(), 4242, 77, None, false, &nobody).map(|s| s.path)
        });
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(
            !parent.join("fcvm-4242-77.scope").exists(),
            "the entry made its scope while another process held the parent's lock"
        );
        drop(held);
        let path = entry.join().unwrap().unwrap();
        assert!(path.exists());
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
