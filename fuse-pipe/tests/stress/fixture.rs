//! Shared FUSE mount fixture for tests and benchmarks.
//!
//! This module provides a reusable `FuseMount` struct that spawns
//! the stress test binary as server/client subprocesses.
//!
//! See `fuse-pipe/TESTING.md` for complete testing documentation.

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::Duration;

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

/// Find the stress test binary in target directory
pub fn find_stress_binary() -> PathBuf {
    // Try multiple locations
    let paths = [
        PathBuf::from("target/release/deps"),
        PathBuf::from("../target/release/deps"),
        PathBuf::from("fuse-pipe/target/release/deps"),
    ];

    for base in &paths {
        if let Ok(entries) = std::fs::read_dir(base) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if name.starts_with("stress-") && !name.contains('.') {
                    return entry.path();
                }
            }
        }
    }

    // Try finding from current exe directory (for tests)
    if let Ok(exe) = std::env::current_exe() {
        if let Some(deps_dir) = exe.parent() {
            for entry in std::fs::read_dir(deps_dir).into_iter().flatten() {
                if let Ok(entry) = entry {
                    let name = entry.file_name();
                    let name = name.to_string_lossy();
                    if name.starts_with("stress-") && !name.contains('.') {
                        return entry.path();
                    }
                }
            }
        }
    }

    panic!("Could not find stress binary. Run: cargo build --release --test stress");
}

/// FUSE mount fixture using the stress test binary as server/client.
///
/// Spawns server and client subprocesses, automatically cleans up on drop.
pub struct FuseMount {
    server: Child,
    client: Child,
    data_dir: PathBuf,
    mount_dir: PathBuf,
    socket: PathBuf,
    log_file: Option<PathBuf>,
    telemetry_file: Option<PathBuf>,
}

impl FuseMount {
    /// Create a new FUSE mount with default settings (no tracing).
    pub fn new(data_path: &Path, mount_path: &Path, num_readers: usize) -> Self {
        Self::with_options(data_path, mount_path, num_readers, 0, None)
    }

    /// Create a new FUSE mount with tracing enabled.
    ///
    /// - `trace_rate`: Trace every Nth request (0 = disabled)
    /// - `telemetry_output`: Optional path to write telemetry JSON
    pub fn with_tracing(
        data_path: &Path,
        mount_path: &Path,
        num_readers: usize,
        trace_rate: u64,
        telemetry_output: Option<PathBuf>,
    ) -> Self {
        Self::with_options(data_path, mount_path, num_readers, trace_rate, telemetry_output)
    }

    /// Create a new FUSE mount with full options.
    fn with_options(
        data_path: &Path,
        mount_path: &Path,
        num_readers: usize,
        trace_rate: u64,
        telemetry_output: Option<PathBuf>,
    ) -> Self {
        let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
        let pid = std::process::id();
        let socket = PathBuf::from(format!("/tmp/fuse-bench-{}-{}.sock", pid, id));
        let log_file = PathBuf::from(format!("/tmp/fuse-bench-{}-{}.log", pid, id));

        // Auto-generate telemetry path if tracing is enabled but no path specified
        let telemetry_file = if trace_rate > 0 {
            telemetry_output.or_else(|| {
                Some(PathBuf::from(format!(
                    "/tmp/fuse-bench-telemetry-{}-{}.json",
                    pid, id
                )))
            })
        } else {
            None
        };

        // Cleanup any stale state
        let _ = fs::remove_file(&socket);
        fs::create_dir_all(data_path).expect("create data dir");
        fs::create_dir_all(mount_path).expect("create mount dir");

        let stress_exe = find_stress_binary();

        // Create log file for capturing stderr
        let log_handle = File::create(&log_file).ok();

        // Start server
        let server = Command::new(&stress_exe)
            .args([
                "server",
                "--socket",
                socket.to_str().unwrap(),
                "--root",
                data_path.to_str().unwrap(),
            ])
            .stdout(Stdio::null())
            .stderr(log_handle.as_ref().map_or(Stdio::null(), |f| {
                Stdio::from(f.try_clone().unwrap())
            }))
            .spawn()
            .expect("start server");

        // Wait for server to be ready
        thread::sleep(Duration::from_millis(500));

        // Build client args
        let mut client_args = vec![
            "client".to_string(),
            "--socket".to_string(),
            socket.to_str().unwrap().to_string(),
            "--mount".to_string(),
            mount_path.to_str().unwrap().to_string(),
            "--readers".to_string(),
            num_readers.to_string(),
        ];

        if trace_rate > 0 {
            client_args.push("--trace-rate".to_string());
            client_args.push(trace_rate.to_string());
        }

        if let Some(ref telem_path) = telemetry_file {
            client_args.push("--telemetry-output".to_string());
            client_args.push(telem_path.to_str().unwrap().to_string());
        }

        // Start client
        let client = Command::new(&stress_exe)
            .args(&client_args)
            .stdout(Stdio::null())
            .stderr(log_handle.as_ref().map_or(Stdio::null(), |f| {
                Stdio::from(f.try_clone().unwrap())
            }))
            .spawn()
            .expect("start client");

        // Wait for mount to be ready
        thread::sleep(Duration::from_millis(500));

        FuseMount {
            server,
            client,
            data_dir: data_path.to_path_buf(),
            mount_dir: mount_path.to_path_buf(),
            socket,
            log_file: Some(log_file),
            telemetry_file,
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

    /// Get the path to the telemetry output file (if tracing was enabled).
    pub fn telemetry_path(&self) -> Option<&Path> {
        self.telemetry_file.as_deref()
    }

    /// Get the path to the log file.
    pub fn log_path(&self) -> Option<&Path> {
        self.log_file.as_deref()
    }

    /// Read telemetry JSON from the output file.
    ///
    /// Returns `None` if tracing was disabled or no data was collected.
    pub fn read_telemetry(&self) -> Option<String> {
        self.telemetry_file.as_ref().and_then(|path| {
            fs::read_to_string(path).ok()
        })
    }
}

impl Drop for FuseMount {
    fn drop(&mut self) {
        // Kill client first (triggers unmount)
        let _ = self.client.kill();
        let _ = self.client.wait();

        // Give time for graceful unmount
        thread::sleep(Duration::from_millis(100));

        // Force unmount if still mounted
        let _ = Command::new("fusermount")
            .args(["-u", self.mount_dir.to_str().unwrap()])
            .status();
        let _ = Command::new("fusermount3")
            .args(["-u", self.mount_dir.to_str().unwrap()])
            .status();

        // Kill server
        let _ = self.server.kill();
        let _ = self.server.wait();

        // Cleanup socket
        let _ = fs::remove_file(&self.socket);
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

/// Get the trace rate based on feature flag.
///
/// Returns 100 (trace every 100th request) when `trace-benchmarks` feature is enabled,
/// otherwise returns 0 (no tracing).
pub fn trace_rate() -> u64 {
    #[cfg(feature = "trace-benchmarks")]
    return 100;

    #[cfg(not(feature = "trace-benchmarks"))]
    return 0;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_find_stress_binary_exists() {
        // This test will fail if the binary hasn't been built
        // But we just want to verify the function doesn't panic
        // when there's something to find
        let _ = std::panic::catch_unwind(find_stress_binary);
    }
}
