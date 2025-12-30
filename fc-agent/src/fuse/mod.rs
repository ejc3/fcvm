//! FUSE-over-vsock volume mounting for guest.
//!
//! This module uses fuse-pipe to mount host directories inside the guest VM.
//! The fuse-pipe library handles all the complexity of multiplexed FUSE
//! request handling over vsock.

use fuse_pipe::transport::HOST_CID;

/// Default number of FUSE reader threads for parallel I/O.
///
/// Benchmarks (256 workers, 1024 × 4KB files):
/// - 1 reader: 3.0s writes (serialization bottleneck, 18x slower)
/// - 16 readers: 61ms reads (optimal for reads)
/// - 64 readers: 165ms writes ≈ host 161ms (optimal for writes)
/// - 256 readers: 162ms writes (negligible improvement, 4x memory)
///
/// 64 balances performance with memory (each reader stack ≈ 8MB).
/// Can be overridden via FCVM_FUSE_READERS environment variable.
const DEFAULT_NUM_READERS: usize = 64;

/// Get the configured number of FUSE readers.
/// Checks (in order):
/// 1. FCVM_FUSE_READERS environment variable
/// 2. fuse_readers=N kernel boot parameter (from /proc/cmdline)
/// 3. DEFAULT_NUM_READERS (64)
fn get_num_readers() -> usize {
    // First check environment variable
    if let Some(n) = std::env::var("FCVM_FUSE_READERS")
        .ok()
        .and_then(|s| s.parse().ok())
    {
        return n;
    }

    // Then check kernel command line
    if let Ok(cmdline) = std::fs::read_to_string("/proc/cmdline") {
        for part in cmdline.split_whitespace() {
            if let Some(value) = part.strip_prefix("fuse_readers=") {
                if let Ok(n) = value.parse() {
                    return n;
                }
            }
        }
    }

    DEFAULT_NUM_READERS
}

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
    let num_readers = get_num_readers();
    eprintln!(
        "[fc-agent] mounting FUSE volume at {} via vsock port {} ({} readers)",
        mount_point, port, num_readers
    );
    fuse_pipe::mount_vsock_with_readers(HOST_CID, port, mount_point, num_readers)
}

/// Mount a FUSE filesystem with multiple reader threads.
///
/// Same as `mount_vsock` but creates multiple FUSE reader threads for
/// better parallel performance.
#[allow(dead_code)]
pub fn mount_vsock_with_readers(
    port: u32,
    mount_point: &str,
    num_readers: usize,
) -> anyhow::Result<()> {
    eprintln!(
        "[fc-agent] mounting FUSE volume at {} via vsock port {} ({} readers)",
        mount_point, port, num_readers
    );
    fuse_pipe::mount_vsock_with_readers(HOST_CID, port, mount_point, num_readers)
}
