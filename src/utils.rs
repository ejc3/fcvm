//! Utility functions for process management and system operations.

use anyhow::Context as _;
use std::os::fd::AsRawFd;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tracing::{info, warn};

/// Event-driven watch on a directory for "a file appeared / changed" conditions.
///
/// Wraps an inotify fd in tokio's `AsyncFd` so readiness waits are epoll-driven
/// — no fixed-interval polling. The zero-race usage pattern is:
///
/// 1. `DirWatch::new(dir)` — register the watch FIRST;
/// 2. check the real condition (file exists / socket connects);
/// 3. if not met, `next_event().await`, then re-check — a file that appears
///    between (2) and (3) produced an event that (3) consumes, so it can never
///    be missed. Callers keep their own deadline/child-exit select arms.
///
/// Events are only wakeups: callers ALWAYS re-check the real condition, so
/// coalesced events, queue overflow, or unrelated files in the directory are
/// all safe (just extra re-checks).
pub struct DirWatch {
    async_fd: tokio::io::unix::AsyncFd<std::os::fd::RawFd>,
    // Keeps the inotify fd open; declared after async_fd so the AsyncFd is
    // dropped (deregistered from the reactor) before the fd is closed.
    inotify: nix::sys::inotify::Inotify,
}

impl DirWatch {
    /// Register an inotify watch on `dir` for file-appearance events
    /// (create, rename-in, close-after-write, attribute change).
    pub fn new(dir: &Path) -> anyhow::Result<Self> {
        use nix::sys::inotify::{AddWatchFlags, InitFlags, Inotify};
        let inotify = Inotify::init(InitFlags::IN_NONBLOCK | InitFlags::IN_CLOEXEC)
            .map_err(|e| anyhow::anyhow!("inotify_init1: {e}"))?;
        inotify
            .add_watch(
                dir,
                AddWatchFlags::IN_CREATE
                    | AddWatchFlags::IN_MOVED_TO
                    | AddWatchFlags::IN_CLOSE_WRITE
                    | AddWatchFlags::IN_ATTRIB,
            )
            .map_err(|e| anyhow::anyhow!("inotify_add_watch {}: {e}", dir.display()))?;
        let raw = std::os::fd::AsFd::as_fd(&inotify).as_raw_fd();
        let async_fd = tokio::io::unix::AsyncFd::new(raw)
            .map_err(|e| anyhow::anyhow!("registering inotify fd with tokio: {e}"))?;
        Ok(Self { async_fd, inotify })
    }

    /// Wait until at least one filesystem event has been consumed.
    ///
    /// Drains everything currently queued (the event batch is only a wakeup —
    /// the caller re-checks its condition afterwards).
    pub async fn next_event(&mut self) -> anyhow::Result<()> {
        loop {
            let mut guard = self
                .async_fd
                .readable()
                .await
                .map_err(|e| anyhow::anyhow!("waiting for inotify readability: {e}"))?;
            match self.inotify.read_events() {
                Ok(_events) => return Ok(()),
                Err(nix::errno::Errno::EAGAIN) => {
                    guard.clear_ready();
                    continue;
                }
                Err(e) => return Err(anyhow::anyhow!("reading inotify events: {e}")),
            }
        }
    }
}

/// Filesystem-event source used by readiness loops.
///
/// Keeping the wait loops generic over this tiny boundary lets their rare
/// inotify-unavailable and inotify-read-error paths be driven deterministically
/// in unit tests. Production uses `Option<DirWatch>`: `None` is an unavailable
/// watch whose event future never resolves, leaving the caller's safety tick and
/// deadline as the correctness path.
pub(crate) trait DirEventSource {
    fn is_available(&self) -> bool;

    async fn next_event(&mut self) -> anyhow::Result<()>;
}

impl DirEventSource for Option<DirWatch> {
    fn is_available(&self) -> bool {
        self.is_some()
    }

    async fn next_event(&mut self) -> anyhow::Result<()> {
        match self {
            Some(watch) => watch.next_event().await,
            None => std::future::pending().await,
        }
    }
}

/// Check if a process is alive by checking /proc/{pid} existence.
///
/// This is more reliable than sending signal 0 because it doesn't require
/// any special permissions - any user can check if /proc/{pid} exists.
///
/// # Arguments
/// * `pid` - Process ID to check
///
/// # Returns
/// `true` if the process exists, `false` otherwise
pub fn is_process_alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{}", pid)).exists()
}

