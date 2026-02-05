//! FUSE mount with multi-reader support.
//!
//! Uses FUSE_DEV_IOC_CLONE to create multiple reader threads that share
//! a single FUSE mount, enabling parallel request processing.

use super::{FuseClient, Multiplexer};
use crate::telemetry::SpanCollector;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;
use tracing::{debug, error, info, warn};

#[cfg(target_os = "linux")]
use crate::transport::VsockTransport;

use fuser::SessionUnmounter;

/// Join a thread with timeout. Returns true if joined successfully, false if timed out.
fn join_with_timeout<T>(thread: JoinHandle<T>, timeout: Duration) -> bool {
    let start = std::time::Instant::now();
    while !thread.is_finished() {
        if start.elapsed() > timeout {
            return false;
        }
        thread::sleep(Duration::from_millis(10));
    }
    let _ = thread.join();
    true
}

/// Maximum retries for Session::new when kernel resources not yet released.
const SESSION_NEW_MAX_RETRIES: u32 = 5;
/// Delay between Session::new retries.
const SESSION_NEW_RETRY_DELAY: Duration = Duration::from_millis(50);

/// Configuration for FUSE mount.
#[derive(Clone, Default)]
pub struct MountConfig {
    /// Number of FUSE reader threads (default: 1).
    pub num_readers: usize,
    /// Trace every Nth request for telemetry (0 = disabled).
    pub trace_rate: u64,
    /// Optional span collector for telemetry aggregation.
    pub collector: Option<SpanCollector>,
}

impl MountConfig {
    /// Create a new mount config with defaults (1 reader, no tracing).
    pub fn new() -> Self {
        Self {
            num_readers: 1,
            trace_rate: 0,
            collector: None,
        }
    }

    /// Set number of reader threads.
    pub fn readers(mut self, n: usize) -> Self {
        self.num_readers = n;
        self
    }

    /// Set trace rate for telemetry.
    pub fn trace_rate(mut self, rate: u64) -> Self {
        self.trace_rate = rate;
        self
    }

    /// Set span collector for telemetry.
    pub fn collector(mut self, collector: SpanCollector) -> Self {
        self.collector = Some(collector);
        self
    }
}

/// Handle for a spawned FUSE mount.
///
/// Created by [`mount_spawn`]. Automatically unmounts when dropped.
/// Use [`join`] to wait for external unmount without triggering unmount.
pub struct MountHandle {
    thread: Option<JoinHandle<anyhow::Result<()>>>,
    unmounter: Option<SessionUnmounter>,
    mount_path: PathBuf,
}

impl Drop for MountHandle {
    fn drop(&mut self) {
        debug!(target: "fuse-pipe::client", "MountHandle::drop() starting");
        // Unmount first (triggers FUSE_DESTROY, causes session.run() to return)
        if let Some(mut unmounter) = self.unmounter.take() {
            debug!(target: "fuse-pipe::client", "MountHandle::drop() calling unmount()");
            let _ = unmounter.unmount();
            debug!(target: "fuse-pipe::client", "MountHandle::drop() unmount() returned");
        }
        // Then wait for mount thread to finish with timeout
        if let Some(thread) = self.thread.take() {
            debug!(target: "fuse-pipe::client", "MountHandle::drop() joining mount thread");
            if join_with_timeout(thread, Duration::from_secs(5)) {
                debug!(target: "fuse-pipe::client", "MountHandle::drop() mount thread joined");
            } else {
                warn!(target: "fuse-pipe::client", "MountHandle::drop() mount thread join timed out, forcing unmount");
                force_unmount(&self.mount_path);
            }
        }
        debug!(target: "fuse-pipe::client", "MountHandle::drop() complete");
    }
}

impl MountHandle {
    /// Wait for external unmount (e.g., user ran `fusermount3 -u`).
    ///
    /// This does NOT trigger unmount - it just waits for the mount thread to exit.
    /// Use this when something else will unmount the filesystem.
    pub fn join(mut self) -> anyhow::Result<()> {
        // Don't unmount - just wait for thread
        self.unmounter.take();
        self.thread
            .take()
            .unwrap()
            .join()
            .map_err(|_| anyhow::anyhow!("mount thread panicked"))?
    }
}

