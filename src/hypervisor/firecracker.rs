//! Firecracker backend.
//!
//! Implements [`Hypervisor`](super::Hypervisor) over the low-level
//! [`crate::firecracker`] API client + process manager ([`VmManager`]). Every method
//! is a direct delegation to the existing call, so routing the orchestration through
//! the trait is behavior-identical to the pre-trait code.
//!
//! The snapshot/restore path is Firecracker-specific and not part of the trait (P0).
//! That code reaches the concrete backend via [`Hypervisor::as_any`](super::Hypervisor::as_any)
//! / [`Hypervisor::as_any_mut`](super::Hypervisor::as_any_mut) and uses [`Self::client`],
//! [`Self::vm`], [`Self::vm_mut`].

use anyhow::Result;
use std::any::Any;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use tokio::sync::mpsc;

use super::{Backend, Capabilities, DriveSpec, Hypervisor, NetIfaceSpec, ProcessSpec};
use crate::firecracker::{api, FirecrackerClient, FirecrackerConfig, VmManager};

/// Guest CID for the host↔guest vsock device (host is always CID 2).
const GUEST_CID: u32 = 3;

/// Firecracker implementation of the [`Hypervisor`](super::Hypervisor) trait.
pub struct FirecrackerBackend {
    vm: VmManager,
    /// Host-side vsock Unix socket path, recorded when [`Hypervisor::set_vsock`] runs.
    vsock_path: Option<PathBuf>,
}

impl FirecrackerBackend {
    /// Create a backend that will manage a fresh Firecracker process.
    pub fn new(vm_id: String, socket_path: PathBuf, log_path: Option<PathBuf>) -> Self {
        Self {
            vm: VmManager::new(vm_id, socket_path, log_path),
            vsock_path: None,
        }
    }

    /// Wrap an already-constructed [`VmManager`]. Used by the Firecracker-specific
    /// restore path, which builds and configures the `VmManager` itself before
    /// loading a snapshot.
    pub fn from_vm_manager(vm: VmManager) -> Self {
        Self {
            vm,
            vsock_path: None,
        }
    }

    /// Low-level Firecracker API client (Firecracker-only snapshot/restore path).
    pub fn client(&self) -> Result<&FirecrackerClient> {
        self.vm.client()
    }

    /// The underlying [`VmManager`] (Firecracker-only snapshot/restore path).
    pub fn vm(&self) -> &VmManager {
        &self.vm
    }

    /// The underlying [`VmManager`], mutably (Firecracker-only snapshot/restore path).
    pub fn vm_mut(&mut self) -> &mut VmManager {
        &mut self.vm
    }
}

#[async_trait::async_trait]
impl Hypervisor for FirecrackerBackend {
    fn backend(&self) -> Backend {
        Backend::Firecracker
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::firecracker()
    }

    async fn spawn(&mut self, spec: &ProcessSpec) -> Result<()> {
        if let Some(name) = &spec.vm_name {
            self.vm.set_vm_name(name.clone());
        }
        if let Some(id) = &spec.namespace_id {
            self.vm.set_namespace(id.clone());
        }
        if let Some(pid) = spec.holder_pid {
            self.vm.set_holder_pid(pid);
        }
        if let Some(path) = &spec.user_namespace_path {
            self.vm.set_user_namespace_path(path.clone());
        }
        if let Some(path) = &spec.net_namespace_path {
            self.vm.set_net_namespace_path(path.clone());
        }
        if let Some((baseline_dirs, clone_dir)) = &spec.mount_redirects {
            self.vm
                .set_mount_redirects(baseline_dirs.clone(), clone_dir.clone());
        }
        self.vm
            .start(&spec.binary, None, spec.extra_args.as_deref())
            .await
    }

    fn pid(&self) -> Result<u32> {
        self.vm.pid()
    }

    fn try_wait(&mut self) -> Result<Option<ExitStatus>> {
        self.vm.try_wait()
    }

    async fn wait(&mut self) -> Result<ExitStatus> {
        self.vm.wait().await
    }

    fn start_kill(&mut self) -> Result<()> {
        self.vm.start_kill()
    }