/// Read the start time of a process in clock ticks since boot (field 22 of
/// `/proc/<pid>/stat`). Returns `None` if the process doesn't exist or the field
/// can't be parsed.
///
/// A `(pid, start_time)` pair uniquely identifies a process even after the OS
/// reuses the PID for something else. This is fcvm's ONE definition of that
/// identity: the state manager persists it as `VmState::pid_start_time`, and the
/// UFFD server pins the VMM on the other end of a connection with it before ever
/// signalling that PID.
pub fn process_start_time(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{}/stat", pid)).ok()?;
    // The comm field (field 2) may contain spaces and parentheses; everything
    // after the last ')' is space-separated starting at field 3 (state), so
    // starttime (field 22) is the 20th token after the closing paren.
    let after_comm = stat.rsplit_once(')')?.1;
    after_comm.split_whitespace().nth(19)?.parse().ok()
}

/// A watch on another process's lifetime, established up front and awaited later.
///
/// Split deliberately into "open" and "wait": opening can FAIL (no permission, kernel
/// too old, process already gone), and a caller must be able to tell that apart from
/// "the process died". Conflating them means a clone that merely could not be watched
/// gets killed as though its dependency had crashed — a false positive that destroys
/// healthy work. Open at startup, fail loudly there, and treat every later wakeup as a
/// real death.
pub struct ProcessWatch {
    async_fd: tokio::io::unix::AsyncFd<std::os::fd::OwnedFd>,
    pid: u32,
}

impl ProcessWatch {
    /// Pin `pid` for later waiting, or explain why it cannot be pinned.
    ///
    /// `Ok(None)` means the process was ALREADY gone — not an error, and not something to
    /// wait for; the caller has simply learned the answer immediately.
    ///
    /// PID reuse is handled by bracketing the `pidfd_open` with `/proc/<pid>/stat`
    /// start-time reads. A pidfd pins whatever process holds the PID *at the moment of
    /// the call*, so if the target died just before, this could otherwise pin a stranger
    /// and then wait forever on a process nobody cares about — the failure mode being
    /// watched for would pass silently.
    pub fn open(pid: u32) -> anyhow::Result<Option<Self>> {
        use std::os::fd::{FromRawFd, OwnedFd};

        let before = process_start_time(pid);
        if before.is_none() {
            return Ok(None); // already gone
        }

        // SAFETY: pidfd_open(2) with no flags; the fd is owned by the OwnedFd below.
        let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0) };
        if raw < 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::ESRCH) {
                return Ok(None); // died while we were looking
            }
            return Err(anyhow::Error::new(err))
                .with_context(|| format!("pidfd_open on PID {pid}"));
        }
        // SAFETY: a fresh, valid fd we just created and have not shared.
        let fd = unsafe { OwnedFd::from_raw_fd(raw as std::os::fd::RawFd) };

        // If the start time moved, the PID was recycled between our two reads: the
        // process we meant to watch is already dead and this fd points at a stranger.
        if process_start_time(pid) != before {
            return Ok(None);
        }

        let async_fd = tokio::io::unix::AsyncFd::new(fd)
            .with_context(|| format!("registering pidfd for PID {pid} with the reactor"))?;
        Ok(Some(Self { async_fd, pid }))
    }

    /// The PID being watched, for messages.
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// Resolves when the watched process exits. A pidfd becomes readable on exit, so this
    /// is a real edge, not a poll.
    pub async fn exited(&mut self) {
        // A readability error here would mean the reactor itself is broken; there is no
        // meaningful recovery and reporting "still alive" would be a lie, so treat any
        // wakeup as the exit it almost certainly is.
        let _ = self.async_fd.readable().await.map(|mut g| g.clear_ready());
    }
}

/// Check whether `pid` runs the same executable name (`/proc/<pid>/comm`) as the current process.
///
/// State files can outlive a crashed/SIGKILL'd fcvm process, so a PID recorded in a state file
/// may have been reused by an unrelated process. Callers use this as an identity check before
/// signalling such PIDs: an fcvm process must never SIGTERM a stranger's PID.
///
/// Returns `false` if the process does not exist or its comm cannot be read.
pub fn is_same_process_name(pid: u32) -> bool {
    let other = std::fs::read_to_string(format!("/proc/{}/comm", pid));
    let me = std::fs::read_to_string("/proc/self/comm");
    match (other, me) {
        (Ok(other), Ok(me)) => other.trim() == me.trim(),
        _ => false,
    }
}

