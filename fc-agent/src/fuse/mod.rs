//! FUSE-over-vsock volume mounting for guest.
//!
//! This module uses fuse-pipe to mount host directories inside the guest VM.
//! The fuse-pipe library handles all the complexity of multiplexed FUSE
//! request handling over vsock.

use fuse_pipe::transport::HOST_CID;

/// Mount a FUSE filesystem from host via vsock.
///
/// This connects to the host VolumeServer at the given port and mounts
/// the FUSE filesystem at the specified path. The function blocks until
/// the filesystem is unmounted.
///
/// # Arguments
///
/// * `port` - The vsock port where the host VolumeServer is listening
/// * `mount_point` - The path where the filesystem will be mounted
pub fn mount_vsock(port: u32, mount_point: &str) -> anyhow::Result<()> {
    eprintln!("[fc-agent] mounting FUSE volume at {} via vsock port {}", mount_point, port);
    fuse_pipe::mount_vsock(HOST_CID, port, mount_point)
}

/// Mount a FUSE filesystem with multiple reader threads.
///
/// Same as `mount_vsock` but creates multiple FUSE reader threads for
/// better parallel performance.
#[allow(dead_code)]
pub fn mount_vsock_with_readers(port: u32, mount_point: &str, num_readers: usize) -> anyhow::Result<()> {
    eprintln!("[fc-agent] mounting FUSE volume at {} via vsock port {} ({} readers)",
        mount_point, port, num_readers);
    fuse_pipe::mount_vsock_with_readers(HOST_CID, port, mount_point, num_readers)
}
