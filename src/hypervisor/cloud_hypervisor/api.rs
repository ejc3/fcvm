//! Cloud Hypervisor REST API client (HTTP/1.1 over the `--api-socket` Unix socket).
//!
//! Mirrors the [`crate::firecracker::api`] client shape. Cloud Hypervisor's control plane
//! is REST-over-UDS under `/api/v1/`: the VMM is started with `--api-socket <path>` and no
//! VM, then the VM is created and booted via `PUT /api/v1/vm.create` + `PUT /api/v1/vm.boot`.
//! 204 No Content indicates success.
//!
//! The config structs are the subset of Cloud Hypervisor's `VmConfig` fcvm needs for a
//! cold boot. Field names match Cloud Hypervisor's serde representation (verified against
//! `ch-remote info` output on v52).

use anyhow::Result;
use hyper::{Body, Client, Method, Request, StatusCode};
use hyperlocal::{UnixClientExt, Uri as UnixUri};
use serde::Serialize;
use std::path::PathBuf;
use std::time::Duration;

/// Default timeout for Cloud Hypervisor API requests (local UDS RPCs; generous).
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Cloud Hypervisor REST API client over the api-socket.
#[derive(Debug, Clone)]
pub struct ChClient {
    socket_path: PathBuf,
    client: Client<hyperlocal::UnixConnector>,
    request_timeout: Duration,
}

impl ChClient {
    pub fn new(socket_path: PathBuf) -> Self {
        Self {
            socket_path,
            client: Client::unix(),
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
        }
    }

    /// Return a clone with a different request timeout (for long ops like snapshot).
    pub fn with_timeout(&self, timeout: Duration) -> Self {
        Self {
            socket_path: self.socket_path.clone(),
            client: self.client.clone(),
            request_timeout: timeout,
        }
    }

    fn uri(&self, path: &str) -> hyper::Uri {
        UnixUri::new(&self.socket_path, path).into()
    }

    /// PUT with a JSON body; expects 204/200.
    async fn put_json<T: Serialize>(&self, path: &str, body: &T) -> Result<()> {
        let json = serde_json::to_string(body)?;
        let req = Request::builder()
            .method(Method::PUT)
            .uri(self.uri(path))
            .header("Content-Type", "application/json")
            .body(Body::from(json))?;
        self.send(path, req).await
    }

    /// PUT with no body; expects 204/200 (vm.boot, vm.pause, vm.resume, vmm.shutdown).
    async fn put_empty(&self, path: &str) -> Result<()> {
        let req = Request::builder()
            .method(Method::PUT)
            .uri(self.uri(path))
            .body(Body::empty())?;
        self.send(path, req).await
    }

    async fn send(&self, path: &str, req: Request<Body>) -> Result<()> {
        let resp = tokio::time::timeout(self.request_timeout, self.client.request(req))
            .await
            .map_err(|_| {
                anyhow::anyhow!(
                    "Cloud Hypervisor API {} timed out after {:?}",
                    path,
                    self.request_timeout
                )
            })??;
        let status = resp.status();
        if status != StatusCode::NO_CONTENT && status != StatusCode::OK {
            let body_bytes = hyper::body::to_bytes(resp.into_body()).await?;
            let body_str = String::from_utf8_lossy(&body_bytes);
            anyhow::bail!(
                "Cloud Hypervisor API error: {} {} - {}",
                path,
                status,
                body_str
            );
        }
        Ok(())
    }

    /// Liveness probe for the api server (used while waiting for the socket).
    pub async fn ping(&self) -> Result<()> {
        let req = Request::builder()
            .method(Method::GET)
            .uri(self.uri("/api/v1/vmm.ping"))
            .body(Body::empty())?;
        self.send("/api/v1/vmm.ping", req).await
    }

    /// Create the VM from a full config (does not boot it).
    pub async fn create_vm(&self, config: &VmConfig) -> Result<()> {
        self.put_json("/api/v1/vm.create", config).await
    }

