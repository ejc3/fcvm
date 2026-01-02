use anyhow::{Context, Result};
use std::path::PathBuf;
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
///
/// The disk is a raw partition image (layer2-{sha}.raw) with partitions.
/// fc-agent is injected at boot via initrd, not installed to disk.
/// This allows completely rootless per-VM disk creation.
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

    /// Create a CoW disk from base rootfs using btrfs reflinks
    ///
    /// The base rootfs is a raw disk image with partitions (e.g., /dev/vda1 for root).
    /// This operation is completely rootless - just a file copy with btrfs reflinks.
    ///
    /// Reflinks work through nested FUSE mounts when the kernel has the
    /// FUSE_REMAP_FILE_RANGE patch (kernel 6.18+ with nested profile).
    pub async fn create_cow_disk(&self) -> Result<PathBuf> {
        info!(vm_id = %self.vm_id, "creating CoW disk");

        // Ensure VM directory exists
        fs::create_dir_all(&self.vm_dir)
            .await
            .context("creating VM directory")?;

        // Use .raw extension to match the new raw disk format
        let disk_path = self.vm_dir.join("rootfs.raw");

        if !disk_path.exists() {
            info!(
                base = %self.base_rootfs.display(),
                disk = %disk_path.display(),
                "creating instant reflink copy (btrfs CoW)"
            );

            let reflink_output = tokio::process::Command::new("cp")
                .arg("--reflink=always")
                .arg(&self.base_rootfs)
                .arg(&disk_path)
                .output()
                .await
                .context("executing cp --reflink=always")?;

            if !reflink_output.status.success() {
                let stderr = String::from_utf8_lossy(&reflink_output.stderr);
                anyhow::bail!(
                    "Reflink copy failed (required for CoW disk). Error: {}. \
                    Ensure the kernel has FUSE_REMAP_FILE_RANGE support (kernel 6.18+ with --kernel-profile nested).",
                    stderr
                );
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
