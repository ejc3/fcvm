use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use tracing::{info, warn};

/// Build a unique sibling temp path for atomically publishing a content-addressed cache file.
///
/// The image-cache dir is content-addressed and SHARED across VMs: nested tests `--map` the
/// host `/mnt/fcvm-btrfs` into multiple L1 VMs, so two VMs preparing the same image write to
/// the same directory on the host backing store. The per-digest `flock` guarding image prep
/// does NOT coordinate this: it is `flock()` over a fuse-pipe mount that negotiates no
/// `FUSE_FLOCK_LOCKS`, so each guest kernel grants it locally and never forwards it to the
/// host. A shared `"<final>.tmp"` therefore lets two builders mkfs/resize the same file
/// (corruption) and one's rename ENOENT the other's in-flight temp.
///
/// Keyed by a fresh UUID — NOT the pid, which is not unique across VMs (separate PID
/// namespaces reuse the same numbers). The caller atomically renames to `final_path`; because
/// the bytes are content-addressed, a rename race between builders is idempotent (same result).
pub(super) fn unique_cache_tmp(final_path: &Path) -> PathBuf {
    PathBuf::from(format!(
        "{}.{}.tmp",
        final_path.display(),
        uuid::Uuid::new_v4()
    ))
}

/// Build identity of a cached image-delivery artifact: inode, size, and mtime.
///
/// The content-addressed cache path names the IMAGE the artifact was built
/// from, not the build itself — and `podman load` randomizes overlay layer
/// link IDs on every build, so two builds of the same digest are NOT
/// interchangeable once a snapshot has provisioned a container against one of
/// them. Atomic-rename installation (see `unique_cache_tmp`) means a rebuild
/// always produces a new inode, so this triple distinguishes builds cheaply
/// (one stat) without hashing multi-hundred-MB files.
pub(super) fn file_identity(path: &Path) -> anyhow::Result<String> {
    use std::os::unix::fs::MetadataExt;
    let md = std::fs::metadata(path)?;
    Ok(format!(
        "{}:{}:{}.{:09}",
        md.ino(),
        md.size(),
        md.mtime(),
        md.mtime_nsec()
    ))
}

/// Remove podman state files from a storage root, keeping only image/layer data.
///
/// `podman load` creates state files (db.sql, storage.lock, libpod/, etc.) that
/// contain hardcoded paths. When the storage root is mounted at a different path
/// in the guest, these stale files cause "database graph driver does not match".
async fn clean_podman_state(dir: &Path, keep: &[&str]) {
    if let Ok(mut entries) = tokio::fs::read_dir(dir).await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if keep.iter().any(|&k| k == name_str.as_ref()) {
                continue;
            }
            let path = entry.path();
            if path.is_dir() {
                tokio::fs::remove_dir_all(&path).await.ok();
            } else {
                tokio::fs::remove_file(&path).await.ok();
            }
        }
    }
}

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

/// An image's cache key and (for localhost images) its immutable export source.
///
/// `cache_key` is the snapshot/image-cache key: the SHA256 manifest digest for
/// `localhost/` images, or the image reference itself for remote images. `image_id`
/// is the immutable image ID (`sha256:…`) used to export by content; it is `None` for
/// remote images (which are pulled in the guest, not exported).
pub(super) struct ImageCacheRef {
    pub cache_key: String,
    pub image_id: Option<String>,
}

/// Resolve an image's cache key and immutable export id in a SINGLE `podman image
/// inspect`.
///
/// Capturing both fields from one observation is required for correctness (#598):
/// taking the digest and the id from two separate inspects lets a parallel
/// `podman build` repoint the tag in between, so the cache key would name the old
/// manifest while the export id names the new image — caching the wrong content under
/// the old key. One inspect makes them an atomic view of the same image.
///
/// For remote images there is no local id and the reference is used as the key.
pub(super) async fn get_image_cache_ref(image: &str) -> Result<ImageCacheRef> {
    if !image.starts_with("localhost/") {
        // Remote images: the reference is the key; no local id (pulled in the guest).
        return Ok(ImageCacheRef {
            cache_key: image.to_string(),
            image_id: None,
        });
    }

    // `\t` never appears in a digest or an image id, so it is a safe field separator.
    let output = tokio::process::Command::new("podman")
        .args([
            "image",
            "inspect",
            image,
            "--format",
            "{{.Digest}}\t{{.Id}}",
        ])
        .output()
        .await
        .context("running podman inspect")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("Failed to inspect image '{}': {}", image, stderr);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_image_cache_ref(image, &stdout)
}

/// Parse the `{{.Digest}}\t{{.Id}}` output of `podman image inspect` into an
/// [`ImageCacheRef`]. Split out from the command call so the empty-digest fallback can
/// be unit-tested without a live podman.
fn parse_image_cache_ref(image: &str, stdout: &str) -> Result<ImageCacheRef> {
    // Strip ONLY the trailing newline. A locally-built image reports an EMPTY digest,
    // so podman emits "\t<id>" — a leading tab. `str::trim()` would eat that leading tab
    // (a tab is whitespace), collapsing the two fields into one and making `split_once`
    // fail before the empty-digest fallback below could run (#623).
    let line = stdout.trim_end_matches(['\n', '\r']);
    let (digest, id) = line.split_once('\t').ok_or_else(|| {
        anyhow::anyhow!(
            "unexpected `podman image inspect` output for '{}': {:?}",
            image,
            line
        )
    })?;

    let image_id = id.trim().to_string();
    if image_id.is_empty() {
        bail!("podman returned an empty image id for '{}'", image);
    }

    // A locally-built image can report an empty manifest digest. Fall back to the
    // (always-present, immutable) image id so distinct images never collide on an empty
    // cache key — which would conflate their cached archives/snapshots.
    let mut cache_key = digest.trim().trim_start_matches("sha256:").to_string();
    if cache_key.is_empty() {
        cache_key = image_id.trim_start_matches("sha256:").to_string();
    }

    Ok(ImageCacheRef {
        cache_key,
        image_id: Some(image_id),
    })
}

