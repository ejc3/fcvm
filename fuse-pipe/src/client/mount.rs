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

/// Does this /etc/fuse.conf let a non-root user mount with allow_other?
///
/// libfuse reads the file line by line and takes a line as the directive only
/// when it IS the directive: `user_allow_other # comment`, `user_allow_others`
/// and a commented-out line all leave allow_other unavailable, and a mount
/// that asks for it anyway falls back to SessionACL::Owner. Surrounding
/// whitespace is not part of the directive.
pub fn fuse_conf_allows_other(contents: &str) -> bool {
    contents.lines().any(|l| l.trim() == "user_allow_other")
}

/// Maximum retries for Session::new when kernel resources not yet released.
const SESSION_NEW_MAX_RETRIES: u32 = 5;
/// Delay between Session::new retries.
const SESSION_NEW_RETRY_DELAY: Duration = Duration::from_millis(50);

/// How one FUSE mount is made: whether the kernel refuses writes, and whether
/// the kernel or the server owns the size and mtime of a file it has cached.
///
/// [`MountSettings::for_volume`] is the only way to build one, so every mount
/// this crate makes, over a Unix socket or over vsock, takes both settings
/// from the same decision.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MountSettings {
    read_only: bool,
    writeback_cache: bool,
}

impl MountSettings {
    /// The settings for a volume that is `read_only` or not, on a machine
    /// whose `no_writeback_cache` switch is set or not.
    ///
    /// A read-only volume is mounted read-only and without the writeback
    /// cache, whatever the switch says. With FUSE_WRITEBACK_CACHE the kernel
    /// keeps the size and mtime it has cached for a regular file and drops the
    /// ones the server sends, which is right only while this mount is the
    /// file's one writer. A read-only mount writes nothing, so every change to
    /// a file comes from the server's side, and the kernel sees it only by
    /// taking the server's size and mtime.
    ///
    /// A read-write volume gets the writeback cache unless the switch turns
    /// it off.
    pub const fn for_volume(read_only: bool, no_writeback_cache: bool) -> Self {
        Self {
            read_only,
            writeback_cache: !read_only && !no_writeback_cache,
        }
    }

    /// Whether the filesystem is mounted with MS_RDONLY. The kernel then
    /// refuses every write with EROFS before it reaches FUSE.
    pub const fn read_only(self) -> bool {
        self.read_only
    }

    /// Whether INIT asks the kernel for FUSE_WRITEBACK_CACHE.
    pub const fn writeback_cache(self) -> bool {
        self.writeback_cache
    }

    /// The mount options for these settings.
    ///
    /// - Suid: let SUID and SGID bits take effect (fusermount mounts nosuid
    ///   by default). Needs root.
    /// - Dev: allow device nodes (fusermount mounts nodev by default). Needs
    ///   root.
    /// - DefaultPermissions: the kernel does the POSIX permission checks
    ///   (path traversal, owner and mode) before it sends an operation to
    ///   FUSE. Without it a passthrough filesystem that resolves cached inodes
    ///   would skip the parent directory's search permission.
    /// - RO, for a read-only volume only.
    fn mount_options(self) -> Vec<fuser::MountOption> {
        let mut options = vec![
            fuser::MountOption::FSName("fuse-pipe".to_string()),
            fuser::MountOption::Suid,
            fuser::MountOption::Dev,
            fuser::MountOption::DefaultPermissions,
        ];
        if self.read_only {
            options.push(fuser::MountOption::RO);
        }
        options
    }
}

/// Configuration for FUSE mount.
#[derive(Clone)]
pub struct MountConfig {
    /// What the mount may do, and who owns a cached file's size and mtime.
    pub settings: MountSettings,
    /// Number of FUSE reader threads (default: 1).
    pub num_readers: usize,
    /// Trace every Nth request for telemetry (0 = disabled).
    pub trace_rate: u64,
    /// Optional span collector for telemetry aggregation.
    pub collector: Option<SpanCollector>,
}

