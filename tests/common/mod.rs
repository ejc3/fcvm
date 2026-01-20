// Common test utilities for fcvm integration tests
#![allow(dead_code)]

use fs2::FileExt;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// Default test image - use AWS ECR to avoid Docker Hub rate limits
pub const TEST_IMAGE: &str = "public.ecr.aws/nginx/nginx:alpine";

/// Standard log directory for test logs
const TEST_LOG_DIR: &str = "/tmp/fcvm-test-logs";

/// Test logger that writes detailed logs to a file while keeping console output clean.
///
/// Usage:
/// ```ignore
/// let logger = TestLogger::new("my_test_name");
/// logger.info("Starting test...");
/// logger.debug("Detailed info that would clutter console");
/// // At test end, logger.finish() prints the log file path
/// ```
pub struct TestLogger {
    test_name: String,
    log_path: PathBuf,
    file: Arc<Mutex<std::fs::File>>,
    start_time: std::time::Instant,
}

impl TestLogger {
    /// Create a new test logger. Logs are written to /tmp/fcvm-test-logs/{test_name}-{timestamp}-{pid}.log
    pub fn new(test_name: &str) -> Self {
        // Create log directory if needed
        std::fs::create_dir_all(TEST_LOG_DIR).ok();

        // Include PID to avoid conflicts between host and container tests
        let timestamp = chrono::Utc::now().format("%Y%m%d-%H%M%S");
        let pid = std::process::id();
        let log_path = PathBuf::from(format!(
            "{}/{}-{}-{}.log",
            TEST_LOG_DIR, test_name, timestamp, pid
        ));

        // Try to create the file, fall back to /tmp if log dir has permission issues
        let (file, log_path) = match std::fs::File::create(&log_path) {
            Ok(f) => (f, log_path),
            Err(_) => {
                // Fall back to /tmp with a unique name
                let fallback = PathBuf::from(format!(
                    "/tmp/fcvm-test-{}-{}-{}.log",
                    test_name, timestamp, pid
                ));
                let f = std::fs::File::create(&fallback)
                    .expect("Failed to create test log file even in /tmp");
                (f, fallback)
            }
        };

        let logger = Self {
            test_name: test_name.to_string(),
            log_path,
            file: Arc::new(Mutex::new(file)),
            start_time: std::time::Instant::now(),
        };

        logger.log_raw(&format!(
            "=== Test: {} ===\nStarted: {}\n\n",
            test_name,
            chrono::Utc::now().format("%Y-%m-%d %H:%M:%S UTC")
        ));

        logger
    }

    /// Log a raw message (no prefix)
    pub fn log_raw(&self, msg: &str) {
        if let Ok(mut file) = self.file.lock() {
            writeln!(file, "{}", msg).ok();
        }
    }

    /// Log an info message with timestamp
    pub fn info(&self, msg: &str) {
        let elapsed = self.start_time.elapsed().as_secs_f64();
        self.log_raw(&format!("[{:>8.3}s] INFO  {}", elapsed, msg));
    }

    /// Log a debug message with timestamp (detailed info)
    pub fn debug(&self, msg: &str) {
        let elapsed = self.start_time.elapsed().as_secs_f64();
        self.log_raw(&format!("[{:>8.3}s] DEBUG {}", elapsed, msg));
    }

    /// Log an error message with timestamp
    pub fn error(&self, msg: &str) {
        let elapsed = self.start_time.elapsed().as_secs_f64();
        self.log_raw(&format!("[{:>8.3}s] ERROR {}", elapsed, msg));
    }

    /// Log a section header
    pub fn section(&self, name: &str) {
        let elapsed = self.start_time.elapsed().as_secs_f64();
        self.log_raw(&format!("\n[{:>8.3}s] === {} ===", elapsed, name));
    }

    /// Log command output (stdout and stderr)
    pub fn log_output(&self, label: &str, output: &std::process::Output) {
        self.debug(&format!("{} status: {}", label, output.status));
        if !output.stdout.is_empty() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            self.debug(&format!("{} stdout:\n{}", label, stdout));
        }
        if !output.stderr.is_empty() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            self.debug(&format!("{} stderr:\n{}", label, stderr));
        }
    }

    /// Get the log file path
    pub fn path(&self) -> &PathBuf {
        &self.log_path
    }

    /// Finish logging and print the log file path to console.
    /// Call this at the end of the test.
    pub fn finish(&self, success: bool) {
        let status = if success { "PASSED" } else { "FAILED" };
        let elapsed = self.start_time.elapsed();

        self.log_raw(&format!(
            "\n=== Test {} in {:.2}s ===",
            status,
            elapsed.as_secs_f64()
        ));

        // Print log path to console (visible in test output)
        eprintln!(
            "\n📋 Test log: {} ({:.2}s)",
            self.log_path.display(),
            elapsed.as_secs_f64()
        );
    }

    /// Finish with failure and print the log file path prominently
    pub fn finish_failed(&self, error: &str) {
        self.error(error);
        self.finish(false);
        // Also print error to console for immediate visibility
        eprintln!("❌ Test failed: {}", error);
    }
}

