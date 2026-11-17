//! The source guard of a memory snapshot create (#1936).
//!
//! A create changes its source VM: `--prepare-snapshot-network` closes the guest's gate,
//! freezes the payload cgroup and holds the console, and then the VMM is paused. The create
//! undoes both on its own return paths, which a process that is killed never reaches, and
//! nothing else knows the source was paused: its own fcvm keeps reporting it healthy.
//!
//! So a create whose source must keep running spawns a guard process before its first
//! change (`fcvm snapshot source-guard`, an exec of this binary). The guard inherits three
//! descriptors:
//!
//! - its end of a socket pair, on which the create reports how far it got ([`PREPARE`],
//!   [`PAUSE`]) and finally that it reopened the source itself ([`DONE`]);
//! - a pidfd of the create process, which the kernel makes readable when that process is
//!   gone, whoever still holds the other end of the socket pair (a child of the create in
//!   the window between its fork and its exec);
//! - the create's per-VM snapshot lock, the same open file description, so the flock stays
//!   held from the create's death until the guard has finished: no other create, and no
//!   balloon change, can pause or snapshot the VM in between.
//!
//! When the create stops without [`DONE`] (its process died, or its future was dropped and
//! closed the socket), the guard resumes the VMM if the create had asked to pause it, then
//! runs the guest's `--resume-snapshot-network` with the host's clock, the same steps a
//! create that finishes takes, and logs one line. The guard runs in its own session so a
//! signal to the create's process group does not reach it, and it has no parent-death
//! signal: it has to outlive the create. It exits on [`DONE`], at once when the create
//! changed nothing, or after the undo, whose every step has a timeout.

use anyhow::{Context, Result};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::Path;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::{debug, error, warn};

use crate::cli::{Hypervisor, SnapshotSourceGuardArgs};

/// Guard to create: the guard has checked its descriptors and watches.
const READY: u8 = b'R';
/// Guard to create, after [`DONE`]: the guard has closed its copy of the per-VM lock.
const RELEASED: u8 = b'K';
/// Create to guard: the guest boundary's prepare is about to run.
pub(crate) const PREPARE: u8 = b'P';
/// Create to guard: the VMM pause is about to be asked for.
pub(crate) const PAUSE: u8 = b'V';
/// Create to guard: the create ran its own reopen of the source; stand down.
const DONE: u8 = b'D';

/// How long a create waits for a guard told [`DONE`] to answer [`RELEASED`] before killing
/// it. The guard is an exec of this binary that loads its config before it reads anything,
/// so a create shorter than that start waits here for it.
const STAND_DOWN_TIMEOUT: Duration = Duration::from_secs(30);

/// How long the guard waits for the guest's reopen. A healthy guest answers in well under a
/// second once its vCPUs run. An exec whose command never finishes would otherwise wait for
/// ever, and the guard holds the per-VM snapshot lock until it exits.
const GUEST_REOPEN_TIMEOUT: Duration = Duration::from_secs(60);

/// How far a create got with its source, as the guard knows it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Phase {
    /// Nothing changed yet.
    Untouched,
    /// The guest's gate may be closed and its payload frozen.
    Prepared,
    /// The VMM may be paused as well.
    Paused,
}

/// What the guard does once it knows the create's fate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// The create reopened its source itself.
    StandDown,
    /// The create stopped at this phase without reopening the source.
    Undo(Phase),
}

/// The bytes a create has sent, folded into what the guard must do if it stops now.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Progress {
    phase: Phase,
    done: bool,
}

impl Progress {
    pub(crate) fn new() -> Self {
        Self {
            phase: Phase::Untouched,
            done: false,
        }
    }

    /// Fold one byte from the create. Returns true once the create has said [`DONE`].
    pub(crate) fn apply(&mut self, byte: u8) -> bool {
        match byte {
            DONE => self.done = true,
            PREPARE if !self.done => self.phase = self.phase.max(Phase::Prepared),
            PAUSE if !self.done => self.phase = self.phase.max(Phase::Paused),
            PREPARE | PAUSE => {}
            other => warn!(
                byte = other,
                "source guard ignored an unknown byte from its create"
            ),
        }
        self.done
    }

