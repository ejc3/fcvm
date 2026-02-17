use anyhow::{bail, Result};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::cli::RunArgs;
use crate::firecracker::VmManager;
use crate::network::{NetworkConfig, NetworkManager};
use crate::state::{StateManager, VmState};
use crate::storage::SnapshotMetadata;
use crate::volume::VolumeConfig;

/// All state accumulated during VM setup, bundled for the event loop and cleanup.
pub struct VmContext {
    pub vm_id: String,
    pub vm_name: String,
    pub data_dir: PathBuf,
    pub vm_manager: VmManager,
    pub holder_child: Option<tokio::process::Child>,
    pub volume_server_handles: Vec<tokio::task::JoinHandle<()>>,
    pub network: Box<dyn NetworkManager>,
    pub network_config: NetworkConfig,
    pub state_manager: StateManager,
    pub health_cancel_token: CancellationToken,
    pub health_monitor_handle: tokio::task::JoinHandle<()>,
    pub status_handle: tokio::task::JoinHandle<()>,
    pub tty_handle: Option<std::thread::JoinHandle<Result<i32>>>,
    pub output_handle: Option<tokio::task::JoinHandle<Vec<(String, String)>>>,
    pub cache_rx: Option<mpsc::Receiver<CacheRequest>>,
    pub startup_rx: Option<oneshot::Receiver<()>>,
    pub snapshot_key: Option<String>,
    pub volume_configs: Vec<VolumeConfig>,
    pub args: RunArgs,
    pub disk_path: PathBuf,
    pub log_tx: tokio::sync::broadcast::Sender<LogLine>,
    /// Notify the output listener to drop its current connection and re-accept.
    /// Triggered after each snapshot (vsock connections reset during snapshot).
    pub output_reconnect: Arc<tokio::sync::Notify>,
    /// Extra disk entries for the image disk (btrfs mode), included in snapshot metadata.
    pub image_extra_disks: Vec<crate::storage::SnapshotExtraDisk>,
}

/// A log line from the VM's container output.
#[derive(Clone, Debug)]
pub struct LogLine {
    /// Stream type: "stdout", "stderr", or "system"
    pub stream: String,
    /// Line content
    pub content: String,
}

/// Handle to a running VM. Returned by `start_vm()`.
///
/// The VM runs in a background tokio task. Use `stop()` to gracefully shut it down
/// or `wait()` to wait for it to exit naturally.
///
/// On drop, the VM is cancelled automatically (the background task will clean up
/// resources on its next poll). For explicit shutdown with exit code, use `stop()`.
pub struct VmHandle {
    /// Unique VM identifier (e.g., "vm-abc123")
    pub vm_id: String,
    /// Human-readable VM name
    pub name: String,
    /// Process ID of the fcvm process managing this VM
    pub pid: u32,
    pub(super) cancel: CancellationToken,
    pub(super) task: Option<tokio::task::JoinHandle<Result<Option<i32>>>>,
    pub(super) log_tx: tokio::sync::broadcast::Sender<LogLine>,
}

impl VmHandle {
    /// Get a clone of the cancellation token (for external cancellation).
    pub fn cancel_token(&self) -> CancellationToken {
        self.cancel.clone()
    }

    /// Get the vsock socket path for this VM (used for exec/terminal connections).
    pub fn vsock_socket_path(&self) -> PathBuf {
        crate::paths::vm_runtime_dir(&self.vm_id).join("vsock.sock")
    }

    /// Gracefully stop the VM and wait for cleanup to complete.
    /// Returns the container exit code (None if the container didn't exit naturally).
    pub async fn stop(&mut self) -> Result<Option<i32>> {
        self.cancel.cancel();
        match self.task.take() {
            Some(task) => task.await?,
            None => Ok(None),
        }
    }

    /// Wait for the VM to exit naturally (without cancelling).
    /// Returns the container exit code.
    pub async fn wait(&mut self) -> Result<Option<i32>> {
        match self.task.take() {
            Some(task) => task.await?,
            None => Ok(None),
        }
    }

    /// Query current VM state (health, IP, ports, labels, etc.) from the state manager.
    pub async fn state(&self) -> Result<VmState> {
        let mgr = StateManager::new(crate::paths::state_dir());
        mgr.load_state(&self.vm_id).await
    }

