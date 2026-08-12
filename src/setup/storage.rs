use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;
use tracing::info;

use crate::setup::rootfs::load_config;

/// Required asset subdirectories under the btrfs mount.
/// Data dirs (state, snapshots, vm-disks) are created lazily by the code
/// that uses them, allowing FCVM_DATA_DIR to point elsewhere.
const REQUIRED_DIRS: &[&str] = &["kernels", "rootfs", "initrd", "cache", "image-cache"];

/// Unmount a path, ignoring errors
fn cleanup_mount(path: &Path) {
    let _ = Command::new("umount").arg(path).status();
}

/// Get the total size of the filesystem containing `path` (e.g., "1.8T").
/// Returns None if the size can't be determined.
fn get_filesystem_size(path: &Path) -> Option<String> {
    let output = Command::new("df")
        .args(["--output=size", "-B1"])
        .arg(path)
        .output()
        .ok()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    // df output: header line then value line with size in bytes
    let bytes: u64 = stdout.lines().nth(1)?.trim().parse().ok()?;
    if bytes == 0 {
        return None;
    }
    Some(format!("{}", bytes))
}

/// Check if a path is on a btrfs filesystem
fn is_btrfs(path: &Path) -> bool {
    Command::new("stat")
        .args(["-f", "-c", "%T"])
        .arg(path)
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "btrfs")
        .unwrap_or(false)
}

/// Get storage paths and btrfs size from config
fn get_storage_paths(config_path: Option<&str>) -> Result<(PathBuf, PathBuf, String)> {
    let (config, _, _) = load_config(config_path)?;
    let btrfs_size = config.paths.btrfs_size.clone();
    let mount_point = PathBuf::from(&config.paths.assets_dir);

    // Canonicalize the mount point to resolve .., ., and symlinks
    // If it doesn't exist yet, canonicalize as much as possible
    let canonical_mount = if mount_point.exists() {
        mount_point
            .canonicalize()
            .context("canonicalizing mount point path")?
    } else {
        // For non-existent paths, try to canonicalize the parent
        if let Some(parent) = mount_point.parent() {
            let canonical_parent = if parent.exists() {
                parent
                    .canonicalize()
                    .context("canonicalizing mount point parent")?
            } else {
                parent.to_path_buf()
            };
            canonical_parent.join(mount_point.file_name().unwrap_or_default())
        } else {
            mount_point.clone()
        }
    };

    // Loopback image is a sibling of mount point (e.g., /mnt/fcvm-btrfs -> /mnt/fcvm-btrfs.img)
    // Use the canonical path and proper PathBuf API to construct the loopback path
    let mut loopback_image = canonical_mount.clone();
    let current_name = loopback_image
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("fcvm-btrfs");
    loopback_image.set_file_name(format!("{}.img", current_name));

    Ok((canonical_mount, loopback_image, btrfs_size))
}

