use anyhow::Result;
use hyper::{Body, Client, Method, Request, StatusCode};
use hyperlocal::{UnixClientExt, Uri as UnixUri};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::Duration;

/// Firecracker API client for managing VMs via HTTP over Unix socket
#[derive(Debug, Clone)]
pub struct FirecrackerClient {
    socket_path: PathBuf,
    client: Client<hyperlocal::UnixConnector>,
    /// Timeout for individual API requests
    request_timeout: Duration,
}

/// Default timeout for Firecracker API requests.
/// Firecracker API calls are local Unix socket RPCs and should complete quickly.
/// 30s is generous — if an API call takes this long, Firecracker is stuck.
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

impl FirecrackerClient {
    pub fn new(socket_path: PathBuf) -> Result<Self> {
        let client = Client::unix();
        Ok(Self {
            socket_path,
            client,
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
        })
    }

    /// Return a clone with a different request timeout.
    /// Use for long-running operations like snapshot create/load.
    pub fn with_timeout(&self, timeout: Duration) -> Self {
        Self {
            socket_path: self.socket_path.clone(),
            client: self.client.clone(),
            request_timeout: timeout,
        }
    }

    /// Build Unix socket URI for Firecracker API
    fn uri(&self, path: &str) -> hyper::Uri {
        UnixUri::new(&self.socket_path, path).into()
    }

    /// Make a PUT request
    async fn put<T: Serialize>(&self, path: &str, body: &T) -> Result<()> {
        let json = serde_json::to_string(body)?;
        let req = Request::builder()
            .method(Method::PUT)
            .uri(self.uri(path))
            .header("Content-Type", "application/json")
            .body(Body::from(json))?;

        let resp = tokio::time::timeout(self.request_timeout, self.client.request(req))
            .await
            .map_err(|_| {
                anyhow::anyhow!(
                    "Firecracker API PUT {} timed out after {:?}",
                    path,
                    self.request_timeout
                )
            })??;
        if resp.status() != StatusCode::NO_CONTENT && resp.status() != StatusCode::OK {
            let status = resp.status();
            let body_bytes = hyper::body::to_bytes(resp.into_body()).await?;
            let body_str = String::from_utf8_lossy(&body_bytes);
            anyhow::bail!("Firecracker API error: {} - {}", status, body_str);
        }
        Ok(())
    }

    /// Make a PATCH request
    async fn patch<T: Serialize>(&self, path: &str, body: &T) -> Result<()> {
        let json = serde_json::to_string(body)?;
        let req = Request::builder()
            .method(Method::PATCH)
            .uri(self.uri(path))
            .header("Content-Type", "application/json")
            .body(Body::from(json))?;

        let resp = tokio::time::timeout(self.request_timeout, self.client.request(req))
            .await
            .map_err(|_| {
                anyhow::anyhow!(
                    "Firecracker API PATCH {} timed out after {:?}",
                    path,
                    self.request_timeout
                )
            })??;
        if resp.status() != StatusCode::NO_CONTENT && resp.status() != StatusCode::OK {
            let status = resp.status();
            let body_bytes = hyper::body::to_bytes(resp.into_body()).await?;
            let body_str = String::from_utf8_lossy(&body_bytes);
            anyhow::bail!("Firecracker API error: {} - {}", status, body_str);
        }
        Ok(())
    }

    /// Configure boot source (kernel + optional initrd)
    pub async fn set_boot_source(&self, config: BootSource) -> Result<()> {
        self.put("/boot-source", &config).await
    }

    /// Configure machine (vCPU, memory)
    pub async fn set_machine_config(&self, config: MachineConfig) -> Result<()> {
        self.put("/machine-config", &config).await
    }

    /// Add a drive (rootfs or data disk)
    pub async fn add_drive(&self, drive_id: &str, config: Drive) -> Result<()> {
        self.put(&format!("/drives/{}", drive_id), &config).await
    }

    /// Update an existing drive configuration (e.g., host path) after snapshot load
    pub async fn patch_drive(&self, drive_id: &str, patch: DrivePatch) -> Result<()> {
        self.patch(&format!("/drives/{}", drive_id), &patch).await
    }

    /// Add a network interface
    pub async fn add_network_interface(
        &self,
        iface_id: &str,
        config: NetworkInterface,
    ) -> Result<()> {
        self.put(&format!("/network-interfaces/{}", iface_id), &config)
            .await
    }

