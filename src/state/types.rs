use serde::{Deserialize, Serialize};

use crate::network::NetworkConfig;

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
    /// Namespace holder PID for rootless networking (used for nsenter health checks)
    #[serde(default)]
    pub holder_pid: Option<u32>,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VmConfig {
    pub image: String,
    pub vcpu: u8,
    pub memory_mib: u32,
    pub network: NetworkConfig,
    pub volumes: Vec<String>,
    pub env: Vec<String>,
    /// Extra block devices (paths to raw disk images)
    #[serde(default)]
    pub extra_disks: Vec<ExtraDisk>,
    /// HTTP health check URL. None means check container running status via fc-agent.
    pub health_check_url: Option<String>,
    /// Which snapshot this process is serving or was cloned from
    pub snapshot_name: Option<String>,
    /// Process type: vm (podman run), serve (snapshot serve), clone (snapshot run)
    pub process_type: Option<ProcessType>,
    /// For clones: which serve process PID spawned this clone
    pub serve_pid: Option<u32>,
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
            holder_pid: None,
            created_at: now,
            last_updated: now,
            config: VmConfig {
                image,
                vcpu,
                memory_mib,
                network: NetworkConfig::default(),
                volumes: Vec::new(),
                env: Vec::new(),
                extra_disks: Vec::new(),
                health_check_url: None,
                snapshot_name: None,
                process_type: Some(ProcessType::Vm),
                serve_pid: None,
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
                "env": [],
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
