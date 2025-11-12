use serde::{Deserialize, Serialize};

/// VM state information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VmState {
    pub vm_id: String,
    pub name: Option<String>,
    pub status: VmStatus,
    pub pid: Option<u32>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub config: VmConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VmStatus {
    Starting,
    Running,
    Stopped,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VmConfig {
    pub image: String,
    pub vcpu: u8,
    pub memory_mib: u32,
    pub network: serde_json::Value,
    pub volumes: Vec<String>,
    pub env: Vec<String>,
}

impl VmState {
    pub fn new(vm_id: String, image: String, vcpu: u8, memory_mib: u32) -> Self {
        Self {
            vm_id,
            name: None,
            status: VmStatus::Starting,
            pid: None,
            created_at: chrono::Utc::now(),
            config: VmConfig {
                image,
                vcpu,
                memory_mib,
                network: serde_json::Value::Null,
                volumes: Vec::new(),
                env: Vec::new(),
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