/// Mount a FUSE filesystem via Unix socket (blocking).
///
/// Connects to a server at `socket_path` and mounts at `mount_point`.
/// **Blocks** until the filesystem is unmounted (e.g., via fusermount -u).
///
/// For programmatic unmount control, use [`mount_spawn`] instead.
///
/// # Example
///
/// ```ignore
/// use fuse_pipe::{mount, MountConfig};
///
/// // Mount with 256 readers (blocks until Ctrl+C or fusermount -u)
/// mount("/tmp/fuse.sock", "/mnt/fuse", MountConfig::new().readers(256))?;
/// ```
pub fn mount<P: AsRef<Path>>(
    socket_path: &str,
    mount_point: P,
    config: MountConfig,
) -> anyhow::Result<()> {
    mount_internal(
        socket_path,
        mount_point,
        config.num_readers.max(1),
        config.trace_rate,
        config.collector,
        None,
    )
}

/// Mount a FUSE filesystem via Unix socket (spawned).
///
/// Like [`mount`], but spawns the mount in a thread and returns a handle.
/// The filesystem is automatically unmounted when the handle is dropped.
///
/// # Example
///
/// ```ignore
/// use fuse_pipe::{mount_spawn, MountConfig};
///
/// let handle = mount_spawn("/tmp/fuse.sock", "/mnt/fuse", MountConfig::new().readers(256))?;
///
/// // Do work with the mounted filesystem...
///
/// // Unmount happens automatically when handle is dropped
/// drop(handle);
/// ```
pub fn mount_spawn<P: AsRef<Path> + Send + 'static>(
    socket_path: &str,
    mount_point: P,
    config: MountConfig,
) -> anyhow::Result<MountHandle> {
    let (tx, rx) = std::sync::mpsc::channel();
    let socket_path = socket_path.to_string();
    let num_readers = config.num_readers.max(1);
    let trace_rate = config.trace_rate;
    let collector = config.collector;

    // Keep a copy of mount_point for cleanup on failure
    let mount_path_for_cleanup = mount_point.as_ref().to_path_buf();

    let thread = thread::spawn(move || {
        mount_internal(
            &socket_path,
            mount_point,
            num_readers,
            trace_rate,
            collector,
            Some(tx),
        )
    });

    // Wait for unmounter with timeout - mount thread might fail before sending
    match rx.recv_timeout(Duration::from_secs(10)) {
        Ok(unmounter) => Ok(MountHandle {
            thread: Some(thread),
            unmounter: Some(unmounter),
            mount_path: mount_path_for_cleanup,
        }),
        Err(e) => {
            // Mount failed or timed out - clean up the thread with short timeout.
            // The thread may be stuck in Session::new() or similar blocking call.
            warn!(target: "fuse-pipe::client", "mount_spawn failed, cleaning up thread: {:?}", e);

            // Try to get the actual error from the mount thread
            let thread_error = {
                let start = std::time::Instant::now();
                let timeout = Duration::from_secs(2);
                while !thread.is_finished() {
                    if start.elapsed() > timeout {
                        break;
                    }
                    thread::sleep(Duration::from_millis(10));
                }
                if thread.is_finished() {
                    match thread.join() {
                        Ok(Ok(())) => None,
                        Ok(Err(mount_err)) => {
                            error!(target: "fuse-pipe::client", "mount thread failed: {:#}", mount_err);
                            Some(mount_err)
                        }
                        Err(_panic) => {
                            error!(target: "fuse-pipe::client", "mount thread panicked");
                            Some(anyhow::anyhow!("mount thread panicked"))
                        }
                    }
                } else {
                    warn!(target: "fuse-pipe::client", "mount thread stuck, abandoning");
                    // Try to forcefully unmount in case the mount succeeded but thread hung
                    force_unmount(&mount_path_for_cleanup);
                    None
                }
            };

            Err(match (e, thread_error) {
                (_, Some(thread_err)) => thread_err,
                (std::sync::mpsc::RecvTimeoutError::Timeout, None) => {
                    anyhow::anyhow!("mount timed out after 10s - check if running as root for FUSE")
                }
                (std::sync::mpsc::RecvTimeoutError::Disconnected, None) => {
                    anyhow::anyhow!("mount thread failed before sending unmounter")
                }
            })
        }
    }
}

