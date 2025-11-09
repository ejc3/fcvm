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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::types::VmState;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_state_manager_init() {
        let temp_dir = TempDir::new().unwrap();
        let state_dir = temp_dir.path().join("state");

        let manager = StateManager::new(state_dir.clone());
        manager.init().await.unwrap();

        assert!(state_dir.exists());
    }

    #[tokio::test]
    async fn test_save_and_load_state() {
        let temp_dir = TempDir::new().unwrap();
        let manager = StateManager::new(temp_dir.path().to_path_buf());
        manager.init().await.unwrap();

        let state = VmState::new(
            "vm-test".to_string(),
            "nginx:latest".to_string(),
            2,
            512,
        );

        manager.save_state(&state).await.unwrap();
        let loaded = manager.load_state("vm-test").await.unwrap();

        assert_eq!(state.vm_id, loaded.vm_id);
        assert_eq!(state.config.image, loaded.config.image);
    }

    #[tokio::test]
    async fn test_delete_state() {
        let temp_dir = TempDir::new().unwrap();
        let manager = StateManager::new(temp_dir.path().to_path_buf());
        manager.init().await.unwrap();

        let state = VmState::new(
            "vm-delete".to_string(),
            "redis:alpine".to_string(),
            1,
            256,
        );

        manager.save_state(&state).await.unwrap();
        manager.delete_state("vm-delete").await.unwrap();

        assert!(manager.load_state("vm-delete").await.is_err());
    }

    #[tokio::test]
    async fn test_list_vms() {
        let temp_dir = TempDir::new().unwrap();
        let manager = StateManager::new(temp_dir.path().to_path_buf());
        manager.init().await.unwrap();

        let state1 = VmState::new("vm-1".to_string(), "nginx".to_string(), 1, 256);
        let state2 = VmState::new("vm-2".to_string(), "redis".to_string(), 2, 512);

        manager.save_state(&state1).await.unwrap();
        manager.save_state(&state2).await.unwrap();

        let vms = manager.list_vms().await.unwrap();
        assert_eq!(vms.len(), 2);
    }
}
