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
///
/// # Arguments
///
/// * `socket_path` - Path to the Unix socket where the server is listening
/// * `mount_point` - Directory where the FUSE filesystem will be mounted
///
/// # Example
///
/// ```rust,ignore
/// use fuse_pipe::client::mount;
/// use std::path::PathBuf;
///
/// mount("/tmp/fuse.sock", &PathBuf::from("/mnt/fuse"))?;
/// ```
pub fn mount<P: AsRef<Path>>(socket_path: &str, mount_point: P) -> anyhow::Result<()> {
    mount_with_readers(socket_path, mount_point, 1)
}

/// Mount a FUSE filesystem with multiple reader threads.
///
/// This creates multiple FUSE reader threads using FUSE_DEV_IOC_CLONE,
/// allowing parallel processing of FUSE requests. Each reader thread
/// shares a single socket connection via the multiplexer.
///
/// # Arguments
///
/// * `socket_path` - Path to the Unix socket where the server is listening
/// * `mount_point` - Directory where the FUSE filesystem will be mounted
/// * `num_readers` - Number of FUSE reader threads (1-8 recommended)
///
/// # Example
///
/// ```rust,ignore
/// use fuse_pipe::client::mount_with_readers;
/// use std::path::PathBuf;
///
/// mount_with_readers("/tmp/fuse.sock", &PathBuf::from("/mnt/fuse"), 4)?;
/// ```
pub fn mount_with_readers<P: AsRef<Path>>(
    socket_path: &str,
    mount_point: P,
    num_readers: usize,
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
    let mux = Multiplexer::new(socket, num_readers);
    eprintln!(
        "[client] multiplexer started with {} reader slots",
        num_readers
    );

    let options = vec![
        fuser::MountOption::FSName("fuse-pipe".to_string()),
        fuser::MountOption::AutoUnmount,
        fuser::MountOption::AllowOther,
    ];

    // For single reader, just run directly
    if num_readers == 1 {
        let fs = FuseClient::new(Arc::clone(&mux), 0);
        let mut session = fuser::Session::new(fs, mount_point.as_ref(), &options)?;
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
    let cloned_fds_for_callback = Arc::clone(&cloned_fds);
    let mux_for_callback = Arc::clone(&mux);

    let init_callback = Box::new(move || {
        // Take ownership of all cloned fds
        let fds_vec: Vec<_> = std::mem::take(&mut *cloned_fds_for_callback.lock().unwrap());

        for (reader_id, cloned_fd) in fds_vec {
            let fs = FuseClient::new(Arc::clone(&mux_for_callback), reader_id as u32);
            let mut reader_session =
                fuser::Session::from_fd_initialized(fs, cloned_fd, fuser::SessionACL::All);

            thread::spawn(move || {
                if let Err(e) = reader_session.run() {
                    eprintln!("[client] reader {} error: {}", reader_id, e);
                }
            });
        }
    });

    // Create primary FuseClient with callback
    let fs = FuseClient::with_init_callback(Arc::clone(&mux), 0, init_callback);
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
                eprintln!("[client] failed to clone fd for reader {}: {}", reader_id, e);
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