/// Force unmount a path using fusermount3 -u (lazy unmount).
/// This is used as a fallback when normal unmount fails or thread is stuck.
fn force_unmount(path: &Path) {
    if let Some(path_str) = path.to_str() {
        debug!(target: "fuse-pipe::client", path = %path_str, "attempting force unmount with fusermount3");
        let result = std::process::Command::new("fusermount3")
            .args(["-u", "-z", path_str]) // -z for lazy unmount
            .status();
        match result {
            Ok(status) if status.success() => {
                info!(target: "fuse-pipe::client", path = %path_str, "force unmount succeeded");
            }
            Ok(status) => {
                debug!(target: "fuse-pipe::client", path = %path_str, ?status, "force unmount returned non-zero");
            }
            Err(e) => {
                debug!(target: "fuse-pipe::client", path = %path_str, error = %e, "force unmount failed");
            }
        }
    }
}

/// Internal mount implementation with optional unmounter channel.
fn mount_internal<P: AsRef<Path>>(
    socket_path: &str,
    mount_point: P,
    num_readers: usize,
    trace_rate: u64,
    collector: Option<SpanCollector>,
    unmounter_tx: Option<std::sync::mpsc::Sender<SessionUnmounter>>,
) -> anyhow::Result<()> {
    info!(target: "fuse-pipe::client", socket_path, num_readers, "connecting");

    // Create socket connection
    let socket = UnixStream::connect(socket_path)?;
    socket.set_read_timeout(Some(Duration::from_secs(30)))?;
    socket.set_write_timeout(Some(Duration::from_secs(30)))?;
    debug!(target: "fuse-pipe::client", "connected to server");

    // Create multiplexer for request/response handling
    let mux = Multiplexer::with_collector(socket, num_readers, trace_rate, collector)?;
    debug!(target: "fuse-pipe::client", num_readers, "multiplexer started");

    // Mount options:
    // - AllowOther: Allow non-root users to access the mount (requires user_allow_other in /etc/fuse.conf or running as root)
    // - Suid: Allow SUID/SGID bits to take effect (fusermount uses nosuid by default). Requires root.
    // - Dev: Allow device nodes (fusermount uses nodev by default). Requires root.
    // - DefaultPermissions: Let the kernel perform standard POSIX permission checks (path traversal,
    //   owner/mode checks) before sending operations to FUSE. This is required for correct behavior
    //   like "can't access file if parent dir has no search permission". Without this, a passthrough
    //   fs using cached inodes would bypass parent directory permission checks.
    //
    // Note: We intentionally do NOT use DefaultPermissions because it causes ftruncate on
    // already-opened file handles to fail with EACCES if the file mode is restrictive. The
    // kernel with DefaultPermissions checks inode permissions before sending SETATTR to FUSE,
    // but ftruncate on a valid fd should use the fd's access rights, not the current file mode.
    //
    // Instead, we rely on set_creds() in fuse-backend-rs to switch to the caller's uid/gid
    // before filesystem operations. This ensures:
    // - Path traversal checks work (lookup calls openat which checks parent dir permissions)
    // - File operations use caller's credentials
    // - fd-based operations (ftruncate on open handle) work correctly
    //
    // default_permissions: Let the kernel check basic file permissions (rwx) before
    // calling FUSE. This handles parent directory permission checking correctly and
    // reduces round-trips to the FUSE server for operations that would fail anyway.
    // Build mount options.
    let options = vec![
        fuser::MountOption::FSName("fuse-pipe".to_string()),
        fuser::MountOption::Suid,
        fuser::MountOption::Dev,
        fuser::MountOption::DefaultPermissions,
    ];

    // AllowOther (SessionACL::All) lets other users access the mount. It's needed when:
    // - Tests switch to different uids (pjdfstest)
    // - Multiple users need to access the filesystem
    // Root can always use it; non-root needs user_allow_other in /etc/fuse.conf
    let is_root = unsafe { libc::geteuid() } == 0;
    let fuse_conf_allows = std::fs::read_to_string("/etc/fuse.conf")
        .map(|s| s.lines().any(|l| l.trim() == "user_allow_other"))
        .unwrap_or(false);

    let acl = if is_root || fuse_conf_allows {
        debug!(target: "fuse-pipe::client", is_root, fuse_conf_allows, "using SessionACL::All (allow_other)");
        fuser::SessionACL::All
    } else {
        debug!(target: "fuse-pipe::client", "using SessionACL::Owner (not root and user_allow_other not in /etc/fuse.conf)");
        fuser::SessionACL::Owner
    };
    info!(target: "fuse-pipe::client", ?options, "using mount options");
    let mut config = fuser::Config::default();
    config.mount_options = options;
    config.acl = acl;
    // Use fuser's built-in multi-threading with clone_fd
    config.n_threads = Some(num_readers);

    // Shared flag set by FuseClient::destroy() when kernel sends FUSE_DESTROY.
    let destroyed = Arc::new(AtomicBool::new(false));

    // Retry Session::new if kernel hasn't released resources from previous mount
    let mut session = None;
    let mut last_error = None;
    for attempt in 0..=SESSION_NEW_MAX_RETRIES {
        // Note: We need to clone fs for each attempt since Session::new consumes it on failure
        let fs_attempt = FuseClient::with_destroyed_flag(Arc::clone(&mux), Arc::clone(&destroyed));
        match fuser::Session::new(fs_attempt, mount_point.as_ref(), &config) {
            Ok(s) => {
                if attempt > 0 {
                    info!(target: "fuse-pipe::client", attempt, "Session::new succeeded after retry");
                }
                session = Some(s);
                break;
            }
            Err(e) => {
                if attempt < SESSION_NEW_MAX_RETRIES {
                    debug!(target: "fuse-pipe::client", attempt, max_retries = SESSION_NEW_MAX_RETRIES, error = %e, "Session::new failed, retrying");
                    thread::sleep(SESSION_NEW_RETRY_DELAY);
                }
                last_error = Some(e);
            }
        }
    }
    let mut session = session.ok_or_else(|| last_error.unwrap())?;
    info!(target: "fuse-pipe::client", mount_point = ?mount_point.as_ref(), num_readers, "mounted");

    // Send unmounter before blocking on run()
    if let Some(tx) = unmounter_tx {
        let _ = tx.send(session.unmount_callable());
    }

    debug!(target: "fuse-pipe::client", num_readers, "FUSE session starting with n_threads");
    // spawn() handles all threading internally with clone_fd, join() waits for completion
    let bg_session = session.spawn()?;
    let join_result = bg_session.join();

    if let Err(e) = join_result {
        let destroyed_flag = destroyed.load(Ordering::SeqCst);
        if destroyed_flag {
            debug!(target: "fuse-pipe::client", "FUSE session exited (clean shutdown)");
        } else {
            error!(target: "fuse-pipe::client", error = %e, "FUSE session error");
        }
    }

    debug!(target: "fuse-pipe::client", "FUSE session exited");
    Ok(())
}

