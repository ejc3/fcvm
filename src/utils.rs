//! Utility functions for process management and system operations.

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

    /// [`next_event`](Self::next_event) over an optional watch: a `None` watch
    /// (inotify init failed — e.g. `fs.inotify.max_user_instances` exhausted by
    /// many concurrent fcvm processes) never completes, so a `select!` caller
    /// degrades to its safety-tick/deadline arms instead of failing hard.
    pub async fn next_event_opt(watch: &mut Option<Self>) -> anyhow::Result<()> {
        match watch {
            Some(w) => w.next_event().await,
            None => std::future::pending().await,
        }
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

/// Spawn a command and stream its output via tracing logs.
///
/// Takes a closure that receives each line and logs it appropriately.
/// Returns the child process handle (caller must manage lifecycle).
pub fn spawn_streaming<F>(
    mut cmd: tokio::process::Command,
    log_line: F,
) -> anyhow::Result<tokio::process::Child>
where
    F: Fn(&str, bool) + Send + Sync + Clone + 'static,
{
    // Stdin is null to prevent child from competing with parent for terminal input
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let mut child = cmd.spawn()?;

    // Stream stdout (is_stderr=false)
    if let Some(stdout) = child.stdout.take() {
        let reader = BufReader::new(stdout);
        let mut lines = reader.lines();
        let log = log_line.clone();
        tokio::spawn(async move {
            while let Ok(Some(line)) = lines.next_line().await {
                log(&line, false);
            }
        });
    }

    // Stream stderr (is_stderr=true)
    if let Some(stderr) = child.stderr.take() {
        let reader = BufReader::new(stderr);
        let mut lines = reader.lines();
        let log = log_line;
        tokio::spawn(async move {
            while let Ok(Some(line)) = lines.next_line().await {
                log(&line, true);
            }
        });
    }

    Ok(child)
}

/// Run a command and stream its output via tracing at INFO/WARN level.
///
/// Simple version with a prefix. For custom logging logic, use spawn_streaming.
pub async fn run_streaming(
    cmd: tokio::process::Command,
    prefix: &str,
) -> anyhow::Result<std::process::ExitStatus> {
    let prefix = prefix.to_string();
    let mut child = spawn_streaming(cmd, move |line, is_stderr| {
        if is_stderr {
            warn!("[{}] {}", prefix, line);
        } else {
            info!("[{}] {}", prefix, line);
        }
    })?;
    Ok(child.wait().await?)
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
}
