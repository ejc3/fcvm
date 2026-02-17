use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use tracing::{debug, info, warn};

/// Validate that a Docker archive contains manifest.json.
///
/// Docker archive format requires manifest.json to be loadable.
/// If this file is missing, the archive is corrupted and will fail to load.
pub(super) fn validate_docker_archive(archive_path: &Path) -> Result<bool> {
    let tar_file = std::fs::File::open(archive_path)
        .with_context(|| format!("opening archive {} for validation", archive_path.display()))?;

    let mut archive = tar::Archive::new(tar_file);

    for entry in archive.entries().context("reading archive entries")? {
        let entry = entry.context("reading archive entry")?;
        if let Ok(path) = entry.path() {
            if path.to_str() == Some("manifest.json") {
                return Ok(true);
            }
        }
    }

    Ok(false)
}

/// Get the image identifier for cache key computation.
///
/// For localhost/ images: returns SHA256 digest from podman (requires podman)
/// For remote images: returns the image URL/name as-is (no podman needed)
pub(super) async fn get_image_identifier(image: &str) -> Result<String> {
    if image.starts_with("localhost/") {
        // Use podman to get the digest for localhost images
        let output = tokio::process::Command::new("podman")
            .args(["image", "inspect", image, "--format", "{{.Digest}}"])
            .output()
            .await
            .context("running podman inspect")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("Failed to get digest for image '{}': {}", image, stderr);
        }

        let digest = String::from_utf8_lossy(&output.stdout)
            .trim()
            .trim_start_matches("sha256:")
            .to_string();

        Ok(digest)
    } else {
        // For remote images, use the image name/URL as identifier
        Ok(image.to_string())
    }
}

/// Create an ext4 disk image from a directory's contents.
///
/// When `shrink` is true, runs `resize2fs -M` after creation to minimize the image size.
/// Use `shrink: true` for read-only images (e.g. additionalImageStore) and
/// `shrink: false` for read-write images (e.g. --disk-dir) that need free space.
pub(super) async fn create_disk_from_dir(
    source_dir: &std::path::Path,
    output_path: &std::path::Path,
    shrink: bool,
) -> Result<()> {
    // Calculate directory size for ext4 image sizing
    let dir_size = tokio::process::Command::new("du")
        .args(["-sb", source_dir.to_str().unwrap()])
        .output()
        .await
        .context("calculating directory size")?;

    let size_str = String::from_utf8_lossy(&dir_size.stdout);
    let size_bytes: u64 = size_str
        .split_whitespace()
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or(16 * 1024 * 1024);

    // Use 2x the data size for the image. mkfs.ext4 needs space for inodes, journal,
    // superblock, and directory entries — 20% is insufficient for images with many small
    // files (like CA certificate bundles). Since the image is a sparse file, unused space
    // doesn't consume actual disk. After mkfs, we shrink with resize2fs -M.
    let image_size = std::cmp::max(size_bytes * 2, 64 * 1024 * 1024);

    info!(
        "Creating disk image from {}: {} bytes -> {} bytes",
        source_dir.display(),
        size_bytes,
        image_size
    );

    // Create sparse file
    let truncate_status = tokio::process::Command::new("truncate")
        .args(["-s", &image_size.to_string(), output_path.to_str().unwrap()])
        .status()
        .await
        .context("creating sparse file")?;

    if !truncate_status.success() {
        bail!(
            "truncate failed with exit code: {:?}",
            truncate_status.code()
        );
    }

    // Format as ext4 and populate from source directory in one step.
    // Uses mkfs.ext4 -d which doesn't require root (no mount/loop device needed).
    let mkfs = tokio::process::Command::new("mkfs.ext4")
        .args([
            "-q",
            "-F",
            "-d",
            source_dir.to_str().unwrap(),
            output_path.to_str().unwrap(),
        ])
        .output()
        .await
        .context("formatting as ext4 with directory contents")?;

    if !mkfs.status.success() {
        bail!(
            "mkfs.ext4 -d failed: {}",
            String::from_utf8_lossy(&mkfs.stderr)
        );
    }

    if shrink {
        // Shrink the filesystem to its minimum size. The sparse file was deliberately
        // oversized to ensure mkfs.ext4 had enough space; resize2fs -M reclaims the slack.
        // Only used for read-only images; read-write images need the free space.
        let resize = tokio::process::Command::new("resize2fs")
            .args(["-M", output_path.to_str().unwrap()])
            .output()
            .await
            .context("shrinking ext4 image")?;

        if !resize.status.success() {
            warn!(
                "resize2fs -M failed (non-fatal): {}",
                String::from_utf8_lossy(&resize.stderr)
            );
        }
    }

    info!("Created disk image: {}", output_path.display());
    Ok(())
}

