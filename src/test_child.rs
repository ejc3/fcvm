//! Helper processes that tests start for themselves and that cannot outlive the test.
//!
//! Several tests need a live process to aim code at. The UFFD server tests pin a stand-in
//! VMM and assert that it gets killed, the pasta tests hand a live child to
//! `wait_for_pid_file`, and a few integration tests do the same. All of them use `sleep`.
//! Neither `std::process::Child` nor `tokio::process::Child` ends its process when dropped,
//! so a test that failed between the spawn and its own cleanup left the `sleep` running for
//! the rest of its duration, up to an hour. An orphan keeps every descriptor it inherited,
//! including the lock `make` holds on the worktree's cargo target directory, so the next
//! `make` in that checkout blocked in `flock -x` until the sleep ran out.
//!
//! Two things end a [`TestChild`], because neither covers every exit on its own:
//!
//! * Dropping it kills the process and reaps it. That covers a test that returns and a test
//!   that panics, since unwinding drops it.
//! * The spawn arms `PR_SET_PDEATHSIG`, so the kernel signals the process when the thread
//!   that spawned it goes away. That covers a test process that is killed outright (what
//!   nextest does to a test that outlives its timeout), where nothing unwinds and no drop
//!   runs. It is also why a test child has to be spawned from the thread that runs the
//!   test, not from a helper thread that returns.
//!
//! The pdeath signal is SIGTERM on purpose. The UFFD tests assert that production code killed
//! their stand-in VMM with SIGKILL, and `a_dropped_clone_guard_kills_the_clone_it_guards`
//! asserts the same of the guard it tests. With SIGKILL as the pdeath signal too, a child
//! spawned by mistake from a short-lived helper thread would die by signal 9 when that
//! thread returned, and those assertions would pass without the kill under test.
//!
//! There is one implementation. The library declares this file as a `cfg(test)` module, and
//! `tests/common` includes the same file by path, because integration tests link the library
//! without its `cfg(test)` modules. That is why nothing in here refers to the rest of the
//! crate. `set_test_pdeathsig` in `tests/common` is a different hook for a different
//! process: it arms the fcvm processes that integration tests spawn, with SIGKILL.

use std::ops::{Deref, DerefMut};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::process::Stdio;
use std::time::{Duration, Instant};

/// A child process that is killed and reaped when it goes out of scope.
///
/// It dereferences to the child it wraps, so a test reads the PID, waits on it, or lends it
/// to production code the way it would a bare child. The guard acts only when it is dropped.
/// A test that asserts how its child died reads that status first, so the guard's own
/// SIGKILL can never stand in for the kill under test.
pub(crate) struct TestChild<C: KillAndReap>(C);

/// How a [`TestChild`] ends each kind of child. Both implementations do nothing to a child
/// the test already reaped.
pub(crate) trait KillAndReap {
    fn kill_and_reap(&mut self);
}

impl KillAndReap for std::process::Child {
    fn kill_and_reap(&mut self) {
        // Once the exit status has been collected `kill` returns `Ok` without signalling
        // (which also keeps it off a recycled PID) and `wait` returns that status again.
        if let Err(error) = self.kill() {
            not_killed(self.id(), &error);
            return;
        }
        let _ = self.wait();
    }
}

impl KillAndReap for tokio::process::Child {
    fn kill_and_reap(&mut self) {
        // `id` is `None` once tokio has collected the exit status.
        let Some(pid) = self.id() else { return };
        if let Err(error) = self.start_kill() {
            not_killed(pid, &error);
            return;
        }
        // tokio reaps from an async `wait`, which a drop cannot await. Block until the kernel
        // reports the exit but leave the status in place (WNOWAIT), then let tokio collect
        // it, so the `Child` and the kernel agree that the process is gone. Waiting by PID
        // is safe here: the child is this process's own and tokio has not reaped it, so the
        // number cannot have been recycled.
        let _ = waitid(
            libc::P_PID,
            pid as libc::id_t,
            libc::WEXITED | libc::WNOWAIT,
        );
        let _ = self.try_wait();
    }
}

/// A kill that failed signalled nothing, so there is no exit to wait for, and waiting would
/// block for the rest of the sleep. The process is already gone or is not this process's to
/// kill. Say so and leave it.
fn not_killed(pid: u32, error: &std::io::Error) {
    eprintln!("test child {pid} could not be killed and is not waited for: {error}");
}

impl<C: KillAndReap> TestChild<C> {
    /// Guard a child that the test spawned itself, from a command that needs more than
    /// `sleep` with no stdio. Start from [`sleep_command`], or arm a command of the test's
    /// own with [`die_with_the_spawning_thread`]. Without that only the drop protects the
    /// child.
    pub(crate) fn new(child: C) -> Self {
        Self(child)
    }
}

impl<C: KillAndReap> Deref for TestChild<C> {
    type Target = C;

    fn deref(&self) -> &C {
        &self.0
    }
}

impl<C: KillAndReap> DerefMut for TestChild<C> {
    fn deref_mut(&mut self) -> &mut C {
        &mut self.0
    }
}

impl<C: KillAndReap> Drop for TestChild<C> {
    fn drop(&mut self) {
        self.0.kill_and_reap();
    }
}

