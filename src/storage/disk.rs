use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use tokio::fs;
use tracing::{debug, info, warn};

/// The inode number every btrfs subvolume root has.
const BTRFS_FIRST_FREE_OBJECTID: u64 = 256;

/// Whether `path` is the root of a btrfs subvolume: a directory on btrfs whose
/// inode is 256, which no other directory on btrfs has. The filesystem check
/// keeps a FUSE mount that forwards a host subvolume's inode (a `--map` in a
/// nested guest), or an inode-256 directory on another filesystem, from being
/// taken for one. A path that cannot be read is not one.
pub fn is_btrfs_subvolume(path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let on_btrfs = nix::sys::statfs::statfs(path)
        .map(|fs| fs.filesystem_type() == nix::sys::statfs::BTRFS_SUPER_MAGIC)
        .unwrap_or(false);
    on_btrfs
        && std::fs::metadata(path)
            .map(|m| m.is_dir() && m.ino() == BTRFS_FIRST_FREE_OBJECTID)
            .unwrap_or(false)
}

/// Whether `dir` is a snapshot's disk directory that a clone may snapshot: it is
/// named as one, it is a btrfs subvolume, and this process would own the result.
/// The name check keeps a base disk in some other subvolume (an operator who made
/// the rootfs store one) from having its whole directory snapshotted.
fn is_snapshot_disk_subvolume(dir: &Path) -> bool {
    dir.file_name()
        .is_some_and(|name| name == crate::storage::snapshot::SNAPSHOT_DISK_SUBDIR)
        && is_btrfs_subvolume(dir)
        && can_own_subvolume_snapshot_of(dir)
}

/// Whether a subvolume snapshot of `base_dir` would give this process a disk
/// directory it can write to and remove. A snapshot keeps the owner of every
/// inode it copies, so an unprivileged clone of a directory another user made
/// would get a disk it cannot open for writing and a directory it cannot
/// remove at teardown; it reflinks the disk instead, which makes a copy it
/// owns. Root can write and remove anything.
pub fn can_own_subvolume_snapshot_of(base_dir: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let euid = nix::unistd::geteuid().as_raw();
    euid == 0
        || std::fs::metadata(base_dir)
            .map(|m| m.uid() == euid)
            .unwrap_or(false)
}

/// Create `path` as a btrfs subvolume, or as an ordinary directory where that
/// fails (a store that is not btrfs, or btrfs behind a FUSE map in a nested
/// guest, whose ioctls do not pass). A snapshot's disk directory that is a
/// subvolume is what lets a clone get its disks with one `btrfs subvolume
/// snapshot`, whatever the disks' extent count; a reflink of a 154 GB disk with
/// 860,177 extents measured 9 to 11 s per clone, a subvolume snapshot 20 ms.
pub async fn create_dir_or_subvolume(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .await
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let output = tokio::process::Command::new("btrfs")
        .args(["subvolume", "create"])
        .arg(path)
        .output()
        .await;
    let reason = match output {
        Ok(output) if output.status.success() && is_btrfs_subvolume(path) => {
            debug!(path = %path.display(), "created as a btrfs subvolume");
            return Ok(());
        }
        Ok(output) => String::from_utf8_lossy(&output.stderr).trim().to_string(),
        Err(error) => format!("btrfs tool unavailable: {error}"),
    };
    info!(
        path = %path.display(),
        %reason,
        "not a btrfs subvolume; created as a directory (copies of it reflink its files)"
    );
    fs::create_dir_all(path)
        .await
        .with_context(|| format!("creating {}", path.display()))
}