    /// The verdict when the create has stopped (or said [`DONE`]).
    pub(crate) fn verdict(&self) -> Verdict {
        if self.done {
            Verdict::StandDown
        } else {
            Verdict::Undo(self.phase)
        }
    }
}

/// The VM a guard watches over.
pub(crate) struct GuardTarget<'a> {
    pub hypervisor: Hypervisor,
    pub api_socket: &'a Path,
    /// None for a guest that runs no fc-agent.
    pub vsock_socket: Option<&'a Path>,
    pub vm_id: &'a str,
    pub snapshot: &'a str,
    pub resume_timeout: Duration,
}

/// The create's handle on its guard. Dropping it without [`SourceGuard::disarm`] closes
/// the socket pair, which the guard takes as a create that stopped.
pub(crate) struct SourceGuard {
    channel: tokio::net::UnixStream,
    child: tokio::process::Child,
}

impl SourceGuard {
    /// Spawn the guard. `vm_lock` is the per-VM snapshot lock the caller holds.
    pub(crate) fn spawn(target: &GuardTarget<'_>, vm_lock: &std::fs::File) -> Result<Self> {
        let (ours, theirs) = std::os::unix::net::UnixStream::pair()
            .context("creating the source guard's socket pair")?;
        let pidfd = own_pidfd().context("opening a pidfd of this snapshot create")?;
        let inherited: [RawFd; 3] = [theirs.as_raw_fd(), pidfd.as_raw_fd(), vm_lock.as_raw_fd()];

        // /proc/self/exe is the image this process runs, even if the file was replaced.
        let mut command = tokio::process::Command::new("/proc/self/exe");
        command
            .args(["snapshot", "source-guard", "--channel-fd"])
            .arg(inherited[0].to_string())
            .arg("--pidfd")
            .arg(inherited[1].to_string())
            .arg("--lock-fd")
            .arg(inherited[2].to_string())
            .args(["--vm-id", target.vm_id, "--snapshot", target.snapshot])
            .arg("--hypervisor")
            .arg(match target.hypervisor {
                Hypervisor::Firecracker => "firecracker",
                Hypervisor::CloudHypervisor => "cloud-hypervisor",
            })
            .arg("--api-socket")
            .arg(target.api_socket)
            .arg("--resume-timeout-secs")
            .arg(target.resume_timeout.as_secs().max(1).to_string());
        if let Some(vsock) = target.vsock_socket {
            command.arg("--vsock-socket").arg(vsock);
        }
        command
            // The create's failpoints are the create's: a hold armed for it must not hold
            // the guard too.
            .env_remove("FCVM_FAILPOINT")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::inherit());
        // SAFETY: fcntl(2) and setsid(2) are async-signal-safe and touch only the child.
        // Every descriptor this process opens is close-on-exec; the child clears that flag
        // on exactly the three it is handed. setsid puts it in a session of its own, so a
        // signal to the create's process group (a terminal's Ctrl-C, a harness's killpg)
        // does not reach it. No parent-death signal is set: it must outlive the create.
        unsafe {
            command.pre_exec(move || {
                for fd in inherited {
                    if libc::fcntl(fd, libc::F_SETFD, 0) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command
            .spawn()
            .context("spawning the snapshot source guard")?;
        // The guard has its copies; this process keeps only its own end.
        drop(theirs);
        drop(pidfd);
        ours.set_nonblocking(true)
            .context("setting the source guard's socket non-blocking")?;
        let channel = tokio::net::UnixStream::from_std(ours)
            .context("registering the source guard's socket")?;
        Ok(Self { channel, child })
    }

    /// Tell the guard the prepare is next. Nothing waits for the guard to start: it holds
    /// its descriptors from the fork, and what is written here stays in the socket for it to
    /// read, also after this process is gone. [`SourceGuard::disarm`] checks that it started.
    pub(crate) async fn arm(&mut self) -> Result<()> {
        self.mark(PREPARE)
            .await
            .context("telling the snapshot source guard the prepare is next")
    }

    /// Tell the guard how far the create has got.
    pub(crate) async fn mark(&mut self, phase: u8) -> Result<()> {
        self.channel
            .write_all(&[phase])
            .await
            .context("writing to the snapshot source guard")
    }

    /// The create has run its own reopen of the source (whatever its result): tell the
    /// guard to stand down and wait until it has closed its copy of the per-VM lock, so it
    /// holds the lock no longer than the create does. The guard's exit is not waited for.
    ///
    /// Fails closed: a guard that never reported ready (it could not start, or rejected its
    /// descriptors) did not guard this create, and the create fails.
    pub(crate) async fn disarm(mut self) -> Result<()> {
        let sent = self.channel.write_all(&[DONE]).await;
        let mut seen = Vec::new();
        let answer = tokio::time::timeout(STAND_DOWN_TIMEOUT, async {
            let mut byte = [0u8; 1];
            loop {
                match self.channel.read(&mut byte).await {
                    Ok(0) | Err(_) => return false,
                    Ok(_) if byte[0] == RELEASED => return true,
                    Ok(_) => seen.push(byte[0]),
                }
            }
        })
        .await;
        let ready = seen.contains(&READY);
        match answer {
            Ok(true) if ready => {
                debug!("snapshot source guard stood down");
                Ok(())
            }
            Ok(_) => {
                let status = self.child.try_wait().ok().flatten();
                let error = anyhow::anyhow!(
                    "the snapshot source guard did not guard this create (ready: {ready}, \
                     stand-down sent: {}, exit status {status:?})",
                    sent.is_ok()
                );
                error!(error = %error, "snapshot source guard");
                Err(error)
            }
            Err(_) => {
                let _ = self.child.kill().await;
                let error = anyhow::anyhow!(
                    "the snapshot source guard did not stand down within {}s; killed it",
                    STAND_DOWN_TIMEOUT.as_secs()
                );
                error!(error = %error, "snapshot source guard");
                Err(error)
            }
        }
    }
}

/// A pidfd of this process.
fn own_pidfd() -> std::io::Result<OwnedFd> {
    // SAFETY: pidfd_open(2) with no flags returns a new close-on-exec descriptor or -1.
    let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, libc::getpid(), 0) };
    if raw < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: a fresh descriptor owned by nothing else.
    Ok(unsafe { OwnedFd::from_raw_fd(raw as RawFd) })
}

/// Take ownership of an inherited descriptor after checking it is open and of the
/// expected file type, and make it close-on-exec again.
fn inherited_fd(fd: RawFd, what: &str, file_type: Option<libc::mode_t>) -> Result<OwnedFd> {
    anyhow::ensure!(
        fd > 2,
        "{what} descriptor {fd} is not an inherited descriptor"
    );
    // SAFETY: fcntl on a descriptor number only reads this process's table.
    if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("{what} descriptor {fd}"));
    }
    if let Some(expected) = file_type {
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        // SAFETY: fstat writes the struct on success.
        if unsafe { libc::fstat(fd, stat.as_mut_ptr()) } < 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("{what} descriptor {fd}"));
        }
        // SAFETY: fstat succeeded.
        let mode = unsafe { stat.assume_init() }.st_mode;
        anyhow::ensure!(
            mode & libc::S_IFMT == expected,
            "{what} descriptor {fd} is not of the expected file type"
        );
    }
    // SAFETY: the descriptor is open, and the create handed it to this process alone.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// `fcvm snapshot source-guard`: watch one create and undo its changes if it stops.
