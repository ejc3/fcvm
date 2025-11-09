use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tokio::fs;
use uuid::Uuid;

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

/// Manages VM state persistence
pub struct StateManager {
    state_dir: PathBuf,
}

impl StateManager {
    pub fn new(state_dir: PathBuf) -> Self {
        Self { state_dir }
    }

    /// Initialize state directory
    pub async fn init(&self) -> Result<()> {
        fs::create_dir_all(&self.state_dir).await
            .context("creating state directory")?;
        Ok(())
    }

    /// Save VM state
    pub async fn save_state(&self, state: &VmState) -> Result<()> {
        let state_file = self.state_dir.join(format!("{}.json", state.vm_id));
        let state_json = serde_json::to_string_pretty(state)?;
        fs::write(&state_file, state_json).await
            .context("writing VM state")?;
        Ok(())
    }

    /// Load VM state
    pub async fn load_state(&self, vm_id: &str) -> Result<VmState> {
        let state_file = self.state_dir.join(format!("{}.json", vm_id));
        let state_json = fs::read_to_string(&state_file).await
            .context("reading VM state")?;
        let state: VmState = serde_json::from_str(&state_json)
            .context("parsing VM state")?;
        Ok(state)
    }

    /// Delete VM state
    pub async fn delete_state(&self, vm_id: &str) -> Result<()> {
        let state_file = self.state_dir.join(format!("{}.json", vm_id));
        if state_file.exists() {
            fs::remove_file(&state_file).await
                .context("deleting VM state")?;
        }
        Ok(())
    }

    /// List all VMs
    pub async fn list_vms(&self) -> Result<Vec<VmState>> {
        let mut vms = Vec::new();

        if !self.state_dir.exists() {
            return Ok(vms);
        }

        let mut entries = fs::read_dir(&self.state_dir).await
            .context("reading state directory")?;

        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("json") {
                if let Ok(state_json) = fs::read_to_string(&path).await {
                    if let Ok(state) = serde_json::from_str::<VmState>(&state_json) {
                        vms.push(state);
                    }
                }
            }
        }

        Ok(vms)
    }
}

/// Generate a new VM ID
pub fn generate_vm_id() -> String {
    format!("vm-{}", Uuid::new_v4().simple())
}