/// Gracefully kill a process by sending SIGTERM first, then SIGKILL if needed.
///
/// This allows the process to run cleanup handlers (network teardown, file cleanup, etc.)
/// before being forcefully terminated.
///
/// # Arguments
/// * `pid` - Process ID to kill
/// * `timeout_ms` - Maximum time to wait for graceful shutdown (in milliseconds)
///
/// # Behavior
/// 1. Sends SIGTERM to allow graceful shutdown
/// 2. Waits up to `timeout_ms` for process to exit
/// 3. Sends SIGKILL if still running
pub fn graceful_kill(pid: u32, timeout_ms: u64) {
    // Send SIGTERM first for graceful shutdown
    let _ = Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .output();

    // Wait for process to exit gracefully
    let interval = Duration::from_millis(100);
    let iterations = (timeout_ms / 100).max(1);

    for _ in 0..iterations {
        if !is_process_alive(pid) {
            return; // Process exited gracefully
        }
        thread::sleep(interval);
    }

    // Force kill if still running
    let _ = Command::new("kill")
        .args(["-KILL", &pid.to_string()])
        .output();
}

/// Async version of graceful_kill for use in async contexts.
///
/// Same behavior as `graceful_kill` but uses tokio for sleeping.
pub async fn graceful_kill_async(pid: u32, timeout_ms: u64) {
    // Send SIGTERM first for graceful shutdown
    let _ = tokio::process::Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .output()
        .await;

    // Wait for process to exit gracefully
    let interval = tokio::time::Duration::from_millis(100);
    let iterations = (timeout_ms / 100).max(1);

    for _ in 0..iterations {
        if !is_process_alive(pid) {
            return; // Process exited gracefully
        }
        tokio::time::sleep(interval).await;
    }

    // Force kill if still running
    let _ = tokio::process::Command::new("kill")
        .args(["-KILL", &pid.to_string()])
        .output()
        .await;
}

/// Strip Firecracker timestamp and instance prefix from log lines.
/// Input:  "2025-11-15T17:18:55.027478889 [anonymous-instance:main] message"
/// Output: "message"
pub fn strip_firecracker_prefix(line: &str) -> &str {
    let mut result = line;

    // Strip timestamp if present (starts with year like "20XX-")
    if let Some(pos) = result.find(' ') {
        if result.starts_with("20") && result.chars().nth(4) == Some('-') {
            result = &result[pos + 1..];
        }
    }

    // Strip [anonymous-instance:xxx] prefix if present.
    // Only strip brackets containing ':' (Firecracker format is [instance:thread]).
    // This preserves guest-originated prefixes like [fc-agent] which have no colon.
    if result.starts_with('[') {
        if let Some(end_pos) = result.find("] ") {
            if result[1..end_pos].contains(':') {
                result = &result[end_pos + 2..];
            }
        }
    }

    result
}

/// A process spawned by [`spawn_streaming`], plus the background tasks draining
/// its stdout and stderr pipes.
///
/// The reader handles are returned, not detached, so that a caller which sees
/// the child exit early can WAIT for the stderr reader to reach EOF before
/// formatting an error out of the tail it captured. Without a handle there is
/// nothing to await: every such caller resorted to a fixed sleep, which is both
/// a tax on the happy path and too short under load — reporting "no stderr
/// captured" exactly when the process printed the reason it died.
pub struct StreamingChild {
    /// The spawned process. The caller owns its lifecycle.
    pub child: tokio::process::Child,
    /// Completes when stdout hits EOF and every line has reached the callback.
    pub stdout_reader: tokio::task::JoinHandle<()>,
    /// Completes when stderr hits EOF and every line has reached the callback.
    pub stderr_reader: tokio::task::JoinHandle<()>,
}

/// Wait for a [`StreamingChild`] stderr reader to finish, bounded by `timeout`.
///
/// Call this on an error path that has already observed the child exit and is
/// about to render its stderr tail. Completion is the real signal that the tail
/// is whole: the reader task ends only at EOF, and the write end of the pipe is
/// closed by the exit that got us here, so EOF normally arrives in microseconds.
///
/// The bound covers the one case where EOF can be delayed indefinitely — a
/// grandchild inherited the pipe and holds it open. There the error is reported
/// with whatever was captured rather than hanging.
///
/// The handle is taken, so a second call is a no-op.
pub async fn wait_for_stderr_eof(
    reader: &mut Option<tokio::task::JoinHandle<()>>,
    timeout: Duration,
) {
    let Some(mut handle) = reader.take() else {
        return;
    };
    if tokio::time::timeout(timeout, &mut handle).await.is_err() {
        // Timing out must also stop the reader: dropping a JoinHandle only
        // detaches the task, which would leave it consuming the pipe (and
        // logging under this attempt's context) while the next attempt runs.
        handle.abort();
        warn!(
            timeout_ms = timeout.as_millis() as u64,
            "stderr pipe did not reach EOF within the bound; error output may be truncated"
        );
    }
}