impl Clone for TestLogger {
    fn clone(&self) -> Self {
        Self {
            test_name: self.test_name.clone(),
            log_path: self.log_path.clone(),
            file: self.file.clone(),
            start_time: self.start_time,
        }
    }
}

/// Polling interval for status checks (100ms)
pub const POLL_INTERVAL: Duration = Duration::from_millis(100);
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Once;
use std::time::Duration;
use tokio::time::sleep;

/// Global counter for unique test IDs
static TEST_COUNTER: AtomicUsize = AtomicUsize::new(0);

/// Ensure config is generated once per test run
static CONFIG_INIT: Once = Once::new();

/// Ensure the fcvm config file exists.
///
/// Uses file locking to prevent race conditions when multiple test processes
/// try to generate the config simultaneously. Each process:
/// 1. Acquires an exclusive lock on /tmp/fcvm-config-gen.lock
/// 2. Checks if config already exists and is valid
/// 3. If not, generates it with `fcvm setup --generate-config --force`
/// 4. Releases lock (automatically on drop)
fn ensure_config_exists() {
    CONFIG_INIT.call_once(|| {
        let uid = unsafe { libc::getuid() };
        let lock_path = format!("/tmp/fcvm-config-gen-{}.lock", uid);
        let lock_file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(lock_path)
            .expect("failed to open config generation lock file");

        // Acquire exclusive lock (blocks until available)
        lock_file
            .lock_exclusive()
            .expect("failed to acquire config generation lock");

        // Check if config already exists and is valid
        let config_path = std::env::var("HOME")
            .map(|h| PathBuf::from(h).join(".config/fcvm/rootfs-config.toml"))
            .unwrap_or_else(|_| PathBuf::from("/tmp/fcvm-config/rootfs-config.toml"));

        let needs_generation = if config_path.exists() {
            // Try to parse it - if parsing fails, regenerate
            match std::fs::read_to_string(&config_path) {
                Ok(content) => {
                    // Check if it has the required [base] section
                    !content.contains("[base]")
                }
                Err(_) => true,
            }
        } else {
            true
        };

        if needs_generation {
            let fcvm_path = find_fcvm_binary().expect("fcvm binary not found");
            let output = Command::new(&fcvm_path)
                .args(["setup", "--generate-config", "--force"])
                .output()
                .expect("failed to run fcvm setup --generate-config");

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                panic!("Failed to generate config: {}", stderr);
            }
            eprintln!(">>> Generated config at ~/.config/fcvm/rootfs-config.toml");
        }
        // Lock is released when lock_file is dropped
    });
}

/// Check if we're running inside a container.
///
/// Containers create marker files that we can use to detect containerized environments.
fn is_in_container() -> bool {
    // Podman creates /run/.containerenv
    if std::path::Path::new("/run/.containerenv").exists() {
        return true;
    }
    // Docker creates /.dockerenv
    if std::path::Path::new("/.dockerenv").exists() {
        return true;
    }
    false
}

/// Generate unique names for snapshot/clone tests.
///
/// Returns (baseline_name, clone_name, snapshot_name, serve_name) with unique suffixes.
/// Uses process ID and atomic counter to ensure uniqueness across parallel tests.
///
/// # Arguments
/// * `prefix` - Base name for the test (e.g., "portfwd", "internet")
///
/// # Returns
/// Tuple of (baseline, clone, snapshot, serve) names
pub fn unique_names(prefix: &str) -> (String, String, String, String) {
    let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
    let pid = std::process::id();
    let suffix = format!("{}-{}", pid, id);

    (
        format!("{}-base-{}", prefix, suffix),
        format!("{}-clone-{}", prefix, suffix),
        format!("{}-snap-{}", prefix, suffix),
        format!("{}-serve-{}", prefix, suffix),
    )
}

/// Fixture for managing a VM with FUSE volume for testing
pub struct VmFixture {
    pub child: tokio::process::Child,
    pub pid: u32,
    pub vm_name: String,
    pub host_dir: PathBuf,
    pub mount_dir: PathBuf,
}

