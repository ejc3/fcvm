// Common test utilities for fcvm integration tests
#![allow(dead_code)]

use std::path::PathBuf;

/// Default test image - use AWS ECR to avoid Docker Hub rate limits
pub const TEST_IMAGE: &str = "public.ecr.aws/nginx/nginx:alpine";
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::time::sleep;

/// Global counter for unique test IDs
static TEST_COUNTER: AtomicUsize = AtomicUsize::new(0);

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
/// For prefixed output like `[vm-name] ...`, use `spawn_fcvm_with_logs()` instead.
///
/// # Arguments
/// * `args` - Arguments to pass to fcvm (e.g., ["podman", "run", "--name", "test", ...])
///
/// # Returns
/// Tuple of (Child process, PID)
pub async fn spawn_fcvm(args: &[&str]) -> anyhow::Result<(tokio::process::Child, u32)> {
    let fcvm_path = find_fcvm_binary()?;
    let final_args = maybe_add_strace_flag(args);
    let child = tokio::process::Command::new(&fcvm_path)
        .args(&final_args)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| anyhow::anyhow!("failed to spawn fcvm: {}", e))?;

    let pid = child
        .id()
        .ok_or_else(|| anyhow::anyhow!("failed to get fcvm PID"))?;

    Ok((child, pid))
}

/// Check FCVM_STRACE_AGENT env var and insert --strace-agent flag for podman run commands
fn maybe_add_strace_flag(args: &[&str]) -> Vec<String> {
    let strace_enabled = std::env::var("FCVM_STRACE_AGENT")
        .map(|v| v == "1")
        .unwrap_or(false);

    let mut result: Vec<String> = args.iter().map(|s| s.to_string()).collect();

    // Only add for "podman run" commands
    if strace_enabled && args.len() >= 2 && args[0] == "podman" && args[1] == "run" {
        // Find position to insert (before the image name, which is the last non-flag arg)
        // Insert after "run" and before any positional args
        // Simplest: insert right after "run" at position 2
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
/// This is safe from pipe buffer deadlock because log consumer tasks are
/// spawned immediately to drain the pipes.
///
/// # Arguments
/// * `args` - Arguments to pass to fcvm
/// * `name` - Prefix for log output (e.g., "baseline", "clone-1")
///
/// # Returns
/// Tuple of (Child process, PID)
///
/// # Example
/// ```ignore
/// let (mut child, pid) = spawn_fcvm_with_logs(&[
///     "podman", "run", "--name", "test", "--network", "bridged", TEST_IMAGE,
/// ], "test-vm").await?;
/// // Output will appear as:
/// // [test-vm] Starting container...
/// // [test-vm ERR] Warning: ...
/// ```
pub async fn spawn_fcvm_with_logs(
    args: &[&str],
    name: &str,
) -> anyhow::Result<(tokio::process::Child, u32)> {
    let fcvm_path = find_fcvm_binary()?;
    let final_args = maybe_add_strace_flag(args);
    let mut child = tokio::process::Command::new(&fcvm_path)
        .args(&final_args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| anyhow::anyhow!("failed to spawn fcvm: {}", e))?;

    let pid = child
        .id()
        .ok_or_else(|| anyhow::anyhow!("failed to get fcvm PID"))?;

    // Spawn log consumers immediately to prevent pipe buffer deadlock
    spawn_log_consumer(child.stdout.take(), name);
    spawn_log_consumer_stderr(child.stderr.take(), name);

    Ok((child, pid))
}

/// Spawn a task to consume stdout and print with `[name]` prefix
pub fn spawn_log_consumer(stdout: Option<tokio::process::ChildStdout>, name: &str) {
    use tokio::io::{AsyncBufReadExt, BufReader};
    if let Some(stdout) = stdout {
        let name = name.to_string();
        tokio::spawn(async move {
            let reader = BufReader::new(stdout);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                eprintln!("[{}] {}", name, line);
            }
        });
    }
}

/// Spawn a task to consume stderr and print with `[name ERR]` prefix
pub fn spawn_log_consumer_stderr(stderr: Option<tokio::process::ChildStderr>, name: &str) {
    use tokio::io::{AsyncBufReadExt, BufReader};
    if let Some(stderr) = stderr {
        let name = name.to_string();
        tokio::spawn(async move {
            let reader = BufReader::new(stderr);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                eprintln!("[{} ERR] {}", name, line);
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
    poll_serve_state_by_pid(serve_pid, 10).await?;

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
    let socket_path = PathBuf::from(format!(
        "/mnt/fcvm-btrfs/uffd-{}-{}.sock",
        snapshot_name, serve_pid
    ));

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