/// Spawn a command and stream its output via tracing logs.
///
/// Takes a closure that receives each line and logs it appropriately.
/// Returns the child and its reader tasks (caller must manage lifecycle).
pub fn spawn_streaming<F>(
    mut cmd: tokio::process::Command,
    log_line: F,
) -> anyhow::Result<StreamingChild>
where
    F: Fn(&str, bool) + Send + Sync + Clone + 'static,
{
    // Stdin is null to prevent child from competing with parent for terminal input
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let mut child = cmd.spawn()?;

    // Both pipes were just configured above, so `take()` yields them here.
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("spawned child has no stdout pipe"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow::anyhow!("spawned child has no stderr pipe"))?;

    // Stream stdout (is_stderr=false)
    let log = log_line.clone();
    let stdout_reader = tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            log(&line, false);
        }
    });

    // Stream stderr (is_stderr=true)
    let stderr_reader = tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            log_line(&line, true);
        }
    });

    Ok(StreamingChild {
        child,
        stdout_reader,
        stderr_reader,
    })
}

/// Run a command and stream its output via tracing at INFO/WARN level.
///
/// Simple version with a prefix. For custom logging logic, use spawn_streaming.
pub async fn run_streaming(
    cmd: tokio::process::Command,
    prefix: &str,
) -> anyhow::Result<std::process::ExitStatus> {
    let prefix = prefix.to_string();
    // The reader handles are deliberately dropped: this helper logs each line as
    // it arrives and renders no tail afterwards, so it has nothing to wait for.
    // Dropping detaches the tasks, which run on to EOF on their own.
    let mut spawned = spawn_streaming(cmd, move |line, is_stderr| {
        if is_stderr {
            warn!("[{}] {}", prefix, line);
        } else {
            info!("[{}] {}", prefix, line);
        }
    })?;
    Ok(spawned.child.wait().await?)
}

/// Result of waiting for namespace readiness.
#[derive(Debug, PartialEq)]
pub enum NamespaceReadyResult {
    /// Namespace is ready (uid_map/gid_map written, nsenter probe succeeded)
    Ready,
    /// Holder process died while waiting
    HolderDied,
    /// Deadline expired while holder was still alive
    TimedOut,
}

