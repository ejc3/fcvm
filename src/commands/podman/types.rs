use anyhow::{bail, Result};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use std::sync::atomic::AtomicBool;

use crate::cli::RunArgs;
use crate::firecracker::FirecrackerConfig;
use crate::hypervisor::Hypervisor;
use crate::network::{NetworkConfig, NetworkManager};
use crate::state::{StateManager, VmState};
use crate::volume::VolumeConfig;

/// Everything needed to re-run the Firecracker API configuration + boot for an
/// in-place relaunch (a guest reboot). Captured once during initial setup; the
/// host-side substrate (disk, network namespace/holder, vsock listeners) is reused
/// untouched, so a relaunch only replays the per-firecracker-child config.
///
/// Consumed by the shared `configure_and_boot_vm` primitive (vm_config.rs),
/// which both the initial boot and the reboot relaunch call.
pub struct RebootSpec {
    pub firecracker_bin: PathBuf,
    pub fc_args: Option<String>,
    /// Fully-resolved launch config (rootfs_path points at the per-VM CoW disk).
    pub launch_config: FirecrackerConfig,
    pub boot_args: String,
    pub track_dirty_pages: bool,
    pub image_disk_path: Option<PathBuf>,
    pub vsock_socket_path: PathBuf,
    /// Whether the boot plan is delivered over vsock (VMMs without a metadata service)
    /// rather than MMDS. Baked into `boot_args` too (`fcvm_bootplan=vsock`), so an
    /// in-place reboot relaunch must re-serve over the same transport.
    pub bootplan_over_vsock: bool,
}

/// Where one `podman prepare` installs its startup snapshot, and what an already-installed
/// generation there has to look like to answer for that invocation.
///
/// Resolved once during setup and carried on [`VmContext`] so the pre-boot cache check and
/// the post-health install cannot disagree about the name, the type, or the content.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreparedTarget {
    /// Snapshot name the generation is installed under: the content-addressed key, or the
    /// caller's `--tag`. Every consumer that takes a snapshot name addresses this.
    pub name: String,
    /// Content-addressed key whose content the generation must hold.
    pub content_key: String,
    /// `System` for the content-addressed cache entry, `User` for a `--tag` artifact.
    pub snapshot_type: crate::storage::SnapshotType,
    /// Whether a matching installed generation may be published without booting.
    /// Cleared by `--force`.
    pub publish_installed: bool,
    /// What to do with a generation already installed at `name` once the disposable
    /// source is healthy.
    pub existing: super::ExistingGeneration,
}

/// All state accumulated during VM setup, bundled for the event loop and cleanup.
pub struct VmContext {
    pub vm_id: String,
    pub vm_name: String,
    pub data_dir: PathBuf,
    pub vm_manager: Box<dyn Hypervisor>,
    pub holder_child: Option<tokio::process::Child>,
    /// Boot-plan vsock listener task (Some only when the plan is served over vsock for
    /// VMMs without a metadata service). Aborted during cleanup.
    pub bootplan_handle: Option<tokio::task::JoinHandle<()>>,
    pub volume_servers: crate::volume::SpawnedVolumes,
    pub network: Box<dyn NetworkManager>,
    pub network_config: NetworkConfig,
    pub state_manager: StateManager,
    pub health_cancel_token: CancellationToken,
    pub health_monitor_handle: tokio::task::JoinHandle<()>,
    pub status_handle: tokio::task::JoinHandle<()>,
    pub tty_handle: Option<std::thread::JoinHandle<Result<i32>>>,
    /// Host-side Unix socket path for the TTY vsock port (set when running with -t).
    /// Used to unblock the TTY accept thread if the guest never connected.
    pub tty_socket_path: Option<String>,
    pub output_handle: Option<tokio::task::JoinHandle<()>>,
    /// Egress proxy task (rootless mode only); aborted during cleanup.
    pub egress_proxy_handle: Option<tokio::task::JoinHandle<()>>,
    pub cache_rx: Option<mpsc::Receiver<CacheRequest>>,
    /// Startup-snapshot trigger from the health monitor. Carries the ack the
    /// snapshot path must send (or drop) before the monitor publishes Healthy.
    pub startup_rx: Option<oneshot::Receiver<crate::health::StartupSnapshotAck>>,
    pub snapshot_key: Option<String>,
    /// Set only for the `podman prepare` lifecycle: where its startup snapshot goes.
    pub prepare_target: Option<PreparedTarget>,
    pub volume_configs: Vec<VolumeConfig>,
    pub args: RunArgs,
    pub disk_path: PathBuf,
    pub log_tx: tokio::sync::broadcast::Sender<LogLine>,
    /// Notify the output listener to drop its current connection and re-accept.
    /// Triggered after each snapshot (vsock connections reset during snapshot).
    pub output_reconnect: Arc<tokio::sync::Notify>,
    /// VM state snapshot for cache snapshot creation. Config fields (image, vcpu,
    /// memory_mib, network, original_vsock_vm_id, etc.) are immutable after setup.
    pub vm_state: crate::state::VmState,
    /// Set by run_vm_loop right after the pre-start cache snapshot is created:
    /// instead of resuming this VM, fcvm tears it down and relaunches by
    /// restoring the snapshot it just produced, so the snapshot-miss path goes
    /// through the exact same restore flow as a snapshot hit. (Resuming the
    /// paused VM is the one lifecycle that intermittently starves Firecracker's
    /// device event loop — see #630.)
    pub restore_from_cache: Option<String>,
    /// Set by the status listener when the guest signals a reboot. run_vm_loop checks
    /// it when Firecracker exits and relaunches in place instead of terminating.
    pub reboot_requested: Arc<AtomicBool>,
    /// Set by the status listener when the container's "exit:" notification arrives.
    /// Distinguishes a real termination from a reboot in wait_for_reboot_decision and
    /// gates the exit-code read (the listener stays alive, so it can't be joined).
    pub container_exit_seen: Arc<AtomicBool>,
    /// Inputs to replay the Firecracker API config + boot on an in-place relaunch.
    pub reboot_spec: RebootSpec,
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
    /// Exact host socket bound for this VM, including a custom `--vsock-dir`.
    pub(super) vsock_socket_path: PathBuf,
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
        self.vsock_socket_path.clone()
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
pub(crate) struct VolumeMapping {
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
