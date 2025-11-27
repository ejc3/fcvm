//! Multi-reader FUSE mount helpers.
//!
//! Uses FUSE_DEV_IOC_CLONE to create multiple reader threads that share
//! a single FUSE mount, enabling parallel request processing.

use super::{FuseClient, Multiplexer};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

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
    eprintln!(
        "[client] connecting to {} (multiplexer slots: {})",
        socket_path, num_readers
    );

    // Create socket connection
    let socket = UnixStream::connect(socket_path)?;
    socket.set_read_timeout(Some(Duration::from_secs(30)))?;
    socket.set_write_timeout(Some(Duration::from_secs(30)))?;
    eprintln!("[client] connected to server");

    // Create multiplexer for request/response handling
    let mux = Multiplexer::with_trace_rate(socket, num_readers, trace_rate);
    eprintln!(
        "[client] multiplexer started with {} reader slots",
        num_readers
    );

    // Mount options kept minimal to avoid requiring user_allow_other
    let options = vec![fuser::MountOption::FSName("fuse-pipe".to_string())];

    let mount_with_options =
        |opts: &[fuser::MountOption]| -> Result<fuser::Session<FuseClient>, std::io::Error> {
            let fs = FuseClient::new(Arc::clone(&mux), 0);
            fuser::Session::new(fs, mount_point.as_ref(), opts)
        };

    // For single reader, just run directly
    if num_readers == 1 {
        let mut session = mount_with_options(&options)?;
        eprintln!("[client] mounted at {:?}", mount_point.as_ref());
        if let Err(e) = session.run() {
            eprintln!("[client] reader 0 error: {}", e);
        }
        eprintln!("[client] FUSE session exited");
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
                let mut reader_session =
                    fuser::Session::from_fd_initialized(fs, cloned_fd, fuser::SessionACL::Owner);

                thread::spawn(move || {
                    if let Err(e) = reader_session.run() {
                        eprintln!("[client] reader {} error: {}", reader_id, e);
                    }
                });
            }
        })
    };

    // Create primary FuseClient with callback
    let fs = FuseClient::with_init_callback(Arc::clone(&mux), 0, make_init_callback());
    let mut session = fuser::Session::new(fs, mount_point.as_ref(), &options)?;
    eprintln!("[client] mounted at {:?}", mount_point.as_ref());

    // Clone fds AFTER session created but BEFORE run()
    let mut clone_failures = 0;
    for reader_id in 1..num_readers {
        match session.channel().clone_fd() {
            Ok(fd) => {
                cloned_fds.lock().unwrap().push((reader_id, fd));
            }
            Err(e) => {
                eprintln!(
                    "[client] failed to clone fd for reader {}: {}",
                    reader_id, e
                );
                clone_failures += 1;
            }
        }
    }

    let actual_readers = num_readers - clone_failures;
    eprintln!(
        "[client] FUSE session starting with {} readers (cloned {} fds)",
        actual_readers,
        actual_readers - 1
    );
    if let Err(e) = session.run() {
        eprintln!("[client] reader 0 error: {}", e);
    }

    eprintln!("[client] FUSE session exited");
    Ok(())
}