/// Remove a snapshot directory. A snapshot directory that is a btrfs subvolume (and its
/// `disk/` subvolume) is deleted as a subvolume, which detaches it at once and leaves the
/// cleaner thread to drop its extent references in the background: unlinking a memory file
/// written by in-place diffs drops one reference per extent first, 52 to 93 s for one with
/// 11.8 million extents. Anything else, or a subvolume this process may not delete (an
/// unprivileged caller without user_subvol_rm_allowed), is removed file by file.
pub async fn remove_snapshot_dir(dir: &Path) -> std::io::Result<()> {
    if is_btrfs_subvolume(dir) {
        let disk = dir.join(crate::storage::snapshot::SNAPSHOT_DISK_SUBDIR);
        let disk_gone = !is_btrfs_subvolume(&disk) || delete_subvolume(&disk).await;
        if disk_gone && delete_subvolume(dir).await && !dir.exists() {
            return Ok(());
        }
    }
    match fs::remove_dir_all(dir).await {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

/// Remove a snapshot create's leftover staging directory, and fail if it is still there: a
/// subvolume snapshot made onto an existing directory lands inside it, and a subvolume
/// cannot be created over one, so a leftover would be silently reused.
pub async fn clear_staging(staging: &Path) -> Result<()> {
    clear_staging_with(staging, remove_snapshot_dir(staging).await)
}

/// `clear_staging` after its removal returned `removed`.
fn clear_staging_with(staging: &Path, removed: std::io::Result<()>) -> Result<()> {
    if staging.exists() {
        let reason = match removed {
            Err(error) => error.to_string(),
            Ok(()) => "it is still there after its removal".to_string(),
        };
        bail!(
            "could not remove the leftover staging directory {}: {reason}",
            staging.display()
        );
    }
    Ok(())
}

/// Wait for btrfs to free the space of subvolumes deleted on the filesystem holding `path`
/// (`btrfs subvolume sync`, bounded): a deleted subvolume's space comes back on the cleaner
/// thread after the delete returns, so a free-space check right after one can read low.
pub async fn wait_for_deleted_subvolumes(path: &Path) {
    let sync = tokio::process::Command::new("btrfs")
        .args(["subvolume", "sync"])
        .arg(path)
        .kill_on_drop(true)
        .output();
    match tokio::time::timeout(std::time::Duration::from_secs(120), sync).await {
        Ok(Ok(output)) if output.status.success() => {}
        Ok(Ok(output)) => debug!(path = %path.display(),
            stderr = %String::from_utf8_lossy(&output.stderr).trim(), "btrfs subvolume sync refused"),
        Ok(Err(error)) => debug!(path = %path.display(), %error, "btrfs tool unavailable"),
        Err(_) => warn!(path = %path.display(), "btrfs subvolume sync did not finish in 120 s"),
    }
}

/// `btrfs subvolume delete`, without waiting for the cleaner. Whether it succeeded.
async fn delete_subvolume(path: &Path) -> bool {
    let output = tokio::process::Command::new("btrfs")
        .args(["subvolume", "delete"])
        .arg(path)
        .output()
        .await;
    match output {
        Ok(output) if output.status.success() => true,
        Ok(output) => {
            debug!(path = %path.display(), stderr = %String::from_utf8_lossy(&output.stderr).trim(),
                "btrfs subvolume delete refused; removing file by file");
            false
        }
        Err(error) => {
            debug!(path = %path.display(), %error, "btrfs tool unavailable; removing file by file");
            false
        }
    }
}

/// Stage a diff snapshot's directory as a subvolume snapshot of its parent's, keeping only
/// the parent's memory file, which the diff is then written into. A subvolume snapshot costs
/// one transaction commit whatever the memory file's extent count, where a reflink of the
/// file costs one reference per extent: 133 s for a memory file of 11.8 million extents
/// written by eight in-place diffs. Returns false, leaving no `staging`, when the parent's
/// directory is not a subvolume this process may snapshot or the snapshot fails; the caller
/// then reflinks the memory file. A failed staging that cannot be removed is an error, since
/// the reflink path would otherwise create its directory over the parent's leftover files.
///
/// The parent's other files (its config, vmstate, recorded working set, locks) are removed
/// from the staging directory, and so is its `disk/`, which a subvolume snapshot shows as an
/// empty placeholder directory when it is a subvolume of its own.
pub async fn stage_from_parent_subvolume(parent: &Path, staging: &Path) -> Result<bool> {
    if !is_btrfs_subvolume(parent) || !can_own_subvolume_snapshot_of(parent) {
        return Ok(false);
    }
    if staging.exists() {
        bail!(
            "{} exists; a subvolume snapshot would land inside it",
            staging.display()
        );
    }
    let started = std::time::Instant::now();
    let output = tokio::process::Command::new("btrfs")
        .args(["subvolume", "snapshot"])
        .arg(parent)
        .arg(staging)
        .output()
        .await;
    let refused = match output {
        Ok(output) if output.status.success() => None,
        Ok(output) => Some(String::from_utf8_lossy(&output.stderr).trim().to_string()),
        Err(error) => Some(format!("btrfs tool unavailable: {error}")),
    };
    if let Some(reason) = refused {
        info!(parent = %parent.display(), %reason,
            "could not snapshot the parent's subvolume; reflinking its memory file instead");
        clear_staging(staging).await?;
        return Ok(false);
    }
    let snapshot_ms = started.elapsed().as_millis() as u64;
    if let Err(error) = keep_only_memory_file(staging).await {
        warn!(staging = %staging.display(), %error,
            "could not clear the parent's other files from the staged snapshot; reflinking instead");
        clear_staging(staging).await?;
        return Ok(false);
    }
    debug!(parent = %parent.display(), staging = %staging.display(), snapshot_ms,
        "staged the snapshot as a subvolume snapshot of its parent");
    Ok(true)
}

/// Remove everything in `dir` but the memory file: files, directories, and the empty
/// placeholder a nested subvolume leaves in a subvolume snapshot.
async fn keep_only_memory_file(dir: &Path) -> Result<()> {
    let mut entries = fs::read_dir(dir)
        .await
        .with_context(|| format!("reading {}", dir.display()))?;
    while let Some(entry) = entries.next_entry().await? {
        if entry.file_name() == crate::storage::snapshot::SNAPSHOT_MEMORY_FILE {
            continue;
        }
        let path = entry.path();
        if entry.file_type().await?.is_dir() {
            fs::remove_dir_all(&path)
                .await
                .with_context(|| format!("removing {}", path.display()))?;
        } else {
            fs::remove_file(&path)
                .await
                .with_context(|| format!("removing {}", path.display()))?;
        }
    }
    Ok(())
}

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

        // Use .raw extension to match the new raw disk format
        let disk_path = self.vm_dir.join("rootfs.raw");

        if disk_path.exists() {
            return Ok(disk_path);
        }

        // A base in a snapshot's disk directory (a btrfs subvolume holding the
        // snapshot's disks and nothing else) is cloned with one subvolume
        // snapshot of that directory. That costs one btrfs transaction commit
        // whatever the disks' extent count: 20 to 60 ms on a quiet host, 0.2 to
        // 1.0 s on a shared devserver at load 14 to 46. A reflink costs one
        // extent reference per extent: 12 ms for a test VM's disk, 9 to 11 s
        // for a 154 GB disk of 860,177 extents. The snapshot becomes this VM's
        // disk directory, with the base disk renamed to the rootfs name; the
        // extra disks it carried are already in place.
        if let Some(base_dir) = self
            .base_rootfs
            .parent()
            .filter(|d| is_snapshot_disk_subvolume(d))
        {
            let started = std::time::Instant::now();
            match self.snapshot_base_subvolume(base_dir, &disk_path).await {
                Ok(()) => {
                    info!(
                        base = %self.base_rootfs.display(),
                        disk = %disk_path.display(),
                        duration_ms = started.elapsed().as_millis() as u64,
                        "created the disk directory as a btrfs subvolume snapshot of the snapshot's"
                    );
                    return Ok(disk_path);
                }
                Err(error) => {
                    warn!(
                        base = %base_dir.display(),
                        %error,
                        "subvolume snapshot of the snapshot's disk directory failed; reflinking the disk instead"
                    );
                    if let Err(error) = fs::remove_dir_all(&self.vm_dir).await {
                        if error.kind() != std::io::ErrorKind::NotFound {
                            return Err(error).with_context(|| {
                                format!("removing the failed snapshot at {}", self.vm_dir.display())
                            });
                        }
                    }
                }
            }
        }

        {
            let started = std::time::Instant::now();
            fs::create_dir_all(&self.vm_dir)
                .await
                .context("creating VM directory")?;
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
            info!(
                disk = %disk_path.display(),
                duration_ms = started.elapsed().as_millis() as u64,
                "reflink copy done"
            );
        }

        Ok(disk_path)
    }

    /// Snapshot `base_dir` (a btrfs subvolume holding the base disk and the
    /// snapshot's extra disks) into this VM's disk directory, which must not
    /// exist yet, and give the base disk `disk_path`'s name.
    async fn snapshot_base_subvolume(&self, base_dir: &Path, disk_path: &Path) -> Result<()> {
        if let Some(parent) = self.vm_dir.parent() {
            fs::create_dir_all(parent)
                .await
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let output = tokio::process::Command::new("btrfs")
            .args(["subvolume", "snapshot"])
            .arg(base_dir)
            .arg(&self.vm_dir)
            .output()
            .await
            .context("running btrfs subvolume snapshot")?;
        if !output.status.success() {
            anyhow::bail!(
                "btrfs subvolume snapshot {} {}: {}",
                base_dir.display(),
                self.vm_dir.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        let source_name = self
            .base_rootfs
            .file_name()
            .context("base rootfs has no file name")?;
        fs::rename(self.vm_dir.join(source_name), disk_path)
            .await
            .context("naming the clone's disk")?;
        Ok(())
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

#[cfg(test)]
mod snapshot_dir_tests {
    use super::*;

    /// A leftover staging directory that cannot be removed fails the create: a subvolume
    /// snapshot made onto it would land inside it. The removal's failure is given to the
    /// check, since root removes files from a read-only directory.
    #[tokio::test]
    async fn an_unremovable_leftover_staging_directory_fails() {
        let dir = tempfile::tempdir().unwrap();
        let staging = dir.path().join("snap.creating");
        std::fs::create_dir_all(staging.join("disk")).unwrap();
        let refused = std::io::Error::from_raw_os_error(libc::EPERM);
        let error = clear_staging_with(&staging, Err(refused))
            .expect_err("an unremovable leftover must fail the create");
        assert!(
            error.to_string().contains("leftover staging directory"),
            "{error}"
        );
        clear_staging_with(&staging, Ok(())).expect_err("a leftover still present must fail");
        clear_staging(&staging).await.unwrap();
        assert!(!staging.exists());
        clear_staging(&staging).await.unwrap();
    }

    /// A staged snapshot keeps the parent's memory file and nothing else of its directory.
    #[tokio::test]
    async fn staging_keeps_only_the_memory_file() {
        let dir = tempfile::tempdir().unwrap();
        let staged = dir.path();
        std::fs::write(staged.join("memory.bin"), b"memory").unwrap();
        std::fs::write(staged.join("config.json"), b"{}").unwrap();
        std::fs::write(staged.join("memory.bin.working-set"), b"set").unwrap();
        std::fs::create_dir(staged.join("disk")).unwrap();
        std::fs::write(staged.join("disk").join("disk.raw"), b"disk").unwrap();
        std::fs::create_dir(staged.join("empty-placeholder")).unwrap();
        keep_only_memory_file(staged).await.unwrap();
        let mut left: Vec<String> = std::fs::read_dir(staged)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(left, ["memory.bin"]);
        assert_eq!(std::fs::read(staged.join("memory.bin")).unwrap(), b"memory");
    }

    /// A parent directory that is not a subvolume stages nothing, and the caller reflinks.
    #[tokio::test]
    async fn a_plain_parent_directory_is_not_staged() {
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().join("parent");
        std::fs::create_dir(&parent).unwrap();
        std::fs::write(parent.join("memory.bin"), b"memory").unwrap();
        let staging = dir.path().join("parent.creating");
        assert!(!stage_from_parent_subvolume(&parent, &staging)
            .await
            .unwrap());
        assert!(
            !staging.exists(),
            "a refused staging left {}",
            staging.display()
        );
    }

    /// A plain snapshot directory is removed file by file, and a missing one is not an error.
    #[tokio::test]
    async fn a_plain_snapshot_directory_is_removed() {
        let dir = tempfile::tempdir().unwrap();
        let snapshot = dir.path().join("snap");
        std::fs::create_dir_all(snapshot.join("disk")).unwrap();
        std::fs::write(snapshot.join("memory.bin"), b"memory").unwrap();
        std::fs::write(snapshot.join("disk").join("disk.raw"), b"disk").unwrap();
        remove_snapshot_dir(&snapshot).await.unwrap();
        assert!(!snapshot.exists());
        remove_snapshot_dir(&snapshot).await.unwrap();
    }
}
