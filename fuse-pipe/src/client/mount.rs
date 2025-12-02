//! Multi-reader FUSE mount helpers.
//!
//! Uses FUSE_DEV_IOC_CLONE to create multiple reader threads that share
//! a single FUSE mount, enabling parallel request processing.

use super::{FuseClient, Multiplexer};
use crate::telemetry::SpanCollector;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;
use tracing::{debug, error, info, warn};

#[cfg(target_os = "linux")]
use crate::transport::VsockTransport;

/// Mount a FUSE filesystem using a Unix socket connection.
///
/// This connects to a server at `socket_path` and mounts a FUSE filesystem
/// at `mount_point`. The function blocks until the filesystem is unmounted.
pub fn mount<P: AsRef<Path>>(socket_path: &str, mount_point: P) -> anyhow::Result<()> {
    mount_with_options(socket_path, mount_point, 1, 0)
}

/// Mount a FUSE filesystem with multiple reader threads.
///
/// This creates multiple FUSE reader threads using FUSE_DEV_IOC_CLONE,
/// allowing parallel processing of FUSE requests.
pub fn mount_with_readers<P: AsRef<Path>>(
    socket_path: &str,
    mount_point: P,
    num_readers: usize,
) -> anyhow::Result<()> {
    mount_with_options(socket_path, mount_point, num_readers, 0)
}

/// Mount a FUSE filesystem with full configuration.
///
/// # Arguments
///
/// * `socket_path` - Path to the Unix socket where the server is listening
/// * `mount_point` - Directory where the FUSE filesystem will be mounted
/// * `num_readers` - Number of FUSE reader threads (1-8 recommended)
/// * `trace_rate` - Trace every Nth request (0 = disabled)
pub fn mount_with_options<P: AsRef<Path>>(
    socket_path: &str,
    mount_point: P,
    num_readers: usize,
    trace_rate: u64,
) -> anyhow::Result<()> {
    mount_with_telemetry(socket_path, mount_point, num_readers, trace_rate, None)
}