/// Build a podman storage image from a Docker archive.
///
/// Loads the archive into a temporary podman storage root using the overlay driver,
/// then packages the result as an ext4 image. The guest can mount this read-only
/// and use it as an `additionalImageStore`, eliminating the need for `podman load`.
pub(super) async fn build_storage_image(
    archive_path: &std::path::Path,
    output_path: &std::path::Path,
) -> Result<()> {
    // Use output_path's parent (the image-cache dir on btrfs) for temp storage,
    // not /tmp which may be tmpfs with limited space.
    let cache_dir = output_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("output_path has no parent directory"))?;
    let tmp_dir = cache_dir.join(format!("tmp-storage-{}", std::process::id()));

    // Clean up any stale temp dir from a previous interrupted run
    if tmp_dir.exists() {
        tokio::fs::remove_dir_all(&tmp_dir).await.ok();
    }
    tokio::fs::create_dir_all(&tmp_dir)
        .await
        .context("creating temp storage dir")?;

    // Load the docker archive into a custom podman storage root
    info!(
        "Loading archive into podman storage: {} -> {}",
        archive_path.display(),
        tmp_dir.display()
    );

    let load_output = tokio::process::Command::new("podman")
        .args([
            "--root",
            tmp_dir.to_str().unwrap(),
            "--storage-driver",
            "overlay",
            "load",
            "-i",
            archive_path.to_str().unwrap(),
        ])
        .output()
        .await
        .context("running podman load into storage root")?;

    if !load_output.status.success() {
        let stderr = String::from_utf8_lossy(&load_output.stderr);
        tokio::fs::remove_dir_all(&tmp_dir).await.ok();
        bail!("podman load into storage root failed: {}", stderr);
    }

    let loaded_msg = String::from_utf8_lossy(&load_output.stdout);
    info!("podman load output: {}", loaded_msg.trim());

    // Package the storage tree as an ext4 image using the existing helper.
    // NOTE: Can't use with_extension() here because output_path ends in .storage.img
    // -- with_extension replaces after the last dot, producing a double "storage".
    let tmp_img = PathBuf::from(format!("{}.tmp", output_path.display()));
    let result = create_disk_from_dir(&tmp_dir, &tmp_img, true).await;

    // Clean up temp storage dir regardless of result
    tokio::fs::remove_dir_all(&tmp_dir).await.ok();

    if let Err(e) = result {
        // Clean up partial .tmp image file on failure
        tokio::fs::remove_file(&tmp_img).await.ok();
        return Err(e).context("creating ext4 image from storage dir");
    }

    // Atomic rename to final path
    if let Err(e) = tokio::fs::rename(&tmp_img, output_path).await {
        // Clean up .tmp image file if rename fails
        tokio::fs::remove_file(&tmp_img).await.ok();
        return Err(e).context("renaming storage image to final path");
    }

    info!("Built storage image: {}", output_path.display());
    Ok(())
}

