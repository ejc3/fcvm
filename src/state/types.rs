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
    pub pid: Option<u32>,
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VmConfig {
    pub image: String,
    pub vcpu: u8,
    pub memory_mib: u32,
    pub network: NetworkConfig,
    pub volumes: Vec<String>,
    pub env: Vec<String>,
    #[serde(default = "default_health_check_path")]
    pub health_check_path: String,
    /// Which snapshot this process is serving or was cloned from
    #[serde(default)]
    pub snapshot_name: Option<String>,
    /// Process type: "vm" (podman run), "serve" (snapshot serve), "clone" (snapshot run)
    #[serde(default)]
    pub process_type: Option<String>,
    /// For clones: which serve process PID spawned this clone
    #[serde(default)]
    pub serve_pid: Option<u32>,
}

fn default_health_check_path() -> String {
    "/".to_string()
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
            pid: None,
            created_at: now,
            last_updated: now,
            config: VmConfig {
                image,
                vcpu,
                memory_mib,
                network: NetworkConfig::default(),
                volumes: Vec::new(),
                env: Vec::new(),
                health_check_path: default_health_check_path(),
                snapshot_name: None,
                process_type: Some("vm".to_string()),
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
}