pub async fn cmd_snapshot_source_guard(args: SnapshotSourceGuardArgs) -> Result<()> {
    // Executed as /proc/self/exe, so its name would be "exe" in ps and /proc/<pid>/comm.
    // SAFETY: PR_SET_NAME reads a NUL-terminated string of at most 16 bytes.
    unsafe { libc::prctl(libc::PR_SET_NAME, c"fcvm-src-guard".as_ptr()) };
    let channel = inherited_fd(args.channel_fd, "channel", Some(libc::S_IFSOCK))?;
    let pidfd = inherited_fd(args.pidfd, "pidfd", None)?;
    // Held, never read, until the create stands this guard down or the undo is finished: the
    // create's per-VM snapshot lock.
    let vm_lock = inherited_fd(args.lock_fd, "lock", Some(libc::S_IFREG))?;
    // SAFETY: pidfd_send_signal(2) with signal 0 checks the descriptor is a pidfd.
    let probe = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            pidfd.as_raw_fd(),
            0,
            std::ptr::null::<libc::siginfo_t>(),
            0,
        )
    };
    if probe < 0 {
        let error = std::io::Error::last_os_error();
        anyhow::ensure!(
            error.raw_os_error() == Some(libc::ESRCH),
            "pidfd descriptor {}: {error}",
            args.pidfd
        );
    }
    let mut channel = std::os::unix::net::UnixStream::from(channel);
    // A create that is already gone cannot read this, and what it sent before it went is
    // still in the socket: the watch reads it either way.
    if let Err(error) = std::io::Write::write_all(&mut channel, &[READY]) {
        debug!(error = %error, "the snapshot create is gone before its guard was ready");
    }
    channel
        .set_nonblocking(true)
        .context("setting the channel non-blocking")?;

    let (verdict, mut channel) = tokio::task::spawn_blocking(move || {
        let verdict = watch(&mut channel, &pidfd);
        (verdict, channel)
    })
    .await
    .context("the source guard's watch")?;
    match verdict? {
        Verdict::StandDown => {
            // The create waits for this answer, not for this process's exit.
            drop(vm_lock);
            let _ = channel.set_nonblocking(false);
            let _ = std::io::Write::write_all(&mut channel, &[RELEASED]);
            Ok(())
        }
        Verdict::Undo(Phase::Untouched) => Ok(()),
        Verdict::Undo(phase) => {
            undo(&args, phase, GUEST_REOPEN_TIMEOUT).await;
            // An exec abandoned by the timeout keeps a blocking thread, which the runtime's
            // shutdown would wait for. Exit now: that also releases the per-VM lock.
            std::process::exit(0);
        }
    }
}

