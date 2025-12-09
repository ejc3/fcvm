//! Shared test fixture for in-process FUSE mount testing.
//!
//! This module provides `FuseMount` - an in-process FUSE server+client fixture
//! that runs the server in a tokio task and the FUSE client in a dedicated thread.
//!
//! Benefits over subprocess approach:
//! - Stack traces on failure
//! - Easier debugging
//! - Faster startup
//! - No binary discovery issues
//!
//! See `fuse-pipe/TESTING.md` for complete testing documentation.

// Allow dead code - these utilities are conditionally used by different test files
#![allow(dead_code)]

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Once;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use fuse_pipe::{AsyncServer, MountConfig, MountHandle, PassthroughFs, ServerConfig};
use tracing::{debug, info};

/// Target name for fixture logs (consistent with library naming)
const TARGET: &str = "fuse_pipe::fixture";
use tracing_subscriber::EnvFilter;

/// Initialize tracing once for the test process.
static TRACING_INIT: Once = Once::new();

fn init_tracing() {
    TRACING_INIT.call_once(|| {
        tracing_subscriber::fmt()
            .with_env_filter(EnvFilter::from_default_env())
            .with_writer(std::io::stderr)
            .init();
    });
}

/// Global counter for unique test IDs
static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Check if a path is a FUSE mount by looking in /proc/mounts.
pub fn is_fuse_mount(path: &Path) -> bool {
    if let Ok(mounts) = fs::read_to_string("/proc/mounts") {
        let path_str = path.to_str().unwrap_or("");
        mounts
            .lines()
            .any(|line| line.contains(path_str) && line.contains("fuse"))
    } else {
        false
    }
}

/// Create unique paths for each test with the given prefix.
pub fn unique_paths(prefix: &str) -> (PathBuf, PathBuf) {
    let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
    let pid = std::process::id();
    let data_dir = PathBuf::from(format!("/tmp/{}-data-{}-{}", prefix, pid, id));
    let mount_dir = PathBuf::from(format!("/tmp/{}-mount-{}-{}", prefix, pid, id));

    // Cleanup any stale state - only unmount if actually mounted
    let _ = fs::remove_dir_all(&data_dir);
    if is_fuse_mount(&mount_dir) {
        let _ = std::process::Command::new("fusermount3")
            .args(["-u", mount_dir.to_str().unwrap()])
            .status();
    }
    let _ = fs::remove_dir_all(&mount_dir);

    (data_dir, mount_dir)
}

/// Cleanup directories after test.
pub fn cleanup(data_dir: &Path, mount_dir: &Path) {
    let _ = fs::remove_dir_all(data_dir);
    let _ = fs::remove_dir_all(mount_dir);
}

/// Increase file descriptor limit for high-concurrency tests
pub fn increase_ulimit() {
    use std::mem::MaybeUninit;

    unsafe {
        let mut rlim = MaybeUninit::<libc::rlimit>::uninit();
        if libc::getrlimit(libc::RLIMIT_NOFILE, rlim.as_mut_ptr()) == 0 {
            let mut rlim = rlim.assume_init();
            let target = rlim.rlim_max.max(65536);
            rlim.rlim_cur = target;
            rlim.rlim_max = target;
            let _ = libc::setrlimit(libc::RLIMIT_NOFILE, &rlim);
        }
    }
}

/// In-process FUSE mount fixture.
///
/// Spawns server and client in-process:
/// - Server runs in a dedicated thread with its own tokio runtime
/// - Client/FUSE runs in a dedicated thread (fuser blocks)
///
/// Automatically unmounts and cleans up on drop.
pub struct FuseMount {
    server_thread: Option<JoinHandle<()>>,
    data_dir: PathBuf,
    mount_dir: PathBuf,
    socket: PathBuf,
    /// Handle for FUSE mount - dropping this unmounts automatically
    mount_handle: Option<MountHandle>,
}