    /// Configure MMDS (metadata service)
    pub async fn set_mmds_config(&self, config: MmdsConfig) -> Result<()> {
        self.put("/mmds/config", &config).await
    }

    /// Put data into MMDS (replaces entire MMDS content)
    pub async fn put_mmds(&self, data: serde_json::Value) -> Result<()> {
        self.put("/mmds", &data).await
    }

    /// Patch data into MMDS (merges with existing MMDS content)
    pub async fn patch_mmds(&self, data: serde_json::Value) -> Result<()> {
        self.patch("/mmds", &data).await
    }

    /// Create a snapshot
    pub async fn create_snapshot(&self, config: SnapshotCreate) -> Result<()> {
        self.put("/snapshot/create", &config).await
    }

    /// Load a snapshot
    pub async fn load_snapshot(&self, config: SnapshotLoad) -> Result<()> {
        self.put("/snapshot/load", &config).await
    }

    /// Perform an action (InstanceStart, SendCtrlAltDel, etc.)
    pub async fn put_action(&self, action: InstanceAction) -> Result<()> {
        self.put("/actions", &action).await
    }

    /// Change VM state (Pause/Resume)
    pub async fn patch_vm_state(&self, state: VmState) -> Result<()> {
        self.patch("/vm", &state).await
    }

    /// Configure balloon device
    pub async fn set_balloon(&self, config: Balloon) -> Result<()> {
        self.put("/balloon", &config).await
    }

    /// Update balloon statistics polling interval
    pub async fn update_balloon_stats(&self, config: BalloonStats) -> Result<()> {
        self.patch("/balloon/statistics", &config).await
    }

    /// Configure entropy device (virtio-rng)
    pub async fn set_entropy_device(&self, config: EntropyDevice) -> Result<()> {
        self.put("/entropy", &config).await
    }

    /// Configure vsock device for host-guest communication
    pub async fn set_vsock(&self, config: Vsock) -> Result<()> {
        self.put("/vsock", &config).await
    }
}

// API data structures

#[derive(Debug, Serialize, Deserialize)]
pub struct BootSource {
    pub kernel_image_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub initrd_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub boot_args: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct MachineConfig {
    pub vcpu_count: u8,
    pub mem_size_mib: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub smt: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_template: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub track_dirty_pages: Option<bool>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Drive {
    pub drive_id: String,
    pub path_on_host: String,
    pub is_root_device: bool,
    pub is_read_only: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub partuuid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate_limiter: Option<RateLimiter>,
}

#[derive(Debug, Serialize, Deserialize, Default)]
pub struct DrivePatch {
    pub drive_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path_on_host: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate_limiter: Option<RateLimiter>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct NetworkInterface {
    pub iface_id: String,
    pub host_dev_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub guest_mac: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rx_rate_limiter: Option<RateLimiter>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tx_rate_limiter: Option<RateLimiter>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RateLimiter {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bandwidth: Option<TokenBucket>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ops: Option<TokenBucket>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct TokenBucket {
    pub size: u64,
    pub refill_time: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct MmdsConfig {
    pub version: String, // "V1" or "V2"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub network_interfaces: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ipv4_address: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SnapshotCreate {
    pub snapshot_path: String,
    pub mem_file_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot_type: Option<String>, // "Full" or "Diff"
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SnapshotLoad {
    pub snapshot_path: String,
    pub mem_backend: MemBackend,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enable_diff_snapshots: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resume_vm: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub network_overrides: Option<Vec<NetworkOverride>>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct NetworkOverride {
    pub iface_id: String,
    pub host_dev_name: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct MemBackend {
    pub backend_path: String,
    pub backend_type: String, // "File" or "Uffd"
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "action_type")]
pub enum InstanceAction {
    InstanceStart,
    SendCtrlAltDel,
    FlushMetrics,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct VmState {
    pub state: String, // "Paused" or "Resumed"
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Balloon {
    pub amount_mib: u32,
    pub deflate_on_oom: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stats_polling_interval_s: Option<u32>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct BalloonStats {
    pub stats_polling_interval_s: u32,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct EntropyDevice {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate_limiter: Option<RateLimiter>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Vsock {
    /// Guest CID (must be > 2, typically 3)
    pub guest_cid: u32,
    /// Path to Unix socket on host
    pub uds_path: String,
}
