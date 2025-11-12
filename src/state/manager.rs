use anyhow::{Context, Result};
use std::path::PathBuf;
use tokio::fs;

use super::types::VmState;

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
        fs::create_dir_all(&self.state_dir)
            .await
            .context("creating state directory")?;
        Ok(())
    }

    /// Save VM state
    pub async fn save_state(&self, state: &VmState) -> Result<()> {
        let state_file = self.state_dir.join(format!("{}.json", state.vm_id));
        let state_json = serde_json::to_string_pretty(state)?;
        fs::write(&state_file, state_json)
            .await
            .context("writing VM state")?;
        Ok(())
    }

    /// Load VM state
    pub async fn load_state(&self, vm_id: &str) -> Result<VmState> {
        let state_file = self.state_dir.join(format!("{}.json", vm_id));
        let state_json = fs::read_to_string(&state_file)
            .await
            .context("reading VM state")?;
        let state: VmState = serde_json::from_str(&state_json).context("parsing VM state")?;
        Ok(state)
    }

    /// Delete VM state
    pub async fn delete_state(&self, vm_id: &str) -> Result<()> {
        let state_file = self.state_dir.join(format!("{}.json", vm_id));
        if state_file.exists() {
            fs::remove_file(&state_file)
                .await
                .context("deleting VM state")?;
        }
        Ok(())
    }

    /// Load VM state by name
    pub async fn load_state_by_name(&self, name: &str) -> Result<VmState> {
        let vms = self.list_vms().await?;
        vms.into_iter()
            .find(|vm| vm.name.as_deref() == Some(name))
            .ok_or_else(|| anyhow::anyhow!("VM not found: {}", name))
    }

    /// List all VMs
    pub async fn list_vms(&self) -> Result<Vec<VmState>> {
        let mut vms = Vec::new();

        if !self.state_dir.exists() {
            return Ok(vms);
        }

        let mut entries = fs::read_dir(&self.state_dir)
            .await
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

// StateManager tests moved to tests/test_state_integration.rs for better integration testing
