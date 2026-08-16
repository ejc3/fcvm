use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use tokio::fs;
use tracing::{debug, info};

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
                // No copy fallback. A reflink that silently becomes a copy is
                // not a slower success, it is a different operation: O(1)
                // becomes O(image size), per VM, and the only trace is a WARN.
                // Issue #810 lived seven months behind exactly that fallback
                // (55ef6350), surfacing as nested tests timing out at 843s
                // under load while passing on a quiet box. Fail loudly instead
                // and name both causes, because the fix differs per cause.
                let cross_device =
                    stderr.contains("cross-device") || stderr.contains("Invalid cross-device link");
                anyhow::bail!(
                    "Reflink copy failed (required for CoW disk): {}\n\
                     base: {}\n\
                     disk: {}\n\
                     {}",
                    stderr.trim(),
                    self.base_rootfs.display(),
                    disk_path.display(),
                    if cross_device {
                        "Cause: the base image and the VM disk are on DIFFERENT filesystems, so \
                         the kernel refuses FICLONE with EXDEV before the filesystem is ever \
                         consulted. Put the data directory on the same filesystem as the base \
                         image (for a nested VM, that is the mapped /mnt/fcvm-btrfs, not the \
                         guest's local disk)."
                    } else {
                        "Cause: the filesystem holding these paths does not implement clone. \
                         Ensure the kernel has FUSE_REMAP_FILE_RANGE support (a kernel profile \
                         carrying kernel/patches/0001-fuse-add-remap_file_range-support.patch), \
                         and that the backing store is btrfs."
                    }
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

/// Ensure the filesystem has at least `min_free + extra_bytes` of free space.
/// `extra_bytes` accounts for content that will be written after boot (e.g., container image layers).
/// Auto-detects filesystem type (ext4 or btrfs) and uses the appropriate tools.
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

    let fs_type = detect_filesystem_type(disk_path).await?;
    match fs_type.as_str() {
        "btrfs" => ensure_free_space_btrfs(disk_path, min_free).await,
        _ => ensure_free_space_ext4(disk_path, min_free).await,
    }
}

/// Detect filesystem type of an image file using blkid.
async fn detect_filesystem_type(path: &Path) -> Result<String> {
    let output = tokio::process::Command::new("blkid")
        .args(["-o", "value", "-s", "TYPE", path.to_string_lossy().as_ref()])
        .output()
        .await
        .context("running blkid")?;
    if !output.status.success() {
        anyhow::bail!(
            "blkid failed for {}: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Ensure ext4 filesystem has sufficient free space by expanding and resizing.
async fn ensure_free_space_ext4(disk_path: &Path, min_free: u64) -> Result<()> {
    // Get current free space via dumpe2fs
    let output = tokio::process::Command::new("dumpe2fs")
        .args(["-h", disk_path.to_string_lossy().as_ref()])
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
        "expanding ext4 rootfs to ensure minimum free space"
    );

    // Expand the sparse file
    let output = tokio::process::Command::new("truncate")
        .args([
            "-s",
            &format!("+{}", expand_by),
            disk_path.to_string_lossy().as_ref(),
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
    let e2fsck_output = tokio::process::Command::new("e2fsck")
        .args(["-f", "-y", disk_path.to_string_lossy().as_ref()])
        .output()
        .await
        .context("running e2fsck")?;

    // e2fsck exit codes: 0=clean, 1=corrected, 2=corrected+reboot needed
    // Exit code >= 4 means uncorrected errors
    if e2fsck_output.status.code().unwrap_or(8) >= 4 {
        bail!(
            "e2fsck found uncorrectable errors: {}",
            String::from_utf8_lossy(&e2fsck_output.stderr)
        );
    }

    // Resize ext4 filesystem to fill the new space
    let output = tokio::process::Command::new("resize2fs")
        .arg(disk_path.to_string_lossy().as_ref())
        .output()
        .await
        .context("resizing ext4 filesystem")?;

    if !output.status.success() {
        bail!(
            "resize2fs failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    info!(disk = %disk_path.display(), "ext4 rootfs expanded successfully");
    Ok(())
}

/// Ensure btrfs filesystem has sufficient free space by expanding the sparse file.
/// The guest resizes the btrfs filesystem at boot via `btrfs filesystem resize max /`.
async fn ensure_free_space_btrfs(disk_path: &Path, min_free: u64) -> Result<()> {
    // Parse btrfs superblock to get size info (no mount needed)
    let output = tokio::process::Command::new("btrfs")
        .args([
            "inspect-internal",
            "dump-super",
            disk_path.to_string_lossy().as_ref(),
        ])
        .output()
        .await
        .context("running btrfs dump-super")?;

    if !output.status.success() {
        bail!(
            "btrfs dump-super failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let total_bytes = parse_dump_super_value(&stdout, "total_bytes")?;
    let bytes_used = parse_dump_super_value(&stdout, "bytes_used")?;
    let free_bytes = total_bytes.saturating_sub(bytes_used);

    if free_bytes >= min_free {
        debug!(
            disk = %disk_path.display(),
            free_bytes,
            min_free,
            "btrfs disk already has sufficient free space"
        );
        return Ok(());
    }

    let expand_by = min_free - free_bytes;
    info!(
        disk = %disk_path.display(),
        free_bytes,
        min_free,
        expand_by,
        "expanding btrfs rootfs sparse file"
    );

    // Expand sparse file — fc-agent resizes btrfs at boot
    let output = tokio::process::Command::new("truncate")
        .args([
            "-s",
            &format!("+{}", expand_by),
            disk_path.to_string_lossy().as_ref(),
        ])
        .output()
        .await
        .context("expanding btrfs sparse file")?;

    if !output.status.success() {
        bail!(
            "truncate failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    info!(disk = %disk_path.display(), "btrfs rootfs sparse file expanded");
    Ok(())
}

/// Parse a value from `btrfs inspect-internal dump-super` output.
/// Format: "total_bytes\t\t10737418240" or "bytes_used\t\t2147483648"
fn parse_dump_super_value(output: &str, key: &str) -> Result<u64> {
    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with(key) {
            if let Some(value) = trimmed.split_whitespace().last() {
                return value
                    .parse::<u64>()
                    .with_context(|| format!("parsing {} value '{}'", key, value));
            }
        }
    }
    bail!("'{}' not found in btrfs dump-super output", key)
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

/// Parse size strings like "10G", "500M", "1024K", or plain bytes.
/// Integers only — "10.5G" is not supported (matches truncate(1) convention).
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

    num.checked_mul(multiplier)
        .with_context(|| format!("size overflow: {} * {}", num, multiplier))
}
