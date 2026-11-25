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
//! | the run directory (certificates, logs, injected files) | `remove_dir_all`, when the path was claimed (`RunLock::claim`) |
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

    out.run_dir_left = remove_run_dir(&target.run_dir);
    out
}

/// `<run dir>.lock`, beside the run directory: the lock one module holds, and the record
/// that the path is a pages-mode run directory.
fn lock_path(run_dir: &Path) -> anyhow::Result<PathBuf> {
    let mut name = run_dir
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("--run-dir {} has no name", run_dir.display()))?
        .to_os_string();
    name.push(".lock");
    Ok(run_dir.with_file_name(name))
}

/// What the lock file holds once the path is the module's.
const CLAIMED: &str = "pages-mode run directory\n";

/// Whether `run_dir` is claimed as a pages-mode run directory ([`RunLock::claim`]). The
/// record is beside the directory, never in it: nothing a removal does to the directory,
/// cut short or refused part-way, changes whose it is, and a sweep removes only a claimed
/// one. Without the record a directory that holds files cannot be told from someone
/// else's, so a lock file that was removed while its directory held a run's files leaves
/// a directory a start refuses until a person removes it ([`RunLock::usable`] says so).
pub fn run_dir_owned(run_dir: &Path) -> bool {
    lock_path(run_dir)
        .ok()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .is_some_and(|text| text == CLAIMED)
}

/// Remove a run directory with everything in it, when the path was claimed; a path nobody
/// claimed stays as it is. Once the directory is gone the claim ends with it, so what
/// someone else later puts at the path is theirs. Returns whether the directory is still
/// there. The caller holds the path's lock ([`RunLock`]): a module at its start, or its
/// reaper.
fn remove_run_dir(run_dir: &Path) -> bool {
    let gone = |dir: &Path| matches!(std::fs::symlink_metadata(dir), Err(e) if e.kind() == std::io::ErrorKind::NotFound);
    if run_dir_owned(run_dir) {
        let _ = std::fs::remove_dir_all(run_dir);
        if gone(run_dir) {
            // A sweep ended between the two leaves a claimed path with no directory, which
            // the next start claims again.
            let _ = lock_path(run_dir)
                .and_then(|path| Ok(std::fs::OpenOptions::new().write(true).open(path)?))
                .and_then(|file| Ok(file.set_len(0)?));
        }
    } else {
        // An empty directory at a path nobody claimed (a person's `mkdir`) is nothing
        // left; `remove_dir` removes nothing that holds anything.
        let _ = std::fs::remove_dir(run_dir);
    }
    !gone(run_dir)
}

/// One module at a time per run directory: an exclusive lock on `<run dir>.lock`, held by
/// the module and inherited by its reaper, so it is free only when both are gone. A second
/// module with the same run directory would otherwise sweep the first one's files at its
/// start, and a new module's directory could be removed by an old module's reaper.
pub struct RunLock {
    file: std::fs::File,
    path: PathBuf,
}

impl RunLock {
    pub fn take(run_dir: &Path) -> anyhow::Result<Self> {
        let path = lock_path(run_dir)?;
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
        Ok(RunLock { file, path })
    }

    /// Whether a module may use `run_dir`, before anything is removed or made. A claimed
    /// path may be, whatever its directory holds (what a module that lost its host left).
    /// A path nobody claimed may be only while its directory is absent or empty: one that
    /// holds files is someone else's as far as anything here can tell, and is refused and
    /// never removed. A lock file that holds anything but the record, or a record cut
    /// short while it was written, is someone else's file and is never written over.
    pub fn usable(&self, run_dir: &Path) -> anyhow::Result<()> {
        if run_dir_owned(run_dir) {
            return Ok(());
        }
        let held = std::fs::read(&self.path)
            .map_err(|e| anyhow::anyhow!("reading {}: {e}", self.path.display()))?;
        anyhow::ensure!(
            CLAIMED.as_bytes().starts_with(&held),
            "{} is not a pages-mode lock file (it holds something else); name a --run-dir whose `.lock` file is free",
            self.path.display()
        );
        let free = match std::fs::read_dir(run_dir) {
            Ok(mut entries) => entries.next().is_none(),
            Err(e) => e.kind() == std::io::ErrorKind::NotFound,
        };
        anyhow::ensure!(
            free,
            "--run-dir {} holds files and {} does not record it as a pages-mode run directory. The module removes its run directory at start and at the end, so name a directory of its own; if an earlier pages-mode run left these, remove the directory by hand",
            run_dir.display(),
            self.path.display()
        );
        Ok(())
    }

