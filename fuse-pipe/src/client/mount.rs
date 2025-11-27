//! FUSE mount helpers.

use super::{FuseClient, Multiplexer};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::Arc;
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

/// Mount a FUSE filesystem with socket multiplexing support.
///
/// This creates a multiplexer for the socket connection, allowing multiple
/// concurrent requests. Currently uses a single FUSE reader thread since
/// the standard fuser crate doesn't support multi-reader sessions.
///
/// # Arguments
///
/// * `socket_path` - Path to the Unix socket where the server is listening
/// * `mount_point` - Directory where the FUSE filesystem will be mounted
/// * `num_readers` - Number of reader slots in the multiplexer (protocol-level)
///
/// # Note
///
/// The `num_readers` parameter configures the protocol-level multiplexer
/// but the FUSE session itself uses a single reader thread due to fuser
/// crate limitations. True multi-reader support requires a modified fuser.
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

    // Create FUSE client using reader 0
    let fs = FuseClient::new(Arc::clone(&mux), 0);

    let options = vec![
        fuser::MountOption::FSName("fuse-pipe".to_string()),
        fuser::MountOption::AutoUnmount,
        fuser::MountOption::AllowOther,
    ];

    // Create and run FUSE session
    let mut session = fuser::Session::new(fs, mount_point.as_ref(), &options)?;
    eprintln!("[client] mounted at {:?}", mount_point.as_ref());

    eprintln!("[client] FUSE session starting");
    if let Err(e) = session.run() {
        eprintln!("[client] FUSE session error: {}", e);
        return Err(e.into());
    }
    eprintln!("[client] FUSE session exited");

    Ok(())
}
