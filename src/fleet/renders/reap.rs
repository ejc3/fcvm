//! What ends pages mode's processes when the module does, however the module ends.
//!
//! The module starts one reaper before anything else and holds the write end of a pipe to
//! it. The reaper reads the pipe. End of file means the module is gone (stopped, failed or
//! SIGKILLed: the kernel closes the pipe in each case), and the reaper then removes
//! everything the module made:
//!
//! | made by the module | removed by |
//! |---|---|
//! | every process (replays, the memory server, clones, engines, workers) | `cgroup.kill` on the module's cgroups, then a wait until they are empty |
//! | the cgroups | rmdir, children first |
//! | a killed clone's runtime directory | fcvm's owner records (`reclaim::reclaim_abandoned`) |
//! | a killed clone's and a killed memory server's state file | fcvm's stale-state sweep |
//! | a killed memory server's socket | by name: the snapshot's sockets whose process is gone |
//! | the run directory (certificates, logs, injected files) | `remove_dir_all` |
//!
//! Listening ports, network namespaces and 2 MiB page reservations go with the processes.
//! No child is trusted to end itself: an engine is a `node` that starts `workerd` and arms
//! nothing, and a process that enters a user namespace loses its parent-death signal.
//!
//! The reaper is in its own session and ignores SIGTERM, SIGINT and SIGHUP, so a signal
//! sent to the module's process group or a unit being stopped lets it finish. It runs in
//! the module's own cgroup, never in one it kills. The same sweep runs when a module
//! starts, for what a host that lost power left.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::cli::FleetRendersReapArgs;
use crate::fleet::cpuset;

/// What a sweep found and what it could not remove.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Reaped {
    /// Cgroups that held a process when the sweep began.
    pub cgroups_populated: usize,
    /// Cgroups still there after it (a process outlived `cgroup.kill` for the deadline).
    pub cgroups_left: Vec<String>,
    /// Abandoned fcvm owners undone, and their resources.
    pub owners: usize,
    pub resources: usize,
    /// Owner resources that could not be undone yet.
    pub resources_left: usize,
    /// Memory server sockets of the snapshot whose process was gone.
    pub sockets: usize,
    /// Whether the run directory is still there.
    pub run_dir_left: bool,
}

impl Reaped {
    /// Nothing of the module is left.
    pub fn clean(&self) -> bool {
        self.cgroups_left.is_empty() && self.resources_left == 0 && !self.run_dir_left
    }
}

/// What a reaper removes.
#[derive(Debug, Clone)]
pub struct Target {
    pub cgroups: Vec<PathBuf>,
    pub run_dir: PathBuf,
    pub snapshot_tag: String,
    /// How long a cgroup may take to empty after `cgroup.kill`.
    pub deadline: Duration,
}

/// `uffd-<tag>-<pid>-<start>.sock`: the process a memory server's socket names.
fn socket_owner(name: &str, tag: &str) -> Option<(u32, u64)> {
    let rest = name
        .strip_prefix("uffd-")?
        .strip_prefix(tag)?
        .strip_prefix('-')?
        .strip_suffix(".sock")?;
    let (pid, start) = rest.split_once('-')?;
    Some((pid.parse().ok()?, start.parse().ok()?))
}