    /// Boot the created VM.
    pub async fn boot_vm(&self) -> Result<()> {
        self.put_empty("/api/v1/vm.boot").await
    }

    /// Pause the VM (for snapshotting).
    pub async fn pause_vm(&self) -> Result<()> {
        self.put_empty("/api/v1/vm.pause").await
    }

    /// Resume a paused VM.
    pub async fn resume_vm(&self) -> Result<()> {
        self.put_empty("/api/v1/vm.resume").await
    }

    /// Snapshot the (paused) VM to a destination URL, e.g. `file:///path/to/dir`.
    pub async fn snapshot_vm(&self, destination_url: &str) -> Result<()> {
        self.put_json(
            "/api/v1/vm.snapshot",
            &SnapshotConfig {
                destination_url: destination_url.to_string(),
            },
        )
        .await
    }

    /// Shut down the VMM process via the API.
    pub async fn shutdown_vmm(&self) -> Result<()> {
        self.put_empty("/api/v1/vmm.shutdown").await
    }
}

// --- VmConfig subset (Cloud Hypervisor serde field names) ---

/// Cloud Hypervisor VM configuration for `vm.create`.
#[derive(Debug, Serialize)]
pub struct VmConfig {
    pub cpus: CpusConfig,
    pub memory: MemoryConfig,
    pub payload: PayloadConfig,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub disks: Vec<DiskConfig>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub net: Vec<NetConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vsock: Option<VsockConfig>,
    pub console: ConsoleConfig,
    pub serial: ConsoleConfig,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rng: Option<RngConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub balloon: Option<BalloonConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub platform: Option<PlatformConfig>,
}

#[derive(Debug, Serialize)]
pub struct CpusConfig {
    pub boot_vcpus: u8,
    pub max_vcpus: u8,
}

#[derive(Debug, Serialize)]
pub struct MemoryConfig {
    /// Guest RAM in bytes.
    pub size: u64,
    /// Back guest RAM with a shared mmap (required for vhost-user / some restore modes).
    pub shared: bool,
}

#[derive(Debug, Serialize)]
pub struct PayloadConfig {
    pub kernel: String,
    pub cmdline: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub initramfs: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DiskConfig {
    pub path: String,
    pub readonly: bool,
    /// Image format. MUST be "Raw" for fcvm's raw ext4/btrfs rootfs + data disks: Cloud
    /// Hypervisor's auto-detect (the "Unknown" default) treats the image as
    /// possibly-qcow2 and **disables sector-0 writes** as an anti-corruption safety,
    /// which makes the guest's ext4 superblock write fail ("I/O error ... op WRITE ...
    /// mount failed"). Setting it explicitly allows sector-0 writes.
    pub image_type: String,
}

#[derive(Debug, Serialize)]
pub struct NetConfig {
    /// Pre-created host tap device name.
    pub tap: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mac: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct VsockConfig {
    pub cid: u32,
    pub socket: String,
}

/// Console/serial device config. `mode` is one of "Off", "Tty", "File", "Null", "Pty".
#[derive(Debug, Serialize)]
pub struct ConsoleConfig {
    pub mode: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct RngConfig {
    pub src: String,
}

#[derive(Debug, Serialize)]
pub struct BalloonConfig {
    pub size: u64,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub deflate_on_oom: bool,
}

/// Platform-level configuration (x86 IOAPIC, ACPI, reboot behavior, etc.).
/// Cloud Hypervisor defaults `on_reboot` to in-place reboot; fcvm needs it set to
/// `"shutdown"` so the VMM process exits on guest reboot (matching Firecracker behavior)
/// and `run_vm_loop` can detect the exit and decide whether to relaunch.
#[derive(Debug, Serialize)]
pub struct PlatformConfig {
    /// What happens when the guest triggers a reboot: `"shutdown"` makes the VMM exit
    /// (like Firecracker), `"reboot"` (the CH default) reboots in place.
    pub on_reboot: String,
}

#[derive(Debug, Serialize)]
struct SnapshotConfig {
    destination_url: String,
}
