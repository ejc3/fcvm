// Common test utilities for fcvm integration tests
#![allow(dead_code)]

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
    /// Create a new test logger. Logs are written to /tmp/fcvm-test-logs/{test_name}-{timestamp}.log
    pub fn new(test_name: &str) -> Self {
        // Create log directory if needed
        std::fs::create_dir_all(TEST_LOG_DIR).ok();

        let timestamp = chrono::Utc::now().format("%Y%m%d-%H%M%S");
        let log_path = PathBuf::from(format!("{}/{}-{}.log", TEST_LOG_DIR, test_name, timestamp));

        let file = std::fs::File::create(&log_path).expect("Failed to create test log file");

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
/// Runs `fcvm setup --generate-config --force` once per test process to ensure
/// the config file exists at ~/.config/fcvm/rootfs-config.toml.
/// Uses std::sync::Once to ensure this runs only once even with parallel tests.
fn ensure_config_exists() {
    CONFIG_INIT.call_once(|| {
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
        // Kill the VM process (start_kill is synchronous)
        let _ = self.child.start_kill();
        // try_wait is synchronous - check for exit without blocking
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

    // Enable nested virtualization when using inception kernel (--kernel flag)
    // FCVM_NV2=1 tells fcvm to pass --enable-nv2 to Firecracker for HAS_EL2 vCPU feature
    if args.contains(&"--kernel") {
        cmd.env("FCVM_NV2", "1");
    }

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

/// Poll VM health status by PID
pub async fn poll_health_by_pid(pid: u32, timeout_secs: u64) -> anyhow::Result<()> {
    let start = std::time::Instant::now();
    let timeout = Duration::from_secs(timeout_secs);

    loop {
        if start.elapsed() > timeout {
            anyhow::bail!("timeout waiting for VM to become healthy");
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
        .args(["exec", "--pid", &pid.to_string(), "--", "sh", "-c", &script])
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