/// Ensure btrfs storage is set up at the configured assets_dir.
///
/// If the host filesystem is already btrfs, just creates the directory — no loopback needed.
/// Otherwise, creates a loopback btrfs image as a sibling file (e.g., /mnt/fcvm-btrfs.img).
///
/// Creating the loopback and mounting requires root privileges.
pub fn ensure_storage(config_path: Option<&str>) -> Result<()> {
    let (mount_point, loopback_image, btrfs_size) = get_storage_paths(config_path)?;

    // Already btrfs? Just ensure directories exist (no root needed)
    if is_btrfs(&mount_point) {
        for dir in REQUIRED_DIRS {
            let path = mount_point.join(dir);
            std::fs::create_dir_all(&path).with_context(|| {
                format!(
                    "creating directory {} (if mount was unmounted, run 'sudo fcvm setup' again)",
                    path.display()
                )
            })?;
        }
        return Ok(());
    }

    // Mount point doesn't exist (or isn't btrfs) — check if parent is already btrfs.
    // If so, just create the directory directly. A loopback mount on a btrfs host
    // causes podman namespace divergence (podman's pause process gets its own mount
    // namespace, and file operations in the host namespace become invisible to podman).
    if let Some(parent) = mount_point.parent() {
        if parent.exists() && is_btrfs(parent) {
            info!(
                "Parent {} is already btrfs, creating {} directly (no loopback needed)",
                parent.display(),
                mount_point.display()
            );
            std::fs::create_dir_all(&mount_point)
                .context("creating assets directory on existing btrfs")?;
            for dir in REQUIRED_DIRS {
                let path = mount_point.join(dir);
                std::fs::create_dir_all(&path)
                    .with_context(|| format!("creating directory {}", path.display()))?;
            }
            return Ok(());
        }
    }

    // Need to create/mount btrfs - requires root
    if !nix::unistd::Uid::effective().is_root() {
        anyhow::bail!(
            "Storage not initialized. Run with sudo:\n\n  \
            sudo fcvm setup\n\n\
            This creates a {} btrfs filesystem at {} for CoW disk snapshots.",
            btrfs_size,
            mount_point.display()
        );
    }

    info!("Initializing btrfs storage at {}", mount_point.display());

    // Check if already mounted but wrong filesystem type
    if mount_point.exists() && mount_point.is_dir() {
        let output = Command::new("mountpoint")
            .arg("-q")
            .arg(&mount_point)
            .status()?;

        if output.success() {
            // Something is mounted but it's not btrfs
            anyhow::bail!(
                "{} is mounted but not btrfs. fcvm requires btrfs for CoW disk snapshots.\n\
                Either unmount and let fcvm create btrfs, or mount a btrfs filesystem there.",
                mount_point.display()
            );
        }
    }

    // Create loopback image if it doesn't exist
    if !loopback_image.exists() {
        // Ensure parent directory exists
        if let Some(parent) = loopback_image.parent() {
            std::fs::create_dir_all(parent).context("creating loopback image parent directory")?;
        }

        // Size the sparse loopback to the full parent filesystem capacity.
        // Since it's sparse, only written blocks use real space — so we can
        // share the full disk capacity without reserving anything up front.
        let loopback_size = if let Some(parent) = loopback_image.parent() {
            get_filesystem_size(parent).unwrap_or_else(|| btrfs_size.clone())
        } else {
            btrfs_size.clone()
        };

        info!(
            "Creating {} sparse loopback image at {}",
            loopback_size,
            loopback_image.display()
        );

        // Create sparse file
        let status = Command::new("truncate")
            .arg("-s")
            .arg(&loopback_size)
            .arg(&loopback_image)
            .status()
            .context("executing truncate")?;

        if !status.success() {
            anyhow::bail!("Failed to create loopback image");
        }

        // Format as btrfs
        info!("Formatting as btrfs...");
        let status = Command::new("mkfs.btrfs")
            .arg(&loopback_image)
            .status()
            .context("executing mkfs.btrfs")?;

        if !status.success() {
            // Clean up the file on failure
            let _ = std::fs::remove_file(&loopback_image);
            anyhow::bail!("Failed to format loopback image as btrfs. Is btrfs-progs installed?");
        }
    }

    // Create mount point
    std::fs::create_dir_all(&mount_point).context("creating mount point")?;

    // Mount the loopback image
    info!("Mounting btrfs filesystem...");
    let status = Command::new("mount")
        .arg("-o")
        .arg("loop")
        .arg(&loopback_image)
        .arg(&mount_point)
        .status()
        .context("executing mount")?;

    if !status.success() {
        anyhow::bail!(
            "Failed to mount {}. Check dmesg for errors.",
            loopback_image.display()
        );
    }

    // Create required subdirectories (cleanup mount on failure)
    for dir in REQUIRED_DIRS {
        let path = mount_point.join(dir);
        if let Err(e) = std::fs::create_dir_all(&path) {
            // Clean up the mount before returning error
            cleanup_mount(&mount_point);
            return Err(e).with_context(|| format!("creating directory {}", path.display()));
        }
    }

    // Hand the mount point back to the user who invoked sudo (non-recursive:
    // we just created it). Uses the shared helper, which resolves the real
    // primary group from passwd instead of assuming group == user name.
    super::give_store_entry_to_invoker(&mount_point);

    info!(
        "✓ btrfs storage ready at {} ({})",
        mount_point.display(),
        btrfs_size
    );

    Ok(())
}