impl VmFixture {
    /// Create a new VM with a FUSE volume mounted
    pub async fn new(test_name: &str) -> anyhow::Result<Self> {
        let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
        let pid = std::process::id();
        let vm_name = format!("{}-{}-{}", test_name, pid, id);

        let base_dir = PathBuf::from(format!("/tmp/fcvm-test-{}-{}", pid, id));
        std::fs::create_dir_all(&base_dir)?;

        let host_dir = base_dir.join("host");
        let mount_dir = base_dir.join("mount");
        std::fs::create_dir_all(&host_dir)?;
        std::fs::create_dir_all(&mount_dir)?;

        // Start VM with FUSE volume using spawn_fcvm helper
        // (uses Stdio::null() to prevent pipe buffer deadlock)
        let map_arg = format!("{}:/mnt/test", host_dir.display());
        let (mut child, pid) = spawn_fcvm(&[
            "podman",
            "run",
            "--name",
            &vm_name,
            "--network",
            "bridged", // Use bridged for more reliable health checks
            "--map",
            &map_arg,
            "--",
            TEST_IMAGE,
        ])
        .await?;

        // Wait for VM to become healthy
        // Use 180 second timeout to account for rootfs creation on first run (~60 sec)
        if let Err(e) = poll_health_by_pid(pid, 180).await {
            let _ = child.kill().await;
            anyhow::bail!("VM failed to become healthy: {}", e);
        }

        Ok(Self {
            child,
            pid,
            vm_name,
            host_dir,
            mount_dir,
        })
    }

    /// Get the mount directory inside the guest (guest path)
    pub fn guest_mount(&self) -> &str {
        "/mnt/test"
    }

    /// Get the host directory (host path)
    pub fn host_dir(&self) -> &PathBuf {
        &self.host_dir
    }

    /// Get the PID of the VM process
    pub fn pid(&self) -> u32 {
        self.pid
    }
}

impl Drop for VmFixture {
    fn drop(&mut self) {
        // Use centralized graceful kill (SIGTERM first, then SIGKILL)
        // This allows fcvm to run cleanup handlers
        fcvm::utils::graceful_kill(self.pid, 2000);
        let _ = self.child.try_wait();

        // Cleanup directories
        if let Some(parent) = self.host_dir.parent() {
            std::fs::remove_dir_all(parent).ok();
        }
    }
}

/// Spawn fcvm with safe stdio settings to prevent pipe buffer deadlock.
///
/// Uses `Stdio::inherit()` - output goes directly to parent's stdout/stderr.
/// Simple and safe, but output is not prefixed with process name.
///
/// **Debug logging:** When `FCVM_DEBUG_LOGS=1`, logs are written to
/// `/tmp/fcvm-test-logs/` with RUST_LOG=debug.
///
/// For prefixed output like `[vm-name] ...`, use `spawn_fcvm_with_logs()` instead.
///
/// # Arguments
/// * `args` - Arguments to pass to fcvm (e.g., ["podman", "run", "--name", "test", ...])
///
/// # Returns
/// Tuple of (Child process, PID)
pub async fn spawn_fcvm(args: &[&str]) -> anyhow::Result<(tokio::process::Child, u32)> {
    // Extract name from args (--name value) for log file naming
    let name = args
        .windows(2)
        .find(|w| w[0] == "--name")
        .map(|w| w[1])
        .unwrap_or("fcvm");

    // Delegate to spawn_fcvm_with_logs which handles debug logging
    spawn_fcvm_with_logs(args, name).await
}

/// Add implicit flags to fcvm commands for tests
fn maybe_add_test_flags(args: &[&str]) -> Vec<String> {
    let strace_enabled = std::env::var("FCVM_STRACE_AGENT")
        .map(|v| v == "1")
        .unwrap_or(false);

    let mut result: Vec<String> = args.iter().map(|s| s.to_string()).collect();

    // Only add flags for "podman run" and "snapshot run" commands
    let is_podman_run = args.len() >= 2 && args[0] == "podman" && args[1] == "run";
    let is_snapshot_run = args.len() >= 2 && args[0] == "snapshot" && args[1] == "run";

    if (is_podman_run || is_snapshot_run) && strace_enabled {
        result.insert(2, "--strace-agent".to_string());
        eprintln!(">>> STRACE MODE: Adding --strace-agent flag");
    }

    result
}

