//! Read-only host images attached through Firecracker's virtio-pmem device (`--pmem`).
//!
//! Firecracker maps the image MAP_SHARED in its own KVM memory slot outside guest
//! RAM, so with a DAX mount in the guest the file's data never enters guest memory
//! or the memory snapshot, and clones of one snapshot share it through the host
//! page cache. A snapshot records the device's host path and Firecracker reopens
//! that path on restore; no load-time override exists.

use crate::state::types::PmemDevice;
use anyhow::{bail, ensure, Context, Result};

/// Firecracker maps a pmem file in 2 MiB units. A shorter tail is backed by
/// anonymous memory that is never written back to the file, so fcvm refuses an
/// image whose length is not a multiple of this.
pub const PMEM_ALIGNMENT: u64 = 2 * 1024 * 1024;

/// Parse and validate one `--pmem HOST_IMAGE:GUEST_MOUNT:ro` spec. The image must
/// be a non-empty regular file whose length is a multiple of 2 MiB. Only read-only
/// devices are supported: VMs and clones map the same file, and a guest write to a
/// read-only device is discarded on x86_64 and stops the VM on aarch64.
pub fn parse_pmem_spec(spec: &str) -> Result<PmemDevice> {
    let Some(without_ro) = spec.strip_suffix(":ro") else {
        bail!(
            "--pmem '{spec}' must end with ':ro': pmem images are read-only \
             (expected HOST_IMAGE:GUEST_MOUNT:ro)"
        );
    };
    // The image path can hold ':' and the mount path cannot, so split at the last one.
    let Some((host, mount_path)) = without_ro.rsplit_once(':') else {
        bail!("invalid --pmem spec '{spec}': expected HOST_IMAGE:GUEST_MOUNT:ro");
    };
    ensure!(
        !host.is_empty(),
        "invalid --pmem spec '{spec}': empty host image path"
    );
    ensure!(
        mount_path.starts_with('/'),
        "--pmem mount path must be absolute: {spec} (got '{mount_path}')"
    );
    ensure!(
        std::path::Path::new(mount_path)
            .components()
            .any(|component| matches!(component, std::path::Component::Normal(_))),
        "--pmem mount path cannot be the guest's root: {spec}"
    );
    ensure!(
        !mount_path.contains(':'),
        "--pmem mount path cannot contain ':': {spec} (got '{mount_path}')"
    );
    ensure!(
        !std::path::Path::new(mount_path)
            .components()
            .any(|component| component == std::path::Component::ParentDir),
        "--pmem mount path cannot contain '..': {spec} (got '{mount_path}')"
    );
    let path = std::path::Path::new(host)
        .canonicalize()
        .with_context(|| format!("--pmem image not found: {host}"))?;
    let metadata = image_metadata(&path)?;
    let identity = image_identity(&metadata);
    Ok(PmemDevice {
        path: path.display().to_string(),
        mount_path: mount_path.to_string(),
        identity,
    })
}

/// The identity a snapshot records for a pmem image: inode, length and modification time,
/// then change time. An in-place rewrite can keep the first three (`cp -p`,
/// `rsync --inplace -t`, images built with normalized timestamps). write(2), truncate,
/// fallocate and a reflink into the file move the change time, which userspace cannot set
/// back; a write through a writable shared mapping may not (on tmpfs never after a read
/// fault), and no stat-based identity can see it. The device number is left out: btrfs and
/// loop-backed stores number a filesystem when it is mounted, so it can differ after a host
/// reboot while the image is unchanged. The length stays the second field, where
/// `check_pmem_images` reads it.
fn image_identity(metadata: &std::fs::Metadata) -> String {
    use std::os::unix::fs::MetadataExt;
    format!(
        "{}:{}.{:09}",
        crate::utils::metadata_identity(metadata),
        metadata.ctime(),
        metadata.ctime_nsec()
    )
}

/// Metadata of a usable pmem image: a non-empty regular file, 2 MiB aligned.
fn image_metadata(path: &std::path::Path) -> Result<std::fs::Metadata> {
    let metadata = std::fs::metadata(path)
        .with_context(|| format!("reading --pmem image {}", path.display()))?;
    ensure!(
        metadata.is_file(),
        "--pmem image {} is not a regular file",
        path.display()
    );
    let len = metadata.len();
    ensure!(len > 0, "--pmem image {} is empty", path.display());
    ensure!(
        len.is_multiple_of(PMEM_ALIGNMENT),
        "--pmem image {} is {len} bytes, not a multiple of 2 MiB; pad it, for example \
         with truncate -s <size rounded up to 2 MiB>",
        path.display()
    );
    Ok(metadata)
}