impl MountConfig {
    /// Create a mount config with the given settings (1 reader, no tracing).
    pub fn new(settings: MountSettings) -> Self {
        Self {
            settings,
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
/// use fuse_pipe::{mount, MountConfig, MountSettings};
///
/// // A read-write mount with 256 readers (blocks until Ctrl+C or fusermount -u)
/// let settings = MountSettings::for_volume(false, false);
/// mount("/tmp/fuse.sock", "/mnt/fuse", MountConfig::new(settings).readers(256))?;
/// ```
pub fn mount<P: AsRef<Path>>(
    socket_path: &str,
    mount_point: P,
    config: MountConfig,
) -> anyhow::Result<()> {
    mount_internal(
        socket_path,
        mount_point,
        config.settings,
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
/// use fuse_pipe::{mount_spawn, MountConfig, MountSettings};
///
/// let settings = MountSettings::for_volume(false, false);
/// let handle = mount_spawn("/tmp/fuse.sock", "/mnt/fuse", MountConfig::new(settings).readers(256))?;
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
    let settings = config.settings;
    let num_readers = config.num_readers.max(1);
    let trace_rate = config.trace_rate;
    let collector = config.collector;

    // Keep a copy of mount_point for cleanup on failure
    let mount_path_for_cleanup = mount_point.as_ref().to_path_buf();

    let thread = thread::spawn(move || {
        mount_internal(
            &socket_path,
            mount_point,
            settings,
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

/// Connect to the server on a Unix socket and run the mount until it is unmounted.
fn mount_internal<P: AsRef<Path>>(
    socket_path: &str,
    mount_point: P,
    settings: MountSettings,
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

    mount_fuse_session(
        mux,
        mount_point,
        num_readers,
        0,
        settings,
        "unix",
        unmounter_tx,
    )
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
/// * `settings` - How the mount is made (see [`MountSettings::for_volume`])
///
/// # Example
///
/// ```rust,ignore
/// use fuse_pipe::client::mount_vsock;
/// use fuse_pipe::transport::HOST_CID;
/// use fuse_pipe::MountSettings;
///
/// // Connect from guest to host on port 5000
/// let settings = MountSettings::for_volume(false, false);
/// mount_vsock(HOST_CID, 5000, "/mnt/volume", settings)?;
/// ```
#[cfg(target_os = "linux")]
pub fn mount_vsock<P: AsRef<Path>>(
    cid: u32,
    port: u32,
    mount_point: P,
    settings: MountSettings,
) -> anyhow::Result<()> {
    mount_vsock_with_options(cid, port, mount_point, 1, 0, 0, settings)
}

/// Mount a FUSE filesystem via vsock with transparent reconnection.
///
/// Like `mount_vsock_with_options`, but uses a reconnectable multiplexer.
/// When the vsock connection dies (e.g., after snapshot/restore), the FUSE
/// session stays alive. The multiplexer automatically reconnects to the same
/// CID:port and re-sends pending requests — the kernel never knows anything
/// happened.
///
/// This function blocks until the FUSE session is unmounted.
#[cfg(target_os = "linux")]
pub fn mount_vsock_with_reconnect<P: AsRef<Path>>(
    cid: u32,
    port: u32,
    mount_point: P,
    num_readers: usize,
    trace_rate: u64,
    max_write: u32,
    settings: MountSettings,
) -> anyhow::Result<()> {
    info!(target: "fuse-pipe::client", cid, port, num_readers, "connecting via vsock (reconnectable)");

    // Create initial vsock connection
    let transport = VsockTransport::connect(cid, port)?;
    debug!(target: "fuse-pipe::client", cid, port, "connected to server via vsock");

    use std::os::unix::io::{AsRawFd, FromRawFd};
    let fd = unsafe { libc::dup(transport.as_raw_fd()) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let socket = unsafe { UnixStream::from_raw_fd(fd) };
    socket.set_read_timeout(Some(Duration::from_secs(30)))?;
    socket.set_write_timeout(Some(Duration::from_secs(30)))?;

    // Create reconnection closure that establishes a new vsock connection.
    // Called by the multiplexer writer thread when the current socket dies.
    let reconnect_fn: Box<dyn Fn() -> std::io::Result<UnixStream> + Send> = Box::new(move || {
        let transport = VsockTransport::connect(cid, port)?;
        let new_fd = unsafe { libc::dup(transport.as_raw_fd()) };
        if new_fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(unsafe { UnixStream::from_raw_fd(new_fd) })
    });

    // Create reconnectable multiplexer
    let mux = Multiplexer::new_reconnectable(socket, num_readers, trace_rate, reconnect_fn)?;
    debug!(target: "fuse-pipe::client", num_readers, "reconnectable multiplexer started");

    mount_fuse_session(
        mux,
        mount_point,
        num_readers,
        max_write,
        settings,
        "reconnectable",
        None,
    )
}

/// Mount a FUSE filesystem via vsock with multiple reader threads.
#[cfg(target_os = "linux")]
pub fn mount_vsock_with_readers<P: AsRef<Path>>(
    cid: u32,
    port: u32,
    mount_point: P,
    num_readers: usize,
    settings: MountSettings,
) -> anyhow::Result<()> {
    mount_vsock_with_options(cid, port, mount_point, num_readers, 0, 0, settings)
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
/// * `max_write` - Maximum write size in bytes (0 = unbounded, use kernel default)
/// * `settings` - How the mount is made (see [`MountSettings::for_volume`])
#[cfg(target_os = "linux")]
pub fn mount_vsock_with_options<P: AsRef<Path>>(
    cid: u32,
    port: u32,
    mount_point: P,
    num_readers: usize,
    trace_rate: u64,
    max_write: u32,
    settings: MountSettings,
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

    mount_fuse_session(
        mux,
        mount_point,
        num_readers,
        max_write,
        settings,
        "vsock",
        None,
    )
}

/// Shared FUSE session setup for every transport: mount with `settings`, create
/// the session, run until unmount. `unmounter_tx`, when given, receives the
/// handle that unmounts the session.
fn mount_fuse_session<P: AsRef<Path>>(
    mux: Arc<Multiplexer>,
    mount_point: P,
    num_readers: usize,
    max_write: u32,
    settings: MountSettings,
    mode_label: &str,
    unmounter_tx: Option<std::sync::mpsc::Sender<SessionUnmounter>>,
) -> anyhow::Result<()> {
    let options = settings.mount_options();

    // AllowOther (SessionACL::All) lets other users access the mount. It's needed when:
    // - Tests switch to different uids (pjdfstest)
    // - Multiple users need to access the filesystem
    // Root can always use it; non-root needs user_allow_other in /etc/fuse.conf
    let is_root = unsafe { libc::geteuid() } == 0;
    let fuse_conf_allows = std::fs::read_to_string("/etc/fuse.conf")
        .map(|s| fuse_conf_allows_other(&s))
        .unwrap_or(false);

    let acl = if is_root || fuse_conf_allows {
        debug!(target: "fuse-pipe::client", is_root, fuse_conf_allows, "using SessionACL::All (allow_other)");
        fuser::SessionACL::All
    } else {
        debug!(target: "fuse-pipe::client", "using SessionACL::Owner (not root and user_allow_other not in /etc/fuse.conf)");
        fuser::SessionACL::Owner
    };
    info!(target: "fuse-pipe::client", ?options, ?settings, "using mount options");
    let mut config = fuser::Config::default();
    config.mount_options = options;
    config.acl = acl;
    // Use fuser's built-in multi-threading with clone_fd for true parallel request processing
    config.n_threads = Some(num_readers);
    config.clone_fd = true; // Opt-in to FUSE_DEV_IOC_CLONE for parallel requests

    // Shared flag set by FuseClient::destroy() when kernel sends FUSE_DESTROY.
    let destroyed = Arc::new(AtomicBool::new(false));

    // Retry Session::new if kernel hasn't released resources from previous mount
    let mut session = None;
    let mut last_error = None;
    for attempt in 0..=SESSION_NEW_MAX_RETRIES {
        // Session::new consumes the client, so each attempt gets its own.
        let fs = FuseClient::with_options(
            Arc::clone(&mux),
            Arc::clone(&destroyed),
            max_write,
            settings,
        );
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
    let mut session = session.ok_or_else(|| last_error.unwrap())?;
    info!(target: "fuse-pipe::client", mount_point = ?mount_point.as_ref(), num_readers, mode_label, "mounted");

    // Send unmounter before blocking on run()
    if let Some(tx) = unmounter_tx {
        let _ = tx.send(session.unmount_callable());
    }

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

#[cfg(test)]
mod tests {
    use super::{fuse_conf_allows_other, MountSettings};

    /// The fresh-box quickstart tells a reader to add user_allow_other to
    /// /etc/fuse.conf when a grep says it is absent, and names `make
    /// test-unit` as what fails without it. If that grep accepts a file this
    /// mount does not, the reader is told the box is configured while every
    /// mount here still takes SessionACL::Owner, and the test the quickstart
    /// cites keeps failing with nothing to change.
    ///
    /// RED BEFORE THE FIX: the documented `grep -q '^user_allow_other'`
    /// accepted `user_allow_other # comment` and `user_allow_otherwise`,
    /// which this file rejects, and rejected the two indented spellings,
    /// which it accepts. Four of the ten cases below disagreed; the
    /// assertion reports the first.
    #[test]
    fn the_quickstart_guard_accepts_exactly_what_this_mount_accepts() {
        let doc = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../.claude/CLAUDE.md");
        let text = std::fs::read_to_string(&doc)
            .unwrap_or_else(|e| panic!("BLOCKED: cannot read {}: {e}", doc.display()));
        let line = text
            .lines()
            .find(|l| l.starts_with("grep") && l.contains("user_allow_other"))
            .unwrap_or_else(|| {
                panic!(
                    "the quickstart's /etc/fuse.conf guard is gone from {}",
                    doc.display()
                )
            });
        // The guard is the test half of `<guard> || <append>`.
        let guard = line
            .split("||")
            .next()
            .expect("a guard before the append")
            .trim();
        assert!(
            guard.contains("/etc/fuse.conf"),
            "the documented guard does not read /etc/fuse.conf: {guard}"
        );

        let dir = tempfile::tempdir().expect("temp dir");
        let conf = dir.path().join("fuse.conf");
        let cases = [
            "user_allow_other\n",
            "  user_allow_other  \n",
            "\tuser_allow_other\n",
            "user_allow_other # comment\n",
            "user_allow_otherwise\n",
            "#user_allow_other\n",
            "# user_allow_other\n",
            "mount_max = 1000\nuser_allow_other\n",
            "mount_max = 1000\n",
            "",
        ];
        for case in cases {
            std::fs::write(&conf, case).expect("write fuse.conf");
            let command = guard.replace("/etc/fuse.conf", conf.to_str().expect("utf-8 path"));
            let status = std::process::Command::new("sh")
                .arg("-c")
                .arg(&command)
                .status()
                .unwrap_or_else(|e| panic!("BLOCKED: cannot run the documented guard: {e}"));
            // grep answers 0 (matched) or 1 (did not). Anything else is a
            // guard that could not evaluate the file, which says nothing.
            let code = status
                .code()
                .unwrap_or_else(|| panic!("BLOCKED: {command} was signalled"));
            assert!(
                code == 0 || code == 1,
                "BLOCKED: the documented guard exited {code} on {case:?}, so it evaluated nothing"
            );
            assert_eq!(
                code == 0,
                fuse_conf_allows_other(case),
                "the quickstart's guard and this mount disagree about {case:?}: \
                 the guard says {}, the mount says {}",
                if code == 0 {
                    "configured"
                } else {
                    "not configured"
                },
                if fuse_conf_allows_other(case) {
                    "configured"
                } else {
                    "not configured"
                },
            );
        }
    }

    /// The whole decision, row by row: a volume's own read-only flag and the
    /// VM-wide writeback switch on the left, what the mount gets on the right.
    ///
    /// RED BEFORE THE FIX: only the VM-wide switch decided, so a read-only
    /// volume was mounted read-write with the writeback cache. The two
    /// read-only rows failed; the assertion reports the first.
    #[test]
    fn a_read_only_volume_is_mounted_read_only_and_without_the_writeback_cache() {
        // (volume read_only, VM-wide no_writeback_cache) -> (read-only mount, writeback cache)
        let table = [
            ((false, false), (false, true)),
            ((false, true), (false, false)),
            ((true, false), (true, false)),
            ((true, true), (true, false)),
        ];
        for ((read_only, no_writeback_cache), want) in table {
            let settings = MountSettings::for_volume(read_only, no_writeback_cache);
            assert_eq!(
                (settings.read_only(), settings.writeback_cache()),
                want,
                "volume read_only={read_only}, VM-wide no_writeback_cache={no_writeback_cache}: \
                 (read-only mount, writeback cache)"
            );
        }
    }

    /// A read-write volume is mounted with the options every volume had before
    /// read-only volumes got their own, and a read-only volume adds `ro` to them.
    #[test]
    fn only_a_read_only_volume_adds_the_ro_mount_option() {
        use fuser::MountOption::{DefaultPermissions, Dev, FSName, Suid, RO};
        let read_write = vec![
            FSName("fuse-pipe".to_string()),
            Suid,
            Dev,
            DefaultPermissions,
        ];
        assert_eq!(
            MountSettings::for_volume(false, false).mount_options(),
            read_write
        );
        assert_eq!(
            MountSettings::for_volume(false, true).mount_options(),
            read_write
        );
        let mut read_only = read_write;
        read_only.push(RO);
        assert_eq!(
            MountSettings::for_volume(true, false).mount_options(),
            read_only
        );
    }
}
