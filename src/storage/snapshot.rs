use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tokio::fs;
use tracing::info;

use crate::network::NetworkConfig;

/// Snapshot configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotConfig {
    pub name: String,
    pub vm_id: String,
    pub memory_path: PathBuf,
    pub vmstate_path: PathBuf,
    pub disk_path: PathBuf,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub metadata: SnapshotMetadata,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotMetadata {
    pub image: String,
    pub vcpu: u8,
    pub memory_mib: u32,
    pub network_config: NetworkConfig,
}

/// Manages VM snapshots
pub struct SnapshotManager {
    snapshots_dir: PathBuf,
}

impl SnapshotManager {
    pub fn new(snapshots_dir: PathBuf) -> Self {
        Self { snapshots_dir }
    }

    /// Save a snapshot
    pub async fn save_snapshot(&self, config: SnapshotConfig) -> Result<()> {
        info!(
            snapshot = %config.name,
            vm_id = %config.vm_id,
            "saving snapshot"
        );

        // Create snapshot directory
        let snapshot_dir = self.snapshots_dir.join(&config.name);
        fs::create_dir_all(&snapshot_dir)
            .await
            .context("creating snapshot directory")?;

        // Save metadata
        let metadata_path = snapshot_dir.join("config.json");
        let metadata_json = serde_json::to_string_pretty(&config)?;
        fs::write(&metadata_path, metadata_json)
            .await
            .context("writing snapshot metadata")?;

        info!(
            snapshot = %config.name,
            memory = %config.memory_path.display(),
            disk = %config.disk_path.display(),
            "snapshot saved successfully"
        );

        Ok(())
    }

    /// Load a snapshot configuration
    pub async fn load_snapshot(&self, name: &str) -> Result<SnapshotConfig> {
        let snapshot_dir = self.snapshots_dir.join(name);
        let metadata_path = snapshot_dir.join("config.json");

        if !metadata_path.exists() {
            anyhow::bail!("snapshot '{}' not found", name);
        }

        let metadata_json = fs::read_to_string(&metadata_path)
            .await
            .context("reading snapshot metadata")?;

        let config: SnapshotConfig =
            serde_json::from_str(&metadata_json).context("parsing snapshot metadata")?;

        Ok(config)
    }

    /// List all snapshots
    pub async fn list_snapshots(&self) -> Result<Vec<String>> {
        let mut snapshots = Vec::new();

        if !self.snapshots_dir.exists() {
            return Ok(snapshots);
        }

        let mut entries = fs::read_dir(&self.snapshots_dir)
            .await
            .context("reading snapshots directory")?;

        while let Some(entry) = entries.next_entry().await? {
            if entry.file_type().await?.is_dir() {
                if let Some(name) = entry.file_name().to_str() {
                    snapshots.push(name.to_string());
                }
            }
        }

        Ok(snapshots)
    }

    /// Delete a snapshot
    pub async fn delete_snapshot(&self, name: &str) -> Result<()> {
        let snapshot_dir = self.snapshots_dir.join(name);

        if snapshot_dir.exists() {
            fs::remove_dir_all(&snapshot_dir)
                .await
                .context("removing snapshot directory")?;

            info!(snapshot = name, "snapshot deleted");
        }

        Ok(())
    }
}
