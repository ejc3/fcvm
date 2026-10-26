use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;
use tracing::info;

use crate::setup::rootfs::load_config;

/// Required asset subdirectories under the btrfs mount.
/// Data dirs (state, snapshots, vm-disks) are created lazily by the code
/// that uses them, allowing FCVM_DATA_DIR to point elsewhere.
const REQUIRED_DIRS: &[&str] = &["kernels", "rootfs", "initrd", "cache", "image-cache"];

/// Hand the assets directory and each asset store under it to `invoker`.
///
/// `sudo fcvm setup` creates these as root on the operator's behalf, and a
/// later rootless run must be able to write them. Non-recursive: the stores'
/// contents are handed back by the code that writes them.
fn hand_back_asset_dirs(mount_point: &Path, invoker: Option<&nix::unistd::User>) {
    super::give_store_entry_to(mount_point, invoker);
    for dir in REQUIRED_DIRS {
        super::give_store_entry_to(&mount_point.join(dir), invoker);
    }
}

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

/// The filesystem family that holds a store directory, as far as setup cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StoreFs {
    Btrfs,
    /// A FUSE mount, which is what a nested guest sees of the host store
    /// (`--map`, served by fuse-pipe). `statfs` reports the FUSE magic whatever
    /// backs it, so the backing store is told apart by what it can do.
    Fuse,
    Other,
}

fn store_fs(path: &Path) -> StoreFs {
    match nix::sys::statfs::statfs(path).map(|fs| fs.filesystem_type()) {
        Ok(nix::sys::statfs::BTRFS_SUPER_MAGIC) => StoreFs::Btrfs,
        Ok(nix::sys::statfs::FUSE_SUPER_MAGIC) => StoreFs::Fuse,
        _ => StoreFs::Other,
    }
}

/// `ioctl(FICLONE)`: make the destination share the source's extents.
const FICLONE: libc::c_ulong = 0x4004_9409;

/// Clone a small file inside `dir` and report whether the filesystem did it.
///
/// Both files get a unique name (a uuid, not a pid, because VMs sharing the
/// store have separate pid namespaces), so two guests probing one shared store
/// never touch each other's files. Both are removed before returning.
fn reflink_probe(dir: &Path) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    let id = uuid::Uuid::new_v4();
    let src = dir.join(format!(".fcvm-reflink-probe.{id}.src"));
    let dst = dir.join(format!(".fcvm-reflink-probe.{id}.dst"));
    let result = (|| {
        std::fs::write(&src, [0u8; 4096])?;
        let src_file = std::fs::File::open(&src)?;
        let dst_file = std::fs::File::create(&dst)?;
        // SAFETY: both descriptors are open for the duration of the call.
        let rc = unsafe { libc::ioctl(dst_file.as_raw_fd(), FICLONE as _, src_file.as_raw_fd()) };
        if rc == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    })();
    let _ = std::fs::remove_file(&src);
    let _ = std::fs::remove_file(&dst);
    result
}

/// Decide whether a directory on `fs` can hold the store.
///
/// `Ok(true)`: use it as is. `Ok(false)`: not a store filesystem; the caller
/// goes on to create or mount btrfs. `Err`: a FUSE mount that cannot clone
/// files, which must not be mounted over or treated as absent.
///
/// A FUSE mount is accepted when reflinks work through it. The store needs
/// reflinks (CoW disks) and unique-temp-plus-rename writes (which the store
/// builders already use, because `flock` does not cross fuse-pipe). It does not
/// need the guest to see btrfs: the host side of the map does the cloning, and
/// a clone that works proves it is btrfs there.
fn judge_store_fs(
    fs: StoreFs,
    dir: &Path,
    probe: impl FnOnce(&Path) -> std::io::Result<()>,
) -> Result<bool> {
    match fs {
        StoreFs::Btrfs => Ok(true),
        StoreFs::Other => Ok(false),
        StoreFs::Fuse => match probe(dir) {
            Ok(()) => Ok(true),
            Err(err) => anyhow::bail!(
                "{} is a FUSE mount that cannot clone files ({err}), so it cannot hold the \
                 fcvm store. A nested VM shares the host's store by mapping a btrfs \
                 directory of the host, e.g. `--map /mnt/fcvm-btrfs:/mnt/fcvm-btrfs`, and \
                 needs a guest kernel with FUSE_REMAP_FILE_RANGE (`--kernel-profile nested`). \
                 Check that the host directory is on btrfs, that the guest kernel is the \
                 nested profile, and that the directory is writable by this user.",
                dir.display()
            ),
        },
    }
}

/// [`judge_store_fs`] for a real directory.
fn store_dir_usable(path: &Path) -> Result<bool> {
    judge_store_fs(store_fs(path), path, reflink_probe)
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
    ensure_storage_with(config_path, super::sudo_invoker())
}