/// Block until the create says [`DONE`] or stops: its end of the channel closes, or its
/// process is gone. Everything the create sent is read before its death is acted on.
fn watch(channel: &mut std::os::unix::net::UnixStream, pidfd: &OwnedFd) -> Result<Verdict> {
    use std::io::Read;
    let mut progress = Progress::new();
    let mut creator_gone = false;
    loop {
        let mut buffer = [0u8; 16];
        loop {
            match channel.read(&mut buffer) {
                Ok(0) => return Ok(progress.verdict()),
                Ok(n) => {
                    for &byte in &buffer[..n] {
                        if progress.apply(byte) {
                            return Ok(progress.verdict());
                        }
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                // A broken channel is a create that is gone.
                Err(_) => return Ok(progress.verdict()),
            }
        }
        if creator_gone {
            return Ok(progress.verdict());
        }
        let mut fds = [
            libc::pollfd {
                fd: channel.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: pidfd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        // SAFETY: poll(2) on two valid descriptors this function borrows.
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, -1) };
        if ready < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error).context("polling the create");
        }
        creator_gone = fds[1].revents != 0;
    }
}

/// Reopen the source the way a create that finishes does: the vsock epoch and the VMM
/// resume if the create asked for a pause, then the guest's reopen with the host's clock.
async fn undo(args: &SnapshotSourceGuardArgs, phase: Phase, guest_timeout: Duration) {
    let vm_id = args.vm_id.as_str();
    let snapshot = args.snapshot.as_str();
    // An owner process that died took its VMM with it: nothing is left to reopen.
    if let Err(error) = tokio::net::UnixStream::connect(&args.api_socket).await {
        warn!(vm_id, snapshot, ?phase, error = %error,
            "a snapshot create stopped before reopening its source, whose VMM is gone; nothing to undo");
        return;
    }
    let vmm = if phase == Phase::Paused {
        super::common::record_vsock_reset_for_snapshot(vm_id, snapshot).await;
        let timeout = Duration::from_secs(args.resume_timeout_secs);
        let resumed = match args.hypervisor {
            Hypervisor::Firecracker => {
                match crate::firecracker::FirecrackerClient::new(args.api_socket.clone()) {
                    Ok(client) => {
                        client
                            .with_timeout(timeout)
                            .patch_vm_state(crate::firecracker::api::VmState {
                                state: "Resumed".to_string(),
                            })
                            .await
                    }
                    Err(error) => Err(error),
                }
            }
            Hypervisor::CloudHypervisor => {
                crate::hypervisor::cloud_hypervisor::api::ChClient::new(args.api_socket.clone())
                    .with_timeout(timeout)
                    .resume_vm()
                    .await
            }
        };
        outcome(resumed)
    } else {
        "not paused".to_string()
    };
    let guest = match &args.vsock_socket {
        Some(vsock) => match tokio::time::timeout(
            guest_timeout,
            super::common::resume_paused_source(vsock, vm_id),
        )
        .await
        {
            Ok(result) => outcome(result),
            Err(_) => format!("no answer within {}s", guest_timeout.as_secs_f64()),
        },
        None => "no fc-agent".to_string(),
    };
    warn!(vm_id, snapshot, ?phase, vmm_resume = %vmm, guest_reopen = %guest,
        "undid the changes of a snapshot create that stopped before reopening its source");
}