    async fn reap(&mut self) {
        self.vm.reap().await
    }

    async fn kill(&mut self) -> Result<()> {
        self.vm.kill().await
    }

    async fn stream_console(&self, console_path: &Path) -> Result<mpsc::Receiver<String>> {
        self.vm.stream_console(console_path).await
    }

    fn console_line_counter(&self) -> std::sync::Arc<std::sync::atomic::AtomicU64> {
        self.vm.console_line_counter()
    }

    async fn apply_launch_config(
        &mut self,
        config: &FirecrackerConfig,
        runtime_boot_args: &str,
        track_dirty: bool,
    ) -> Result<()> {
        let client = self.vm.client()?;
        config.apply(client, runtime_boot_args, track_dirty).await
    }

    async fn add_drive(&mut self, drive: &DriveSpec) -> Result<()> {
        self.vm
            .client()?
            .add_drive(
                &drive.drive_id,
                api::Drive {
                    drive_id: drive.drive_id.clone(),
                    path_on_host: drive.path_on_host.display().to_string(),
                    is_root_device: drive.is_root_device,
                    is_read_only: drive.is_read_only,
                    partuuid: None,
                    rate_limiter: None,
                },
            )
            .await
    }

    async fn add_network_interface(&mut self, iface: &NetIfaceSpec) -> Result<()> {
        self.vm
            .client()?
            .add_network_interface(
                &iface.iface_id,
                api::NetworkInterface {
                    iface_id: iface.iface_id.clone(),
                    host_dev_name: iface.host_dev_name.clone(),
                    guest_mac: iface.guest_mac.clone(),
                    rx_rate_limiter: None,
                    tx_rate_limiter: None,
                },
            )
            .await
    }

    async fn configure_metadata_service(&mut self) -> Result<()> {
        self.vm
            .client()?
            .set_mmds_config(api::MmdsConfig {
                version: "V2".to_string(),
                network_interfaces: Some(vec!["eth0".to_string()]),
                ipv4_address: Some("169.254.169.254".to_string()),
            })
            .await
    }

    async fn set_vsock(&mut self, guest_cid: u32, uds_path: &Path) -> Result<()> {
        // Remove any stale host-side vsock socket left by a prior Firecracker child so
        // the new one can bind it (a reboot relaunch reuses the same uds_path). This
        // removes ONLY the main uds_path socket — the per-port listener sockets
        // (uds_path_4999 / _4997) use their own paths and are untouched.
        let _ = std::fs::remove_file(uds_path);
        self.vsock_path = Some(uds_path.to_path_buf());
        self.vm
            .client()?
            .set_vsock(api::Vsock {
                guest_cid,
                uds_path: uds_path.display().to_string(),
            })
            .await
    }

    async fn publish_boot_plan(&mut self, plan: serde_json::Value) -> Result<()> {
        self.vm.client()?.put_mmds(plan).await
    }

    async fn add_entropy_device(&mut self) -> Result<()> {
        self.vm
            .client()?
            .set_entropy_device(api::EntropyDevice { rate_limiter: None })
            .await
    }

    async fn add_balloon(&mut self, amount_mib: u32) -> Result<()> {
        self.vm
            .client()?
            .set_balloon(api::Balloon {
                amount_mib,
                deflate_on_oom: true,
                stats_polling_interval_s: Some(1),
            })
            .await
    }

    async fn boot(&mut self) -> Result<()> {
        self.vm
            .client()?
            .put_action(api::InstanceAction::InstanceStart)
            .await
    }

    async fn pause(&self) -> Result<()> {
        self.vm
            .client()?
            .patch_vm_state(api::VmState {
                state: "Paused".to_string(),
            })
            .await
    }

    async fn resume(&self) -> Result<()> {
        self.vm
            .client()?
            .patch_vm_state(api::VmState {
                state: "Resumed".to_string(),
            })
            .await
    }

    fn vsock_socket_path(&self) -> Option<&Path> {
        self.vsock_path.as_deref()
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

/// The default guest CID fcvm uses for the host↔guest vsock device.
pub const fn default_guest_cid() -> u32 {
    GUEST_CID
}
