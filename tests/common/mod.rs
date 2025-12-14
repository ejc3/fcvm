// Common test utilities for fcvm integration tests
#![allow(dead_code)]

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::time::sleep;

/// Global counter for unique test IDs
static TEST_COUNTER: AtomicUsize = AtomicUsize::new(0);

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

        // Start VM with FUSE volume
        let fcvm_path = find_fcvm_binary()?;
        let mut child = tokio::process::Command::new(&fcvm_path)
            .args([
                "podman",
                "run",
                "--name",
                &vm_name,
                "--network",
                "rootless",
                "--map",
                &format!("{}:/mnt/test", host_dir.display()),
                "--",
                "nginx:alpine",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;

        let pid = child
            .id()
            .ok_or_else(|| anyhow::anyhow!("failed to get VM PID"))?;

        // Wait for VM to become healthy
        if let Err(e) = poll_health_by_pid(pid, 120).await {
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
            sleep(Duration::from_secs(1)).await;
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
                sleep(Duration::from_secs(1)).await;
                continue;
            }
        };

        // Check if VM is healthy using proper enum comparison
        if let Some(display) = vms.first() {
            if matches!(display.vm.health_status, fcvm::state::HealthStatus::Healthy) {
                return Ok(());
            }
        }

        sleep(Duration::from_secs(1)).await;
    }
}

/// Execute a command in the guest VM via SSH/exec
pub async fn exec_in_vm(pid: u32, cmd: &[&str]) -> anyhow::Result<String> {
    let fcvm_path = find_fcvm_binary()?;

    // For now, we'll use a simple approach - write a script and execute it
    // TODO: Implement proper exec via fcvm when available
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
    let fcvm_path = find_fcvm_binary()?;
    let child = tokio::process::Command::new(&fcvm_path)
        .args(["snapshot", "serve", snapshot_name])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    let serve_pid = child
        .id()
        .ok_or_else(|| anyhow::anyhow!("failed to get serve PID"))?;

    // Wait for serve process to register in state manager
    // The serve process will show up in `fcvm ls --pid` once it's running
    poll_health_by_pid(serve_pid, 10).await?;

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
    let fcvm_path = find_fcvm_binary()?;
    let child = tokio::process::Command::new(&fcvm_path)
        .args([
            "snapshot",
            "run",
            "--pid",
            &serve_pid.to_string(),
            "--name",
            clone_name,
            "--network",
            network,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    let clone_pid = child
        .id()
        .ok_or_else(|| anyhow::anyhow!("failed to get clone PID"))?;

    Ok((child, clone_pid))
}

/// Kill a process by PID using kill -9
pub async fn kill_process(pid: u32) {
    let _ = tokio::process::Command::new("kill")
        .arg("-9")
        .arg(pid.to_string())
        .output()
        .await;
}
