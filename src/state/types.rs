use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::network::{NetworkConfig, PortMapping};

/// Safely truncate a string to at most `max_len` characters.
/// Returns a string slice without panicking for short inputs.
pub fn truncate_id(s: &str, max_len: usize) -> &str {
    &s[..max_len.min(s.len())]
}

/// VM state information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VmState {
    /// Schema version for future migrations (defaults to 1)
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    pub vm_id: String,
    pub name: Option<String>,
    pub status: VmStatus,
    pub health_status: HealthStatus,
    /// Container exit code (set when health_status is Stopped)
    #[serde(default)]
    pub exit_code: Option<i32>,
    pub pid: Option<u32>,
    /// Start time of the process recorded in `pid`, in clock ticks since boot
    /// (field 22 of /proc/<pid>/stat). Recorded automatically by
    /// `StateManager::save_state` and used to detect PID reuse: if the process
    /// currently at `pid` has a different start time, the state file is stale
    /// even though /proc/<pid> exists.
    #[serde(default)]
    pub pid_start_time: Option<u64>,
    /// Namespace holder PID for rootless networking (used for nsenter health checks)
    #[serde(default)]
    pub holder_pid: Option<u32>,
    /// Monotonically increasing count of host-side vsock transport resets.
    /// Bumped (locked read-modify-write via `StateManager::bump_vsock_epoch`)
    /// after every snapshot pause/save of this VM and BEFORE the VM resumes:
    /// the pause silently orphans in-flight vsock connections (no error on
    /// either side), so a changed epoch tells a blocked exec client its
    /// session is dead (see `commands::exec::SnapshotOrphanGuard`). Defaults
    /// to 0 so state files written before this field existed still load.
    #[serde(default)]
    pub vsock_epoch: u64,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub last_updated: chrono::DateTime<chrono::Utc>,
    pub config: VmConfig,
}

fn default_schema_version() -> u32 {
    1
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VmStatus {
    Starting,
    Running,
    Stopped,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HealthStatus {
    Unknown,
    Healthy,
    Unhealthy,
    Timeout,
    Unreachable,
    /// Container has stopped (process exited)
    Stopped,
}

/// Type of fcvm process
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProcessType {
    /// Standard VM from `fcvm podman run`
    Vm,
    /// Memory server from `fcvm snapshot serve`
    Serve,
    /// Clone from `fcvm snapshot run`
    Clone,
}

/// Extra disk configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtraDisk {
    pub path: String,
    pub mount_path: String,
    pub read_only: bool,
}

/// NFS share configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NfsShare {
    /// Host directory being exported
    pub host_path: String,
    /// Mount path inside guest/container
    pub mount_path: String,
    /// Read-only mount
    pub read_only: bool,
}