    /// Subscribe to live container output (stdout/stderr) from this VM.
    /// Returns a broadcast receiver that gets each log line as it's produced.
    /// Late subscribers only see lines from the point of subscription.
    pub fn subscribe_logs(&self) -> tokio::sync::broadcast::Receiver<LogLine> {
        self.log_tx.subscribe()
    }
}

impl Drop for VmHandle {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

/// Request to create a podman cache snapshot.
/// Sent from status listener to main task when fc-agent signals cache-ready.
pub struct CacheRequest {
    /// Image digest from fc-agent
    pub digest: String,
    /// Oneshot channel to signal completion back to status listener
    pub ack_tx: oneshot::Sender<()>,
}

/// Parameters for creating a snapshot, used by both podman run and snapshot run.
/// This allows snapshot creation from both fresh VMs (using RunArgs) and
/// restored VMs (using existing snapshot metadata).
pub struct SnapshotCreationParams {
    /// Container image name
    pub image: String,
    /// Number of vCPUs
    pub vcpu: u8,
    /// Memory in MiB
    pub memory_mib: u32,
    /// Health check URL from the baseline VM (None = no HTTP health check)
    pub health_check_url: Option<String>,
    /// Whether VM uses 2MB hugepage-backed memory
    pub hugepages: bool,
    /// Username for rootless container health checks
    pub username: Option<String>,
    /// Extra disks to include in snapshot (e.g., btrfs image disk)
    pub extra_disks: Vec<crate::storage::SnapshotExtraDisk>,
}

impl SnapshotCreationParams {
    /// Create from RunArgs (for fresh VMs).
    /// Note: `extra_disks` must be set separately after VM setup (drive IDs are assigned then).
    pub fn from_run_args(args: &RunArgs) -> Self {
        // Resolve username from USER= env var (set during podman arg parsing)
        let username = args
            .env
            .iter()
            .find_map(|s| s.strip_prefix("USER="))
            .map(|s| s.to_string());
        Self {
            image: args.image.clone(),
            vcpu: args.cpu,
            memory_mib: args.mem,
            health_check_url: args.health_check.clone(),
            hugepages: args.hugepages,
            username,
            extra_disks: vec![],
        }
    }

    /// Create from SnapshotMetadata (for restored VMs)
    pub fn from_metadata(metadata: &SnapshotMetadata) -> Self {
        Self {
            image: metadata.image.clone(),
            vcpu: metadata.vcpu,
            memory_mib: metadata.memory_mib,
            health_check_url: metadata.health_check_url.clone(),
            hugepages: metadata.hugepages,
            username: metadata.username.clone(),
            extra_disks: metadata.extra_disks.clone(),
        }
    }
}

/// Result of a snapshot creation attempt that can be interrupted by signals.
pub enum SnapshotOutcome {
    /// Snapshot created successfully
    Created,
    /// Snapshot creation failed
    Failed(anyhow::Error),
    /// Signal received during creation (caller should break and shutdown)
    Interrupted,
}

/// Parsed volume mapping from --map HOST:GUEST[:ro] specification.
pub(super) struct VolumeMapping {
    pub host_path: PathBuf,
    pub guest_path: String,
    pub read_only: bool,
}

impl VolumeMapping {
    /// Parse a volume spec string: HOST:GUEST[:ro]
    pub fn parse(spec: &str) -> Result<Self> {
        let parts: Vec<&str> = spec.split(':').collect();
        if parts.len() < 2 {
            bail!("Invalid volume spec '{}': expected HOST:GUEST[:ro]", spec);
        }

        let host_path = PathBuf::from(parts[0]);
        let guest_path = parts[1].to_string();
        let read_only = parts.len() > 2 && parts[2] == "ro";

        // Validate host path exists
        if !host_path.exists() {
            bail!("Volume host path does not exist: {}", host_path.display());
        }

        // Validate guest path is absolute
        if !guest_path.starts_with('/') {
            bail!(
                "Volume guest path must be absolute: {} (from spec '{}')",
                guest_path,
                spec
            );
        }

        Ok(Self {
            host_path,
            guest_path,
            read_only,
        })
    }
}
