//! Pluggable hypervisor (VMM) abstraction.
//!
//! fcvm's VMM is selected behind the [`Hypervisor`] trait so the same orchestration
//! (networking, storage, volumes, health, the boot plan) drives **Firecracker** today
//! and **Cloud Hypervisor** (epic #632). Backends declare [`Capabilities`]; the
//! orchestration layer degrades gracefully where a backend lacks one.
//!
//! ## Layering
//! - [`crate::firecracker`] is the low-level Firecracker API client / launch config /
//!   process manager (unchanged). It is the *implementation detail* of the Firecracker
//!   backend, not part of the abstraction.
//! - This module is the abstraction: the [`Hypervisor`] trait plus the neutral spec
//!   types the orchestration builds. [`firecracker::FirecrackerBackend`] wraps the
//!   low-level client; the Cloud Hypervisor backend (P1) lives alongside it.
//!
//! ## Seam shape (the "fine-grained" trait)
//! Each VMM control operation is one trait method, called in order by the shared
//! cold-boot orchestration in `commands::podman::vm_config`. For Firecracker each
//! method is a direct REST call (zero behavior change vs. the pre-trait code). For a
//! VMM with a batch create API (Cloud Hypervisor: one `vm.create` then `vm.boot`), the
//! `configure_*`/`add_*` methods buffer into a pending config and [`Hypervisor::boot`]
//! performs the create+boot. The host-side work (NFS export, network `post_start`)
//! stays in the orchestration between the calls — it never touches the VMM.
//!
//! ## Snapshots (P2)
//! Snapshot create/restore/clone is **capability-gated** (`Capabilities::snapshots`) and
//! per-backend in `commands::common`: each backend has its own snapshot/restore (the memory
//! image format and restore mechanism are VMM-specific — Firecracker `LoadSnapshot` + external
//! UFFD + `patch_drive` + MMDS restore-epoch vs Cloud Hypervisor `--restore` + in-process
//! UFFD + mount-redirect disk retarget + vsock restore-epoch). The network/namespace/CoW-disk
//! prologue is shared (`prepare_clone_substrate`); the FC-specific snapshot-create path is
//! still reached on the concrete backend via [`Hypervisor::as_any`]. (CH ARM64 snapshots
//! needed a post-#8268 cloud-hypervisor build — the blocker was the SVE register-save bug,
//! not nested/NV2.)

pub mod cloud_hypervisor;
pub mod firecracker;

use anyhow::Result;
use std::any::Any;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use tokio::sync::mpsc;

/// Which VMM a backend drives.
///
/// `Default` is `Firecracker` so a state/snapshot file written before the backend was
/// recorded (no `hypervisor` field) deserializes as Firecracker — the only backend that
/// existed then. Serialized in `VmConfig` / `SnapshotConfig` so `snapshot create`/`run`
/// know which VMM to drive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum Backend {
    #[default]
    Firecracker,
    CloudHypervisor,
}

impl std::fmt::Display for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Backend::Firecracker => write!(f, "firecracker"),
            Backend::CloudHypervisor => write!(f, "cloud-hypervisor"),
        }
    }
}

/// What a backend can do. The orchestration reads these instead of branching on the
/// concrete VMM, so adding a backend is a matter of declaring its capabilities.
///
/// The two lazy-restore booleans are split because Firecracker and Cloud Hypervisor
/// implement *different, non-substitutable* UFFD mechanisms (see #632): Firecracker
/// hands the guest-memory fd to an external page server over a socket; Cloud
/// Hypervisor runs the userfaultfd handler in-process with no external attach point.
#[derive(Debug, Clone, Copy)]
pub struct Capabilities {
    /// Diff/incremental snapshots (Firecracker dirty-page tracking). CH: full only.
    pub diff_snapshots: bool,
    /// CoW page-cache sharing of a `MAP_PRIVATE` snapshot file across clones
    /// (Firecracker `File` backend; measured ~230 MiB for 3×1 GiB clones, #632).
    pub file_backed_cow_restore: bool,
    /// Lazy restore driven by an *external* UFFD page server over a socket
    /// (Firecracker fork: SCM_RIGHTS fd handoff + page_size handshake).
    pub external_uffd_lazy_restore: bool,
    /// Lazy restore with the UFFD handler *inside* the VMM process
    /// (Cloud Hypervisor `memory_restore_mode=ondemand`).
    pub internal_uffd_lazy_restore: bool,
    /// Repoint a drive's host path after snapshot load (Firecracker `PATCH /drives`).
    /// Backends without it use the bind-mount namespace redirect instead.
    pub drive_retarget: bool,
    /// A native guest metadata service for boot-plan delivery (Firecracker MMDS).
    /// Backends without it receive the boot plan over vsock (P0.5).
    pub native_metadata_service: bool,
    /// Can run nested guests (virtual EL2 / FEAT_NV2) on ARM64 (Firecracker fork).
    pub nested_arm64: bool,
    /// Master gate: can this backend create and restore snapshots at all.
    pub snapshots: bool,
}