/// Remove the snapshot's memory server sockets in `dir` whose process is gone.
fn remove_dead_sockets(dir: &Path, tag: &str, alive: &dyn Fn(u32, u64) -> bool) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut removed = 0;
    for e in entries.flatten() {
        let name = e.file_name();
        let Some((pid, start)) = socket_owner(&name.to_string_lossy(), tag) else {
            continue;
        };
        if !alive(pid, start) && std::fs::remove_file(e.path()).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// Remove everything a module with this target made. Idempotent.
pub async fn sweep(target: &Target) -> Reaped {
    let mut out = Reaped::default();
    let cgroups = target.cgroups.clone();
    let deadline = target.deadline;
    let (populated, left) = tokio::task::spawn_blocking(move || {
        let populated = cgroups.iter().filter(|c| cpuset::populated(c)).count();
        // Kill all of them before waiting for any: a clone's guard dies with its worker,
        // and nothing should start in one cgroup while another is being emptied.
        for c in &cgroups {
            let _ = std::fs::write(c.join("cgroup.kill"), "1");
        }
        let left: Vec<String> = cgroups
            .iter()
            .filter(|c| !cpuset::destroy(c, deadline))
            .map(|c| c.display().to_string())
            .collect();
        (populated, left)
    })
    .await
    .unwrap_or_default();
    out.cgroups_populated = populated;
    out.cgroups_left = left;

    let reclaimed = crate::reclaim::reclaim_abandoned().await;
    out.owners = reclaimed.owners;
    out.resources = reclaimed.resources;
    out.resources_left = reclaimed.remaining;
    crate::state::StateManager::new(crate::paths::state_dir())
        .sweep_stale()
        .await;
    out.sockets = remove_dead_sockets(
        &crate::paths::data_dir(),
        &target.snapshot_tag,
        &|pid, start| crate::utils::process_start_time(pid) == Some(start),
    );

    // Only a directory this module made: a caller's path that holds something else stays,
    // and is reported as left.
    if run_dir_is_ours(&target.run_dir) {
        remove_run_dir(&target.run_dir);
    }
    out.run_dir_left = target.run_dir.exists();
    out
}

/// The file a module puts in its run directory, so a sweep removes that directory and no
/// other.
pub const RUN_MARKER: &str = ".fcvm-renders-run";

/// Whether `dir` may be removed as a run directory: it is absent, empty, or carries the
/// module's marker.
pub fn run_dir_is_ours(dir: &Path) -> bool {
    match std::fs::read_dir(dir) {
        Ok(mut entries) => dir.join(RUN_MARKER).is_file() || entries.next().is_none(),
        Err(e) => e.kind() == std::io::ErrorKind::NotFound,
    }
}

/// Remove everything in a run directory but its marker. The marker goes last
/// ([`remove_run_dir`]), so a removal that is cut short (the reaper killed, the host
/// losing power) leaves a directory the next start still knows as the module's.
fn remove_run_dir_contents(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if entry.file_name() == RUN_MARKER {
            continue;
        }
        let path = entry.path();
        let _ = if entry.file_type().is_ok_and(|t| t.is_dir()) {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };
    }
}

/// Remove a run directory: its contents, then its marker, then the directory.
fn remove_run_dir(dir: &Path) {
    remove_run_dir_contents(dir);
    let _ = std::fs::remove_file(dir.join(RUN_MARKER));
    let _ = std::fs::remove_dir(dir);
}

/// One module at a time per run directory: an exclusive lock on `<run dir>.lock`, held by
/// the module and inherited by its reaper, so it is free only when both are gone. A second
/// module with the same run directory would otherwise sweep the first one's files at its
/// start, and a new module's directory could be removed by an old module's reaper.
pub struct RunLock(std::fs::File);

