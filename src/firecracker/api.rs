use anyhow::{Context, Result};
use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::{Method, Request, StatusCode};
use hyper_util::client::legacy::Client;
use hyperlocal::{UnixClientExt, Uri as UnixUri};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::Duration;

/// Firecracker API client for managing VMs via HTTP over Unix socket
#[derive(Debug, Clone)]
pub struct FirecrackerClient {
    socket_path: PathBuf,
    client: Client<hyperlocal::UnixConnector, Full<Bytes>>,
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

    /// Send one request and read the whole reply. The deadline covers the reply's
    /// body as well as its head, so a VMM that sends the head and then stalls fails
    /// the call at the deadline.
    async fn request(
        &self,
        method: Method,
        path: &str,
        json: Option<String>,
    ) -> Result<(StatusCode, Bytes)> {
        let mut req = Request::builder()
            .method(method.clone())
            .uri(self.uri(path));
        if json.is_some() {
            req = req.header("Content-Type", "application/json");
        }
        let req = req.body(Full::new(Bytes::from(json.unwrap_or_default())))?;

        tokio::time::timeout(self.request_timeout, async {
            let resp = self.client.request(req).await?;
            let status = resp.status();
            let body = resp.into_body().collect().await?.to_bytes();
            anyhow::Ok((status, body))
        })
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "Firecracker API {} {} timed out after {:?}",
                method,
                path,
                self.request_timeout
            )
        })?
    }

    /// Send a JSON body. Firecracker answers 204 (or 200) when it accepts one.
    async fn send_json<T: Serialize>(&self, method: Method, path: &str, body: &T) -> Result<()> {
        let json = serde_json::to_string(body)?;
        let (status, reply) = self.request(method, path, Some(json)).await?;
        if status != StatusCode::NO_CONTENT && status != StatusCode::OK {
            return Err(ApiRefusal::new(status, &reply).into());
        }
        Ok(())
    }

    /// Make a PUT request
    async fn put<T: Serialize>(&self, path: &str, body: &T) -> Result<()> {
        self.send_json(Method::PUT, path, body).await
    }

    /// Make a PATCH request
    async fn patch<T: Serialize>(&self, path: &str, body: &T) -> Result<()> {
        self.send_json(Method::PATCH, path, body).await
    }

    /// Make a GET request and parse the JSON body of the reply
    async fn get<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T> {
        let (status, reply) = self.request(Method::GET, path, None).await?;
        if status != StatusCode::OK {
            return Err(ApiRefusal::new(status, &reply).into());
        }
        serde_json::from_slice(&reply)
            .with_context(|| format!("parsing the reply to Firecracker API GET {path}"))
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
    pub async fn update_balloon_stats(&self, config: BalloonStatsUpdate) -> Result<()> {
        self.patch("/balloon/statistics", &config).await
    }

    /// Set the target size of the balloon device. Firecracker answers 400 when the
    /// VM has no balloon device, when the guest never activated the device, and when
    /// the target is above the guest's memory.
    pub async fn patch_balloon(&self, update: BalloonUpdate) -> Result<()> {
        self.patch("/balloon", &update).await
    }

    /// Target and current size of the balloon device. Firecracker answers 400 when
    /// the VM has no balloon device, when the device was attached with statistics
    /// off (fcvm attaches it with statistics on), and before the VM has booted.
    pub async fn balloon_stats(&self) -> Result<BalloonStats> {
        self.get("/balloon/statistics").await
    }

    /// Target of the VM's balloon device in MiB, or None when the VM has no balloon
    /// device. Read from `GET /vm/config`, which answers 200 either way and reports
    /// the device as it is now: at the target it was restored with, or the one set
    /// since.
    pub async fn balloon_target_mib(&self) -> Result<Option<u32>> {
        let config: VmConfig = self.get("/vm/config").await?;
        Ok(config.balloon.map(|balloon| balloon.amount_mib))
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

/// A reply whose status says Firecracker did not do what was asked. A caller that
/// treats a refusal differently from a timeout or a dead VMM finds it in the error
/// chain.
#[derive(Debug)]
pub struct ApiRefusal {
    pub status: StatusCode,
    pub reply: String,
}

impl ApiRefusal {
    fn new(status: StatusCode, reply: &[u8]) -> Self {
        Self {
            status,
            reply: String::from_utf8_lossy(reply).into_owned(),
        }
    }

    /// Whether Firecracker answered 400: it understood the request and refused it.
    pub fn is_bad_request(&self) -> bool {
        self.status == StatusCode::BAD_REQUEST
    }
}

impl std::fmt::Display for ApiRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Firecracker API error: {} - {}", self.status, self.reply)
    }
}

impl std::error::Error for ApiRefusal {}

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
    /// Enable 2MB hugepage-backed guest memory ("2M" or None)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub huge_pages: Option<String>,
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
    pub track_dirty_pages: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resume_vm: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub network_overrides: Option<Vec<NetworkOverride>>,
    /// Host UDS path for the restored vsock device (Firecracker 1.16.0 and later).
    /// Omitted when None, so older binaries never see the field.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vsock_override: Option<VsockOverride>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct NetworkOverride {
    pub iface_id: String,
    pub host_dev_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VsockOverride {
    pub uds_path: String,
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
pub struct BalloonStatsUpdate {
    pub stats_polling_interval_s: u32,
}

/// Body of `PATCH /balloon`: the target and nothing else. Firecracker refuses a
/// body with any other member, so the `Balloon` a device is attached with cannot be
/// sent here.
#[derive(Debug, Serialize, Deserialize)]
pub struct BalloonUpdate {
    pub amount_mib: u32,
}

/// The part of the `GET /balloon/statistics` reply fcvm reads. The reply also
/// carries page counts and the guest's memory counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub struct BalloonStats {
    /// Size the device was asked to reach, in MiB.
    pub target_mib: u32,
    /// Size the guest has given the device so far, in MiB.
    pub actual_mib: u32,
}

/// The part of the `GET /vm/config` reply fcvm reads.
#[derive(Debug, Deserialize)]
struct VmConfig {
    /// In every reply: the device's configuration, or null when the VM has none.
    #[serde(deserialize_with = "Option::deserialize")]
    balloon: Option<VmConfigBalloon>,
}

/// A balloon device as `GET /vm/config` reports it.
#[derive(Debug, Deserialize)]
struct VmConfigBalloon {
    /// Target size in MiB.
    amount_mib: u32,
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