fn outcome(result: Result<()>) -> String {
    match result {
        Ok(()) => "ok".to_string(),
        Err(error) => format!("{error:#}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn verdict(bytes: &[u8]) -> Verdict {
        let mut progress = Progress::new();
        for &byte in bytes {
            if progress.apply(byte) {
                break;
            }
        }
        progress.verdict()
    }

    /// What a guard does with each prefix of what a create sends.
    #[test]
    fn a_stopped_create_is_undone_to_the_phase_it_reached() {
        assert_eq!(verdict(b""), Verdict::Undo(Phase::Untouched));
        assert_eq!(verdict(&[PREPARE]), Verdict::Undo(Phase::Prepared));
        assert_eq!(verdict(&[PREPARE, PAUSE]), Verdict::Undo(Phase::Paused));
        assert_eq!(verdict(&[PREPARE, PAUSE, DONE]), Verdict::StandDown);
        assert_eq!(verdict(&[PREPARE, DONE]), Verdict::StandDown);
        assert_eq!(verdict(&[DONE]), Verdict::StandDown);
        // Nothing after DONE re-arms it.
        assert_eq!(verdict(&[PREPARE, DONE, PAUSE]), Verdict::StandDown);
    }

    /// The guard reads what the create sent before it acts on the create's death, so a
    /// create killed right after DONE is not undone, and one killed after PAUSE is.
    #[test]
    fn the_guard_reads_the_channel_before_acting_on_a_dead_create() {
        for (sent, expected) in [
            (vec![PREPARE, PAUSE, DONE], Verdict::StandDown),
            (vec![PREPARE, PAUSE], Verdict::Undo(Phase::Paused)),
        ] {
            let (mut create, mut guard) = std::os::unix::net::UnixStream::pair().unwrap();
            std::io::Write::write_all(&mut create, &sent).unwrap();
            // A pidfd that is already readable: a child that has exited.
            let child = std::process::Command::new("true").spawn().unwrap();
            let pid = child.id();
            let mut child = child;
            // SAFETY: pidfd_open with no flags, before the child is reaped.
            let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0) };
            assert!(raw >= 0, "{}", std::io::Error::last_os_error());
            // SAFETY: a fresh descriptor.
            let pidfd = unsafe { OwnedFd::from_raw_fd(raw as RawFd) };
            child.wait().unwrap();
            // The create's end stays open: only the pidfd says it is gone.
            guard.set_nonblocking(true).unwrap();
            assert_eq!(watch(&mut guard, &pidfd).unwrap(), expected);
            drop(create);
        }
    }

    /// A guest whose reopen is acknowledged and then never finishes does not keep the guard,
    /// and the per-VM lock it holds, for ever. The fake agent completes the exec handshake
    /// (CONNECT, request, ACK, GO) and then says nothing.
    #[test]
    fn a_guest_reopen_that_never_finishes_does_not_hold_the_guard() {
        use std::io::{BufRead, Write};
        let dir = tempfile::tempdir().unwrap();
        let api = dir.path().join("api.sock");
        let vsock = dir.path().join("vsock.sock");
        // Listening, never accepting: the guard's check that the VMM is still there connects.
        let _api = std::os::unix::net::UnixListener::bind(&api).unwrap();
        let agent = std::os::unix::net::UnixListener::bind(&vsock).unwrap();
        std::thread::spawn(move || {
            let (stream, _) = agent.accept().unwrap();
            let mut writer = stream.try_clone().unwrap();
            let mut reader = std::io::BufReader::new(stream);
            let mut line = String::new();
            reader.read_line(&mut line).unwrap(); // CONNECT <port>
            writer.write_all(b"OK 1\n").unwrap();
            line.clear();
            reader.read_line(&mut line).unwrap(); // the request
            writeln!(writer, "{}", exec_proto::HANDSHAKE_ACK).unwrap();
            line.clear();
            reader.read_line(&mut line).unwrap(); // GO
            loop {
                std::thread::park();
            }
        });
        let args = SnapshotSourceGuardArgs {
            channel_fd: -1,
            pidfd: -1,
            lock_fd: -1,
            vm_id: "vm-guard-test-no-state".to_string(),
            snapshot: "guard-test".to_string(),
            hypervisor: Hypervisor::Firecracker,
            api_socket: api,
            vsock_socket: Some(vsock),
            resume_timeout_secs: 1,
        };
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let finished = runtime.block_on(async {
            tokio::time::timeout(
                Duration::from_secs(10),
                undo(&args, Phase::Prepared, Duration::from_millis(500)),
            )
            .await
        });
        // The abandoned exec's blocking thread is still reading; do not wait for it.
        runtime.shutdown_background();
        assert!(
            finished.is_ok(),
            "the guard waited 10 s on a guest reopen that never finishes"
        );
    }

    /// Creates the guard's socket pair and pidfd with close-on-exec, so no other child of
    /// the create keeps them across its exec, and clears the flag only in the guard's own
    /// pre_exec, on exactly the descriptors it is handed.
    #[test]
    fn only_the_guard_inherits_its_descriptors() {
        let source = include_str!("snapshot_guard.rs");
        let spawn = &source[source.find("pub(crate) fn spawn(").unwrap()..];
        let spawn = &spawn[..spawn.find("\n    }\n").unwrap()];
        assert!(
            spawn.contains("UnixStream::pair()"),
            "the pair is created close-on-exec"
        );
        assert!(spawn.contains("libc::fcntl(fd, libc::F_SETFD, 0)"));
        assert!(spawn.contains("for fd in inherited"));
        assert!(spawn.contains("libc::setsid()"));
        assert!(
            !spawn.contains("PR_SET_PDEATHSIG"),
            "the guard must outlive the create"
        );
        assert!(
            spawn.contains("vm_lock.as_raw_fd()"),
            "the guard holds the per-VM lock"
        );
    }
}