/// Mount a FUSE filesystem using a vsock connection.
///
/// This connects to a server via vsock (CID + port) and mounts a FUSE filesystem
/// at `mount_point`. The function blocks until the filesystem is unmounted.
///
/// # Arguments
///
/// * `cid` - The context ID (use `HOST_CID` to connect to host from guest)
/// * `port` - The vsock port number
/// * `mount_point` - Directory where the FUSE filesystem will be mounted
///
/// # Example
///
/// ```rust,ignore
/// use fuse_pipe::client::mount_vsock;
/// use fuse_pipe::transport::HOST_CID;
///
/// // Connect from guest to host on port 5000
/// mount_vsock(HOST_CID, 5000, "/mnt/volume")?;
/// ```
#[cfg(target_os = "linux")]
pub fn mount_vsock<P: AsRef<Path>>(cid: u32, port: u32, mount_point: P) -> anyhow::Result<()> {
    mount_vsock_with_options(cid, port, mount_point, 1, 0)
}

/// Mount a FUSE filesystem via vsock with multiple reader threads.
#[cfg(target_os = "linux")]
pub fn mount_vsock_with_readers<P: AsRef<Path>>(
    cid: u32,
    port: u32,
    mount_point: P,
    num_readers: usize,
) -> anyhow::Result<()> {
    mount_vsock_with_options(cid, port, mount_point, num_readers, 0)
}