/// How to spawn the VMM process: which binary, extra args, and the namespace /
/// mount-redirect isolation to apply in `pre_exec` before the process starts.
///
/// These are VMM-neutral — both Firecracker and Cloud Hypervisor run as a child
/// process inside the same network/user/mount namespaces with the same pdeathsig.
#[derive(Debug, Clone, Default)]
pub struct ProcessSpec {
    /// Path to the VMM binary (firecracker / cloud-hypervisor).
    pub binary: PathBuf,
    /// Extra whitespace-separated CLI args (e.g. Firecracker `--enable-nv2`).
    pub extra_args: Option<String>,
    /// Human-readable VM name for logging.
    pub vm_name: Option<String>,
    /// Bridged/routed network namespace name (`/var/run/netns/<id>`).
    pub namespace_id: Option<String>,
    /// Rootless holder PID (enter its user+net namespace via nsenter fallback).
    pub holder_pid: Option<u32>,
    /// User namespace path for rootless clones (entered via setns in pre_exec).
    pub user_namespace_path: Option<PathBuf>,
    /// Net namespace path for rootless clones (entered via setns in pre_exec).
    pub net_namespace_path: Option<PathBuf>,
    /// Mount-namespace redirects `(baseline_dirs, clone_dir)` for clone isolation.
    pub mount_redirects: Option<(Vec<PathBuf>, PathBuf)>,
}

/// A block device to attach (rootfs or data disk), VMM-neutral.
#[derive(Debug, Clone)]
pub struct DriveSpec {
    pub drive_id: String,
    pub path_on_host: PathBuf,
    pub is_root_device: bool,
    pub is_read_only: bool,
}

/// A network interface to attach, VMM-neutral.
#[derive(Debug, Clone)]
pub struct NetIfaceSpec {
    pub iface_id: String,
    pub host_dev_name: String,
    pub guest_mac: Option<String>,
}

/// The pluggable VMM backend.
///
/// Lifecycle: [`Self::spawn`] starts the process; the `configure_*`/`add_*` methods
/// replay the per-VM device/boot configuration in order; [`Self::boot`] starts the
/// guest. [`Self::pid`]/[`Self::wait`]/[`Self::kill`] manage the process.
///
/// Method ordering for cold boot mirrors the Firecracker API sequence exactly:
/// `apply_launch_config` → `add_drive`* → `add_network_interface` →
/// `configure_metadata_service` → `set_vsock` → `publish_boot_plan` →
/// `add_entropy_device` → (`add_balloon`) → `boot`.
#[async_trait::async_trait]
pub trait Hypervisor: Send {
    /// Which VMM this is.
    fn backend(&self) -> Backend;

    /// What this backend supports. The orchestration gates VMM-specific paths on this.
    fn capabilities(&self) -> Capabilities;

    // --- process lifecycle ---

    /// Spawn the VMM process with the given binary, args, and namespace isolation,
    /// and wait for its control socket to accept connections.
    async fn spawn(&mut self, spec: &ProcessSpec) -> Result<()>;

    /// The VMM process PID.
    fn pid(&self) -> Result<u32>;

    /// Non-blocking exit check. `Some` if the process has exited.
    fn try_wait(&mut self) -> Result<Option<ExitStatus>>;

    /// Wait for the VMM process to exit.
    async fn wait(&mut self) -> Result<ExitStatus>;

    /// SIGKILL the VMM process WITHOUT waiting for it to exit.
    ///
    /// Split from the reap so teardown can signal every child (VMM, namespace holder, pasta)
    /// before blocking on any one of them: the kernel's address-space reclaim of a
    /// multi-GiB guest mapping then overlaps with the other deaths instead of serializing
    /// behind them. Always pair with [`Self::reap`] — a signalled process that is never
    /// waited for is a zombie.
    fn start_kill(&mut self) -> Result<()>;

    /// Wait for a [`Self::start_kill`]ed VMM process to exit, reaping it.
    ///
    /// Returns when the process is genuinely gone: the kernel unmaps the guest's address
    /// space in `exit_mmap()` before the task becomes reapable, so this returning is proof
    /// the reclaim finished — not just that the signal was delivered.
    async fn reap(&mut self);

    /// Kill the VMM process and reap it.
    async fn kill(&mut self) -> Result<()>;

    /// Stream the guest serial/virtio console line-by-line.
    async fn stream_console(&self, console_path: &Path) -> Result<mpsc::Receiver<String>>;

    /// Counter of guest console lines observed on this VMM process's console
    /// stream since it spawned (serial → stdout for Firecracker, hvc0 file
    /// tail for Cloud Hypervisor). The restore path watches it to detect a
    /// restored VM whose serial console is dead — a snapshot that captured
    /// the UART mid-transmit restores with the guest driver waiting on a TX
    /// interrupt that never arrives, so no console line ever surfaces.
    fn console_line_counter(&self) -> std::sync::Arc<std::sync::atomic::AtomicU64>;

