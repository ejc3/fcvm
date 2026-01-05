use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;
use tracing::info;

use crate::setup::rootfs::load_config;

const DEFAULT_SIZE_GB: u64 = 60;

/// Required subdirectories under the btrfs mount
const REQUIRED_DIRS: &[&str] = &[
    "kernels",
    "rootfs",
    "initrd",
    "state",
    "snapshots",
    "vm-disks",
    "cache",
    "image-cache",
];

/// Unmount a path, ignoring errors
fn cleanup_mount(path: &Path) {
    let _ = Command::new("umount").arg(path).status();
}

/// Check if a path is a btrfs filesystem
fn is_btrfs_mount(path: &Path) -> bool {
    // Check if it's a mountpoint first
    let output = Command::new("mountpoint").arg("-q").arg(path).status();

    if output.map(|s| s.success()).unwrap_or(false) {
        // Check filesystem type
        let output = Command::new("stat")
            .arg("-f")
            .arg("-c")
            .arg("%T")
            .arg(path)
            .output();

        if let Ok(output) = output {
            let fstype = String::from_utf8_lossy(&output.stdout);
            return fstype.trim() == "btrfs";
        }
    }
    false
}

/// Get storage paths from config
fn get_storage_paths(config_path: Option<&str>) -> Result<(PathBuf, PathBuf)> {
    let (config, _, _) = load_config(config_path)?;
    let mount_point = PathBuf::from(&config.paths.assets_dir);

    // Canonicalize the mount point to resolve .., ., and symlinks
    // If it doesn't exist yet, canonicalize as much as possible
    let canonical_mount = if mount_point.exists() {
        mount_point.canonicalize()
            .context("canonicalizing mount point path")?
    } else {
        // For non-existent paths, try to canonicalize the parent
        if let Some(parent) = mount_point.parent() {
            let canonical_parent = if parent.exists() {
                parent.canonicalize()
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
    let current_name = loopback_image.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("fcvm-btrfs");
    loopback_image.set_file_name(format!("{}.img", current_name));

    Ok((canonical_mount, loopback_image))
}

/// Ensure btrfs storage is set up at the configured assets_dir.
///
/// If the mount point doesn't exist or isn't btrfs, creates a loopback image
/// as a sibling file (e.g., /mnt/fcvm-btrfs.img for /mnt/fcvm-btrfs),
/// formats it as btrfs, and mounts it.
///
/// Creating the loopback and mounting requires root privileges.
pub fn ensure_storage(config_path: Option<&str>) -> Result<()> {
    let (mount_point, loopback_image) = get_storage_paths(config_path)?;

    // Already btrfs? Just ensure directories exist (no root needed)
    if is_btrfs_mount(&mount_point) {
        for dir in REQUIRED_DIRS {
            let path = mount_point.join(dir);
            std::fs::create_dir_all(&path)
                .with_context(|| {
                    format!(
                        "creating directory {} (if mount was unmounted, run 'sudo fcvm setup' again)",
                        path.display()
                    )
                })?;
        }
        return Ok(());
    }

    // Need to create/mount btrfs - requires root
    if !nix::unistd::Uid::effective().is_root() {
        anyhow::bail!(
            "Storage not initialized. Run with sudo:\n\n  \
            sudo fcvm setup\n\n\
            This creates a {}GB btrfs filesystem at {} for CoW disk snapshots.",
            DEFAULT_SIZE_GB,
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
            std::fs::create_dir_all(parent)
                .context("creating loopback image parent directory")?;
        }

        info!(
            "Creating {}GB loopback image at {}",
            DEFAULT_SIZE_GB,
            loopback_image.display()
        );

        // Create sparse file
        let status = Command::new("truncate")
            .arg("-s")
            .arg(format!("{}G", DEFAULT_SIZE_GB))
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

    // Set ownership to the user who invoked sudo (if SUDO_USER is set)
    if let Ok(sudo_user) = std::env::var("SUDO_USER") {
        info!("Setting ownership to {}", sudo_user);
        // Chown the mount point itself (non-recursive since we just created it)
        let status = Command::new("chown")
            .arg(format!("{}:{}", sudo_user, sudo_user))
            .arg(&mount_point)
            .status()
            .context("executing chown")?;

        if !status.success() {
            // Non-fatal, just warn
            tracing::warn!("Failed to set ownership to {}", sudo_user);
        }
    }

    info!(
        "✓ btrfs storage ready at {} ({}GB)",
        mount_point.display(),
        DEFAULT_SIZE_GB
    );

    Ok(())
}