impl RunLock {
    pub fn take(run_dir: &Path) -> anyhow::Result<Self> {
        let mut name = run_dir
            .file_name()
            .ok_or_else(|| anyhow::anyhow!("--run-dir {} has no name", run_dir.display()))?
            .to_os_string();
        name.push(".lock");
        let path = run_dir.with_file_name(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .map_err(|e| anyhow::anyhow!("opening {}: {e}", path.display()))?;
        // SAFETY: flock on a descriptor this function owns.
        let taken = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0;
        anyhow::ensure!(
            taken,
            "another pages-mode module, or its reaper, holds {} ({})",
            path.display(),
            std::io::Error::last_os_error()
        );
        Ok(RunLock(file))
    }
}

/// The module's hold on its reaper: dropping it (or dying) starts the sweep.
pub struct Reaper {
    child: tokio::process::Child,
    hold: Option<OwnedFd>,
}

impl Reaper {
    /// Start `exe fleet renders-reap` for `target`, with `report` as a file it also writes
    /// its result to (for a caller that will not be there to read it).
    pub fn start(
        exe: &Path,
        target: &Target,
        report: Option<&Path>,
        lock: &RunLock,
    ) -> anyhow::Result<Self> {
        let mut fds = [0 as libc::c_int; 2];
        // SAFETY: a two-element array for pipe2 to fill. O_CLOEXEC keeps both ends out of
        // every other child: one that inherited the write end would hold the reaper off
        // for as long as it lived.
        anyhow::ensure!(
            unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } == 0,
            "pipe for the reaper: {}",
            std::io::Error::last_os_error()
        );
        // SAFETY: both descriptors were just returned by pipe2 and are owned here alone.
        let (read, write) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
        let read_fd = read.as_raw_fd();
        let lock_fd = lock.0.as_raw_fd();
        let mut cmd = std::process::Command::new(exe);
        cmd.args(["fleet", "renders-reap", "--watch-fd", &read_fd.to_string()])
            .args(["--run-dir".as_ref(), target.run_dir.as_os_str()])
            .args(["--snapshot-tag", &target.snapshot_tag])
            .args(["--deadline-ms", &target.deadline.as_millis().to_string()]);
        for c in &target.cgroups {
            cmd.args(["--cgroup".as_ref(), c.as_os_str()]);
        }
        if let Some(r) = report {
            cmd.args(["--report".as_ref(), r.as_os_str()]);
        }
        cmd.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit());
        // SAFETY: runs in the forked child before exec; setsid and fcntl are
        // async-signal-safe. The reaper inherits the read end under its own number: only
        // its close-on-exec flag is cleared, so no descriptor of the child is replaced.
        unsafe {
            use std::os::unix::process::CommandExt;
            cmd.pre_exec(move || {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                // The run directory's lock too: the reaper holds it until its sweep is done.
                for fd in [read_fd, lock_fd] {
                    if libc::fcntl(fd, libc::F_SETFD, 0) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        }
        let child = tokio::process::Command::from(cmd)
            .kill_on_drop(false)
            .spawn()
            .map_err(|e| anyhow::anyhow!("starting the reaper ({}): {e}", exe.display()))?;
        drop(read);
        Ok(Reaper {
            child,
            hold: Some(write),
        })
    }

    /// Let go, and wait for what the reaper found.
    pub async fn finish(mut self) -> anyhow::Result<Reaped> {
        use tokio::io::AsyncReadExt;
        self.hold.take();
        let mut text = String::new();
        if let Some(mut out) = self.child.stdout.take() {
            out.read_to_string(&mut text).await?;
        }
        let status = self.child.wait().await?;
        let line = text.lines().last().unwrap_or_default();
        serde_json::from_str(line)
            .map_err(|e| anyhow::anyhow!("the reaper ({status}) reported {line:?}: {e}"))
    }
}

/// `fcvm fleet renders-reap`: wait for the module to go, then sweep.
pub async fn cmd(args: FleetRendersReapArgs) -> anyhow::Result<()> {
    for sig in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP, libc::SIGPIPE] {
        // SAFETY: SIG_IGN is a valid disposition for each of these signals.
        unsafe { libc::signal(sig, libc::SIG_IGN) };
    }
    let fd = args.watch_fd;
    tokio::task::spawn_blocking(move || {
        let mut byte = [0u8; 1];
        loop {
            // SAFETY: a one-byte buffer of the length given, on the descriptor the module
            // handed over.
            let n = unsafe { libc::read(fd, byte.as_mut_ptr().cast(), 1) };
            if n == 0 {
                return;
            }
            if n < 0 && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
                return;
            }
        }
    })
    .await?;
    let reaped = sweep(&Target {
        cgroups: args.cgroup,
        run_dir: args.run_dir,
        snapshot_tag: args.snapshot_tag,
        deadline: Duration::from_millis(args.deadline_ms),
    })
    .await;
    let line = serde_json::to_string(&reaped)?;
    if let Some(report) = &args.report {
        let tmp = report.with_extension("tmp");
        if std::fs::write(&tmp, format!("{line}\n")).is_ok() {
            let _ = std::fs::rename(&tmp, report);
        }
    }
    // The module may be gone, and its end of this pipe with it.
    use std::io::Write;
    let _ = writeln!(std::io::stdout(), "{line}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_memory_servers_socket_names_its_process() {
        assert_eq!(
            socket_owner("uffd-gold-42-1000.sock", "gold"),
            Some((42, 1000))
        );
        assert_eq!(
            socket_owner("uffd-gold-huge-42-1000.sock", "gold-huge"),
            Some((42, 1000))
        );
        // another snapshot whose tag starts with this one's
        assert_eq!(socket_owner("uffd-gold-huge-42-1000.sock", "gold"), None);
        assert_eq!(socket_owner("uffd-gold-42.sock", "gold"), None);
        assert_eq!(socket_owner("gold-42-1000.sock", "gold"), None);
    }