    /// Make `run_dir` this run's, under the lock, after what an earlier run left is gone
    /// and before anything is made in it. The claim ends when a sweep has removed the
    /// directory ([`remove_run_dir`]).
    pub fn claim(&self, run_dir: &Path) -> anyhow::Result<()> {
        use std::os::unix::fs::FileExt;
        self.usable(run_dir)?;
        if run_dir_owned(run_dir) {
            return Ok(());
        }
        self.file.set_len(0)?;
        // at the start of the file, wherever an earlier claim through this descriptor ended
        self.file.write_all_at(CLAIMED.as_bytes(), 0)?;
        self.file.sync_all()?;
        anyhow::ensure!(
            run_dir_owned(run_dir),
            "{} does not read back as written",
            self.path.display()
        );
        Ok(())
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
        let lock_fd = lock.file.as_raw_fd();
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

    /// A directory that holds someone's files is refused and stays theirs. An absent or
    /// empty one becomes the module's, and stays the module's whatever it holds afterwards.
    #[test]
    fn a_run_directory_is_claimed_once_and_never_taken_from_someone_else() {
        let root = tempfile::tempdir().unwrap();
        let theirs = root.path().join("theirs");
        std::fs::create_dir(&theirs).unwrap();
        std::fs::write(theirs.join("notes.txt"), "keep").unwrap();
        let lock = RunLock::take(&theirs).unwrap();
        let refused = lock.claim(&theirs).unwrap_err();
        assert!(
            refused.to_string().contains("does not record it"),
            "{refused}"
        );
        assert!(!run_dir_owned(&theirs));
        drop(lock);

        for start in ["absent", "empty"] {
            let run = root.path().join(start);
            if start == "empty" {
                std::fs::create_dir(&run).unwrap();
            }
            assert!(!run_dir_owned(&run));
            let lock = RunLock::take(&run).unwrap();
            lock.claim(&run).unwrap();
            assert!(run_dir_owned(&run), "{start}");
        }
    }

    /// However a removal ended, what the module left in its directory is still the module's:
    /// the record is beside the directory, and nothing in the directory decides.
    #[test]
    fn what_a_module_left_behind_is_still_its_run_directory() {
        let root = tempfile::tempdir().unwrap();
        let run = root.path().join("run");
        let lock = RunLock::take(&run).unwrap();
        lock.claim(&run).unwrap();
        // the module made it and filled it; then a removal lost some of it and not the rest
        std::fs::create_dir_all(run.join("certs-fcvm")).unwrap();
        std::fs::write(run.join("certs-fcvm/ca.pem"), "x").unwrap();
        assert!(run_dir_owned(&run));
        // and the next module's claim on the path is the one already made
        lock.claim(&run).unwrap();
        // someone else's directory with the same contents is not: the record decides
        let theirs = root.path().join("theirs");
        std::fs::create_dir_all(theirs.join("certs-fcvm")).unwrap();
        std::fs::write(theirs.join("certs-fcvm/ca.pem"), "x").unwrap();
        assert!(!run_dir_owned(&theirs));
    }

    /// A sweep removes a claimed run directory with everything in it, and leaves alone a
    /// directory nobody claimed.
    #[test]
    fn only_a_claimed_run_directory_is_removed() {
        let root = tempfile::tempdir().unwrap();
        let run = root.path().join("run");
        let lock = RunLock::take(&run).unwrap();
        lock.claim(&run).unwrap();
        std::fs::create_dir_all(run.join("logs")).unwrap();
        std::fs::write(run.join("logs/engine-0.log"), "x").unwrap();
        assert!(!remove_run_dir(&run));
        assert!(!run.exists());
        // The claim ended with the directory: what someone puts at the path afterwards is
        // theirs, refused at the next start and left where it is.
        assert!(!run_dir_owned(&run));
        std::fs::create_dir(&run).unwrap();
        std::fs::write(run.join("thesis.tex"), "keep").unwrap();
        let refused = lock.claim(&run).unwrap_err();
        assert!(
            refused.to_string().contains("remove the directory by hand"),
            "{refused}"
        );
        assert!(remove_run_dir(&run));
        assert!(run.join("thesis.tex").is_file());

        let theirs = root.path().join("theirs");
        std::fs::create_dir(&theirs).unwrap();
        std::fs::write(theirs.join("notes.txt"), "keep").unwrap();
        assert!(remove_run_dir(&theirs));
        assert!(theirs.join("notes.txt").is_file());
    }

    /// A file at the lock's path that holds someone's text is not written over, and a
    /// record cut short while it was written is finished.
    #[test]
    fn a_lock_file_that_holds_something_else_is_left_as_it_is() {
        let root = tempfile::tempdir().unwrap();
        let run = root.path().join("build");
        std::fs::write(root.path().join("build.lock"), "pid 4242\n").unwrap();
        let lock = RunLock::take(&run).unwrap();
        let refused = lock.claim(&run).unwrap_err();
        assert!(refused.to_string().contains("build.lock"), "{refused}");
        assert_eq!(
            std::fs::read_to_string(root.path().join("build.lock")).unwrap(),
            "pid 4242\n"
        );
        assert!(!run_dir_owned(&run));

        let run = root.path().join("run");
        std::fs::write(root.path().join("run.lock"), &CLAIMED[..9]).unwrap();
        assert!(!run_dir_owned(&run));
        let lock = RunLock::take(&run).unwrap();
        lock.claim(&run).unwrap();
        assert!(run_dir_owned(&run));
    }

    /// The start's own order (usable, sweep, claim) on a directory that is there, empty
    /// and nobody's: a person's `mkdir`. The sweep must not report it as left.
    #[test]
    fn an_empty_directory_nobody_claimed_is_taken_at_a_start() {
        let root = tempfile::tempdir().unwrap();
        let run = root.path().join("run");
        std::fs::create_dir(&run).unwrap();
        let lock = RunLock::take(&run).unwrap();
        lock.usable(&run).unwrap();
        assert!(
            !remove_run_dir(&run),
            "an empty directory is not something left"
        );
        lock.claim(&run).unwrap();
        assert!(run_dir_owned(&run));
        // a second claim after a sweep emptied the record writes the whole record again
        std::fs::create_dir(&run).unwrap();
        assert!(!remove_run_dir(&run));
        assert!(!run_dir_owned(&run));
        lock.claim(&run).unwrap();
        assert!(run_dir_owned(&run));
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
        // Dropping it frees the lock. A process another thread is starting holds a copy of
        // the descriptor from its fork to its exec, and the lock with it, so the lock is
        // free a moment after the drop and not always at once.
        drop(first);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while let Err(held) = RunLock::take(&run_dir) {
            assert!(std::time::Instant::now() < deadline, "{held}");
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
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