/// Spawn fcvm with piped IO and automatic log consumers.
///
/// Output is prefixed with `[name]` for stdout and `[name ERR]` for stderr,
/// useful when running multiple VMs in parallel.
///
/// **Logging:** All output is automatically written to `/tmp/fcvm-test-logs/{name}-{timestamp}.log`
/// with RUST_LOG=debug for full debug output. Console shows only INFO/WARN/ERROR.
/// Log files are uploaded as CI artifacts on failure.
///
/// # Arguments
/// * `args` - Arguments to pass to fcvm
/// * `name` - Prefix for log output (e.g., "baseline", "clone-1")
///
/// # Returns
/// Tuple of (Child process, PID)
pub async fn spawn_fcvm_with_logs(
    args: &[&str],
    name: &str,
) -> anyhow::Result<(tokio::process::Child, u32)> {
    // Ensure config exists (runs once per test process)
    ensure_config_exists();

    let fcvm_path = find_fcvm_binary()?;
    let final_args = maybe_add_test_flags(args);

    // Always create logger for debug output to file
    let logger = TestLogger::new(name);

    let mut cmd = tokio::process::Command::new(&fcvm_path);
    cmd.args(&final_args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("RUST_LOG", "debug");

    let mut child = cmd
        .spawn()
        .map_err(|e| anyhow::anyhow!("failed to spawn fcvm: {}", e))?;

    let pid = child
        .id()
        .ok_or_else(|| anyhow::anyhow!("failed to get fcvm PID"))?;

    logger.info(&format!("Spawned fcvm PID={} args={:?}", pid, args));

    // Spawn log consumers immediately to prevent pipe buffer deadlock
    spawn_log_consumer_to_file(child.stdout.take(), name, Some(logger.clone()), false);
    spawn_log_consumer_to_file(child.stderr.take(), name, Some(logger), true);

    Ok((child, pid))
}

/// Spawn a task to consume stdout and print with `[name]` prefix
pub fn spawn_log_consumer(stdout: Option<tokio::process::ChildStdout>, name: &str) {
    spawn_log_consumer_to_file(stdout, name, None, false);
}

/// Spawn a task to consume stderr and print with `[name ERR]` prefix
pub fn spawn_log_consumer_stderr(stderr: Option<tokio::process::ChildStderr>, name: &str) {
    spawn_log_consumer_to_file(stderr, name, None, true);
}

/// Spawn a task to consume stdout with file logging
pub fn spawn_log_consumer_with_logger(
    stdout: Option<tokio::process::ChildStdout>,
    name: &str,
    logger: TestLogger,
) {
    spawn_log_consumer_to_file(stdout, name, Some(logger), false);
}

/// Spawn a task to consume stderr with file logging
pub fn spawn_log_consumer_stderr_with_logger(
    stderr: Option<tokio::process::ChildStderr>,
    name: &str,
    logger: TestLogger,
) {
    spawn_log_consumer_to_file(stderr, name, Some(logger), true);
}

/// Internal: spawn log consumer that writes to console and optionally to a file
///
/// When a logger is provided:
/// - All lines (including DEBUG/TRACE) are written to the file
/// - Only non-debug lines are printed to console for cleaner output
fn spawn_log_consumer_to_file<R: tokio::io::AsyncRead + Unpin + Send + 'static>(
    reader: Option<R>,
    name: &str,
    logger: Option<TestLogger>,
    is_stderr: bool,
) {
    use tokio::io::{AsyncBufReadExt, BufReader};
    if let Some(reader) = reader {
        let name = name.to_string();
        let has_logger = logger.is_some();
        tokio::spawn(async move {
            let reader = BufReader::new(reader);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let prefix = if is_stderr {
                    format!("[{} ERR]", name)
                } else {
                    format!("[{}]", name)
                };
                let formatted = format!("{} {}", prefix, line);

                // Always write to file if logger provided
                if let Some(ref log) = logger {
                    log.log_raw(&formatted);
                }

                // Only print non-debug lines to console when logging to file
                // This keeps console clean while file has full debug output
                let is_debug = line.contains(" DEBUG ") || line.contains(" TRACE ");
                if !has_logger || !is_debug {
                    eprintln!("{}", formatted);
                }
            }

            // Print log file path when stderr stream ends (once per process)
            if is_stderr {
                if let Some(ref log) = logger {
                    eprintln!("📋 Debug log: {}", log.path().display());
                }
            }
        });
    }
}

/// Find the fcvm binary
pub fn find_fcvm_binary() -> anyhow::Result<PathBuf> {
    // Try several possible locations
    let candidates = vec![
        PathBuf::from("./target/release/fcvm"),
        PathBuf::from("./target/debug/fcvm"),
        PathBuf::from("/usr/local/bin/fcvm"),
        PathBuf::from("/usr/bin/fcvm"),
    ];

    for path in candidates {
        if path.exists() {
            return Ok(path);
        }
    }

    // Try which command
    if let Ok(output) = Command::new("which").arg("fcvm").output() {
        if output.status.success() {
            let path = String::from_utf8_lossy(&output.stdout);
            let path = path.trim();
            if !path.is_empty() {
                return Ok(PathBuf::from(path));
            }
        }
    }

    anyhow::bail!("fcvm binary not found. Build with: cargo build --release")
}

