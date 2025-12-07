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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Once};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use fuse_pipe::{AsyncServer, PassthroughFs, ServerConfig};
use tracing::{debug, info, error};

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
    client_thread: Option<JoinHandle<()>>,
    shutdown: Arc<AtomicBool>,
    data_dir: PathBuf,
    mount_dir: PathBuf,
    socket: PathBuf,
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

        let shutdown = Arc::new(AtomicBool::new(false));
        let socket_path = socket.to_str().unwrap().to_string();

        // Start server in dedicated thread with its own runtime
        let server_data_path = data_path.to_path_buf();
        let server_socket = socket_path.clone();
        let server_shutdown = Arc::clone(&shutdown);
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
                // Run server until shutdown
                if let Err(e) = server.serve_unix(&server_socket).await {
                    if !server_shutdown.load(Ordering::SeqCst) {
                        error!(target: TARGET, error = %e, "Server error");
                    }
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

        // Start FUSE client in dedicated thread
        let client_socket = socket_path.clone();
        let client_mount = mount_path.to_path_buf();
        let client_shutdown = Arc::clone(&shutdown);
        let client_thread = thread::spawn(move || {
            info!(target: TARGET, socket = %client_socket, mount = ?client_mount, readers = num_readers, "Client thread started");
            match fuse_pipe::mount_with_readers(&client_socket, &client_mount, num_readers) {
                Ok(()) => {
                    info!(target: TARGET, "Client mount_with_readers returned Ok");
                }
                Err(e) => {
                    if !client_shutdown.load(Ordering::SeqCst) {
                        error!(target: TARGET, error = %e, error_debug = ?e, "Client mount_with_readers failed");
                    } else {
                        debug!(target: TARGET, error = %e, "Client shutdown (expected)");
                    }
                }
            }
            info!(target: TARGET, "Client thread exiting");
        });

        // Wait for mount to appear in /proc/mounts
        info!(target: TARGET, mount = ?mount_path, "Waiting for mount to appear in /proc/mounts");
        let mount_str = mount_path.to_str().unwrap();
        for i in 0..100 {
            if let Ok(mounts) = fs::read_to_string("/proc/mounts") {
                if mounts.lines().any(|line| line.contains(mount_str) && line.contains("fuse")) {
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
            client_thread: Some(client_thread),
            shutdown,
            data_dir: data_path.to_path_buf(),
            mount_dir: mount_path.to_path_buf(),
            socket,
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
        // Signal shutdown
        self.shutdown.store(true, Ordering::SeqCst);

        // Unmount with lazy flag
        let _ = std::process::Command::new("fusermount3")
            .args(["-uz", self.mount_dir.to_str().unwrap()])
            .status();
        let _ = std::process::Command::new("fusermount")
            .args(["-uz", self.mount_dir.to_str().unwrap()])
            .status();

        // Brief wait for client thread (should exit quickly after unmount)
        if let Some(handle) = self.client_thread.take() {
            let (tx, rx) = std::sync::mpsc::channel();
            thread::spawn(move || { let _ = tx.send(handle.join()); });
            let _ = rx.recv_timeout(Duration::from_millis(500));
        }

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
        f.write_all(&vec![0x42u8; file_size]).expect("write test data");
    }
}

#[cfg(test)]
mod tests {
    #[allow(unused_imports)]
    use super::*;

    #[test]
    fn test_in_process_mount() {
        let data_dir = PathBuf::from("/tmp/fuse-common-test-data");
        let mount_dir = PathBuf::from("/tmp/fuse-common-test-mount");

        // Cleanup
        let _ = fs::remove_dir_all(&data_dir);
        let _ = std::process::Command::new("fusermount3")
            .args(["-u", mount_dir.to_str().unwrap()])
            .status();
        let _ = fs::remove_dir_all(&mount_dir);

        let fuse = FuseMount::new(&data_dir, &mount_dir, 1);

        // Test basic operations
        let test_file = fuse.mount_path().join("test.txt");
        fs::write(&test_file, "hello").expect("write");
        let content = fs::read_to_string(&test_file).expect("read");
        assert_eq!(content, "hello");
        fs::remove_file(&test_file).expect("remove");

        drop(fuse);

        // Cleanup
        let _ = fs::remove_dir_all(&data_dir);
        let _ = fs::remove_dir_all(&mount_dir);
    }
}