/// Find mkfs.btrfs with version >= 6.12 (needed for --rootdir --subvol).
///
/// Checks PATH first, then the build-from-source location at
/// `/mnt/fcvm-btrfs/deps/bin/mkfs.btrfs`.
fn find_mkfs_btrfs() -> Result<PathBuf> {
    let candidates = [
        // Check PATH first
        which::which("mkfs.btrfs").ok(),
        // Then check the build-from-source location
        Some(PathBuf::from("/mnt/fcvm-btrfs/deps/bin/mkfs.btrfs")),
    ];

    for candidate in candidates.into_iter().flatten() {
        if !candidate.exists() {
            continue;
        }

        // Check version >= 6.12
        let output = std::process::Command::new(&candidate)
            .arg("--version")
            .output();

        if let Ok(output) = output {
            let version_str = String::from_utf8_lossy(&output.stdout);
            // Parse version from "mkfs.btrfs, part of btrfs-progs v6.12"
            if let Some(version) = version_str
                .split('v')
                .next_back()
                .and_then(|s| s.trim().split_once('.'))
            {
                let major: u32 = version.0.parse().unwrap_or(0);
                let minor: u32 = version
                    .1
                    .chars()
                    .take_while(|c| c.is_ascii_digit())
                    .collect::<String>()
                    .parse()
                    .unwrap_or(0);

                if major > 6 || (major == 6 && minor >= 12) {
                    info!(
                        "Found mkfs.btrfs {}.{} at {}",
                        major,
                        minor,
                        candidate.display()
                    );
                    return Ok(candidate);
                }
                debug!(
                    "mkfs.btrfs at {} is v{}.{} (need >= 6.12)",
                    candidate.display(),
                    major,
                    minor
                );
            }
        }
    }

    bail!(
        "mkfs.btrfs >= 6.12 not found.\n\
         Install with: scripts/build-btrfs-progs.sh\n\
         Ubuntu 24.04 ships 6.6.3 which lacks --rootdir --subvol support."
    )
}