/// Mount a FUSE filesystem via vsock with full configuration.
///
/// # Arguments
///
/// * `cid` - The context ID (use `HOST_CID` to connect to host from guest)
/// * `port` - The vsock port number
/// * `mount_point` - Directory where the FUSE filesystem will be mounted
/// * `num_readers` - Number of FUSE reader threads (1-8 recommended)
/// * `trace_rate` - Trace every Nth request (0 = disabled)
#[cfg(target_os = "linux")]
pub fn mount_vsock_with_options<P: AsRef<Path>>(
    cid: u32,
    port: u32,
    mount_point: P,
    num_readers: usize,
    trace_rate: u64,
) -> anyhow::Result<()> {
    info!(target: "fuse-pipe::client", cid, port, num_readers, "connecting via vsock");

    // Create vsock connection
    let transport = VsockTransport::connect(cid, port)?;
    debug!(target: "fuse-pipe::client", cid, port, "connected to server via vsock");

    // VsockTransport wraps a UnixStream internally, extract it for the multiplexer
    // This is safe because VsockTransport is just a UnixStream created from a vsock fd
    use std::os::unix::io::{AsRawFd, FromRawFd};
    let fd = unsafe { libc::dup(transport.as_raw_fd()) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // SAFETY: fd is a valid file descriptor from dup() which succeeded (fd >= 0)
    let socket = unsafe { UnixStream::from_raw_fd(fd) };
    socket.set_read_timeout(Some(Duration::from_secs(30)))?;
    socket.set_write_timeout(Some(Duration::from_secs(30)))?;

    // Create multiplexer for request/response handling
    let mux = Multiplexer::with_trace_rate(socket, num_readers, trace_rate)?;
    debug!(target: "fuse-pipe::client", num_readers, "multiplexer started");

    // Mount options (same as Unix socket version - see comments there for details)
    // Build mount options.
    let options = vec![
        fuser::MountOption::FSName("fuse-pipe".to_string()),
        fuser::MountOption::Suid,
        fuser::MountOption::Dev,
        fuser::MountOption::DefaultPermissions,
    ];

    // AllowOther (SessionACL::All) lets other users access the mount. It's needed when:
    // - Tests switch to different uids (pjdfstest)
    // - Multiple users need to access the filesystem
    // Root can always use it; non-root needs user_allow_other in /etc/fuse.conf
    let is_root = unsafe { libc::geteuid() } == 0;
    let fuse_conf_allows = std::fs::read_to_string("/etc/fuse.conf")
        .map(|s| s.lines().any(|l| l.trim() == "user_allow_other"))
        .unwrap_or(false);

    let acl = if is_root || fuse_conf_allows {
        debug!(target: "fuse-pipe::client", is_root, fuse_conf_allows, "using SessionACL::All (allow_other)");
        fuser::SessionACL::All
    } else {
        debug!(target: "fuse-pipe::client", "using SessionACL::Owner (not root and user_allow_other not in /etc/fuse.conf)");
        fuser::SessionACL::Owner
    };
    let mut config = fuser::Config::default();
    config.mount_options = options;
    config.acl = acl;
    // Use fuser's built-in multi-threading with clone_fd
    config.n_threads = Some(num_readers);

    // Shared flag set by FuseClient::destroy() when kernel sends FUSE_DESTROY.
    let destroyed = Arc::new(AtomicBool::new(false));

    // Retry Session::new if kernel hasn't released resources from previous mount
    let mut session = None;
    let mut last_error = None;
    for attempt in 0..=SESSION_NEW_MAX_RETRIES {
        let fs = FuseClient::with_destroyed_flag(Arc::clone(&mux), Arc::clone(&destroyed));
        match fuser::Session::new(fs, mount_point.as_ref(), &config) {
            Ok(s) => {
                if attempt > 0 {
                    info!(target: "fuse-pipe::client", attempt, "Session::new succeeded after retry");
                }
                session = Some(s);
                break;
            }
            Err(e) => {
                if attempt < SESSION_NEW_MAX_RETRIES {
                    debug!(target: "fuse-pipe::client", attempt, max_retries = SESSION_NEW_MAX_RETRIES, error = %e, "Session::new failed, retrying");
                    thread::sleep(SESSION_NEW_RETRY_DELAY);
                }
                last_error = Some(e);
            }
        }
    }
    let session = session.ok_or_else(|| last_error.unwrap())?;
    info!(target: "fuse-pipe::client", mount_point = ?mount_point.as_ref(), num_readers, "mounted via vsock");

    debug!(target: "fuse-pipe::client", num_readers, "FUSE session starting with n_threads");
    // spawn() handles all threading internally with clone_fd, join() waits for completion
    let bg_session = session.spawn()?;
    if let Err(e) = bg_session.join() {
        let destroyed_flag = destroyed.load(Ordering::SeqCst);
        if destroyed_flag {
            debug!(target: "fuse-pipe::client", "FUSE session exited (clean shutdown)");
        } else {
            error!(target: "fuse-pipe::client", error = %e, "FUSE session error");
        }
    }

    debug!(target: "fuse-pipe::client", "FUSE session exited");
    Ok(())
}