/// Poll VM health status, checking child process for early exit.
/// Fails immediately if the child process exits.
pub async fn poll_health(
    child: &mut tokio::process::Child,
    timeout_secs: u64,
) -> anyhow::Result<()> {
    let pid = child
        .id()
        .ok_or_else(|| anyhow::anyhow!("child has no pid"))?;
    let start = std::time::Instant::now();
    let timeout = Duration::from_secs(timeout_secs);

    loop {
        if start.elapsed() > timeout {
            anyhow::bail!("timeout waiting for VM to become healthy");
        }

        // Check if child exited
        if let Some(status) = child.try_wait()? {
            anyhow::bail!(
                "fcvm process exited with code {:?}. Check logs above for details.",
                status.code()
            );
        }

        // Query health status
        let fcvm_path = find_fcvm_binary()?;
        let output = tokio::process::Command::new(&fcvm_path)
            .args(["ls", "--json", "--pid", &pid.to_string()])
            .output()
            .await?;

        if !output.status.success() {
            sleep(Duration::from_millis(100)).await;
            continue;
        }

        let stdout = String::from_utf8_lossy(&output.stdout);

        // Deserialize to actual VmState type (with stale field from ls.rs)
        #[derive(serde::Deserialize)]
        struct VmDisplay {
            #[serde(flatten)]
            vm: fcvm::state::VmState,
            stale: bool,
        }

        let vms: Vec<VmDisplay> = match serde_json::from_str(&stdout) {
            Ok(v) => v,
            Err(_) => {
                sleep(Duration::from_millis(100)).await;
                continue;
            }
        };

        // Check if VM is healthy using proper enum comparison
        for display in &vms {
            if matches!(display.vm.health_status, fcvm::state::HealthStatus::Healthy) {
                return Ok(());
            }
        }

        sleep(Duration::from_millis(100)).await;
    }
}

/// Poll VM health status by PID (legacy - prefer poll_health with child handle).
pub async fn poll_health_by_pid(pid: u32, timeout_secs: u64) -> anyhow::Result<()> {
    let start = std::time::Instant::now();
    let timeout = Duration::from_secs(timeout_secs);

    loop {
        if start.elapsed() > timeout {
            anyhow::bail!("timeout waiting for VM to become healthy");
        }

        // Check if process exited (zombie or gone)
        let is_gone = std::fs::read_to_string(format!("/proc/{}/stat", pid))
            .map(|s| s.contains(") Z "))
            .unwrap_or(true);

        if is_gone {
            anyhow::bail!("fcvm process (pid {}) exited. Check logs above.", pid);
        }

        let fcvm_path = find_fcvm_binary()?;
        let output = tokio::process::Command::new(&fcvm_path)
            .args(["ls", "--json", "--pid", &pid.to_string()])
            .output()
            .await?;

        if !output.status.success() {
            sleep(Duration::from_millis(100)).await;
            continue;
        }

        #[derive(serde::Deserialize)]
        struct VmDisplay {
            #[serde(flatten)]
            vm: fcvm::state::VmState,
            #[allow(dead_code)]
            stale: bool,
        }

        if let Ok(vms) =
            serde_json::from_str::<Vec<VmDisplay>>(&String::from_utf8_lossy(&output.stdout))
        {
            for d in &vms {
                if matches!(d.vm.health_status, fcvm::state::HealthStatus::Healthy) {
                    return Ok(());
                }
            }
        }

        sleep(Duration::from_millis(100)).await;
    }
}

/// Poll for serve process state to exist by PID (serve processes don't have health status)
pub async fn poll_serve_state_by_pid(pid: u32, timeout_secs: u64) -> anyhow::Result<()> {
    let start = std::time::Instant::now();
    let timeout = Duration::from_secs(timeout_secs);

    loop {
        if start.elapsed() > timeout {
            anyhow::bail!("timeout waiting for serve state to be saved");
        }

        // Query state - serve processes show up in `fcvm ls` once state is saved
        let fcvm_path = find_fcvm_binary()?;
        let output = tokio::process::Command::new(&fcvm_path)
            .args(["ls", "--json", "--pid", &pid.to_string()])
            .output()
            .await?;

        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            // Check if we got any results (non-empty array)
            if stdout.trim() != "[]" && !stdout.trim().is_empty() {
                return Ok(());
            }
        }

        sleep(Duration::from_millis(100)).await;
    }
}

