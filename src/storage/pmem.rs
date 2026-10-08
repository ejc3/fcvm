//! Read-only host images attached through Firecracker's virtio-pmem device (`--pmem`).
//!
//! Firecracker maps the image MAP_SHARED in its own KVM memory slot outside guest
//! RAM, so with a DAX mount in the guest the file's data never enters guest memory
//! or the memory snapshot, and clones of one snapshot share it through the host
//! page cache. The file Firecracker maps is never the one the user named: `podman run`
//! copies each image into the pmem store (`storage::pmem_store`) and attaches the copy,
//! so a later write to the user's file reaches no VM, snapshot or clone. A snapshot
//! records the copy's path and Firecracker reopens that path on restore; no load-time
//! override exists.

use crate::state::types::PmemDevice;
use crate::storage::pmem_store::{check_entry_owner, is_entry_of, PmemStore, RunIds};
use anyhow::{bail, ensure, Context, Result};
use std::path::{Path, PathBuf};

/// Firecracker maps a pmem file in 2 MiB units. A shorter tail is backed by
/// anonymous memory that is never written back to the file, so fcvm refuses an
/// image whose length is not a multiple of this.
pub const PMEM_ALIGNMENT: u64 = 2 * 1024 * 1024;

/// Parse one `--pmem HOST_IMAGE:GUEST_MOUNT:ro` spec into the image's canonical path and
/// the guest mount path. Only read-only devices are supported: VMs and clones map the
/// same file, and a guest write to a read-only device is discarded on x86_64 and stops
/// the VM on aarch64.
pub fn parse_pmem_spec(spec: &str) -> Result<(PathBuf, String)> {
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
        Path::new(mount_path)
            .components()
            .any(|component| matches!(component, std::path::Component::Normal(_))),
        "--pmem mount path cannot be the guest's root: {spec}"
    );
    ensure!(
        !mount_path.contains(':'),
        "--pmem mount path cannot contain ':': {spec} (got '{mount_path}')"
    );
    ensure!(
        !Path::new(mount_path)
            .components()
            .any(|component| component == std::path::Component::ParentDir),
        "--pmem mount path cannot contain '..': {spec} (got '{mount_path}')"
    );
    let path = Path::new(host)
        .canonicalize()
        .with_context(|| format!("--pmem image not found: {host}"))?;
    Ok((path, mount_path.to_string()))
}

/// Resolve `--pmem` specs into store entries, in device order. A spec naming an image is
/// copied into the store unless that generation is already there. A spec that already
/// names an entry of `store` (a disk-only clone or reboot of a snapshot passes its
/// recorded devices back in) is used as it is, with the source `recorded` gives for it,
/// or the entry itself when nothing recorded it.
pub fn resolve_pmem_devices(
    store: &PmemStore,
    specs: &[String],
    recorded: &[PmemDevice],
) -> Result<Vec<PmemDevice>> {
    specs
        .iter()
        .map(|spec| {
            let (host, mount_path) = parse_pmem_spec(spec)?;
            let (entry, source) = if store.is_entry(&host) {
                let source = recorded
                    .iter()
                    .find(|device| Path::new(&device.path) == host)
                    .map(|device| device.source.clone())
                    .unwrap_or_else(|| host.display().to_string());
                (host, source)
            } else {
                let source = host.display().to_string();
                (store.ingest(&host)?, source)
            };
            let identity = entry_identity(&store.check_entry(&entry, None)?);
            Ok(PmemDevice {
                path: entry.display().to_string(),
                source,
                mount_path,
                identity,
            })
        })
        .collect()
}

/// The identity of an image generation, which names its store entry: inode, length and
/// modification time, then change time, all from one fstat of the descriptor that is
/// copied. An in-place rewrite can keep the first three (`cp -p`, `rsync --inplace -t`,
/// images built with normalized timestamps). write(2), truncate, fallocate and a reflink
/// into the file move the change time, which userspace cannot set back, and so does a
/// write through a writable shared mapping once the file was fsynced (the store fsyncs
/// before it copies). The device number is left out: btrfs and loop-backed stores number
/// a filesystem when it is mounted, so it can differ after a host reboot while the image
/// is unchanged.
pub(crate) fn image_identity(metadata: &std::fs::Metadata) -> String {
    use std::os::unix::fs::MetadataExt;
    format!(
        "{}:{}.{:09}",
        crate::utils::metadata_identity(metadata),
        metadata.ctime(),
        metadata.ctime_nsec()
    )
}