/// Mount a FUSE filesystem with telemetry collection.
///
/// If a `SpanCollector` is provided, trace spans will be collected for later analysis.
/// Use `trace_rate > 0` to enable tracing (e.g., 1 = trace every request, 100 = every 100th).
///
/// # Arguments
///
/// * `socket_path` - Path to the Unix socket where the server is listening
/// * `mount_point` - Directory where the FUSE filesystem will be mounted
/// * `num_readers` - Number of FUSE reader threads (1-8 recommended)
/// * `trace_rate` - Trace every Nth request (0 = disabled)
/// * `collector` - Optional SpanCollector for telemetry aggregation
pub fn mount_with_telemetry<P: AsRef<Path>>(
    socket_path: &str,
    mount_point: P,
    num_readers: usize,
    trace_rate: u64,
    collector: Option<SpanCollector>,
) -> anyhow::Result<()> {
    info!(target: "fuse-pipe::client", socket_path, num_readers, "connecting");

    // Create socket connection
    let socket = UnixStream::connect(socket_path)?;
    socket.set_read_timeout(Some(Duration::from_secs(30)))?;
    socket.set_write_timeout(Some(Duration::from_secs(30)))?;
    debug!(target: "fuse-pipe::client", "connected to server");

    // Create multiplexer for request/response handling
    let mux = Multiplexer::with_collector(socket, num_readers, trace_rate, collector);
    debug!(target: "fuse-pipe::client", num_readers, "multiplexer started");

    // Mount options:
    // - AllowOther: Allow non-root users to access the mount (requires user_allow_other in /etc/fuse.conf or running as root)
    // - DefaultPermissions: Let the kernel enforce basic permission checks before handing off to userspace
    let options = vec![
        fuser::MountOption::FSName("fuse-pipe".to_string()),
        fuser::MountOption::AllowOther,
        fuser::MountOption::DefaultPermissions,
    ];

    let mount_with_options =
        |opts: &[fuser::MountOption]| -> Result<fuser::Session<FuseClient>, std::io::Error> {
            let fs = FuseClient::new(Arc::clone(&mux), 0);
            fuser::Session::new(fs, mount_point.as_ref(), opts)
        };

    // For single reader, just run directly
    if num_readers == 1 {
        let mut session = mount_with_options(&options)?;
        info!(target: "fuse-pipe::client", mount_point = ?mount_point.as_ref(), "mounted");
        if let Err(e) = session.run() {
            error!(target: "fuse-pipe::client", reader_id = 0, error = %e, "reader error");
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

    let cloned_fds: Arc<Mutex<Vec<(usize, OwnedFd)>>> = Arc::new(Mutex::new(Vec::new()));

    let make_init_callback = || {
        let cloned_fds_for_callback = Arc::clone(&cloned_fds);
        let mux_for_callback = Arc::clone(&mux);
        Box::new(move || {
            // Take ownership of all cloned fds
            let fds_vec: Vec<_> = std::mem::take(&mut *cloned_fds_for_callback.lock().unwrap());

            for (reader_id, cloned_fd) in fds_vec {
                let fs = FuseClient::new(Arc::clone(&mux_for_callback), reader_id as u32);
                // Each cloned fd handles its own request/response pairs
                let mut reader_session =
                    fuser::Session::from_fd_initialized(fs, cloned_fd, fuser::SessionACL::Owner);

                thread::spawn(move || {
                    if let Err(e) = reader_session.run() {
                        error!(target: "fuse-pipe::client", reader_id, error = %e, "reader error");
                    }
                });
            }
        })
    };

    // Create primary FuseClient with callback
    let fs = FuseClient::with_init_callback(Arc::clone(&mux), 0, make_init_callback());
    let mut session = fuser::Session::new(fs, mount_point.as_ref(), &options)?;
    info!(target: "fuse-pipe::client", mount_point = ?mount_point.as_ref(), "mounted");

    // Clone fds AFTER session created but BEFORE run()
    let mut clone_failures = 0;
    for reader_id in 1..num_readers {
        match session.channel().clone_fd() {
            Ok(fd) => {
                cloned_fds.lock().unwrap().push((reader_id, fd));
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
        error!(target: "fuse-pipe::client", reader_id = 0, error = %e, "reader error");
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
    let socket = unsafe { UnixStream::from_raw_fd(libc::dup(transport.as_raw_fd())) };
    socket.set_read_timeout(Some(Duration::from_secs(30)))?;
    socket.set_write_timeout(Some(Duration::from_secs(30)))?;

    // Create multiplexer for request/response handling
    let mux = Multiplexer::with_trace_rate(socket, num_readers, trace_rate);
    debug!(target: "fuse-pipe::client", num_readers, "multiplexer started");

    // Mount options:
    // - AllowOther: Allow non-root users to access the mount (requires user_allow_other in /etc/fuse.conf or running as root)
    // Note: We do NOT use DefaultPermissions because we implement our own permission checks
    // in the passthrough handler to properly enforce POSIX ownership rules (chmod/chown/utimes)
    let options = vec![
        fuser::MountOption::FSName("fuse-pipe".to_string()),
        fuser::MountOption::AllowOther,
        fuser::MountOption::DefaultPermissions,
    ];

    let mount_with_options =
        |opts: &[fuser::MountOption]| -> Result<fuser::Session<FuseClient>, std::io::Error> {
            let fs = FuseClient::new(Arc::clone(&mux), 0);
            fuser::Session::new(fs, mount_point.as_ref(), opts)
        };

    // For single reader, just run directly
    if num_readers == 1 {
        let mut session = mount_with_options(&options)?;
        info!(target: "fuse-pipe::client", mount_point = ?mount_point.as_ref(), "mounted via vsock");
        if let Err(e) = session.run() {
            error!(target: "fuse-pipe::client", reader_id = 0, error = %e, "reader error");
        }
        debug!(target: "fuse-pipe::client", "FUSE session exited");
        return Ok(());
    }

    // Multi-reader setup (same as Unix socket version)
    let cloned_fds: Arc<Mutex<Vec<(usize, OwnedFd)>>> = Arc::new(Mutex::new(Vec::new()));

    let make_init_callback = || {
        let cloned_fds_for_callback = Arc::clone(&cloned_fds);
        let mux_for_callback = Arc::clone(&mux);
        Box::new(move || {
            let fds_vec: Vec<_> = std::mem::take(&mut *cloned_fds_for_callback.lock().unwrap());

            for (reader_id, cloned_fd) in fds_vec {
                let fs = FuseClient::new(Arc::clone(&mux_for_callback), reader_id as u32);
                // Each cloned fd handles its own request/response pairs
                let mut reader_session =
                    fuser::Session::from_fd_initialized(fs, cloned_fd, fuser::SessionACL::Owner);

                thread::spawn(move || {
                    if let Err(e) = reader_session.run() {
                        error!(target: "fuse-pipe::client", reader_id, error = %e, "reader error");
                    }
                });
            }
        })
    };

    let fs = FuseClient::with_init_callback(Arc::clone(&mux), 0, make_init_callback());
    let mut session = fuser::Session::new(fs, mount_point.as_ref(), &options)?;
    info!(target: "fuse-pipe::client", mount_point = ?mount_point.as_ref(), "mounted via vsock");

    let mut clone_failures = 0;
    for reader_id in 1..num_readers {
        match session.channel().clone_fd() {
            Ok(fd) => {
                cloned_fds.lock().unwrap().push((reader_id, fd));
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
        error!(target: "fuse-pipe::client", reader_id = 0, error = %e, "reader error");
    }

    debug!(target: "fuse-pipe::client", "FUSE session exited");
    Ok(())
}
