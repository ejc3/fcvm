use anyhow::{Context, Result};
use std::path::PathBuf;
use tokio::fs;
use tracing::{info, warn};

/// Configuration for a VM disk
#[derive(Debug, Clone)]
pub struct DiskConfig {
    pub disk_path: PathBuf,
    pub is_root_device: bool,
    pub is_read_only: bool,
}

/// Manages VM disks with CoW support
pub struct DiskManager {
    vm_id: String,
    base_rootfs: PathBuf,
    vm_dir: PathBuf,
}

impl DiskManager {
    pub fn new(vm_id: String, base_rootfs: PathBuf, vm_dir: PathBuf) -> Self {
        Self {
            vm_id,
            base_rootfs,
            vm_dir,
        }
    }

    /// Create a CoW disk from base rootfs, preferring reflinks but falling back to copies
    pub async fn create_cow_disk(&self) -> Result<PathBuf> {
        info!(vm_id = %self.vm_id, "creating CoW disk");

        // Ensure VM directory exists
        fs::create_dir_all(&self.vm_dir)
            .await
            .context("creating VM directory")?;

        let disk_path = self.vm_dir.join("rootfs.ext4");

        if !disk_path.exists() {
            info!(
                base = %self.base_rootfs.display(),
                disk = %disk_path.display(),
                "creating instant reflink copy (btrfs CoW)"
            );

            // Use cp --reflink=always for instant CoW copy on btrfs
            let status = tokio::process::Command::new("cp")
                .arg("--reflink=always")
                .arg(&self.base_rootfs)
                .arg(&disk_path)
                .status()
                .await
                .context("executing cp --reflink=always")?;

            if !status.success() {
                warn!(
                    vm_id = %self.vm_id,
                    base = %self.base_rootfs.display(),
                    "cp --reflink=always failed, falling back to full copy"
                );

                let fallback_status = tokio::process::Command::new("cp")
                    .arg(&self.base_rootfs)
                    .arg(&disk_path)
                    .status()
                    .await
                    .context("executing cp fallback copy")?;

                if !fallback_status.success() {
                    anyhow::bail!(
                        "cp failed when falling back to full copy - ensure filesystem has space"
                    );
                }
            }
        }

        Ok(disk_path)
    }

    /// Get disk configuration for Firecracker
    pub fn get_disk_config(&self, disk_path: PathBuf, is_root: bool) -> DiskConfig {
        DiskConfig {
            disk_path,
            is_root_device: is_root,
            is_read_only: false,
        }
    }

    /// Cleanup VM disks
    pub async fn cleanup(&self) -> Result<()> {
        info!(vm_id = %self.vm_id, "cleaning up VM disks");

        if self.vm_dir.exists() {
            fs::remove_dir_all(&self.vm_dir)
                .await
                .context("removing VM directory")?;
        }

        Ok(())
    }
}