/// The `--pmem` specs with each image resolved once: its canonical path and the mount
/// point. `podman run` replaces its specs with these before anything reads them, so the
/// snapshot key and the attach name the same file even if a symlink in a typed path is
/// repointed while the run starts.
pub fn resolve_pmem_specs(specs: &[String]) -> Result<Vec<String>> {
    specs
        .iter()
        .map(|spec| parse_pmem_spec(spec).map(|device| device.spec()))
        .collect()
}

/// The `--pmem` specs a snapshot key names: each image's canonical path and mount
/// point, so a relative path, a symlink and its target name one image, and a symlink
/// repointed to another image names that one. A spec that does not parse keeps its
/// text; `podman run` refuses such a spec before it looks up the cache, so that key is
/// never used.
pub fn pmem_key_specs(specs: &[String]) -> Vec<String> {
    specs
        .iter()
        .map(|spec| {
            parse_pmem_spec(spec)
                .map(|device| device.spec())
                .unwrap_or_else(|_| spec.clone())
        })
        .collect()
}

/// Refuse to restore a snapshot whose pmem images are gone, changed length, or were
/// rewritten in place (inode, length, modification time and change time, `image_identity`).
/// Firecracker reopens each image at the recorded path, and the guest's cached
/// view of the filesystem in the memory snapshot describes the image as it was.
pub fn check_snapshot_pmem_images(devices: &[PmemDevice]) -> Result<()> {
    check_pmem_images(
        devices,
        "snapshot pmem image changed",
        "the snapshot was taken",
    )
}

/// Refuse a VM whose pmem images changed since they were attached, after the instance
/// starts, which is when Firecracker maps them.
pub fn check_attached_pmem_images(devices: &[PmemDevice]) -> Result<()> {
    check_pmem_images(devices, "pmem image changed", "it was attached")
}