/// Execute a command in the guest VM via exec (--vm flag for VM-level, default is container)
pub async fn exec_in_vm(pid: u32, cmd: &[&str]) -> anyhow::Result<String> {
    let fcvm_path = find_fcvm_binary()?;
    let script = cmd.join(" ");

    let output = tokio::process::Command::new(&fcvm_path)
        .args([
            "exec",
            "--pid",
            &pid.to_string(),
            "--vm",
            "--",
            "sh",
            "-c",
            &script,
        ])
        .output()
        .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("exec failed: {}", stderr);
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Execute a command in the container via exec (default behavior)
pub async fn exec_in_container(pid: u32, cmd: &[&str]) -> anyhow::Result<String> {
    let fcvm_path = find_fcvm_binary()?;
    let script = cmd.join(" ");

    let output = tokio::process::Command::new(&fcvm_path)
        .args(["exec", "--pid", &pid.to_string(), "sh", "-c", &script])
        .output()
        .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("exec failed: {}", stderr);
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Create a snapshot from a running VM by PID
///
/// # Arguments
/// * `pid` - The process ID of the fcvm process managing the VM
/// * `snapshot_name` - Name for the snapshot (tag)
pub async fn create_snapshot_by_pid(pid: u32, snapshot_name: &str) -> anyhow::Result<()> {
    let fcvm_path = find_fcvm_binary()?;
    let output = tokio::process::Command::new(&fcvm_path)
        .args([
            "snapshot",
            "create",
            "--pid",
            &pid.to_string(),
            "--tag",
            snapshot_name,
        ])
        .output()
        .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Snapshot creation failed: {}", stderr);
    }

    Ok(())
}

/// Start a UFFD memory server for snapshot/clone operations
///
/// Returns the server process handle and its PID.
/// The server must be kept alive while clones are running.
/// Waits for the server to register in the state manager.
///
/// # Arguments
/// * `snapshot_name` - Name of the snapshot to serve
pub async fn start_memory_server(
    snapshot_name: &str,
) -> anyhow::Result<(tokio::process::Child, u32)> {
    // Use spawn_fcvm helper to avoid pipe buffer deadlock
    let (child, serve_pid) = spawn_fcvm(&["snapshot", "serve", snapshot_name]).await?;

    // Wait for serve process to save its state file
    // Serve processes don't have health status, so we just check state exists
    poll_serve_state_by_pid(serve_pid, 30).await?;

    Ok((child, serve_pid))
}

/// Spawn a clone VM from a memory server
///
/// # Arguments
/// * `serve_pid` - PID of the memory server process
/// * `clone_name` - Name for the clone VM
/// * `network` - Network mode ("bridged" or "rootless")
///
/// # Returns
/// The spawned process handle and its PID
pub async fn spawn_clone(
    serve_pid: u32,
    clone_name: &str,
    network: &str,
) -> anyhow::Result<(tokio::process::Child, u32)> {
    let serve_pid_str = serve_pid.to_string();
    // Use spawn_fcvm helper to avoid pipe buffer deadlock
    spawn_fcvm(&[
        "snapshot",
        "run",
        "--pid",
        &serve_pid_str,
        "--name",
        clone_name,
        "--network",
        network,
    ])
    .await
}

/// Kill a process by PID gracefully (SIGTERM first, then SIGKILL after timeout)
///
/// This allows fcvm to cleanup network resources (veth, namespaces, iptables rules)
/// before terminating. Without cleanup, network resources accumulate and cause
/// routing conflicts when the same IPs are reused.
pub async fn kill_process(pid: u32) {
    // First try SIGTERM for graceful shutdown
    let _ = tokio::process::Command::new("kill")
        .arg("-TERM")
        .arg(pid.to_string())
        .output()
        .await;

    // Wait up to 5 seconds for process to exit
    for _ in 0..50 {
        sleep(Duration::from_millis(100)).await;

        // Check if process still exists
        let status = tokio::process::Command::new("kill")
            .arg("-0") // Check existence without signaling
            .arg(pid.to_string())
            .output()
            .await;

        if let Ok(output) = status {
            if !output.status.success() {
                // Process no longer exists
                return;
            }
        }
    }

    // Force kill if still running
    let _ = tokio::process::Command::new("kill")
        .arg("-9")
        .arg(pid.to_string())
        .output()
        .await;
}

/// Wait for a snapshot serve process to be ready by polling for its socket file.
///
/// The serve process creates a socket at `/mnt/fcvm-btrfs/uffd-{snapshot}-{pid}.sock`
/// when it's ready to accept clone connections.
///
/// # Arguments
/// * `snapshot_name` - Name of the snapshot being served
/// * `serve_pid` - PID of the serve process
/// * `timeout_secs` - Maximum seconds to wait
pub async fn poll_serve_ready(
    snapshot_name: &str,
    serve_pid: u32,
    timeout_secs: u64,
) -> anyhow::Result<()> {
    let socket_path =
        fcvm::paths::data_dir().join(format!("uffd-{}-{}.sock", snapshot_name, serve_pid));

    let start = std::time::Instant::now();
    let timeout = Duration::from_secs(timeout_secs);

    loop {
        if start.elapsed() > timeout {
            anyhow::bail!(
                "timeout waiting for serve socket: {}",
                socket_path.display()
            );
        }

        if socket_path.exists() {
            return Ok(());
        }

        sleep(Duration::from_millis(50)).await;
    }
}

/// Build localhost/nested-test image (convenience wrapper)
pub async fn ensure_nested_image() -> anyhow::Result<()> {
    ensure_nested_container("localhost/nested-test", "Containerfile.nested").await
}

/// Build a container image for nested testing.
///
/// Always runs podman build - relies on podman's layer caching for speed.
/// If the container extends localhost/nested-test, call ensure_nested_image() first.
///
/// # Arguments
/// * `image_name` - Full image name (e.g., "localhost/vsock-integrity")
/// * `containerfile` - Path to Containerfile (e.g., "Containerfile.vsock-integrity")
pub async fn ensure_nested_container(image_name: &str, containerfile: &str) -> anyhow::Result<()> {
    let fcvm_path = find_fcvm_binary()?;
    let fcvm_dir = fcvm_path.parent().unwrap();

    // Copy binaries to build context (needed for nested-test base)
    if image_name == "localhost/nested-test" {
        let profile = fcvm::setup::get_kernel_profile("nested")?
            .ok_or_else(|| anyhow::anyhow!("nested kernel profile not found"))?;

        // Get firecracker path from profile (custom or system fallback)
        let src_firecracker = fcvm::setup::get_firecracker_for_profile(&profile, "nested").await?;

        tokio::fs::create_dir_all("artifacts").await.ok();
        std::fs::copy(fcvm_dir.join("fcvm"), "artifacts/fcvm")
            .context("copying fcvm to artifacts/")?;
        std::fs::copy(fcvm_dir.join("fc-agent"), "artifacts/fc-agent")
            .context("copying fc-agent to artifacts/")?;
        std::fs::copy(&src_firecracker, "artifacts/firecracker-nested")
            .context("copying firecracker to artifacts/")?;
    }

    // Always build - podman handles layer caching
    println!("Building {}...", image_name);
    let output = tokio::process::Command::new("podman")
        .args(["build", "-t", image_name, "-f", containerfile, "."])
        .output()
        .await
        .context("running podman build")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Failed to build {}: {}", image_name, stderr);
    }

    // Export to CAS cache so nested VMs can access it
    let digest_out = tokio::process::Command::new("podman")
        .args(["inspect", image_name, "--format", "{{.Digest}}"])
        .output()
        .await?;
    let digest = String::from_utf8_lossy(&digest_out.stdout)
        .trim()
        .to_string();

    if !digest.is_empty() && digest.starts_with("sha256:") {
        let digest_stripped = digest.trim_start_matches("sha256:");
        let archive_path = format!("/mnt/fcvm-btrfs/image-cache/{}.oci.tar", digest_stripped);

        if !std::path::PathBuf::from(&archive_path).exists() {
            println!("Exporting to CAS cache: {}", archive_path);
            tokio::fs::create_dir_all("/mnt/fcvm-btrfs/image-cache")
                .await
                .ok();
            let save_out = tokio::process::Command::new("podman")
                .args([
                    "save",
                    "--format",
                    "oci-archive",
                    "-o",
                    &archive_path,
                    image_name,
                ])
                .output()
                .await?;
            if !save_out.status.success() {
                println!(
                    "Warning: podman save failed: {}",
                    String::from_utf8_lossy(&save_out.stderr)
                );
            }
        }

        println!(
            "✓ {} ready (digest: {})",
            image_name,
            &digest[..std::cmp::min(19, digest.len())]
        );
    } else {
        println!("✓ {} built", image_name);
    }

    Ok(())
}