/// Build a btrfs storage image from a Docker archive.
///
/// Loads the archive into a temporary podman storage root on btrfs (creating real
/// btrfs subvolumes), then packages the result as a btrfs image using
/// `mkfs.btrfs --rootdir --subvol`. The guest mounts this directly as its
/// podman graphroot — no podman load needed, instant startup.
///
/// When `build_uid` is set, runs `podman load` as that user (rootless podman).
/// This creates storage with the correct UID ownership for rootless use in the guest,
/// matching how rootless podman works on a physical host. When None, builds as root.
///
/// Requires:
/// - The temp dir must be on a btrfs filesystem (for real subvolumes)
/// - mkfs.btrfs >= 6.12 (for --rootdir --subvol support)
/// - Kernel 5.18+ (for unprivileged btrfs subvolume creation)
pub(super) async fn build_btrfs_storage_image(
    archive_path: &Path,
    output_path: &Path,
    build_uid: Option<u32>,
) -> Result<()> {
    // Find mkfs.btrfs with --rootdir --subvol support
    let mkfs_btrfs = find_mkfs_btrfs()?;

    // Use the image-cache directory on btrfs for temp storage.
    // This MUST be on a btrfs filesystem so podman creates real btrfs subvolumes.
    let cache_dir = output_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("output_path has no parent directory"))?;

    // Verify cache_dir is on btrfs.
    // Use -T (target) flag to find the filesystem containing the path,
    // not just exact mount points.
    let findmnt = std::process::Command::new("findmnt")
        .args(["-n", "-o", "FSTYPE", "-T", cache_dir.to_str().unwrap()])
        .output()
        .context("checking filesystem type of cache directory")?;
    let fstype = String::from_utf8_lossy(&findmnt.stdout).trim().to_string();
    if fstype != "btrfs" {
        bail!(
            "Image cache directory {} is on {} (need btrfs).\n\
             btrfs mode requires /mnt/fcvm-btrfs to be a btrfs filesystem.",
            cache_dir.display(),
            fstype
        );
    }

    let tmp_dir = cache_dir.join(format!("tmp-btrfs-{}", std::process::id()));
    let tmp_runroot = cache_dir.join(format!("tmp-btrfs-run-{}", std::process::id()));

    // Clean up any stale temp dirs from a previous interrupted run
    if tmp_dir.exists() {
        cleanup_btrfs_tmp(&tmp_dir).await;
    }
    if tmp_runroot.exists() {
        tokio::fs::remove_dir_all(&tmp_runroot).await.ok();
    }
    tokio::fs::create_dir_all(&tmp_dir)
        .await
        .context("creating temp btrfs storage dir")?;
    tokio::fs::create_dir_all(&tmp_runroot)
        .await
        .context("creating temp btrfs runroot dir")?;

    // For user builds: find username and chown temp dirs so rootless podman can use them.
    // Skip runuser if we're already running as the target uid (rootless fcvm, no sudo).
    let build_username = if let Some(uid) = build_uid {
        let current_uid = nix::unistd::getuid().as_raw();
        if current_uid == uid {
            // Already running as the target user — no runuser needed
            info!("Building btrfs image as current user (uid {})", uid);
            None
        } else {
            let user = nix::unistd::User::from_uid(nix::unistd::Uid::from_raw(uid))
                .context("looking up build user")?
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "no user with uid {} on host (needed for rootless btrfs image build)",
                        uid
                    )
                })?;
            let nix_uid = nix::unistd::Uid::from_raw(uid);
            let nix_gid = nix::unistd::Gid::from_raw(user.gid.as_raw());
            nix::unistd::chown(tmp_dir.as_path(), Some(nix_uid), Some(nix_gid))
                .context("chowning temp dir to build user")?;
            nix::unistd::chown(tmp_runroot.as_path(), Some(nix_uid), Some(nix_gid))
                .context("chowning temp runroot to build user")?;
            info!("Building btrfs image as user {} (uid {})", user.name, uid);
            Some(user.name)
        }
    } else {
        None
    };

    // Load the docker archive into a custom podman storage root on btrfs.
    // This creates real btrfs subvolumes for each layer.
    // For user builds, `runuser -u <username>` runs rootless podman, creating
    // storage with correct UID ownership — matching a physical host.
    info!(
        "Loading archive into btrfs podman storage: {} -> {}",
        archive_path.display(),
        tmp_dir.display()
    );

    let mut cmd_args: Vec<String> = Vec::new();
    if let Some(ref username) = build_username {
        cmd_args.extend(
            ["runuser", "-u", username, "--"]
                .iter()
                .map(|s| s.to_string()),
        );
    }
    cmd_args.extend(
        [
            "podman",
            "--root",
            tmp_dir.to_str().unwrap(),
            "--runroot",
            tmp_runroot.to_str().unwrap(),
            "--storage-driver",
            "btrfs",
            "load",
            "-i",
            archive_path.to_str().unwrap(),
        ]
        .iter()
        .map(|s| s.to_string()),
    );

    let load_output = tokio::process::Command::new(&cmd_args[0])
        .args(&cmd_args[1..])
        .output()
        .await
        .context("running podman load into btrfs storage root")?;

    if !load_output.status.success() {
        let stderr = String::from_utf8_lossy(&load_output.stderr);
        cleanup_btrfs_tmp(&tmp_dir).await;
        tokio::fs::remove_dir_all(&tmp_runroot).await.ok();
        bail!("podman load into btrfs storage root failed: {}", stderr);
    }

    let loaded_msg = String::from_utf8_lossy(&load_output.stdout);
    info!("podman load output: {}", loaded_msg.trim());

    // Remove podman database files — they contain hardcoded paths from the temp dir.
    // When the guest mounts the image at a different path, podman would complain about
    // "database static dir does not match". Deleting them forces podman to recreate
    // the database at the correct mount path.
    for db_file in ["db.sql", "libpod", "storage.lock"] {
        let path = tmp_dir.join(db_file);
        if path.is_dir() {
            tokio::fs::remove_dir_all(&path).await.ok();
        } else if path.is_file() {
            tokio::fs::remove_file(&path).await.ok();
        }
    }

    // Enumerate btrfs subvolumes in the temp dir for --subvol flags.
    // Btrfs subvolumes always have inode 256 — this detects them without root.
    // Podman's btrfs driver creates subvolumes at btrfs/subvolumes/<layer-hash>.
    let subvol_args = enumerate_btrfs_subvolumes(&tmp_dir)?;
    info!(
        "Found {} btrfs subvolumes in temp storage",
        subvol_args.len()
    );

    // Create btrfs image from the storage tree using mkfs.btrfs --rootdir --subvol.
    // Each subvolume is passed as --subvol rw:<relative-path> to preserve it as a
    // real subvolume in the output image.
    let tmp_img = PathBuf::from(format!("{}.tmp", output_path.display()));
    info!(
        "Creating btrfs image: {} -> {}",
        tmp_dir.display(),
        tmp_img.display()
    );

    let mut mkfs_args = vec![
        "--rootdir".to_string(),
        tmp_dir.to_str().unwrap().to_string(),
    ];
    for subvol_path in &subvol_args {
        mkfs_args.push("--subvol".to_string());
        mkfs_args.push(format!("rw:{}", subvol_path));
    }
    mkfs_args.push("--shrink".to_string());
    mkfs_args.push(tmp_img.to_str().unwrap().to_string());

    let mkfs_output = tokio::process::Command::new(&mkfs_btrfs)
        .args(&mkfs_args)
        .output()
        .await
        .context("running mkfs.btrfs --rootdir --subvol")?;

    // Clean up temp storage dir (contains btrfs subvolumes, needs special handling)
    cleanup_btrfs_tmp(&tmp_dir).await;
    tokio::fs::remove_dir_all(&tmp_runroot).await.ok();

    if !mkfs_output.status.success() {
        let stderr = String::from_utf8_lossy(&mkfs_output.stderr);
        tokio::fs::remove_file(&tmp_img).await.ok();
        bail!("mkfs.btrfs --rootdir --subvol failed: {}", stderr);
    }

    // Atomic rename to final path
    if let Err(e) = tokio::fs::rename(&tmp_img, output_path).await {
        tokio::fs::remove_file(&tmp_img).await.ok();
        return Err(e).context("renaming btrfs storage image to final path");
    }

    info!("Built btrfs storage image: {}", output_path.display());
    Ok(())
}

