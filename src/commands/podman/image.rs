use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use tracing::{info, warn};

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