use anyhow::Context as _;

/// Ftrace utility for kernel tracing during tests
///
/// Usage:
/// ```ignore
/// let tracer = Ftrace::new()?;
/// tracer.enable_events(&["kvm:kvm_exit", "kvm:kvm_entry"])?;
/// tracer.start()?;
/// // ... run test ...
/// tracer.stop()?;
/// let output = tracer.read(100)?;  // last 100 lines
/// println!("{}", output);
/// ```
///
/// Predefined event sets (by noise level):
/// - `Ftrace::EVENTS_PSCI` - Low noise, good for shutdown/PSCI debugging
/// - `Ftrace::EVENTS_ALL_KVM` - Everything (very noisy!)
pub struct Ftrace {
    tracing_path: std::path::PathBuf,
}

impl Ftrace {
    /// Low-noise events for PSCI/shutdown debugging (ARM64)
    #[cfg(target_arch = "aarch64")]
    pub const EVENTS_PSCI: &'static [&'static str] = &[
        "kvm:kvm_userspace_exit",
        "kvm:kvm_hvc_arm64",
        "kvm:kvm_vcpu_wakeup",
        "kvm:kvm_wfx_arm64",
    ];

    /// Low-noise events for shutdown debugging (x86)
    #[cfg(target_arch = "x86_64")]
    pub const EVENTS_PSCI: &'static [&'static str] = &[
        "kvm:kvm_userspace_exit",
        "kvm:kvm_hypercall",
        "kvm:kvm_vcpu_wakeup",
        "kvm:kvm_hlt",
    ];

    /// Medium-noise events including interrupts (ARM64)
    #[cfg(target_arch = "aarch64")]
    pub const EVENTS_INTERRUPTS: &'static [&'static str] = &[
        "kvm:kvm_userspace_exit",
        "kvm:kvm_set_irq",
        "kvm:kvm_irq_line",
        "kvm:kvm_vcpu_wakeup",
        "kvm:vgic_update_irq_pending",
    ];

    /// Medium-noise events including interrupts (x86)
    #[cfg(target_arch = "x86_64")]
    pub const EVENTS_INTERRUPTS: &'static [&'static str] = &[
        "kvm:kvm_userspace_exit",
        "kvm:kvm_set_irq",
        "kvm:kvm_vcpu_wakeup",
        "kvm:kvm_apic",
        "kvm:kvm_inj_virq",
    ];

    /// High-noise events for detailed VM tracing (arch-independent)
    pub const EVENTS_DETAILED: &'static [&'static str] =
        &["kvm:kvm_exit", "kvm:kvm_entry", "kvm:kvm_userspace_exit"];

    /// Create new Ftrace instance. Requires root.
    pub fn new() -> anyhow::Result<Self> {
        let tracing_path = std::path::PathBuf::from("/sys/kernel/debug/tracing");

        // Mount debugfs if needed
        if !tracing_path.exists() {
            std::process::Command::new("mount")
                .args(["-t", "debugfs", "none", "/sys/kernel/debug"])
                .status()
                .context("mounting debugfs")?;
        }

        if !tracing_path.exists() {
            anyhow::bail!("ftrace not available at {:?}", tracing_path);
        }

        Ok(Self { tracing_path })
    }

    /// Enable specific trace events (e.g., "kvm:kvm_exit")
    pub fn enable_events(&self, events: &[&str]) -> anyhow::Result<()> {
        // Disable all first
        std::fs::write(self.tracing_path.join("events/enable"), "0")?;

        for event in events {
            let path = self
                .tracing_path
                .join(format!("events/{}/enable", event.replace(':', "/")));
            std::fs::write(&path, "1")
                .with_context(|| format!("enabling event {} at {:?}", event, path))?;
        }
        Ok(())
    }

    /// Clear trace buffer
    pub fn clear(&self) -> anyhow::Result<()> {
        std::fs::write(self.tracing_path.join("trace"), "")?;
        Ok(())
    }

    /// Start tracing
    pub fn start(&self) -> anyhow::Result<()> {
        self.clear()?;
        std::fs::write(self.tracing_path.join("tracing_on"), "1")?;
        Ok(())
    }

    /// Stop tracing
    pub fn stop(&self) -> anyhow::Result<()> {
        std::fs::write(self.tracing_path.join("tracing_on"), "0")?;
        Ok(())
    }

    /// Read last N lines of trace
    pub fn read(&self, last_n: usize) -> anyhow::Result<String> {
        let content = std::fs::read_to_string(self.tracing_path.join("trace"))?;
        let lines: Vec<&str> = content.lines().collect();
        let start = if lines.len() > last_n {
            lines.len() - last_n
        } else {
            0
        };
        Ok(lines[start..].join("\n"))
    }

    /// Read trace, filtering for pattern
    pub fn read_grep(&self, pattern: &str, last_n: usize) -> anyhow::Result<String> {
        let content = std::fs::read_to_string(self.tracing_path.join("trace"))?;
        let lines: Vec<&str> = content
            .lines()
            .filter(|l| l.contains(pattern) || l.starts_with('#'))
            .collect();
        let start = if lines.len() > last_n {
            lines.len() - last_n
        } else {
            0
        };
        Ok(lines[start..].join("\n"))
    }

    /// List available KVM events
    pub fn list_kvm_events(&self) -> anyhow::Result<Vec<String>> {
        let events_file = std::fs::read_to_string(self.tracing_path.join("available_events"))?;
        Ok(events_file
            .lines()
            .filter(|l| l.starts_with("kvm:"))
            .map(|s| s.to_string())
            .collect())
    }
}