/// [`ensure_storage`], handing what it creates to `invoker` rather than to the
/// sudo invoker it would look up.
fn ensure_storage_with(
    config_path: Option<&str>,
    invoker: Option<&nix::unistd::User>,
) -> Result<()> {
    let (mount_point, loopback_image, btrfs_size) = get_storage_paths(config_path)?;

    // Already btrfs? Just ensure directories exist (no root needed)
    if store_dir_usable(&mount_point)? {
        for dir in REQUIRED_DIRS {
            let path = mount_point.join(dir);
            std::fs::create_dir_all(&path).with_context(|| {
                format!(
                    "creating directory {} (if mount was unmounted, run 'sudo fcvm setup' again)",
                    path.display()
                )
            })?;
        }
        hand_back_asset_dirs(&mount_point, invoker);
        return Ok(());
    }

    // Mount point doesn't exist (or isn't btrfs) — check if parent is already btrfs.
    // If so, just create the directory directly. A loopback mount on a btrfs host
    // causes podman namespace divergence (podman's pause process gets its own mount
    // namespace, and file operations in the host namespace become invisible to podman).
    if let Some(parent) = mount_point.parent() {
        if parent.exists() && store_dir_usable(parent)? {
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
            hand_back_asset_dirs(&mount_point, invoker);
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

    // Hand the mount point and its asset stores back to the user who invoked
    // sudo. The shared helper resolves the real primary group from passwd
    // instead of assuming group == user name.
    hand_back_asset_dirs(&mount_point, invoker);

    info!(
        "✓ btrfs storage ready at {} ({})",
        mount_point.display(),
        btrfs_size
    );

    Ok(())
}

#[cfg(test)]
mod store_fs_tests {
    use super::*;
    use std::io;

    fn no_probe(_: &Path) -> io::Result<()> {
        panic!("only a FUSE mount is probed");
    }

    /// A nested guest sees the host store as a FUSE mount. Setup refused it
    /// ("mounted but not btrfs"), so nested setup needed a `stat` wrapper that
    /// reported btrfs. A FUSE mount on which reflinks work is the host's btrfs
    /// and is the store.
    #[test]
    fn a_fuse_mount_that_clones_files_is_the_store() {
        let usable = judge_store_fs(StoreFs::Fuse, Path::new("/m"), |_| Ok(())).unwrap();
        assert!(usable);
    }

    /// A FUSE mount that cannot clone is not a store, and it is an error, not
    /// "absent": falling through would mount a loopback over it. The message
    /// says how to set up a nested shared store.
    #[test]
    fn a_fuse_mount_that_cannot_clone_files_names_how_to_share_a_store() {
        let err = judge_store_fs(StoreFs::Fuse, Path::new("/m"), |_| {
            Err(io::Error::from_raw_os_error(libc::EOPNOTSUPP))
        })
        .unwrap_err()
        .to_string();
        for needle in ["/m", "--map", "FUSE_REMAP_FILE_RANGE", "btrfs"] {
            assert!(err.contains(needle), "{needle} missing from: {err}");
        }
    }

    #[test]
    fn btrfs_is_the_store_and_other_filesystems_are_not_probed() {
        assert!(judge_store_fs(StoreFs::Btrfs, Path::new("/m"), no_probe).unwrap());
        assert!(!judge_store_fs(StoreFs::Other, Path::new("/m"), no_probe).unwrap());
    }

    /// The probe leaves nothing behind, success or failure. tmpfs and ext4
    /// refuse FICLONE; btrfs accepts it. Either way the directory is empty.
    #[test]
    fn the_reflink_probe_removes_its_files() {
        let dir = tempfile::tempdir().unwrap();
        let _ = reflink_probe(dir.path());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn the_reflink_probe_reports_an_unwritable_directory() {
        assert!(reflink_probe(Path::new("/nonexistent-probe-dir")).is_err());
    }
}

#[cfg(all(test, feature = "privileged-tests"))]
mod root_handback_tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    /// `sudo fcvm setup` creates the assets directory and its stores as root, and
    /// they must end up owned by the user who ran sudo. Left root-owned, the next
    /// rootless `make setup-fcvm` failed creating /mnt/fcvm-btrfs/tmp on a
    /// devserver. The second call covers a directory an earlier root run left
    /// root-owned, which is the state that devserver was in.
    #[test]
    fn a_root_setup_hands_the_asset_directories_to_the_sudo_user() {
        assert!(
            nix::unistd::Uid::effective().is_root(),
            "this test needs root: run it with make test-root"
        );
        let nobody = nix::unistd::User::from_name("nobody")
            .expect("passwd lookup")
            .expect("user nobody exists");
        let parent = tempfile::tempdir_in("/mnt/fcvm-btrfs").expect("temp dir on the btrfs store");
        let assets = parent.path().join("assets");
        let config = parent.path().join("rootfs-config.toml");
        let repo_config = include_str!("../../rootfs-config.toml");
        let line = "assets_dir = \"/mnt/fcvm-btrfs\"";
        assert_eq!(
            repo_config.matches(line).count(),
            1,
            "rootfs-config.toml no longer sets {line}"
        );
        std::fs::write(
            &config,
            repo_config.replace(line, &format!("assets_dir = \"{}\"", assets.display())),
        )
        .unwrap();
        let config = config.to_str().unwrap();
        let dirs: Vec<PathBuf> = std::iter::once(assets.clone())
            .chain(REQUIRED_DIRS.iter().map(|dir| assets.join(dir)))
            .collect();
        let assert_owned = |when: &str| {
            for dir in &dirs {
                let uid = std::fs::metadata(dir).unwrap().uid();
                assert_eq!(
                    uid,
                    nobody.uid.as_raw(),
                    "{when}: {} is owned by uid {uid}, not the sudo user",
                    dir.display()
                );
            }
        };

        ensure_storage_with(Some(config), Some(&nobody)).unwrap();
        assert_owned("creating the assets directory");

        for dir in &dirs {
            std::os::unix::fs::chown(dir, Some(0), Some(0)).unwrap();
        }
        ensure_storage_with(Some(config), Some(&nobody)).unwrap();
        assert_owned("an assets directory an earlier root run left root-owned");
    }
}