fn default_health_check_timeout() -> u64 {
    5
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VmConfig {
    pub image: String,
    pub vcpu: u8,
    pub memory_mib: u32,
    pub network: NetworkConfig,
    pub volumes: Vec<String>,
    // Note: env vars intentionally NOT stored here - they may contain secrets
    // and state files are world-readable. Env is passed directly to MMDS.
    /// Extra block devices (paths to raw disk images)
    #[serde(default)]
    pub extra_disks: Vec<ExtraDisk>,
    /// NFS shares to mount in guest
    #[serde(default)]
    pub nfs_shares: Vec<NfsShare>,
    /// HTTP health check URL. None means check container running status via fc-agent.
    pub health_check_url: Option<String>,
    /// Timeout in seconds for HTTP health check requests.
    #[serde(default = "default_health_check_timeout")]
    pub health_check_timeout: u64,
    /// Which snapshot this process is serving or was cloned from
    pub snapshot_name: Option<String>,
    /// Process type: vm (podman run), serve (snapshot serve), clone (snapshot run)
    pub process_type: Option<ProcessType>,
    /// For clones: which serve process PID spawned this clone
    pub serve_pid: Option<u32>,
    /// For serve processes: how the UFFD server materialises pages ("copy" | "minor").
    /// Clones read this off the serve state so they ask Firecracker for the matching
    /// memory backend — the two ends of the handshake must agree.
    #[serde(default)]
    pub uffd_mode: Option<String>,
    /// For serve processes: the socket its UFFD server is listening on.
    ///
    /// Published by the serve process rather than recomputed by each clone. The name is
    /// unique per server instance (it embeds the serve process's pid and start time), so
    /// there is exactly one authority for it — the server that bound it.
    #[serde(default)]
    pub uffd_socket: Option<std::path::PathBuf>,
    /// Original VM ID for vsock socket path redirect.
    /// Set when VM is restored from cache or snapshot. The vmstate.bin stores
    /// paths from the original VM, so when this VM is later snapshotted, we need
    /// to preserve this original_vm_id for clones to use the correct redirect.
    #[serde(default)]
    pub original_vsock_vm_id: Option<String>,
    /// Published port mappings (host:guest)
    #[serde(default)]
    pub port_mappings: Vec<PortMapping>,
    /// Guest localhost ports forwarded to the host's 127.0.0.1 (--forward-localhost).
    /// Routed mode needs these to set up the host-side relay; clones inherit them
    /// from snapshots so forwarding is re-established after restore.
    #[serde(default)]
    pub forward_localhost: Vec<u16>,
    /// User-defined labels for tagging/filtering VMs
    #[serde(default)]
    pub labels: HashMap<String, String>,
    /// Kernel profile this VM booted with (None = "default"). Carried into
    /// snapshot metadata so cold-boot clones / reboot plans use the same kernel.
    #[serde(default)]
    pub kernel_profile: Option<String>,
    /// Image delivery mode for localhost images ("overlay" | "btrfs" | "archive");
    /// None for registry-pulled images.
    #[serde(default)]
    pub image_mode: Option<String>,
    /// Host path of the read-only image device attached to this VM (overlay
    /// additionalImageStore or docker archive). Content-addressed cache file.
    #[serde(default)]
    pub image_disk_path: Option<std::path::PathBuf>,
    /// Whether VM uses 2MB hugepage-backed memory
    #[serde(default)]
    pub hugepages: bool,
    /// Whether FUSE volumes use portable inode numbering (RemapFs)
    #[serde(default)]
    pub portable_volumes: bool,
    /// Container user spec (e.g. "UID:GID"). When set, the container runs
    /// as this user via rootless podman. Health checks use this to set
    /// XDG_RUNTIME_DIR=/run/user/<uid>.
    ///
    /// IMPORTANT: When adding fields here that affect snapshot restore behavior,
    /// also add them to SnapshotMetadata (storage/snapshot.rs) and update
    /// snapshot.rs to restore them from metadata. Otherwise warm starts will
    /// have missing config.
    #[serde(default)]
    pub user: Option<String>,
    /// Network mode used for this VM (bridged, rootless, routed).
    /// Stored so clones inherit the same networking mode from snapshots.
    #[serde(default)]
    pub network_mode: crate::firecracker::FcNetworkMode,
    /// Explicit routable IPv6 /64 prefix for routed mode.
    /// When set, MASQUERADE is skipped and auto-detect is bypassed.
    #[serde(default)]
    pub ipv6_prefix: Option<String>,
    /// Whether a PTY is allocated for the container.
    #[serde(default)]
    pub tty: bool,
    /// Whether stdin is forwarded to the container.
    #[serde(default)]
    pub interactive: bool,
    /// Username created in the VM for rootless podman.
    /// Used by health checks to run `runuser -u <username> -- podman inspect`.
    #[serde(default)]
    pub username: Option<String>,
    /// Which VMM backend runs this VM (Firecracker or Cloud Hypervisor). Recorded so
    /// `snapshot create`/`run` drive the right control plane. Defaults to Firecracker for
    /// state files written before this field existed (#632 P2).
    #[serde(default)]
    pub hypervisor: crate::hypervisor::Backend,
}

impl VmState {
    pub fn new(vm_id: String, image: String, vcpu: u8, memory_mib: u32) -> Self {
        let now = chrono::Utc::now();
        Self {
            schema_version: 1,
            vm_id,
            name: None,
            status: VmStatus::Starting,
            health_status: HealthStatus::Unknown,
            exit_code: None,
            pid: None,
            pid_start_time: None,
            holder_pid: None,
            vsock_epoch: 0,
            created_at: now,
            last_updated: now,
            config: VmConfig {
                image,
                vcpu,
                memory_mib,
                network: NetworkConfig::default(),
                volumes: Vec::new(),
                extra_disks: Vec::new(),
                nfs_shares: Vec::new(),
                health_check_url: None,
                health_check_timeout: 5,
                snapshot_name: None,
                process_type: Some(ProcessType::Vm),
                serve_pid: None,
                uffd_mode: None,
                uffd_socket: None,
                original_vsock_vm_id: None,
                port_mappings: Vec::new(),
                forward_localhost: Vec::new(),
                network_mode: crate::firecracker::FcNetworkMode::default(),
                tty: false,
                interactive: false,
                labels: HashMap::new(),
                hugepages: false,
                portable_volumes: false,
                user: None,
                username: None,
                ipv6_prefix: None,
                kernel_profile: None,
                image_mode: None,
                image_disk_path: None,
                hypervisor: crate::hypervisor::Backend::default(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_vm_state_new() {
        let state = VmState::new("vm-123".to_string(), "nginx:latest".to_string(), 2, 512);

        assert_eq!(state.vm_id, "vm-123");
        assert_eq!(state.config.image, "nginx:latest");
        assert_eq!(state.config.vcpu, 2);
        assert_eq!(state.config.memory_mib, 512);
        assert!(matches!(state.status, VmStatus::Starting));
        assert!(state.name.is_none());
        assert!(state.pid.is_none());
    }

    #[test]
    fn test_vm_state_serialization() {
        let state = VmState::new("vm-456".to_string(), "redis:alpine".to_string(), 1, 256);

        let json = serde_json::to_string(&state).unwrap();
        let deserialized: VmState = serde_json::from_str(&json).unwrap();

        assert_eq!(state.vm_id, deserialized.vm_id);
        assert_eq!(state.config.image, deserialized.config.image);
    }

    #[test]
    fn test_process_type_serialization() {
        // ProcessType serializes to lowercase strings (matching JSON convention)
        let vm = ProcessType::Vm;
        let serve = ProcessType::Serve;
        let clone = ProcessType::Clone;

        assert_eq!(serde_json::to_string(&vm).unwrap(), "\"vm\"");
        assert_eq!(serde_json::to_string(&serve).unwrap(), "\"serve\"");
        assert_eq!(serde_json::to_string(&clone).unwrap(), "\"clone\"");

        // Test round-trip deserialization
        let vm_from_str: ProcessType = serde_json::from_str("\"vm\"").unwrap();
        let serve_from_str: ProcessType = serde_json::from_str("\"serve\"").unwrap();
        let clone_from_str: ProcessType = serde_json::from_str("\"clone\"").unwrap();

        assert_eq!(vm_from_str, ProcessType::Vm);
        assert_eq!(serve_from_str, ProcessType::Serve);
        assert_eq!(clone_from_str, ProcessType::Clone);
    }

    #[test]
    fn test_vsock_epoch_defaults_to_zero_for_old_state_files() {
        // State files written before vsock_epoch existed must load as epoch 0,
        // and a bumped epoch must round-trip.
        let mut state = VmState::new("vm-old".to_string(), "alpine:latest".to_string(), 1, 128);
        assert_eq!(state.vsock_epoch, 0);

        let mut json: serde_json::Value = serde_json::to_value(&state).unwrap();
        json.as_object_mut().unwrap().remove("vsock_epoch");
        let loaded: VmState = serde_json::from_value(json).unwrap();
        assert_eq!(loaded.vsock_epoch, 0, "missing field must default to 0");

        state.vsock_epoch = 7;
        let roundtrip: VmState =
            serde_json::from_str(&serde_json::to_string(&state).unwrap()).unwrap();
        assert_eq!(roundtrip.vsock_epoch, 7);
    }

    #[test]
    fn test_vm_config_process_type() {
        // Test that VmConfig correctly serializes process_type as enum
        let state = VmState::new("vm-789".to_string(), "alpine:latest".to_string(), 1, 128);

        let json = serde_json::to_string_pretty(&state).unwrap();
        assert!(json.contains("\"process_type\": \"vm\""));

        // Test that we can deserialize JSON with string process_type
        let json_with_string_type = r#"{
            "schema_version": 1,
            "vm_id": "test-vm",
            "name": null,
            "status": "running",
            "health_status": "unknown",
            "pid": 12345,
            "created_at": "2024-01-01T00:00:00Z",
            "last_updated": "2024-01-01T00:00:00Z",
            "config": {
                "image": "test:latest",
                "vcpu": 1,
                "memory_mib": 256,
                "network": {
                    "tap_device": "tap0",
                    "guest_mac": "00:00:00:00:00:00",
                    "guest_ip": null,
                    "host_ip": null,
                    "host_veth": null
                },
                "volumes": [],
                "health_check_url": null,
                "snapshot_name": null,
                "process_type": "serve",
                "serve_pid": null
            }
        }"#;

        let state: VmState = serde_json::from_str(json_with_string_type).unwrap();
        assert_eq!(state.config.process_type, Some(ProcessType::Serve));
    }
}