/// The identity a VM and its snapshots record for a store entry: inode, length and
/// modification time. The change time is left out because the store moves it after the
/// entry is visible: publication removes the temp that is a second link to the entry,
/// and a run that reads the entry's identity first would record a change time a restore
/// then finds different. An entry is mode 0400 and never written, so these three are
/// enough to see one that is missing or was replaced. The length stays the second field,
/// where `check_pmem_images` reads it.
fn entry_identity(metadata: &std::fs::Metadata) -> String {
    crate::utils::metadata_identity(metadata)
}

/// Check that `metadata` describes a usable pmem image: a non-empty regular file, 2 MiB
/// aligned.
pub(crate) fn check_image_metadata(metadata: &std::fs::Metadata, path: &Path) -> Result<()> {
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
    Ok(())
}

/// Refuse to restore a snapshot whose pmem devices are not entries of the store under
/// `data_dir`, or whose entries are gone, changed length, or were replaced (inode, length
/// and modification time, `entry_identity`). Firecracker reopens each entry at the
/// recorded path, and the guest's cached view of the filesystem in the memory snapshot
/// describes the entry as it was. An entry Firecracker could not open in this run's VMs is
/// refused too, by the rule publishing uses (`check_entry_owner`). Creates nothing: with no
/// store under `data_dir`, every device is missing.
pub fn check_snapshot_pmem_images(data_dir: &Path, devices: &[PmemDevice]) -> Result<()> {
    const WHAT: &str = "snapshot pmem image changed";
    if devices.is_empty() {
        return Ok(());
    }
    let Some(store_dir) = PmemStore::existing_dir(data_dir)? else {
        let copies: Vec<String> = devices
            .iter()
            .map(|device| {
                format!(
                    "{} (copy of {}, mounted at {})",
                    device.path, device.source, device.mount_path
                )
            })
            .collect();
        bail!(
            "{WHAT}: the pmem image store {} is missing, and with it the store copies {}",
            data_dir.join("pmem").display(),
            copies.join(", ")
        );
    };
    let run = RunIds::of_run(crate::setup::sudo_invoker());
    check_pmem_images(&store_dir, devices, WHAT, "the snapshot was taken", run)
}

