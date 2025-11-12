use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use tokio::fs;
use tracing::info;

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

    /// Create a CoW overlay disk from base rootfs using btrfs reflinks
    pub async fn create_cow_disk(&self) -> Result<PathBuf> {
        info!(vm_id = %self.vm_id, "creating CoW overlay disk");

        // Ensure VM directory exists
        fs::create_dir_all(&self.vm_dir)
            .await
            .context("creating VM directory")?;

        let overlay_path = self.vm_dir.join("rootfs.ext4");

        if !overlay_path.exists() {
            info!(
                base = %self.base_rootfs.display(),
                overlay = %overlay_path.display(),
                "creating instant reflink copy (btrfs CoW)"
            );

            // Use cp --reflink=always for instant CoW copy on btrfs
            let status = tokio::process::Command::new("cp")
                .arg("--reflink=always")
                .arg(&self.base_rootfs)
                .arg(&overlay_path)
                .status()
                .await
                .context("executing cp --reflink=always")?;

            if !status.success() {
                anyhow::bail!("cp --reflink=always failed - is filesystem btrfs/xfs?");
            }
        }

        Ok(overlay_path)
    }

    /// Create a snapshot disk (differential from current state)
    pub async fn create_snapshot_disk(&self, snapshot_name: &str) -> Result<PathBuf> {
        let snapshot_path = self.vm_dir.join(format!("{}.disk", snapshot_name));

        info!(
            vm_id = %self.vm_id,
            snapshot = snapshot_name,
            "creating snapshot disk"
        );

        // Copy current overlay to snapshot
        let overlay_path = self.vm_dir.join("rootfs-overlay.ext4");
        fs::copy(&overlay_path, &snapshot_path)
            .await
            .context("creating snapshot disk")?;

        Ok(snapshot_path)
    }

    /// Clone disk from snapshot with CoW
    pub async fn clone_from_snapshot(&self, snapshot_disk: &Path) -> Result<PathBuf> {
        let clone_path = self.vm_dir.join("rootfs-overlay.ext4");

        info!(
            vm_id = %self.vm_id,
            snapshot = %snapshot_disk.display(),
            "cloning disk from snapshot"
        );

        // For fast cloning, we'd use qcow2 backing files:
        // qemu-img create -f qcow2 -b snapshot.disk clone.qcow2
        //
        // For now, copy the snapshot
        fs::copy(snapshot_disk, &clone_path)
            .await
            .context("cloning from snapshot")?;

        Ok(clone_path)
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

/// Create a qcow2 CoW disk (requires qemu-img)
#[allow(dead_code)]
async fn create_qcow2_cow(base: &Path, overlay: &Path) -> Result<()> {
    use tokio::process::Command;

    let output = Command::new("qemu-img")
        .args(&[
            "create",
            "-f",
            "qcow2",
            "-b",
            base.to_str().unwrap(),
            "-F",
            "raw",
            overlay.to_str().unwrap(),
        ])
        .output()
        .await
        .context("running qemu-img")?;

    if !output.status.success() {
        anyhow::bail!(
            "qemu-img failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    Ok(())
}