    #[test]
    fn only_the_snapshots_sockets_of_dead_processes_are_removed() {
        let dir = tempfile::tempdir().unwrap();
        for name in [
            "uffd-gold-42-1000.sock",
            "uffd-gold-43-2000.sock",
            "uffd-other-44-3000.sock",
            "memory.bin",
        ] {
            std::fs::write(dir.path().join(name), "").unwrap();
        }
        let removed = remove_dead_sockets(dir.path(), "gold", &|pid, _| pid == 43);
        assert_eq!(removed, 1);
        let mut left: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(
            left,
            [
                "memory.bin",
                "uffd-gold-43-2000.sock",
                "uffd-other-44-3000.sock"
            ]
        );
    }

    #[test]
    fn a_run_directory_is_removed_only_when_it_is_the_modules() {
        let root = tempfile::tempdir().unwrap();
        // absent, and empty: nothing of anyone's
        assert!(run_dir_is_ours(&root.path().join("absent")));
        let empty = root.path().join("empty");
        std::fs::create_dir(&empty).unwrap();
        assert!(run_dir_is_ours(&empty));
        // a directory with someone's files in it is not removed
        let theirs = root.path().join("theirs");
        std::fs::create_dir(&theirs).unwrap();
        std::fs::write(theirs.join("notes.txt"), "keep").unwrap();
        assert!(!run_dir_is_ours(&theirs));
        // the module's own, by its marker
        let ours = root.path().join("ours");
        std::fs::create_dir_all(ours.join("certs-fcvm")).unwrap();
        std::fs::write(ours.join(RUN_MARKER), "").unwrap();
        assert!(run_dir_is_ours(&ours));
    }

    /// A removal cut short after any part of the contents leaves the marker, so the next
    /// start's sweep takes the directory as the module's and finishes the job.
    #[test]
    fn a_run_directory_half_removed_is_still_the_modules() {
        let root = tempfile::tempdir().unwrap();
        let run = root.path().join("run");
        std::fs::create_dir_all(run.join("certs-fcvm")).unwrap();
        std::fs::write(run.join("certs-fcvm/ca.pem"), "x").unwrap();
        std::fs::write(run.join("inject-fcvm.html"), "").unwrap();
        std::fs::write(run.join(RUN_MARKER), "").unwrap();
        // as far as a removal gets before its last two steps
        remove_run_dir_contents(&run);
        let left: Vec<_> = std::fs::read_dir(&run)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(left, [RUN_MARKER]);
        assert!(run_dir_is_ours(&run));
        // and one cut short earlier: something left beside the marker
        std::fs::write(run.join("inject-kitesurf.html"), "").unwrap();
        assert!(run_dir_is_ours(&run));
        remove_run_dir(&run);
        assert!(!run.exists());
    }

    #[test]
    fn one_module_holds_a_run_directory_at_a_time() {
        let root = tempfile::tempdir().unwrap();
        let run_dir = root.path().join("fcvm-renders");
        let first = RunLock::take(&run_dir).unwrap();
        let refused = RunLock::take(&run_dir)
            .err()
            .expect("a second module is refused");
        assert!(
            refused.to_string().contains("fcvm-renders.lock"),
            "{refused}"
        );
        assert!(
            !run_dir.exists(),
            "the lock is beside the directory, not in it"
        );
        drop(first);
        RunLock::take(&run_dir).unwrap();
    }

    #[test]
    fn a_sweep_is_clean_only_when_nothing_is_left() {
        assert!(Reaped::default().clean());
        assert!(Reaped {
            cgroups_populated: 3,
            owners: 2,
            resources: 2,
            sockets: 1,
            ..Default::default()
        }
        .clean());
        for dirty in [
            Reaped {
                cgroups_left: vec!["x".into()],
                ..Default::default()
            },
            Reaped {
                resources_left: 1,
                ..Default::default()
            },
            Reaped {
                run_dir_left: true,
                ..Default::default()
            },
        ] {
            assert!(!dirty.clean(), "{dirty:?}");
        }
    }
}