/// One stat per entry: the identity and the owner come from the same metadata, so an entry
/// renamed away between two calls cannot fail without the error's first words. Only a
/// missing entry counts as changed; another read error is reported as itself, so a podman
/// cache hit does not delete a snapshot over a transient failure. So is an entry the run
/// `run` cannot use (`check_entry_owner`): a run as its owner can still restore the snapshot.
fn check_pmem_images(
    store_dir: &Path,
    devices: &[PmemDevice],
    what: &str,
    since: &str,
    run: RunIds,
) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    for device in devices {
        // A snapshot maps only store entries, like a boot (`PmemStore::attach_path`).
        ensure!(
            is_entry_of(store_dir, Path::new(&device.path)),
            "{what}: {} (copy of {}, mounted at {}) is not an entry of the pmem image store {}",
            device.path,
            device.source,
            device.mount_path,
            store_dir.display()
        );
        let metadata = match std::fs::metadata(&device.path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => bail!(
                "{what}: the store copy {} of {} (mounted at {}) is missing",
                device.path,
                device.source,
                device.mount_path
            ),
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "reading the store copy {} of pmem image {} (mounted at {})",
                        device.path, device.source, device.mount_path
                    )
                })
            }
        };
        ensure!(
            metadata.is_file(),
            "{what}: the store copy {} of {} (mounted at {}) is not a regular file",
            device.path,
            device.source,
            device.mount_path
        );
        let current = entry_identity(&metadata);
        if current != device.identity {
            // The identity is inode:length:mtime, so the recorded length is its second field.
            let then_len = device.identity.split(':').nth(1).unwrap_or("?");
            ensure!(
                then_len == metadata.len().to_string(),
                "{what}: the store copy {} of {} (mounted at {}) was {then_len} bytes when \
                 {since} and is now {} bytes",
                device.path,
                device.source,
                device.mount_path,
                metadata.len()
            );
            bail!(
                "{what}: the store copy {} of {} (mounted at {}) was replaced since {since} \
                 (inode:length:mtime {} then, {current} now)",
                device.path,
                device.source,
                device.mount_path,
                device.identity
            );
        }
        check_entry_owner(
            Path::new(&device.path),
            (metadata.uid(), metadata.gid()),
            store_dir,
            run,
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::pmem_store::test_image_dir;
    use std::os::unix::fs::PermissionsExt;

    fn image(len: u64) -> tempfile::NamedTempFile {
        let file = tempfile::NamedTempFile::new_in(test_image_dir()).unwrap();
        file.as_file().set_len(len).unwrap();
        file
    }

    /// The store under `data` and the device a spec for `path` resolves to.
    fn resolve_one(data: &Path, path: &Path) -> (PmemStore, PmemDevice) {
        let store = PmemStore::open(data).unwrap();
        let spec = format!("{}:/mnt/cache:ro", path.display());
        let mut devices = resolve_pmem_devices(&store, &[spec], &[]).unwrap();
        (store, devices.remove(0))
    }

    #[test]
    fn pmem_spec_with_ro_is_accepted() {
        let file = image(4 * 1024 * 1024);
        let spec = format!("{}:/mnt/cache:ro", file.path().display());
        let (path, mount_path) = parse_pmem_spec(&spec).unwrap();
        assert_eq!(path, file.path().canonicalize().unwrap());
        assert_eq!(mount_path, "/mnt/cache");
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
        let data = tempfile::tempdir().unwrap();
        let store = PmemStore::open(data.path()).unwrap();
        let spec = format!("{}:/mnt/cache:ro", file.path().display());
        let error = format!(
            "{:#}",
            resolve_pmem_devices(&store, &[spec], &[]).unwrap_err()
        );
        assert!(error.contains("not a multiple of 2 MiB"), "{error}");
    }

    #[test]
    fn pmem_spec_refuses_empty_missing_and_non_file_images() {
        let empty = image(0);
        let dir = tempfile::tempdir_in(test_image_dir()).unwrap();
        let data = tempfile::tempdir().unwrap();
        let store = PmemStore::open(data.path()).unwrap();
        for (host, want) in [
            (empty.path().display().to_string(), "is empty"),
            (dir.path().display().to_string(), "not a regular file"),
            (dir.path().join("absent").display().to_string(), "not found"),
        ] {
            let error = format!(
                "{:#}",
                resolve_pmem_devices(&store, &[format!("{host}:/mnt/cache:ro")], &[]).unwrap_err()
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

    /// A resolved device names its store entry and the image it was copied from.
    #[test]
    fn a_resolved_device_names_its_store_entry_and_its_source() {
        let file = image(4 * 1024 * 1024);
        let data = tempfile::tempdir().unwrap();
        let (store, device) = resolve_one(data.path(), file.path());
        assert!(store.is_entry(Path::new(&device.path)), "{device:?}");
        assert_eq!(
            device.source,
            file.path().canonicalize().unwrap().display().to_string()
        );
        assert_eq!(device.mount_path, "/mnt/cache");
        assert!(device.identity.contains(":4194304:"), "{}", device.identity);
    }

    #[test]
    fn pmem_restore_check_refuses_a_resized_or_missing_entry() {
        let file = image(4 * 1024 * 1024);
        let data = tempfile::tempdir().unwrap();
        let (_store, device) = resolve_one(data.path(), file.path());
        check_snapshot_pmem_images(data.path(), std::slice::from_ref(&device)).unwrap();

        std::fs::set_permissions(&device.path, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::File::options()
            .write(true)
            .open(&device.path)
            .unwrap()
            .set_len(6 * 1024 * 1024)
            .unwrap();
        let error = format!(
            "{:#}",
            check_snapshot_pmem_images(data.path(), std::slice::from_ref(&device)).unwrap_err()
        );
        assert!(error.contains("snapshot pmem image changed"), "{error}");
        assert!(error.contains("4194304 bytes"), "{error}");

        std::fs::remove_file(&device.path).unwrap();
        let error = format!(
            "{:#}",
            check_snapshot_pmem_images(data.path(), std::slice::from_ref(&device)).unwrap_err()
        );
        assert!(error.contains("is missing"), "{error}");
    }

    /// A restore maps only store entries: a recorded path outside the store, or a file in
    /// the store directory that is not named like an entry, is refused even with the
    /// recorded identity, and carries the marker that turns a podman cache hit into a
    /// fresh boot.
    #[test]
    fn pmem_restore_check_refuses_a_path_that_is_not_a_store_entry() {
        let file = image(4 * 1024 * 1024);
        let data = tempfile::tempdir().unwrap();
        let (store, device) = resolve_one(data.path(), file.path());
        let elsewhere = tempfile::tempdir_in(data.path()).unwrap();
        for path in [
            elsewhere
                .path()
                .join(Path::new(&device.path).file_name().unwrap()),
            store.dir().join("cache.img"),
        ] {
            std::fs::hard_link(&device.path, &path).unwrap();
            let mut recorded = device.clone();
            recorded.path = path.display().to_string();
            let error = format!(
                "{:#}",
                check_snapshot_pmem_images(data.path(), std::slice::from_ref(&recorded))
                    .expect_err("a path outside the store was accepted for a restore")
            );
            assert!(error.contains("snapshot pmem image changed"), "{error}");
            assert!(
                error.contains("not an entry of the pmem image store"),
                "{error}"
            );
        }
    }

    /// A store entry replaced by another file of the same length is refused.
    #[test]
    fn pmem_restore_check_refuses_a_replaced_entry() {
        let file = image(4 * 1024 * 1024);
        let data = tempfile::tempdir().unwrap();
        let (_store, device) = resolve_one(data.path(), file.path());
        let other = data.path().join("other");
        std::fs::write(&other, vec![7u8; 4 * 1024 * 1024]).unwrap();
        std::fs::rename(&other, &device.path).unwrap();
        let error = format!(
            "{:#}",
            check_snapshot_pmem_images(data.path(), std::slice::from_ref(&device)).unwrap_err()
        );
        assert!(error.contains("snapshot pmem image changed"), "{error}");
        assert!(error.contains("was replaced"), "{error}");
    }

    /// The restore check stats each entry once. A second stat could see an entry renamed
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

    /// The image a symlink in the typed path points to is the one copied, and the device
    /// names it as its source.
    #[test]
    fn resolving_copies_the_file_a_symlink_points_to() {
        let file = image(2 * 1024 * 1024);
        let dir = tempfile::tempdir().unwrap();
        let link = dir.path().join("current.ext4");
        std::os::unix::fs::symlink(file.path(), &link).unwrap();
        let data = tempfile::tempdir().unwrap();
        let (_store, device) = resolve_one(data.path(), &link);
        assert_eq!(
            device.source,
            file.path().canonicalize().unwrap().display().to_string()
        );
    }

    /// A spec that names a store entry through a data_dir reached by a symlink is used as
    /// it is, without a second copy, and keeps the source recorded for that entry.
    #[test]
    fn a_spec_under_a_symlinked_data_dir_passes_through_without_a_copy() {
        let file = image(2 * 1024 * 1024);
        let real = tempfile::tempdir().unwrap();
        let parent = tempfile::tempdir().unwrap();
        let link = parent.path().join("data");
        std::os::unix::fs::symlink(real.path(), &link).unwrap();
        let (store, device) = resolve_one(&link, file.path());
        let entry = PathBuf::from(&device.path);
        let through_link = link.join("pmem").join(entry.file_name().unwrap());
        let spec = format!("{}:/mnt/cache:ro", through_link.display());

        let passed = resolve_pmem_devices(&store, std::slice::from_ref(&spec), &[]).unwrap();
        assert_eq!(passed[0].path, device.path);
        assert_eq!(passed[0].source, device.path, "nothing recorded a source");
        let recorded =
            resolve_pmem_devices(&store, &[spec], std::slice::from_ref(&device)).unwrap();
        assert_eq!(recorded[0].source, device.source);
        let entries = std::fs::read_dir(store.dir())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| store.is_entry(path))
            .count();
        assert_eq!(entries, 1, "the store entry was copied again");
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
        let (host, mount_path) =
            parse_pmem_spec(&format!("{}:/mnt/cache:ro", path.display())).unwrap();
        assert_eq!(mount_path, "/mnt/cache");
        assert_eq!(host, path.canonicalize().unwrap());
    }

    /// A restore refuses an entry Firecracker could not open in this run's VMs, by the rule
    /// publishing uses: here a run as root without a sudo invoker, as
    /// scripts/root-test-runner.sh starts one, meets an entry an earlier run under sudo
    /// handed to its invoker. The refusal is its own error, not "snapshot pmem image changed",
    /// so a podman cache hit keeps the snapshot for a run that can use it. Root's own entry is
    /// first given to a user, as that earlier run did.
    #[test]
    fn the_restore_check_refuses_an_entry_the_run_cannot_open_without_calling_it_changed() {
        use std::os::unix::fs::MetadataExt;
        let file = image(2 * 1024 * 1024);
        let data = tempfile::tempdir().unwrap();
        let (store, device) = resolve_one(data.path(), file.path());
        if nix::unistd::geteuid().is_root() {
            let (uid, gid) = (
                nix::unistd::Uid::from_raw(3000),
                nix::unistd::Gid::from_raw(3000),
            );
            nix::unistd::chown(device.path.as_str(), Some(uid), Some(gid)).unwrap();
        }
        let metadata = std::fs::metadata(&device.path).unwrap();
        let owner = (metadata.uid(), metadata.gid());
        assert_ne!(
            owner.0, 0,
            "the entry is root's, so this test would not cover another owner"
        );
        let check = |own| {
            check_pmem_images(
                store.dir(),
                std::slice::from_ref(&device),
                "snapshot pmem image changed",
                "the snapshot was taken",
                RunIds { own, invoker: None },
            )
        };
        check(owner).expect("a run as the entry's owner was refused");
        let error = format!(
            "{:#}",
            check((0, 0))
                .expect_err("a run as root without an invoker passed an entry its VMs cannot open")
        );
        assert!(!error.contains("snapshot pmem image changed"), "{error}");
        assert!(error.contains("no sudo invoker"), "{error}");
        assert!(error.contains(&device.path), "{error}");
    }

    /// Only a missing entry is "changed": one that cannot be read for another reason (here
    /// a symlink loop at an entry's name in the store) is an error without the marker, so a
    /// cache hit does not delete a snapshot over a transient failure.
    #[test]
    fn a_pmem_image_that_cannot_be_read_is_not_reported_as_changed() {
        let file = image(2 * 1024 * 1024);
        let data = tempfile::tempdir().unwrap();
        let (store, mut device) = resolve_one(data.path(), file.path());
        let (a, b) = (
            store.dir().join(format!("{}.img", "a".repeat(64))),
            store.dir().join("loop"),
        );
        std::os::unix::fs::symlink(&b, &a).unwrap();
        std::os::unix::fs::symlink(&a, &b).unwrap();
        device.path = a.display().to_string();
        let error = format!(
            "{:#}",
            check_snapshot_pmem_images(data.path(), std::slice::from_ref(&device)).unwrap_err()
        );
        assert!(!error.contains("snapshot pmem image changed"), "{error}");
    }

    /// The restore check creates nothing. With the store directory gone every recorded
    /// copy is reported missing, with the marker that turns a podman cache hit into a
    /// fresh boot.
    #[test]
    fn the_restore_check_creates_no_store() {
        let (first, second) = (image(2 * 1024 * 1024), image(4 * 1024 * 1024));
        let data = tempfile::tempdir().unwrap();
        let store = PmemStore::open(data.path()).unwrap();
        let specs = [first.path(), second.path()].map(|path| {
            format!(
                "{}:/mnt/{}:ro",
                path.display(),
                path.metadata().unwrap().len()
            )
        });
        let devices = resolve_pmem_devices(&store, &specs, &[]).unwrap();
        std::fs::remove_dir_all(store.dir()).unwrap();
        let error = format!(
            "{:#}",
            check_snapshot_pmem_images(data.path(), &devices).unwrap_err()
        );
        assert!(error.contains("snapshot pmem image changed"), "{error}");
        for device in &devices {
            assert!(error.contains(&device.path), "{error}");
        }
        assert!(error.contains("missing"), "{error}");
        assert!(
            !data.path().join("pmem").exists(),
            "the restore check created the store"
        );
    }
}