impl Drop for Ftrace {
    fn drop(&mut self) {
        // Stop tracing on drop
        let _ = self.stop();
    }
}

// ============================================================================
// Snapshot helpers
// ============================================================================

/// Check if a snapshot exists by key
///
/// A snapshot exists if it has a config.json file in its directory.
pub fn snapshot_exists(snapshot_key: &str) -> bool {
    let snapshot_path = fcvm::paths::snapshot_dir().join(snapshot_key);
    snapshot_path.join("config.json").exists()
}

/// Delete a snapshot by key (for test cleanup)
///
/// Removes both the snapshot directory and its lock file.
pub async fn delete_snapshot(snapshot_key: &str) -> anyhow::Result<()> {
    let snapshot_path = fcvm::paths::snapshot_dir().join(snapshot_key);
    if snapshot_path.exists() {
        tokio::fs::remove_dir_all(&snapshot_path).await?;
    }
    // Also delete lock file
    let lock_path = snapshot_path.with_extension("lock");
    let _ = tokio::fs::remove_file(&lock_path).await;
    Ok(())
}

/// Get the startup snapshot key for a base key
///
/// Uses the same format as the production code: `{base_key}-startup`
pub fn startup_snapshot_key(base_key: &str) -> String {
    fcvm::commands::podman::startup_snapshot_key(base_key)
}