/// Wait for a user namespace to be ready by checking uid_map.
///
/// When `unshare --user` creates a namespace, the uid_map initially has
/// an identity mapping "0 0 4294967295" before the actual mapping is written
/// (externally by `setup_namespace_mappings()`).
/// setns() fails with EINVAL until the real mapping (e.g., "0 1000 1") is written.
///
/// This function polls uid_map until it no longer contains the identity mapping,
/// then verifies nsenter works. It checks holder liveness on each iteration to
/// short-circuit if the holder dies.
///
/// # Arguments
/// * `holder_pid` - PID of the namespace holder process
/// * `deadline` - Absolute deadline for readiness
///
/// # Returns
/// `NamespaceReadyResult` indicating ready, holder died, or timed out
pub async fn wait_for_namespace_ready(
    holder_pid: u32,
    deadline: std::time::Instant,
) -> NamespaceReadyResult {
    use tracing::{debug, info, warn};

    let uid_map_path = format!("/proc/{}/uid_map", holder_pid);
    let mut iterations = 0u32;

    loop {
        iterations += 1;

        // Check if uid_map exists and has been properly written
        match tokio::fs::read_to_string(&uid_map_path).await {
            Ok(content) => {
                let trimmed = content.trim();
                // Namespace is ready when:
                // 1. uid_map is not empty (has been written)
                // 2. Does not contain identity mapping (4294967295)
                //
                // On host: initial mapping is "0 0 4294967295", replaced with "0 1000 1"
                // In container: initial mapping is empty, replaced with "0 0 1"
                //
                // ALSO check gid_map - both must be written for setns() to succeed
                let gid_map_path = format!("/proc/{}/gid_map", holder_pid);
                let gid_map = tokio::fs::read_to_string(&gid_map_path)
                    .await
                    .unwrap_or_default();
                let gid_trimmed = gid_map.trim();

                if !trimmed.is_empty() && !gid_trimmed.is_empty() && !content.contains("4294967295")
                {
                    // Maps are written - now verify nsenter actually works
                    // Some kernel states require additional settling time
                    let probe = tokio::process::Command::new("nsenter")
                        .args([
                            "-t",
                            &holder_pid.to_string(),
                            "-U",
                            "-n",
                            "--preserve-credentials",
                            "--",
                            "true",
                        ])
                        .output()
                        .await;

                    match probe {
                        Ok(output) if output.status.success() => {
                            info!(
                                holder_pid = holder_pid,
                                iterations = iterations,
                                uid_map = %trimmed,
                                gid_map = %gid_trimmed,
                                "namespace ready (nsenter probe succeeded)"
                            );
                            return NamespaceReadyResult::Ready;
                        }
                        Ok(output) => {
                            // nsenter failed even though maps are written - continue waiting
                            let stderr = String::from_utf8_lossy(&output.stderr);
                            debug!(
                                holder_pid = holder_pid,
                                iterations = iterations,
                                stderr = %stderr.trim(),
                                "nsenter probe failed, continuing to wait"
                            );
                        }
                        Err(e) => {
                            warn!(holder_pid = holder_pid, error = %e, "nsenter probe spawn failed");
                            return NamespaceReadyResult::HolderDied;
                        }
                    }
                }

                // Log what we're waiting for
                if iterations == 1 || iterations % 50 == 0 {
                    debug!(
                        holder_pid = holder_pid,
                        iterations = iterations,
                        uid_map_empty = trimmed.is_empty(),
                        gid_map_empty = gid_trimmed.is_empty(),
                        has_identity = content.contains("4294967295"),
                        "waiting for namespace to be ready"
                    );
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Process died — /proc/{pid} gone
                if !is_process_alive(holder_pid) {
                    debug!(
                        holder_pid = holder_pid,
                        "holder process died while waiting for uid_map"
                    );
                    return NamespaceReadyResult::HolderDied;
                }
            }
            Err(e) => {
                warn!(holder_pid = holder_pid, error = %e, "failed to read uid_map");
                return NamespaceReadyResult::HolderDied;
            }
        }

        if std::time::Instant::now() >= deadline {
            warn!(
                holder_pid = holder_pid,
                iterations = iterations,
                "namespace not ready, deadline expired"
            );
            return NamespaceReadyResult::TimedOut;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

/// Network/user/mount-namespace isolation to apply to a spawned VMM process before exec.
///
/// VMM-neutral: both Firecracker and Cloud Hypervisor run as a child process inside the
/// same namespaces with the same parent-death signal. See [`install_namespace_pre_exec`].
#[derive(Debug, Clone, Default)]
pub struct NamespaceParams {
    /// VM id, for log context only.
    pub vm_id: String,
    /// Bridged/routed network namespace name (`/var/run/netns/<id>`).
    pub namespace_id: Option<String>,
    /// User namespace path for rootless clones (`/proc/<pid>/ns/user`), entered first to
    /// gain CAP_SYS_ADMIN for the mount-redirect operations.
    pub user_namespace_path: Option<std::path::PathBuf>,
    /// Net namespace path for rootless clones (`/proc/<pid>/ns/net`).
    pub net_namespace_path: Option<std::path::PathBuf>,
    /// Mount-namespace redirects `(baseline_dirs, clone_dir)` for clone isolation.
    pub mount_redirects: Option<(Vec<std::path::PathBuf>, std::path::PathBuf)>,
}

/// Install `pre_exec` hooks on a VMM command that (1) enter the configured user/mount/network
/// namespaces and apply the clone mount redirects, and (2) set `PR_SET_PDEATHSIG=SIGKILL` so
/// the VMM dies with fcvm. Shared by the Firecracker and Cloud Hypervisor backends so both
/// get identical isolation + parent-death behavior. Call once on the command before spawn.
///
/// The pdeathsig hook is installed LAST so it runs AFTER `setns(CLONE_NEWUSER)` — a credential
/// change zeros `task->pdeath_signal`, so setting it earlier would be lost.
pub fn install_namespace_pre_exec(
    cmd: &mut tokio::process::Command,
    ns: &NamespaceParams,
) -> anyhow::Result<()> {
    use anyhow::Context;

    let ns_id_clone = ns.namespace_id.clone();
    let mount_redirects_clone = ns.mount_redirects.clone();
    let user_ns_path_clone = ns.user_namespace_path.clone();
    let net_ns_path_clone = ns.net_namespace_path.clone();

    // Ensure baseline directories exist for bind mount targets. The baseline VMs may have
    // been cleaned up, but we need the directories present as mount targets.
    if let Some((ref baseline_dirs, _)) = mount_redirects_clone {
        for baseline_dir in baseline_dirs {
            if !baseline_dir.exists() {
                std::fs::create_dir_all(baseline_dir)
                    .context("creating baseline directory for mount redirect")?;
            }
        }
    }

    if ns_id_clone.is_some()
        || mount_redirects_clone.is_some()
        || user_ns_path_clone.is_some()
        || net_ns_path_clone.is_some()
    {
        use std::ffi::CString;
        let vm_id = ns.vm_id.clone();

        // Prepare CStrings outside the closure (async-signal-safe requirement).
        let ns_path_cstr = if let Some(ref ns_id) = ns_id_clone {
            info!(target: "vm", vm_id = %vm_id, namespace = %ns_id, "entering network namespace");
            Some(
                CString::new(format!("/var/run/netns/{}", ns_id))
                    .context("namespace ID contains invalid characters (null bytes)")?,
            )
        } else {
            None
        };

        let user_ns_cstr = if let Some(ref path) = user_ns_path_clone {
            info!(target: "vm", vm_id = %vm_id, path = %path.display(), "will enter user namespace in pre_exec");
            Some(
                CString::new(path.to_string_lossy().as_bytes())
                    .context("user namespace path contains invalid characters")?,
            )
        } else {
            None
        };

        let net_ns_cstr = if let Some(ref path) = net_ns_path_clone {
            info!(target: "vm", vm_id = %vm_id, path = %path.display(), "will enter net namespace in pre_exec");
            Some(
                CString::new(path.to_string_lossy().as_bytes())
                    .context("net namespace path contains invalid characters")?,
            )
        } else {
            None
        };

        let mount_paths = if let Some((ref baseline_dirs, ref clone_dir)) = mount_redirects_clone {
            info!(target: "vm", vm_id = %vm_id,
                baseline_dirs = ?baseline_dirs.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
                clone = %clone_dir.display(),
                "setting up mount namespace for mount redirects");
            let clone_cstr = CString::new(clone_dir.to_string_lossy().as_bytes())
                .context("clone path contains invalid characters")?;
            let baseline_cstrs: Vec<CString> = baseline_dirs
                .iter()
                .map(|p| {
                    CString::new(p.to_string_lossy().as_bytes())
                        .context("baseline path contains invalid characters")
                })
                .collect::<anyhow::Result<Vec<_>>>()?;
            Some((baseline_cstrs, clone_cstr))
        } else {
            None
        };

        // SAFETY: pre_exec runs after fork() but before exec().
        // 1. Only async-signal-safe functions are called (open, setns, unshare, mount).
        // 2. No heap allocations after fork (CStrings created before fork).
        // 3. File descriptors are properly owned via OwnedFd.
        // 4. The closure captures only CStrings and Option types.
        unsafe {
            cmd.pre_exec(move || {
                use nix::fcntl::{open, OFlag};
                use nix::mount::{mount, MsFlags};
                use nix::sched::{setns, unshare, CloneFlags};
                use nix::sys::stat::Mode;
                use std::os::unix::io::{FromRawFd, OwnedFd};

                // Step 0: Enter user namespace if specified (rootless clones). MUST be first
                // to get CAP_SYS_ADMIN for mount operations. The user namespace was created
                // by the holder (unshare --user --net) with external UID/GID mappings, so
                // entering it gives UID 0 with full capabilities inside the namespace.
                if let Some(ref user_ns_path) = user_ns_cstr {
                    let ns_fd_raw = open(user_ns_path.as_c_str(), OFlag::O_RDONLY, Mode::empty())
                        .map_err(|e| {
                        std::io::Error::other(format!("failed to open user namespace: {}", e))
                    })?;
                    let ns_fd = OwnedFd::from_raw_fd(ns_fd_raw);
                    setns(&ns_fd, CloneFlags::CLONE_NEWUSER).map_err(|e| {
                        std::io::Error::other(format!("failed to enter user namespace: {}", e))
                    })?;
                }

                // Step 1: Mount namespace for path redirects (before entering network ns).
                if let Some((ref baseline_cstrs, ref clone_cstr)) = mount_paths {
                    unshare(CloneFlags::CLONE_NEWNS).map_err(|e| {
                        std::io::Error::other(format!("failed to unshare mount namespace: {}", e))
                    })?;
                    // Make our mount namespace private so mounts don't propagate.
                    mount::<str, str, str, str>(
                        None,
                        "/",
                        None,
                        MsFlags::MS_REC | MsFlags::MS_PRIVATE,
                        None,
                    )
                    .map_err(|e| {
                        std::io::Error::other(format!("failed to make mount private: {}", e))
                    })?;
                    // Bind mount clone_dir over each baseline_dir so the VMM sees the clone's
                    // files when accessing any baseline's path.
                    for baseline_cstr in baseline_cstrs {
                        mount(
                            Some(clone_cstr.as_c_str()),
                            baseline_cstr.as_c_str(),
                            None::<&str>,
                            MsFlags::MS_BIND,
                            None::<&str>,
                        )
                        .map_err(|e| {
                            std::io::Error::other(format!(
                                "failed to bind mount {:?} over {:?}: {}",
                                clone_cstr, baseline_cstr, e
                            ))
                        })?;
                    }
                }

                // Step 2: Enter network namespace if specified — net_ns_cstr
                // (/proc/PID/ns/net, rootless clones, preferred) or ns_path_cstr
                // (/var/run/netns/NAME, bridged mode).
                let net_ns_to_enter = net_ns_cstr.as_ref().or(ns_path_cstr.as_ref());
                if let Some(ns_path) = net_ns_to_enter {
                    let ns_fd_raw = open(ns_path.as_c_str(), OFlag::O_RDONLY, Mode::empty())
                        .map_err(|e| {
                            std::io::Error::other(format!("failed to open net namespace: {}", e))
                        })?;
                    let ns_fd = OwnedFd::from_raw_fd(ns_fd_raw);
                    setns(&ns_fd, CloneFlags::CLONE_NEWNET).map_err(|e| {
                        std::io::Error::other(format!("failed to enter net namespace: {}", e))
                    })?;
                }

                Ok(())
            });
        }
    }

    // Kill the VMM if the parent (fcvm) dies, even from SIGKILL. Unconditional; must be the
    // LAST pre_exec so it runs AFTER setns(CLONE_NEWUSER), which clears pdeath_signal.
    unsafe {
        cmd.pre_exec(|| {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "current_thread")]
    async fn wait_for_stderr_eof_waits_for_delayed_bulk_output() {
        use std::sync::{Arc, Mutex};
        use tokio::io::AsyncWriteExt;

        const BULK_LINES: usize = 512;
        let (read_end, mut write_end) = tokio::io::duplex(256);
        let captured = Arc::new(Mutex::new(Vec::<String>::new()));
        let reader_captured = Arc::clone(&captured);
        let reader = tokio::spawn(async move {
            let mut lines = BufReader::new(read_end).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                reader_captured
                    .lock()
                    .expect("captured stderr mutex poisoned")
                    .push(line);
            }
        });

        let (bulk_written_tx, bulk_written_rx) = tokio::sync::oneshot::channel();
        let (write_final_tx, write_final_rx) = tokio::sync::oneshot::channel();
        let writer = tokio::spawn(async move {
            for index in 0..BULK_LINES {
                write_end
                    .write_all(format!("bulk-{index}\n").as_bytes())
                    .await
                    .expect("writing bulk stderr");
            }
            bulk_written_tx
                .send(())
                .expect("bulk-write observer dropped");
            write_final_rx.await.expect("final-line gate dropped");
            write_end
                .write_all(b"delayed-final\n")
                .await
                .expect("writing delayed final stderr");
            write_end.shutdown().await.expect("closing stderr writer");
        });
        bulk_written_rx.await.expect("bulk writer exited early");

        let (wait_started_tx, wait_started_rx) = tokio::sync::oneshot::channel();
        let waiter = tokio::spawn(async move {
            let mut reader = Some(reader);
            wait_started_tx
                .send(())
                .expect("wait-start observer dropped");
            wait_for_stderr_eof(&mut reader, Duration::from_secs(2)).await;
            reader
        });
        wait_started_rx.await.expect("stderr waiter exited early");
        assert!(
            !waiter.is_finished(),
            "stderr waiter returned while the writer still held the pipe open"
        );

        write_final_tx
            .send(())
            .expect("stderr waiter dropped the final-line gate");
        let remaining_reader = waiter.await.expect("stderr waiter panicked");
        writer.await.expect("stderr writer panicked");
        assert!(
            remaining_reader.is_none(),
            "stderr waiter must consume its reader handle"
        );

        let captured = captured.lock().expect("captured stderr mutex poisoned");
        assert_eq!(captured.len(), BULK_LINES + 1);
        assert_eq!(captured.first().map(String::as_str), Some("bulk-0"));
        assert_eq!(captured.last().map(String::as_str), Some("delayed-final"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn wait_for_stderr_eof_accepts_a_late_waiter_after_eof() {
        use std::sync::{Arc, Mutex};
        use tokio::io::AsyncWriteExt;

        let (read_end, mut write_end) = tokio::io::duplex(64);
        let captured = Arc::new(Mutex::new(Vec::<String>::new()));
        let reader_captured = Arc::clone(&captured);
        let (eof_tx, eof_rx) = tokio::sync::oneshot::channel();
        let reader = tokio::spawn(async move {
            let mut lines = BufReader::new(read_end).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                reader_captured
                    .lock()
                    .expect("captured stderr mutex poisoned")
                    .push(line);
            }
            eof_tx.send(()).expect("EOF observer dropped");
        });

        write_end
            .write_all(b"already-drained\n")
            .await
            .expect("writing stderr before EOF");
        write_end.shutdown().await.expect("closing stderr writer");
        drop(write_end);
        eof_rx.await.expect("stderr reader exited before EOF");
        assert!(
            reader.is_finished(),
            "reader must be terminal before waiting"
        );

        let mut reader = Some(reader);
        wait_for_stderr_eof(&mut reader, Duration::from_secs(2)).await;
        assert!(
            reader.is_none(),
            "late waiter must consume the reader handle"
        );
        assert_eq!(
            captured
                .lock()
                .expect("captured stderr mutex poisoned")
                .as_slice(),
            ["already-drained"]
        );

        // A second waiter is explicitly a no-op, not an error or another wait.
        wait_for_stderr_eof(&mut reader, Duration::ZERO).await;
    }

    #[tokio::test]
    async fn dir_watch_consumes_event_queued_between_check_and_park() {
        let dir = tempfile::tempdir().expect("create watched directory");
        let appeared = dir.path().join("appeared");
        let mut watch = DirWatch::new(dir.path()).expect("register directory watch");

        // This is the readiness-loop ordering: watch first, condition check,
        // then the producer wins immediately before `next_event()` is polled.
        assert!(!appeared.exists());
        std::fs::write(&appeared, b"ready").expect("publish watched file");

        tokio::time::timeout(Duration::from_secs(1), watch.next_event())
            .await
            .expect("queued inotify event was lost")
            .expect("consume queued inotify event");
    }

    #[test]
    fn test_is_process_alive_current_process() {
        // Current process should always be alive
        assert!(is_process_alive(std::process::id()));
    }

    #[test]
    fn test_is_process_alive_nonexistent() {
        // PID 4294967295 (u32::MAX) is extremely unlikely to exist
        assert!(!is_process_alive(u32::MAX));
    }

    #[test]
    fn test_is_process_alive_init() {
        // PID 1 (init/systemd) should always exist on Linux
        assert!(is_process_alive(1));
    }

    #[test]
    fn test_is_same_process_name_current_process() {
        // The current process trivially runs the same executable as itself
        assert!(is_same_process_name(std::process::id()));
    }

    #[test]
    fn test_is_same_process_name_other_process() {
        // PID 1 (init/systemd) is never the test binary
        assert!(!is_same_process_name(1));
    }

    #[test]
    fn test_is_same_process_name_nonexistent() {
        assert!(!is_same_process_name(u32::MAX));
    }

    #[test]
    fn test_strip_firecracker_prefix_full() {
        assert_eq!(
            strip_firecracker_prefix(
                "2025-11-15T17:18:55.027478889 [anonymous-instance:main] Running Firecracker"
            ),
            "Running Firecracker"
        );
    }

    #[test]
    fn test_strip_firecracker_prefix_preserves_fc_agent() {
        // Guest serial output without Firecracker prefix — [fc-agent] must NOT be stripped
        assert_eq!(
            strip_firecracker_prefix("[fc-agent] handling restore (epoch=1)"),
            "[fc-agent] handling restore (epoch=1)"
        );
    }

    #[test]
    fn test_strip_firecracker_prefix_fc_agent_with_instance() {
        // Guest serial output WITH Firecracker prefix — strip instance, keep [fc-agent]
        assert_eq!(
            strip_firecracker_prefix(
                "2025-11-15T17:18:55.027 [anonymous-instance:main] [fc-agent] handling restore"
            ),
            "[fc-agent] handling restore"
        );
    }

    #[test]
    fn test_strip_firecracker_prefix_plain() {
        assert_eq!(strip_firecracker_prefix("plain message"), "plain message");
    }

    #[tokio::test]
    async fn stderr_eof_timeout_aborts_the_reader() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        // A reader whose pipe never reaches EOF: the sender side stays held,
        // standing in for a grandchild that inherited the write end.
        let (tx, mut rx) = tokio::sync::mpsc::channel::<u8>(4);
        let reads = Arc::new(AtomicUsize::new(0));
        let reads_in_task = reads.clone();
        let mut reader = Some(tokio::spawn(async move {
            while rx.recv().await.is_some() {
                reads_in_task.fetch_add(1, Ordering::SeqCst);
            }
        }));

        // The channel is still open, so this must take the timeout path.
        wait_for_stderr_eof(&mut reader, Duration::from_millis(50)).await;
        assert!(reader.is_none(), "the handle must be taken either way");

        // The timed-out reader must be aborted, not detached: data arriving
        // after the bound must never be consumed under the old attempt's
        // context. Detached (the bug), the still-live task consumes the send
        // immediately; aborted, its receiver is gone and the send just fails.
        let _ = tx.send(1).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(
            reads.load(Ordering::SeqCst),
            0,
            "stderr reader survived its EOF timeout and kept consuming the pipe"
        );
    }
}
