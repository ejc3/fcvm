use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use tokio::fs;
use tracing::{debug, info, warn};

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

                // Check if this is a cross-device error (common in nested VMs where
                // source is on FUSE mount but destination is local filesystem)
                if stderr.contains("cross-device") || stderr.contains("Invalid cross-device link") {
                    warn!(
                        base = %self.base_rootfs.display(),
                        disk = %disk_path.display(),
                        "reflink failed (cross-device), falling back to regular copy (slower)"
                    );

                    // Fall back to regular copy
                    let copy_output = tokio::process::Command::new("cp")
                        .arg(&self.base_rootfs)
                        .arg(&disk_path)
                        .output()
                        .await
                        .context("executing cp (fallback)")?;

                    if !copy_output.status.success() {
                        let copy_stderr = String::from_utf8_lossy(&copy_output.stderr);
                        anyhow::bail!("Disk copy failed. Error: {}", copy_stderr);
                    }
                } else {
                    anyhow::bail!(
                        "Reflink copy failed (required for CoW disk). Error: {}. \
                        Ensure the kernel has FUSE_REMAP_FILE_RANGE support (requires a kernel profile with this patch).",
                        stderr
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

/// Ensure the ext4 filesystem has at least `min_free + extra_bytes` of free space.
/// `extra_bytes` accounts for content that will be written after boot (e.g., container image layers).
pub async fn ensure_free_space(
    disk_path: &Path,
    min_free_str: &str,
    extra_bytes: u64,
) -> Result<()> {
    let min_free = parse_size(min_free_str)
        .with_context(|| format!("parsing rootfs-size '{}'", min_free_str))?
        + extra_bytes;

    if min_free == 0 {
        return Ok(());
    }

    // Get current free space via dumpe2fs
    let output = tokio::process::Command::new("dumpe2fs")
        .args(["-h", disk_path.to_str().unwrap()])
        .output()
        .await
        .context("running dumpe2fs")?;

    if !output.status.success() {
        bail!(
            "dumpe2fs failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let block_size = parse_dumpe2fs_value(&stdout, "Block size")?;
    let free_blocks = parse_dumpe2fs_value(&stdout, "Free blocks")?;
    let free_bytes = free_blocks * block_size;

    if free_bytes >= min_free {
        debug!(
            disk = %disk_path.display(),
            free_bytes,
            min_free,
            "disk already has sufficient free space"
        );
        return Ok(());
    }

    let expand_by = min_free - free_bytes;
    info!(
        disk = %disk_path.display(),
        free_bytes,
        min_free,
        expand_by,
        "expanding rootfs to ensure minimum free space"
    );

    // Expand the sparse file
    let output = tokio::process::Command::new("truncate")
        .args([
            "-s",
            &format!("+{}", expand_by),
            disk_path.to_str().unwrap(),
        ])
        .output()
        .await
        .context("expanding disk file")?;

    if !output.status.success() {
        bail!(
            "truncate failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    // Check filesystem before resize (required by resize2fs)
    let _ = tokio::process::Command::new("e2fsck")
        .args(["-f", "-y", disk_path.to_str().unwrap()])
        .output()
        .await;

    // Resize ext4 filesystem to fill the new space
    let output = tokio::process::Command::new("resize2fs")
        .arg(disk_path.to_str().unwrap())
        .output()
        .await
        .context("resizing ext4 filesystem")?;

    if !output.status.success() {
        bail!(
            "resize2fs failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    info!(disk = %disk_path.display(), "rootfs expanded successfully");
    Ok(())
}

/// Parse a value from dumpe2fs -h output (e.g., "Block size:          4096")
fn parse_dumpe2fs_value(output: &str, key: &str) -> Result<u64> {
    for line in output.lines() {
        if line.starts_with(key) {
            if let Some(value) = line.split(':').nth(1) {
                return value
                    .trim()
                    .parse::<u64>()
                    .with_context(|| format!("parsing {} value", key));
            }
        }
    }
    bail!("'{}' not found in dumpe2fs output", key)
}

/// Parse size strings like "10G", "500M", "1024K", or plain bytes
pub fn parse_size(s: &str) -> Result<u64> {
    let s = s.trim();
    if s.is_empty() {
        bail!("empty size string");
    }

    let (num_str, multiplier) = if s.ends_with('G') || s.ends_with('g') {
        (&s[..s.len() - 1], 1024u64 * 1024 * 1024)
    } else if s.ends_with('M') || s.ends_with('m') {
        (&s[..s.len() - 1], 1024u64 * 1024)
    } else if s.ends_with('K') || s.ends_with('k') {
        (&s[..s.len() - 1], 1024u64)
    } else {
        (s, 1u64)
    };

    let num: u64 = num_str
        .parse()
        .with_context(|| format!("parsing size number '{}'", num_str))?;

    Ok(num * multiplier)
}
