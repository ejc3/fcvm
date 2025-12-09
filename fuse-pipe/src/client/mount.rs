//! FUSE mount with multi-reader support.
//!
//! Uses FUSE_DEV_IOC_CLONE to create multiple reader threads that share
//! a single FUSE mount, enabling parallel request processing.

use super::{FuseClient, Multiplexer};
use crate::telemetry::SpanCollector;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;
use tracing::{debug, error, info, warn};

#[cfg(target_os = "linux")]
use crate::transport::VsockTransport;

use fuser::SessionUnmounter;

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
        // Then wait for mount thread to finish
        if let Some(thread) = self.thread.take() {
            debug!(target: "fuse-pipe::client", "MountHandle::drop() joining mount thread");
            let _ = thread.join();
            debug!(target: "fuse-pipe::client", "MountHandle::drop() mount thread joined");
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

    let unmounter = rx
        .recv()
        .map_err(|_| anyhow::anyhow!("mount thread failed before sending unmounter"))?;
    Ok(MountHandle {
        thread: Some(thread),
        unmounter: Some(unmounter),
    })
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
    let options = vec![
        fuser::MountOption::FSName("fuse-pipe".to_string()),
        fuser::MountOption::AllowOther,
        fuser::MountOption::Suid,
        fuser::MountOption::Dev,
        fuser::MountOption::DefaultPermissions,
    ];
    info!(target: "fuse-pipe::client", ?options, "using mount options");

    // For single reader, just run directly
    if num_readers == 1 {
        let destroyed = Arc::new(AtomicBool::new(false));
        let fs = FuseClient::with_destroyed_flag(Arc::clone(&mux), 0, Arc::clone(&destroyed));
        let mut session = fuser::Session::new(fs, mount_point.as_ref(), &options)?;
        info!(target: "fuse-pipe::client", mount_point = ?mount_point.as_ref(), "mounted");

        // Send unmounter before blocking on run()
        if let Some(tx) = unmounter_tx {
            let _ = tx.send(session.unmount_callable());
        }

        let run_result = session.run();
        // Drop the session BEFORE checking destroyed flag. The Session's Drop impl
        // calls destroy() if it wasn't already called during run(). This ensures
        // we see the flag set even when FUSE_DESTROY wasn't delivered (programmatic unmount).
        drop(session);

        if let Err(e) = run_result {
            if destroyed.load(Ordering::SeqCst) {
                debug!(target: "fuse-pipe::client", "primary reader exited (clean shutdown)");
            } else {
                error!(target: "fuse-pipe::client", reader_id = 0, error = %e, "reader error");
            }
        }
        debug!(target: "fuse-pipe::client", "FUSE session exited");
        return Ok(());
    }

    // Multi-reader setup:
    // 1. Create shared storage for cloned fds (filled after Session::new)
    // 2. Create callback that reads from shared storage
    // 3. Create FuseClient with callback
    // 4. Create Session (this mounts)
    // 5. Clone fds and store them
    // 6. Run session (init() fires, callback spawns readers)
    // 7. After session.run() returns, join all secondary reader threads

    let cloned_fds: Arc<Mutex<Vec<(usize, OwnedFd)>>> = Arc::new(Mutex::new(Vec::new()));

    // Shared flag set by FuseClient::destroy() when kernel sends FUSE_DESTROY.
    // Reader threads check this to distinguish clean shutdown from real errors.
    let destroyed = Arc::new(AtomicBool::new(false));

    // Storage for secondary reader thread handles - populated by init callback, joined after session.run()
    let reader_threads: Arc<Mutex<Vec<JoinHandle<()>>>> = Arc::new(Mutex::new(Vec::new()));

    let make_init_callback =
        |destroyed: Arc<AtomicBool>, reader_threads: Arc<Mutex<Vec<JoinHandle<()>>>>| {
            let cloned_fds_for_callback = Arc::clone(&cloned_fds);
            let mux_for_callback = Arc::clone(&mux);
            Box::new(move || {
                // Take ownership of all cloned fds
                let fds_vec: Vec<_> = std::mem::take(
                    &mut *cloned_fds_for_callback
                        .lock()
                        .unwrap_or_else(|e| e.into_inner()),
                );

                for (reader_id, cloned_fd) in fds_vec {
                    let fs = FuseClient::with_destroyed_flag(
                        Arc::clone(&mux_for_callback),
                        reader_id as u32,
                        Arc::clone(&destroyed),
                    );
                    // Each cloned fd handles its own request/response pairs
                    // Use SessionACL::All to allow any user to access the mount (AllowOther is set)
                    let mut reader_session =
                        fuser::Session::from_fd_initialized(fs, cloned_fd, fuser::SessionACL::All);

                    let destroyed_check = Arc::clone(&destroyed);
                    let handle = thread::spawn(move || {
                        debug!(target: "fuse-pipe::client", reader_id, "secondary reader starting session.run()");
                        let run_result = reader_session.run();
                        debug!(target: "fuse-pipe::client", reader_id, "secondary reader session.run() returned, dropping session");
                        // Drop the session BEFORE checking destroyed flag. The Session's Drop impl
                        // calls destroy() if it wasn't already called. This ensures we see the flag
                        // set even when FUSE_DESTROY wasn't delivered (programmatic unmount).
                        drop(reader_session);
                        debug!(target: "fuse-pipe::client", reader_id, "secondary reader session dropped");

                        if let Err(e) = run_result {
                            let destroyed = destroyed_check.load(Ordering::SeqCst);
                            debug!(target: "fuse-pipe::client", reader_id, destroyed, error = %e, raw_os_error = ?e.raw_os_error(), "reader exited with error");
                            if !destroyed {
                                error!(target: "fuse-pipe::client", reader_id, error = %e, "reader error (destroy not called)");
                            }
                        }
                        debug!(target: "fuse-pipe::client", reader_id, "secondary reader thread exiting");
                    });
                    reader_threads
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push(handle);
                }
            })
        };

    // Create primary FuseClient with callback and shared destroyed flag
    let fs = FuseClient::with_init_callback(
        Arc::clone(&mux),
        0,
        make_init_callback(Arc::clone(&destroyed), Arc::clone(&reader_threads)),
        Arc::clone(&destroyed),
    );
    let mut session = fuser::Session::new(fs, mount_point.as_ref(), &options)?;
    info!(target: "fuse-pipe::client", mount_point = ?mount_point.as_ref(), "mounted");

    // Clone fds AFTER session created but BEFORE run()
    let mut clone_failures = 0;
    for reader_id in 1..num_readers {
        match session.channel().clone_fd() {
            Ok(fd) => {
                cloned_fds
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push((reader_id, fd));
            }
            Err(e) => {
                warn!(target: "fuse-pipe::client", reader_id, error = %e, "failed to clone fd");
                clone_failures += 1;
            }
        }
    }

    let actual_readers = num_readers - clone_failures;
    info!(target: "fuse-pipe::client", actual_readers, cloned_fds = actual_readers - 1, "FUSE session starting");

    // Send unmounter before blocking on run()
    if let Some(tx) = unmounter_tx {
        let _ = tx.send(session.unmount_callable());
    }

    debug!(target: "fuse-pipe::client", "primary reader starting session.run()");
    let run_result = session.run();
    debug!(target: "fuse-pipe::client", "primary reader session.run() returned, dropping session");
    // Drop the session BEFORE checking destroyed flag. The Session's Drop impl
    // calls destroy() if it wasn't already called. This ensures we see the flag
    // set even when FUSE_DESTROY wasn't delivered (programmatic unmount).
    drop(session);
    debug!(target: "fuse-pipe::client", "primary reader session dropped");

    if let Err(e) = run_result {
        let destroyed_flag = destroyed.load(Ordering::SeqCst);
        debug!(target: "fuse-pipe::client", reader_id = 0, destroyed = destroyed_flag, error = %e, raw_os_error = ?e.raw_os_error(), "primary reader exited with error");
        if !destroyed_flag {
            error!(target: "fuse-pipe::client", reader_id = 0, error = %e, "reader error (destroy not called)");
        }
    }

    // Join all secondary reader threads before returning
    let threads: Vec<_> =
        std::mem::take(&mut *reader_threads.lock().unwrap_or_else(|e| e.into_inner()));
    let num_threads = threads.len();
    debug!(target: "fuse-pipe::client", num_threads, "joining secondary reader threads");
    for handle in threads {
        let _ = handle.join();
    }
    debug!(target: "fuse-pipe::client", "all secondary reader threads joined");

    debug!(target: "fuse-pipe::client", "primary reader thread exiting, FUSE session exited");
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
    let options = vec![
        fuser::MountOption::FSName("fuse-pipe".to_string()),
        fuser::MountOption::AllowOther,
        fuser::MountOption::Suid,
        fuser::MountOption::Dev,
        fuser::MountOption::DefaultPermissions,
    ];

    // For single reader, just run directly
    if num_readers == 1 {
        let destroyed = Arc::new(AtomicBool::new(false));
        let fs = FuseClient::with_destroyed_flag(Arc::clone(&mux), 0, Arc::clone(&destroyed));
        let mut session = fuser::Session::new(fs, mount_point.as_ref(), &options)?;
        info!(target: "fuse-pipe::client", mount_point = ?mount_point.as_ref(), "mounted via vsock");
        if let Err(e) = session.run() {
            if destroyed.load(Ordering::SeqCst) {
                debug!(target: "fuse-pipe::client", "primary reader exited (clean shutdown)");
            } else {
                error!(target: "fuse-pipe::client", reader_id = 0, error = %e, "reader error");
            }
        }
        debug!(target: "fuse-pipe::client", "FUSE session exited");
        return Ok(());
    }

    // Multi-reader setup (same as Unix socket version)
    let cloned_fds: Arc<Mutex<Vec<(usize, OwnedFd)>>> = Arc::new(Mutex::new(Vec::new()));
    let reader_threads: Arc<Mutex<Vec<JoinHandle<()>>>> = Arc::new(Mutex::new(Vec::new()));

    // Shared flag set by FuseClient::destroy() when kernel sends FUSE_DESTROY.
    let destroyed = Arc::new(AtomicBool::new(false));

    let make_init_callback =
        |destroyed: Arc<AtomicBool>, reader_threads: Arc<Mutex<Vec<JoinHandle<()>>>>| {
            let cloned_fds_for_callback = Arc::clone(&cloned_fds);
            let mux_for_callback = Arc::clone(&mux);
            Box::new(move || {
                let fds_vec: Vec<_> = std::mem::take(
                    &mut *cloned_fds_for_callback
                        .lock()
                        .unwrap_or_else(|e| e.into_inner()),
                );

                for (reader_id, cloned_fd) in fds_vec {
                    let fs = FuseClient::with_destroyed_flag(
                        Arc::clone(&mux_for_callback),
                        reader_id as u32,
                        Arc::clone(&destroyed),
                    );
                    // Each cloned fd handles its own request/response pairs
                    // Use SessionACL::All to allow any user to access the mount (AllowOther is set)
                    let mut reader_session =
                        fuser::Session::from_fd_initialized(fs, cloned_fd, fuser::SessionACL::All);

                    let destroyed_check = Arc::clone(&destroyed);
                    let handle = thread::spawn(move || {
                        if let Err(e) = reader_session.run() {
                            if destroyed_check.load(Ordering::SeqCst) {
                                debug!(target: "fuse-pipe::client", reader_id, "reader exited (clean shutdown)");
                            } else {
                                error!(target: "fuse-pipe::client", reader_id, error = %e, "reader error");
                            }
                        }
                    });
                    reader_threads
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push(handle);
                }
            })
        };

    let fs = FuseClient::with_init_callback(
        Arc::clone(&mux),
        0,
        make_init_callback(Arc::clone(&destroyed), Arc::clone(&reader_threads)),
        Arc::clone(&destroyed),
    );
    let mut session = fuser::Session::new(fs, mount_point.as_ref(), &options)?;
    info!(target: "fuse-pipe::client", mount_point = ?mount_point.as_ref(), "mounted via vsock");

    let mut clone_failures = 0;
    for reader_id in 1..num_readers {
        match session.channel().clone_fd() {
            Ok(fd) => {
                cloned_fds
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push((reader_id, fd));
            }
            Err(e) => {
                warn!(target: "fuse-pipe::client", reader_id, error = %e, "failed to clone fd");
                clone_failures += 1;
            }
        }
    }

    let actual_readers = num_readers - clone_failures;
    info!(target: "fuse-pipe::client", actual_readers, cloned_fds = actual_readers - 1, "FUSE session starting");
    if let Err(e) = session.run() {
        if destroyed.load(Ordering::SeqCst) {
            debug!(target: "fuse-pipe::client", "primary reader exited (clean shutdown)");
        } else {
            error!(target: "fuse-pipe::client", reader_id = 0, error = %e, "reader error");
        }
    }

    // Join all secondary reader threads before returning
    let threads: Vec<_> =
        std::mem::take(&mut *reader_threads.lock().unwrap_or_else(|e| e.into_inner()));
    let num_threads = threads.len();
    debug!(target: "fuse-pipe::client", num_threads, "joining secondary reader threads");
    for handle in threads {
        let _ = handle.join();
    }
    debug!(target: "fuse-pipe::client", "all secondary reader threads joined");

    debug!(target: "fuse-pipe::client", "FUSE session exited");
    Ok(())
}
