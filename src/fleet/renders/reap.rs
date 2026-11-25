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

    let _ = std::fs::remove_dir_all(&target.run_dir);
    out.run_dir_left = target.run_dir.exists();
    out
}

/// The module's hold on its reaper: dropping it (or dying) starts the sweep.
pub struct Reaper {
    child: tokio::process::Child,
    hold: Option<OwnedFd>,
}

impl Reaper {
    /// Start `exe fleet renders-reap` for `target`, with `report` as a file it also writes
    /// its result to (for a caller that will not be there to read it).
    pub fn start(exe: &Path, target: &Target, report: Option<&Path>) -> anyhow::Result<Self> {
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
                if libc::fcntl(read_fd, libc::F_SETFD, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
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