/// Enumerate btrfs subvolumes in a directory tree.
///
/// Returns paths relative to `root_dir`. Detects subvolumes by checking for
/// inode 256, which is the canonical inode number for btrfs subvolume root
/// directories. This works without root privileges.
fn enumerate_btrfs_subvolumes(root_dir: &Path) -> Result<Vec<String>> {
    let mut subvolumes = Vec::new();
    enumerate_subvolumes_recursive(root_dir, root_dir, &mut subvolumes)?;
    Ok(subvolumes)
}

fn enumerate_subvolumes_recursive(
    base_dir: &Path,
    current_dir: &Path,
    subvolumes: &mut Vec<String>,
) -> Result<()> {
    let entries = std::fs::read_dir(current_dir)
        .with_context(|| format!("reading directory {}", current_dir.display()))?;

    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }

        // Check if this directory is a btrfs subvolume (inode 256)
        let metadata = std::fs::metadata(&path)?;
        use std::os::unix::fs::MetadataExt;
        if metadata.ino() == 256 {
            // Found a subvolume — record its path relative to the base dir
            if let Ok(rel) = path.strip_prefix(base_dir) {
                subvolumes.push(rel.to_string_lossy().to_string());
            }
            // Don't recurse into subvolumes — mkfs.btrfs handles their contents
            continue;
        }

        // Regular directory — recurse to find nested subvolumes
        enumerate_subvolumes_recursive(base_dir, &path, subvolumes)?;
    }
    Ok(())
}

/// Clean up a temporary btrfs storage directory.
///
/// btrfs subvolumes can't be removed with regular rm -rf.
/// Use `podman --root <dir> system reset --force` to clean up properly,
/// then remove the directory.
async fn cleanup_btrfs_tmp(tmp_dir: &Path) {
    // First try podman system reset to properly clean up subvolumes
    let reset = tokio::process::Command::new("podman")
        .args([
            "--root",
            tmp_dir.to_str().unwrap_or(""),
            "--storage-driver",
            "btrfs",
            "system",
            "reset",
            "--force",
        ])
        .output()
        .await;

    if let Err(e) = &reset {
        warn!("podman system reset for tmp dir failed: {}", e);
    }

    // Then try btrfs subvolume delete on any remaining subvolumes
    if let Ok(entries) = std::fs::read_dir(tmp_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                // Try btrfs subvolume delete (ignores errors on non-subvolumes)
                let _ = tokio::process::Command::new("btrfs")
                    .args(["subvolume", "delete", path.to_str().unwrap_or("")])
                    .output()
                    .await;
            }
        }
    }

    // Finally remove the directory tree
    if let Err(e) = tokio::fs::remove_dir_all(tmp_dir).await {
        warn!("Failed to remove temp dir {}: {}", tmp_dir.display(), e);
    }
}