/// Export a localhost image's content to a docker-archive at `dest`, by immutable
/// image ID, tagged as `repo_tag`.
///
/// `skopeo copy containers-storage:<image_id>` reads the exact image content named by
/// the ID — immune to a concurrent `podman build` repointing the tag (#598) — while the
/// `docker-archive:<dest>:<repo_tag>` destination records `repo_tag` in the archive's
/// RepoTags so the guest can `podman load` and run it by name. This is non-destructive:
/// unlike `podman tag <id> <repo_tag>`, it never mutates the caller's live image store.
///
/// `repo_tag` may contain a `:` (e.g. `localhost/foo:latest`); skopeo treats everything
/// after the archive path as the reference, so the colon is preserved.
pub async fn export_image_archive(image_id: &str, repo_tag: &str, dest: &Path) -> Result<()> {
    let dest_str = dest
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("archive path is not valid UTF-8: {}", dest.display()))?;

    let output = tokio::process::Command::new("skopeo")
        .args([
            "copy",
            &format!("containers-storage:{}", image_id),
            &format!("docker-archive:{}:{}", dest_str, repo_tag),
        ])
        .output()
        .await
        .context("running skopeo copy")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "skopeo copy failed for image '{}' (id {}): {}",
            repo_tag,
            image_id,
            stderr
        );
    }

    // docker-archive format requires manifest.json; guard against a malformed archive.
    if !validate_docker_archive(dest)? {
        bail!(
            "skopeo copy produced an invalid archive (missing manifest.json) for image '{}'",
            repo_tag
        );
    }

    Ok(())
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

    // Temp paths are UUID-keyed because the image-cache dir is shared across VMs and the
    // per-digest flock does not coordinate cross-VM (see `unique_cache_tmp`). A shared
    // "tmp-storage-{pid}" would collide too — separate PID namespaces reuse pid numbers.
    let tmp_dir = cache_dir.join(format!("tmp-storage-{}", uuid::Uuid::new_v4()));

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

    // Remove podman state files that contain hardcoded paths from the temp dir.
    // Keep only image/layer data directories. When the guest mounts this read-only
    // at a different path, stale state files cause "database graph driver does not match".
    clean_podman_state(&tmp_dir, &["overlay", "overlay-images", "overlay-layers"]).await;

    // Package the storage tree as an ext4 image, building into a UUID-keyed temp and
    // atomically renaming (see `unique_cache_tmp` — a shared "<digest>.tmp" lets two VMs
    // mkfs/resize2fs the same file and one's rename ENOENTs the other's in-flight temp).
    let tmp_img = unique_cache_tmp(output_path);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_digest_falls_back_to_image_id() {
        // Locally-built image: podman reports an empty .Digest, so the line begins with
        // a tab. The cache key must fall back to the image id (regression test for #623,
        // where `stdout.trim()` ate the leading tab and the fallback was dead code).
        let out = "\tsha256:abc123def456\n";
        let r = parse_image_cache_ref("localhost/foo", out).expect("must parse empty digest");
        assert_eq!(r.cache_key, "abc123def456");
        assert_eq!(r.image_id.as_deref(), Some("sha256:abc123def456"));
    }

    #[test]
    fn present_digest_used_as_cache_key() {
        let out = "sha256:digest9999\tsha256:id0000\n";
        let r = parse_image_cache_ref("localhost/bar", out).expect("must parse");
        assert_eq!(r.cache_key, "digest9999");
        assert_eq!(r.image_id.as_deref(), Some("sha256:id0000"));
    }

    #[test]
    fn empty_image_id_is_an_error() {
        // An empty id (both fields blank) is unusable and must error, not silently key
        // the cache on "".
        assert!(parse_image_cache_ref("localhost/baz", "\t\n").is_err());
    }

    #[test]
    fn missing_separator_is_an_error() {
        assert!(parse_image_cache_ref("localhost/qux", "no-tab-here\n").is_err());
    }

    #[test]
    fn cache_tmp_is_unique_per_call() {
        // Regression for the cross-VM image-cache race (PR #677): two builders preparing the
        // same content-addressed file must NOT share a temp name. A fixed "<final>.tmp" let
        // concurrent VMs (sharing the host image-cache via --map, where flock does not
        // coordinate) corrupt each other's image and ENOENT each other's rename. The temp
        // MUST be unique per call.
        let final_path = Path::new("/mnt/fcvm-btrfs/image-cache/abc123.storage-v2.img");
        let a = unique_cache_tmp(final_path);
        let b = unique_cache_tmp(final_path);
        assert_ne!(a, b, "concurrent builders must get distinct temp paths");

        // It must remain a sibling of the final file (same dir → rename is an atomic
        // intra-filesystem replace, not a cross-device copy) and carry the final name.
        assert_eq!(a.parent(), final_path.parent());
        let a_str = a.to_str().unwrap();
        assert!(a_str.contains("abc123.storage-v2.img"), "got {a_str}");
        assert!(a_str.ends_with(".tmp"), "got {a_str}");
    }
}