impl FuseMount {
    /// Create a new FUSE mount with default settings.
    pub fn new(data_path: &Path, mount_path: &Path, num_readers: usize) -> Self {
        // Initialize tracing for debug logging
        init_tracing();

        let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
        let pid = std::process::id();
        let socket = PathBuf::from(format!("/tmp/fuse-test-{}-{}.sock", pid, id));

        // Cleanup any stale state
        let _ = fs::remove_file(&socket);
        fs::create_dir_all(data_path).expect("create data dir");
        fs::create_dir_all(mount_path).expect("create mount dir");

        let socket_path = socket.to_str().unwrap().to_string();

        // Start server in dedicated thread with its own runtime
        let server_data_path = data_path.to_path_buf();
        let server_socket = socket_path.clone();
        let server_thread = thread::spawn(move || {
            info!(target: TARGET, socket = %server_socket, data = ?server_data_path, "Server thread starting");
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("build server runtime");

            rt.block_on(async {
                let fs = PassthroughFs::new(&server_data_path);
                let config = ServerConfig::default();
                let server = AsyncServer::with_config(fs, config);

                info!(target: TARGET, "Server calling serve_unix");
                if let Err(e) = server.serve_unix(&server_socket).await {
                    // Server exits when client disconnects - this is expected
                    debug!(target: TARGET, error = %e, "Server exited");
                }
                info!(target: TARGET, "Server exiting");
            });
        });

        // Wait for server to be ready (socket exists)
        for i in 0..100 {
            if Path::new(&socket_path).exists() {
                debug!(target: TARGET, iterations = i, "Socket ready");
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }

        // Start FUSE client using mount_spawn (returns handle for RAII cleanup)
        info!(target: TARGET, socket = %socket_path, mount = ?mount_path, readers = num_readers, "Starting FUSE client");
        let config = MountConfig::new().readers(num_readers);
        let mount_handle = fuse_pipe::mount_spawn(&socket_path, mount_path.to_path_buf(), config)
            .expect("mount_spawn failed");
        info!(target: TARGET, "mount_spawn succeeded");

        // Wait for mount to appear in /proc/mounts
        info!(target: TARGET, mount = ?mount_path, "Waiting for mount to appear in /proc/mounts");
        let mount_str = mount_path.to_str().unwrap();
        for i in 0..100 {
            if let Ok(mounts) = fs::read_to_string("/proc/mounts") {
                if mounts
                    .lines()
                    .any(|line| line.contains(mount_str) && line.contains("fuse"))
                {
                    info!(target: TARGET, iterations = i, "Mount ready");
                    break;
                }
            }
            if i % 10 == 0 {
                debug!(target: TARGET, attempt = i, "Still waiting for mount");
            }
            thread::sleep(Duration::from_millis(50));
        }

        FuseMount {
            server_thread: Some(server_thread),
            data_dir: data_path.to_path_buf(),
            mount_dir: mount_path.to_path_buf(),
            socket,
            mount_handle: Some(mount_handle), // MountHandle::drop() will unmount
        }
    }

    /// Get the FUSE mount path (where operations should be performed).
    pub fn mount_path(&self) -> &Path {
        &self.mount_dir
    }

    /// Get the underlying data directory path.
    pub fn data_path(&self) -> &Path {
        &self.data_dir
    }
}

impl Drop for FuseMount {
    fn drop(&mut self) {
        info!(target: TARGET, "Drop starting - unmounting");

        // Drop mount handle - MountHandle::drop() calls unmounter and waits for thread
        drop(self.mount_handle.take());

        // Remove socket - don't wait for server thread, let it be orphaned
        // (it'll die when test process exits)
        let _ = fs::remove_file(&self.socket);

        // Don't wait for server_thread - it blocks on accept() forever
        // Just drop the handle and let it be orphaned
        let _ = self.server_thread.take();
        info!(target: TARGET, "Drop complete");
    }
}

/// Setup test data in a directory.
pub fn setup_test_data(base: &Path, num_files: usize, file_size: usize) {
    fs::create_dir_all(base).expect("create test data dir");
    for i in 0..num_files {
        let path = base.join(format!("file_{}.dat", i));
        let mut f = File::create(&path).expect("create test file");
        f.write_all(&vec![0x42u8; file_size])
            .expect("write test data");
    }
}

#[cfg(test)]
mod tests {
    #[allow(unused_imports)]
    use super::*;

    #[test]
    fn test_in_process_mount() {
        let (data_dir, mount_dir) = unique_paths("fuse-common");

        let fuse = FuseMount::new(&data_dir, &mount_dir, 1);

        // Test basic operations
        let test_file = fuse.mount_path().join("test.txt");
        fs::write(&test_file, "hello").expect("write");
        let content = fs::read_to_string(&test_file).expect("read");
        assert_eq!(content, "hello");
        fs::remove_file(&test_file).expect("remove");

        drop(fuse);
        cleanup(&data_dir, &mount_dir);
    }
}