    // --- cold-boot configuration (called in order; see trait docs) ---

    /// Apply the launch config (boot source, machine config, root drives) and the
    /// per-instance boot args. `track_dirty` enables dirty-page tracking for diff
    /// snapshots (ignored by backends without `diff_snapshots`).
    async fn apply_launch_config(
        &mut self,
        config: &crate::firecracker::FirecrackerConfig,
        runtime_boot_args: &str,
        track_dirty: bool,
    ) -> Result<()>;

    /// Attach one extra block device.
    async fn add_drive(&mut self, drive: &DriveSpec) -> Result<()>;

    /// Attach the guest network interface (eth0).
    async fn add_network_interface(&mut self, iface: &NetIfaceSpec) -> Result<()>;

    /// Configure the guest metadata service used to deliver the boot plan.
    /// Firecracker: MMDS V2 on eth0 at 169.254.169.254. Backends without a native
    /// metadata service (CH) make this a no-op and rely on the vsock boot plan.
    async fn configure_metadata_service(&mut self) -> Result<()>;

    /// Configure the vsock device for host↔guest channels.
    async fn set_vsock(&mut self, guest_cid: u32, uds_path: &Path) -> Result<()>;

    /// Deliver the container boot plan to the guest. Firecracker: `PUT /mmds`
    /// (full replace of the metadata store). CH: over the vsock boot-plan channel.
    async fn publish_boot_plan(&mut self, plan: serde_json::Value) -> Result<()>;

    /// Attach a virtio-rng entropy device.
    async fn add_entropy_device(&mut self) -> Result<()>;

    /// Attach a memory balloon device (`amount_mib`, deflate-on-oom).
    async fn add_balloon(&mut self, amount_mib: u32) -> Result<()>;

    /// Start the guest. Firecracker: `InstanceStart`. Batch-API backends (CH) perform
    /// the buffered `vm.create` then `vm.boot` here.
    async fn boot(&mut self) -> Result<()>;

    // --- runtime control (used by the snapshot-create path) ---

    /// Pause the guest (for snapshotting).
    async fn pause(&self) -> Result<()>;

    /// Resume a paused guest.
    async fn resume(&self) -> Result<()>;

    // --- guest channel ---

    /// The host-side vsock Unix socket path, once [`Self::set_vsock`] has run.
    /// Host→guest connections use the `CONNECT <port>\n` proxy protocol on this
    /// socket; guest→host listeners use `{path}_{port}`. Identical for both VMMs.
    fn vsock_socket_path(&self) -> Option<&Path>;

    // --- capability-gated downcast escape hatch ---
    //
    // Snapshot/restore is Firecracker-specific and not abstracted in P0. The snapshot
    // orchestration downcasts to the concrete backend after checking `capabilities()`.

    fn as_any(&self) -> &dyn Any;
    fn as_any_mut(&mut self) -> &mut dyn Any;
}

impl Capabilities {
    /// Firecracker's capabilities (with fcvm's NV2 fork: nested ARM64 supported).
    pub fn firecracker() -> Self {
        Self {
            diff_snapshots: true,
            file_backed_cow_restore: true,
            // Firecracker's lazy restore is the EXTERNAL UFFD page server (fcvm's
            // src/uffd/). It has no in-process userfaultfd handler, so the internal
            // mode (Cloud Hypervisor's `memory_restore_mode=ondemand`) is not supported.
            external_uffd_lazy_restore: true,
            internal_uffd_lazy_restore: false,
            drive_retarget: true,
            native_metadata_service: true,
            nested_arm64: true,
            snapshots: true,
        }
    }

    /// Cloud Hypervisor's capabilities (#632). Cold boot (P1) and snapshot/restore/clone (P2)
    /// both work: the SVE register-save bug is fixed upstream (#8268), so `snapshots` is enabled
    /// below (explicit `snapshot create` / `snapshot run` via `--restore`, copy-mode).
    pub fn cloud_hypervisor() -> Self {
        Self {
            diff_snapshots: false,
            file_backed_cow_restore: false,
            // CH's UFFD handler is in-process — no external page server can drive it.
            external_uffd_lazy_restore: false,
            // memory_restore_mode=ondemand exists (v52) but is unused — CH restore uses
            // memory_restore_mode=copy; the in-process ondemand lazy-restore handler is a follow-on.
            internal_uffd_lazy_restore: false,
            // No PATCH /drives equivalent — uses the bind-mount redirect instead.
            drive_retarget: false,
            // No MMDS — the boot plan is delivered over vsock (P0.5).
            native_metadata_service: false,
            // No aarch64 virtual-EL2 path.
            nested_arm64: false,
            // P2: explicit `snapshot create` / `snapshot run` work for CH (pause→vm.snapshot
            // →resume; `--restore source_url=…/ch,memory_restore_mode=copy`). The automatic
            // pre-start cache integration for CH is a follow-on (still gated off in podman run).
            snapshots: true,
        }
    }
}