/// One stat per image: the identity comes from the same metadata, so an image renamed away
/// between two calls cannot fail without the error's first words. Only a missing image
/// counts as changed; another read error is reported as itself, so a podman cache hit does
/// not delete a snapshot over a transient failure.
fn check_pmem_images(devices: &[PmemDevice], what: &str, since: &str) -> Result<()> {
    for device in devices {
        let metadata = match std::fs::metadata(&device.path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => bail!(
                "{what}: {} (mounted at {}) is missing",
                device.path,
                device.mount_path
            ),
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "reading pmem image {} (mounted at {})",
                        device.path, device.mount_path
                    )
                })
            }
        };
        ensure!(
            metadata.is_file(),
            "{what}: {} (mounted at {}) is not a regular file",
            device.path,
            device.mount_path
        );
        let current = image_identity(&metadata);
        if current == device.identity {
            continue;
        }
        // The identity is inode:length:mtime:ctime, so the recorded length is its second field.
        let then_len = device.identity.split(':').nth(1).unwrap_or("?");
        ensure!(
            then_len == metadata.len().to_string(),
            "{what}: {} (mounted at {}) was {then_len} bytes when {since} and is now {} bytes",
            device.path,
            device.mount_path,
            metadata.len()
        );
        bail!(
            "{what}: {} (mounted at {}) was rewritten since {since} \
             (inode:length:mtime:ctime {} then, {current} now)",
            device.path,
            device.mount_path,
            device.identity
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(len: u64) -> tempfile::NamedTempFile {
        let file = tempfile::NamedTempFile::new().unwrap();
        file.as_file().set_len(len).unwrap();
        file
    }

    #[test]
    fn pmem_spec_with_ro_is_accepted() {
        let file = image(4 * 1024 * 1024);
        let spec = format!("{}:/mnt/cache:ro", file.path().display());
        let device = parse_pmem_spec(&spec).unwrap();
        assert_eq!(
            std::path::Path::new(&device.path),
            file.path().canonicalize().unwrap()
        );
        assert_eq!(device.mount_path, "/mnt/cache");
        assert!(device.identity.contains(":4194304:"), "{}", device.identity);
    }

    #[test]
    fn pmem_spec_without_ro_is_refused() {
        let file = image(4 * 1024 * 1024);
        let spec = format!("{}:/mnt/cache", file.path().display());
        let error = format!("{:#}", parse_pmem_spec(&spec).unwrap_err());
        assert!(error.contains("must end with ':ro'"), "{error}");
    }

    #[test]
    fn pmem_image_not_a_multiple_of_2_mib_is_refused() {
        let file = image(3 * 1024 * 1024);
        let spec = format!("{}:/mnt/cache:ro", file.path().display());
        let error = format!("{:#}", parse_pmem_spec(&spec).unwrap_err());
        assert!(error.contains("not a multiple of 2 MiB"), "{error}");
    }

    #[test]
    fn pmem_spec_refuses_empty_missing_and_non_file_images() {
        let empty = image(0);
        let dir = tempfile::tempdir().unwrap();
        for (host, want) in [
            (empty.path().display().to_string(), "is empty"),
            (dir.path().display().to_string(), "not a regular file"),
            (dir.path().join("absent").display().to_string(), "not found"),
        ] {
            let error = format!(
                "{:#}",
                parse_pmem_spec(&format!("{host}:/mnt/cache:ro")).unwrap_err()
            );
            assert!(error.contains(want), "{host}: {error}");
        }
        let file = image(2 * 1024 * 1024);
        let error = format!(
            "{:#}",
            parse_pmem_spec(&format!("{}:cache:ro", file.path().display())).unwrap_err()
        );
        assert!(error.contains("must be absolute"), "{error}");
    }

    #[test]
    fn pmem_restore_check_refuses_a_resized_or_missing_image() {
        let file = image(4 * 1024 * 1024);
        let device = parse_pmem_spec(&format!("{}:/mnt/cache:ro", file.path().display())).unwrap();
        check_snapshot_pmem_images(std::slice::from_ref(&device)).unwrap();

        file.as_file().set_len(6 * 1024 * 1024).unwrap();
        let error = format!(
            "{:#}",
            check_snapshot_pmem_images(std::slice::from_ref(&device)).unwrap_err()
        );
        assert!(error.contains("snapshot pmem image changed"), "{error}");
        assert!(error.contains("4194304 bytes"), "{error}");

        let path = file.path().to_path_buf();
        drop(file);
        assert!(!path.exists());
        let error = format!(
            "{:#}",
            check_snapshot_pmem_images(std::slice::from_ref(&device)).unwrap_err()
        );
        assert!(error.contains("is missing"), "{error}");
    }

    /// A rewrite in place that keeps the length and puts the old modification time back
    /// (`cp -p`, `rsync --inplace -t`) still changes the identity: their write(2) moves the
    /// change time.
    #[test]
    fn pmem_restore_check_refuses_a_rewrite_that_kept_its_mtime() {
        let file = image(4 * 1024 * 1024);
        let path = file.path().to_str().unwrap().to_string();
        let device = parse_pmem_spec(&format!("{path}:/mnt/cache:ro")).unwrap();
        check_snapshot_pmem_images(std::slice::from_ref(&device))
            .expect("the untouched image was refused");
        let mtime = std::fs::metadata(file.path()).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(file.path(), vec![7u8; 4 * 1024 * 1024]).unwrap();
        std::fs::File::options()
            .write(true)
            .open(file.path())
            .unwrap()
            .set_modified(mtime)
            .unwrap();
        assert_eq!(
            std::fs::metadata(file.path()).unwrap().modified().unwrap(),
            mtime,
            "the test did not put the old modification time back"
        );
        let error = format!(
            "{:#}",
            check_snapshot_pmem_images(std::slice::from_ref(&device))
                .expect_err("a rewrite that kept its mtime was accepted")
        );
        assert!(error.contains("snapshot pmem image changed"), "{error}");
        assert!(
            error.contains("inode:length:mtime:ctime"),
            "the refusal does not name the change time, the only field that moved: {error}"
        );
    }

    /// Putting the original file back at the path after a swap moves its change time, so a
    /// publisher that swaps the image away during a load and back before the check after it
    /// is caught: two `renameat2(RENAME_EXCHANGE)` calls keep the inode, length and
    /// modification time.
    #[test]
    fn pmem_restore_check_refuses_an_image_swapped_away_and_back() {
        use std::os::unix::ffi::OsStrExt;
        let file = image(4 * 1024 * 1024);
        let path = file.path().to_path_buf();
        let device = parse_pmem_spec(&format!("{}:/mnt/cache:ro", path.display())).unwrap();
        check_snapshot_pmem_images(std::slice::from_ref(&device))
            .expect("the untouched image was refused");
        let other = path.with_extension("other");
        std::fs::write(&other, vec![9u8; 4 * 1024 * 1024]).unwrap();
        let a = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        let b = std::ffi::CString::new(other.as_os_str().as_bytes()).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        for _ in 0..2 {
            // SAFETY: both are valid NUL-terminated paths that outlive the call.
            let rc = unsafe {
                libc::renameat2(
                    libc::AT_FDCWD,
                    a.as_ptr(),
                    libc::AT_FDCWD,
                    b.as_ptr(),
                    libc::RENAME_EXCHANGE as _,
                )
            };
            assert_eq!(rc, 0, "renameat2: {}", std::io::Error::last_os_error());
        }
        let error = format!(
            "{:#}",
            check_snapshot_pmem_images(std::slice::from_ref(&device))
                .expect_err("an image swapped away and back was accepted")
        );
        assert!(error.contains("snapshot pmem image changed"), "{error}");
        std::fs::remove_file(&other).ok();
    }

    /// A restore refuses an image rewritten in place at the same length: the guest's
    /// cached view of the filesystem in the memory image describes the old contents.
    #[test]
    fn pmem_restore_check_refuses_an_image_rewritten_at_the_same_length() {
        let file = image(4 * 1024 * 1024);
        let path = file.path().to_str().unwrap().to_string();
        let device = parse_pmem_spec(&format!("{path}:/mnt/cache:ro")).unwrap();
        check_snapshot_pmem_images(std::slice::from_ref(&device)).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(file.path(), vec![7u8; 4 * 1024 * 1024]).unwrap();
        let error = format!(
            "{:#}",
            check_snapshot_pmem_images(std::slice::from_ref(&device)).unwrap_err()
        );
        assert!(error.contains("snapshot pmem image changed"), "{error}");
    }

    /// The restore check stats each image once. A second stat could see an image renamed
    /// away in between and fail without the "snapshot pmem image changed" marker, which
    /// is what turns a podman cache hit into a fresh boot.
    #[test]
    fn the_restore_check_stats_each_image_once() {
        let source = include_str!("pmem.rs");
        let start = source
            .find("fn check_pmem_images(")
            .expect("check_pmem_images is gone");
        let body = &source[start..start + source[start..].find("\n}\n").unwrap()];
        assert_eq!(
            body.matches("metadata(").count(),
            1,
            "the restore check stats an image more than once"
        );
        assert!(
            !body.contains("file_identity("),
            "the restore check stats the image again for its identity"
        );
    }

    /// A pmem image cannot be mounted over the guest's root, and a ':' in the mount path
    /// would split the container's `-v` argument in the wrong place.
    #[test]
    fn pmem_spec_refuses_the_root_and_colons_in_the_mount_path() {
        let file = image(2 * 1024 * 1024);
        let path = file.path().display().to_string();
        for spec in [format!("{path}:/:ro"), format!("{path}:/mnt/a:b:ro")] {
            assert!(parse_pmem_spec(&spec).is_err(), "{spec} was accepted");
        }
    }

    /// `podman run` resolves each spec once; the resolved spec names the file a symlink in
    /// the typed path points to.
    #[test]
    fn resolve_pmem_specs_names_the_file_a_symlink_points_to() {
        let file = image(2 * 1024 * 1024);
        let dir = tempfile::tempdir().unwrap();
        let link = dir.path().join("current.ext4");
        std::os::unix::fs::symlink(file.path(), &link).unwrap();
        let specs = resolve_pmem_specs(&[format!("{}:/mnt/cache:ro", link.display())]).unwrap();
        let canonical = file.path().canonicalize().unwrap();
        assert_eq!(
            specs,
            vec![format!("{}:/mnt/cache:ro", canonical.display())]
        );
    }

    /// A '..' in the mount path would make the guest path differ from the one the host
    /// checks against other mount points and the guest checks against /proc/mounts.
    #[test]
    fn pmem_spec_refuses_dot_dot_in_the_mount_path() {
        let file = image(2 * 1024 * 1024);
        let spec = format!("{}:/mnt/x/../cache:ro", file.path().display());
        assert!(parse_pmem_spec(&spec).is_err(), "{spec} was accepted");
    }

    /// An image path may hold ':' (the mount path cannot), so the spec splits at the last.
    #[test]
    fn pmem_spec_takes_an_image_path_with_a_colon() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("2026-10-07T12:00");
        std::fs::create_dir(&sub).unwrap();
        let path = sub.join("cache.ext4");
        std::fs::File::create(&path)
            .unwrap()
            .set_len(2 * 1024 * 1024)
            .unwrap();
        let device = parse_pmem_spec(&format!("{}:/mnt/cache:ro", path.display())).unwrap();
        assert_eq!(device.mount_path, "/mnt/cache");
        assert_eq!(
            device.path,
            path.canonicalize().unwrap().display().to_string()
        );
    }

    /// Only a missing image is "changed": one that cannot be read for another reason (here
    /// a symlink loop) is an error without the marker, so a cache hit does not delete a
    /// snapshot over a transient failure.
    #[test]
    fn a_pmem_image_that_cannot_be_read_is_not_reported_as_changed() {
        let file = image(2 * 1024 * 1024);
        let mut device =
            parse_pmem_spec(&format!("{}:/mnt/cache:ro", file.path().display())).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (dir.path().join("a"), dir.path().join("b"));
        std::os::unix::fs::symlink(&b, &a).unwrap();
        std::os::unix::fs::symlink(&a, &b).unwrap();
        device.path = a.display().to_string();
        let error = format!(
            "{:#}",
            check_snapshot_pmem_images(std::slice::from_ref(&device)).unwrap_err()
        );
        assert!(!error.contains("snapshot pmem image changed"), "{error}");
    }
}