/// Arm `command` so that the kernel ends the process it spawns when the thread that spawns
/// it goes away. This is the kernel half of a [`TestChild`] on its own. It is for a child
/// that cannot sit behind the guard because the test moves it into the code under test,
/// where a kill on drop would hide the very leak such a test looks for. A tokio command
/// passes `as_std_mut()`.
pub(crate) fn die_with_the_spawning_thread(command: &mut std::process::Command) {
    use std::os::unix::process::CommandExt;

    let test_process = std::process::id() as libc::pid_t;
    // SAFETY: the hook runs in the forked child before exec and makes two async-signal-safe
    // calls. The parent PID was captured before the fork, so the hook allocates nothing.
    unsafe {
        command.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM, 0, 0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            // If the parent died between the fork and the `prctl`, no signal is coming:
            // refuse to start an unsupervised process.
            if libc::getppid() != test_process {
                return Err(std::io::Error::from_raw_os_error(libc::ESRCH));
            }
            Ok(())
        });
    }
}

/// `sleep <seconds>` with no stdio, armed to die with the thread that spawns it. A test
/// whose child needs one more `pre_exec` step adds it to this command and guards the child
/// it spawns with [`TestChild::new`].
pub(crate) fn sleep_command(seconds: u32) -> std::process::Command {
    let mut command = std::process::Command::new("sleep");
    command
        .arg(seconds.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    die_with_the_spawning_thread(&mut command);
    command
}

/// Spawn `sleep <seconds>` as a tokio child. Call it from the thread that runs the test,
/// inside that test's runtime.
pub(crate) fn spawn_sleep(seconds: u32) -> TestChild<tokio::process::Child> {
    TestChild::new(
        tokio::process::Command::from(sleep_command(seconds))
            .spawn()
            .expect("spawning a test's sleep child"),
    )
}

/// Spawn `sleep <seconds>` as a std child. Call it from the thread that runs the test.
pub(crate) fn spawn_sleep_std(seconds: u32) -> TestChild<std::process::Child> {
    TestChild::new(
        sleep_command(seconds)
            .spawn()
            .expect("spawning a test's sleep child"),
    )
}

/// A pidfd on a test's child, for the tests of the guard itself.
///
/// Those tests ask whether a process is gone after the code that owned it has finished with
/// it. By then its PID may have been recycled, so they ask through a handle that names the
/// process itself.
pub(crate) struct Pinned {
    pid: u32,
    pidfd: OwnedFd,
}

impl Pinned {
    pub(crate) fn new(pid: u32) -> Self {
        // SAFETY: pidfd_open(2) with no flags. A successful call returns a fresh descriptor
        // that this handle owns.
        let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0) };
        assert!(
            raw >= 0,
            "pidfd_open({pid}): {}",
            std::io::Error::last_os_error()
        );
        // SAFETY: `raw` was returned successfully above and nothing else owns it.
        let pidfd = unsafe { OwnedFd::from_raw_fd(raw as libc::c_int) };
        Self { pid, pidfd }
    }

    /// The PID, for messages only. Every question goes through the pidfd.
    pub(crate) fn pid(&self) -> u32 {
        self.pid
    }

    /// False only once the process has been reaped. A process that was killed and left as a
    /// zombie still reads as present.
    pub(crate) fn is_present(&self) -> bool {
        self.signal(0)
    }

    /// Wait up to `timeout` for the process to terminate, reaped or not.
    pub(crate) fn terminated_within(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut terminated = libc::pollfd {
            fd: self.pidfd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let millis = libc::c_int::try_from(left.as_millis()).unwrap_or(libc::c_int::MAX);
            // SAFETY: one initialised pollfd over a pidfd this handle owns.
            let ready = unsafe { libc::poll(&mut terminated, 1, millis) };
            // `poll` is never restarted after a signal handler runs, and the SIGCHLD for
            // this very child can be what interrupts it.
            let interrupted = ready < 0
                && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted;
            if !interrupted {
                return ready > 0;
            }
        }
    }

    /// SIGKILL the process. Cleanup for a test that found its guard did not work, so that
    /// the test does not leak the orphan it exists to catch.
    pub(crate) fn kill(&self) {
        self.signal(libc::SIGKILL);
    }

    /// Reap the process, which must be a child of this one that nothing else has reaped.
    /// Returns the signal that ended it, or `None` if it exited on its own.
    pub(crate) fn reap(&self) -> std::io::Result<Option<libc::c_int>> {
        let info = waitid(
            libc::P_PIDFD,
            self.pidfd.as_raw_fd() as libc::id_t,
            libc::WEXITED,
        )?;
        let signalled = matches!(info.si_code, libc::CLD_KILLED | libc::CLD_DUMPED);
        // SAFETY: `waitid` filled `info` for a child that terminated, which is the case
        // `si_status` is defined for.
        Ok(signalled.then(|| unsafe { info.si_status() }))
    }

    fn signal(&self, signal: libc::c_int) -> bool {
        // SAFETY: pidfd_send_signal(2) on an owned pidfd, with no siginfo and no flags.
        let rc = unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                self.pidfd.as_raw_fd(),
                signal,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            )
        };
        rc == 0
    }
}

/// `waitid(2)`, retried when a signal handler interrupts it.
fn waitid(
    id_type: libc::idtype_t,
    id: libc::id_t,
    options: libc::c_int,
) -> std::io::Result<libc::siginfo_t> {
    loop {
        // SAFETY: `info` is a valid out-pointer for the duration of the call, and all-zero
        // bytes are a valid `siginfo_t`.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        if unsafe { libc::waitid(id_type, id, &mut info, options) } == 0 {
            return Ok(info);
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}
