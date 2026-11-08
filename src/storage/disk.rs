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

/// A base disk with at most this many extents is reflinked into a clone rather than given
/// to it as a subvolume snapshot. A subvolume snapshot commits a btrfs transaction, and
/// holds its parent directory's lock until the commit ends: with four restores and four
/// snapshots at once on a fleet host it took 193 ms at the median and 1.8 s at p99, against
/// 17 ms and 192 ms for a reflink of a 684-extent disk (interleaved A/B, 120 restores per
/// arm). A reflink costs one extent reference per extent, about 12 to 25 us each (a
/// 860,177-extent disk took 9 to 11 s), so at this limit it stays near 100 ms.
pub const REFLINK_MAX_EXTENTS: u64 = 4096;

/// `FCVM_REFLINK_MAX_EXTENTS` replaces [`REFLINK_MAX_EXTENTS`] when it holds a number. `0`
/// gives every disk that has an extent the subvolume snapshot, which is how the tests of
/// that path reach it with a test VM's small disk.
pub const REFLINK_MAX_EXTENTS_ENV: &str = "FCVM_REFLINK_MAX_EXTENTS";

/// The extent limit in force, from the value of [`REFLINK_MAX_EXTENTS_ENV`] if any.
fn reflink_max_extents(value: Option<&str>) -> u64 {
    value
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(REFLINK_MAX_EXTENTS)
}

/// Whether a base disk of `extents` extents (None: not counted) is cheaper to reflink than
/// to snapshot as a subvolume, at a limit of `limit` extents.
fn reflink_is_cheaper(extents: Option<u64>, limit: u64) -> bool {
    matches!(extents, Some(n) if n <= limit)
}

/// Whether `files` (with at most [`REFLINK_MAX_EXTENTS`] extents in all, or the limit in
/// [`REFLINK_MAX_EXTENTS_ENV`]) are cheaper to reflink than to snapshot as a subvolume. False
/// when they cannot be counted.
pub fn files_are_cheaper_to_reflink(files: &[PathBuf]) -> bool {
    let limit = reflink_max_extents(std::env::var(REFLINK_MAX_EXTENTS_ENV).ok().as_deref());
    reflink_is_cheaper(
        crate::storage::extents::extents_in_files_up_to(files, limit).ok(),
        limit,
    )
}

/// The file name prefix of a memory file `snapshot create --memory-dir` puts outside the
/// snapshot directory. [`remove_linked_memory_file`] removes only targets named this way.
pub const LINKED_MEMORY_PREFIX: &str = "fcvm-memory-";

/// Make `memory_dir` ready for `snapshot create --memory-dir` and return its resolved path:
/// created with mode 0700 when missing, and refused unless it is a directory owned by this
/// user that no group or other user can write (anyone who could would be able to swap the
/// memory file's name for another file between its creation and Firecracker's write), and
/// unless it is outside `snapshots_root`, whose generations are renamed and removed whole.
pub fn prepare_memory_dir(memory_dir: &Path, snapshots_root: &Path) -> Result<PathBuf> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(memory_dir)
        .with_context(|| format!("creating {}", memory_dir.display()))?;
    let dir = std::fs::canonicalize(memory_dir)
        .with_context(|| format!("resolving {}", memory_dir.display()))?;
    let meta = std::fs::metadata(&dir).with_context(|| format!("reading {}", dir.display()))?;
    let euid = nix::unistd::geteuid().as_raw();
    let egid = nix::unistd::getegid().as_raw();
    anyhow::ensure!(
        meta.is_dir() && meta.uid() == euid && meta.mode() & 0o022 == 0,
        "--memory-dir {} must be a directory owned by uid {euid} that group and others cannot \
         write (it is uid {}, mode {:o})",
        dir.display(),
        meta.uid(),
        meta.mode() & 0o7777
    );
    // Every directory above it must keep others from renaming it away: owned by root or by
    // this user, and closed to group and other writes unless sticky (as /tmp and /dev/shm are).
    for ancestor in dir.ancestors().skip(1) {
        let meta = std::fs::metadata(ancestor)
            .with_context(|| format!("reading {}", ancestor.display()))?;
        let owner_ok = meta.uid() == 0 || meta.uid() == euid;
        // group write is this user's own when the group is its primary one (umask 002 with a
        // private group); others' write needs the sticky bit
        let group_ok = meta.mode() & 0o020 == 0 || meta.gid() == egid;
        let others_kept_out = (group_ok && meta.mode() & 0o002 == 0) || meta.mode() & 0o1000 != 0;
        anyhow::ensure!(
            owner_ok && others_kept_out,
            "--memory-dir {}: {} lets another user replace it (uid {}, mode {:o}); use a \
             directory whose every parent is root's or yours and closed to others or sticky",
            dir.display(),
            ancestor.display(),
            meta.uid(),
            meta.mode() & 0o7777
        );
    }
    let root =
        std::fs::canonicalize(snapshots_root).unwrap_or_else(|_| snapshots_root.to_path_buf());
    anyhow::ensure!(
        !dir.starts_with(&root),
        "--memory-dir {} is inside the snapshot directory {}, whose generations are removed whole",
        dir.display(),
        root.display()
    );
    Ok(dir)
}

/// The path of a new memory file in a prepared memory directory: `fcvm-memory-<uuid>.bin`.
pub fn linked_memory_target(memory_dir: &Path) -> PathBuf {
    memory_dir.join(format!(
        "{LINKED_MEMORY_PREFIX}{}.bin",
        uuid::Uuid::new_v4()
    ))
}

/// Create the empty memory file `target`, readable only by its owner, and make `link` a
/// symbolic link to it. Firecracker follows the link when it writes the snapshot, and so does
/// a restore. The file is removed again if the link cannot be made.
pub fn create_linked_memory_file(target: &Path, link: &Path) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(target)
        .with_context(|| format!("creating {}", target.display()))?;
    if let Err(error) = std::os::unix::fs::symlink(target, link) {
        let _ = std::fs::remove_file(target);
        return Err(error)
            .with_context(|| format!("linking {} to {}", link.display(), target.display()));
    }
    Ok(())
}

/// Refuse a restore of snapshot `name` whose memory file, `memory_path`, is a link to a file
/// that no longer exists: one `snapshot create --memory-dir` left in a tmpfs that a reboot
/// cleared, or that was removed by hand. The restore stops here with that reason, before it
/// sets anything up, rather than in Firecracker's snapshot load.
pub fn ensure_memory_file_present(memory_path: &Path, name: &str) -> Result<()> {
    let linked = std::fs::symlink_metadata(memory_path).is_ok_and(|m| m.file_type().is_symlink());
    if linked && !memory_path.exists() {
        let target = std::fs::read_link(memory_path).unwrap_or_default();
        bail!(
            "the memory file of snapshot {name} is gone: {} (a --memory-dir on a tmpfs loses it \
             at a reboot); the snapshot cannot be restored",
            target.display()
        );
    }
    Ok(())
}

/// Remove the memory file that `dir`'s `memory.bin` links to, when it is one
/// `snapshot create --memory-dir` made: a regular file named `fcvm-memory-*.bin`, owned by
/// the owner of `dir`. A link to anything else is left alone, and so is everything when `dir`
/// is itself a symbolic link (it names another snapshot's directory, whose memory stays).
/// A removal that fails is an error, so the caller keeps the directory and its link rather
/// than lose the only reference to a file that takes the guest's memory size.
pub fn remove_linked_memory_file(dir: &Path) -> std::io::Result<()> {
    remove_linked_memory_file_with(dir, &|target| std::fs::remove_file(target))
}

/// [`remove_linked_memory_file`] with the removal passed in, for the tests.
fn remove_linked_memory_file_with(
    dir: &Path,
    remove: &dyn Fn(&Path) -> std::io::Result<()>,
) -> std::io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let Ok(dir_meta) = std::fs::symlink_metadata(dir) else {
        return Ok(());
    };
    if !dir_meta.is_dir() {
        return Ok(());
    }
    let link = dir.join(crate::storage::snapshot::SNAPSHOT_MEMORY_FILE);
    let Ok(target) = std::fs::read_link(&link) else {
        return Ok(());
    };
    let named = target.is_absolute()
        && target
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with(LINKED_MEMORY_PREFIX) && name.ends_with(".bin"));
    let file = match std::fs::symlink_metadata(&target) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    if !(named && file.file_type().is_file() && file.uid() == dir_meta.uid()) {
        warn!(link = %link.display(), target = %target.display(),
            "memory.bin links to a file fcvm did not make; leaving it");
        return Ok(());
    }
    match remove(&target) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// The extents a clone of `base_dir`'s disks would reflink: every file in it, the root disk
/// and the extra disks, which the reflink path copies one by one. Counted up to one past
/// `limit`; None when they cannot be counted.
fn snapshot_disks_extents(base_dir: &Path, limit: u64) -> Option<u64> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(base_dir).ok()? {
        let entry = entry.ok()?;
        if entry.file_type().ok()?.is_file() {
            files.push(entry.path());
        }
    }
    crate::storage::extents::extents_in_files_up_to(&files, limit).ok()
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
    {
        let dir = dir.to_path_buf();
        tokio::task::spawn_blocking(move || remove_linked_memory_file(&dir))
            .await
            .map_err(std::io::Error::other)??;
    }
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

/// Take a snapshot's disk directory `dest` as a subvolume snapshot of the VM's disk directory `src`,
/// rename the root disk `root_name` to `disk.raw`, and keep only `disk.raw` and `keep`. Returns false,
/// leaving no `dest`, when `src` is not a subvolume this process may snapshot or the snapshot fails;
/// the caller then reflinks the disks. A restored clone's disk directory is a subvolume snapshot of
/// its snapshot's `disk/`, so this costs one transaction commit where a reflink of a 154 GB root disk
/// of 860,177 extents measured 9 to 11 s, 14.5 s with the VM paused in one www snapshot.
pub async fn snapshot_disk_subvolume(
    src: &Path,
    dest: &Path,
    root_name: &std::ffi::OsStr,
    keep: &[String],
) -> Result<bool> {
    if !is_btrfs_subvolume(src) || !can_own_subvolume_snapshot_of(src) {
        return Ok(false);
    }
    if dest.exists() {
        bail!(
            "{} exists; a subvolume snapshot would land inside it",
            dest.display()
        );
    }
    let output = tokio::process::Command::new("btrfs")
        .args(["subvolume", "snapshot"])
        .arg(src)
        .arg(dest)
        .output()
        .await;
    let refused = match output {
        Ok(output) if output.status.success() => None,
        Ok(output) => Some(String::from_utf8_lossy(&output.stderr).trim().to_string()),
        Err(error) => Some(format!("btrfs tool unavailable: {error}")),
    };
    if let Some(reason) = refused {
        info!(src = %src.display(), %reason,
            "could not snapshot the VM's disk subvolume; reflinking the disks instead");
        clear_staging(dest).await?;
        return Ok(false);
    }
    let disk = std::ffi::OsStr::new("disk.raw");
    let kept = async {
        if root_name != disk {
            fs::rename(dest.join(root_name), dest.join(disk))
                .await
                .context("naming the snapshot's root disk")?;
        }
        let mut entries = fs::read_dir(dest).await?;
        while let Some(entry) = entries.next_entry().await? {
            let name = entry.file_name();
            if name == disk || keep.iter().any(|k| std::ffi::OsStr::new(k) == name) {
                continue;
            }
            let path = entry.path();
            if entry.file_type().await?.is_dir() {
                remove_snapshot_dir(&path).await?;
            } else {
                fs::remove_file(&path).await?;
            }
        }
        anyhow::Ok(())
    }
    .await;
    if let Err(error) = kept {
        warn!(dest = %dest.display(), error = %format!("{error:#}"),
            "could not keep only the disks in the snapshot's disk subvolume; reflinking instead");
        clear_staging(dest).await?;
        return Ok(false);
    }
    // An extra disk the snapshot records but the VM's disk directory does not hold would make
    // every clone's restore fail; the reflink path reports it now instead.
    let missing = missing_disks(dest, keep);
    if !missing.is_empty() {
        info!(dest = %dest.display(), missing = ?missing,
            "the VM's disk subvolume lacks disks the snapshot records; reflinking instead");
        clear_staging(dest).await?;
        return Ok(false);
    }
    Ok(true)
}

/// The disks a snapshot's disk directory `dir` must hold that it does not: `disk.raw` and `keep`.
fn missing_disks(dir: &Path, keep: &[String]) -> Vec<String> {
    std::iter::once("disk.raw")
        .chain(keep.iter().map(String::as_str))
        .filter(|name| !dir.join(name).is_file())
        .map(str::to_string)
        .collect()
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
        // snapshot of that directory when the disk has many extents. That costs
        // one btrfs transaction commit whatever the disks' extent count: 20 to
        // 60 ms on a quiet host, 0.2 to 1.0 s on a shared devserver at load 14
        // to 46, 1.8 s at p99 with eight snapshots and restores at once. A
        // reflink costs one extent reference per extent: 12 ms for a test VM's
        // disk, 9 to 11 s for a 154 GB disk of 860,177 extents. A disk of at most
        // REFLINK_MAX_EXTENTS extents is reflinked below instead. The snapshot
        // becomes this VM's disk directory, with the base disk renamed to the
        // rootfs name; the extra disks it carried are already in place.
        let limit = reflink_max_extents(std::env::var(REFLINK_MAX_EXTENTS_ENV).ok().as_deref());
        let base_extents = self
            .base_rootfs
            .parent()
            .filter(|d| is_snapshot_disk_subvolume(d))
            .and_then(|d| snapshot_disks_extents(d, limit));
        if let Some(base_dir) = self
            .base_rootfs
            .parent()
            .filter(|d| is_snapshot_disk_subvolume(d))
            .filter(|_| !reflink_is_cheaper(base_extents, limit))
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
                extents = ?base_extents,
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

/// An e2fsprogs command run in the C locale. e2fsprogs translates dumpe2fs's
/// group headings and labels, and the parsers here read the English ones.
fn e2fsprogs(tool: &str) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(tool);
    cmd.env("LC_ALL", "C");
    cmd
}

/// Ensure ext4 filesystem has sufficient free space by expanding and resizing.
async fn ensure_free_space_ext4(disk_path: &Path, min_free: u64) -> Result<()> {
    // Get current free space via dumpe2fs
    let output = e2fsprogs("dumpe2fs")
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
    let free_bytes = ext4_free_bytes(&stdout)?;
    let before = ext4_geometry(&stdout);

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
    let e2fsck_output = e2fsprogs("e2fsck")
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
    let output = e2fsprogs("resize2fs")
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

    match before {
        Some(before) if before.lazy_itables => {
            mark_added_inode_tables_zeroed(disk_path, before.groups).await?
        }
        Some(_) => {}
        None => warn!(
            disk = %disk_path.display(),
            "dumpe2fs header is ambiguous; added inode tables left to the guest"
        ),
    }

    info!(disk = %disk_path.display(), "ext4 rootfs expanded successfully");
    Ok(())
}

/// Block group descriptor flags (ext4 `bg_flags`), as dumpe2fs names them.
const BG_INODE_UNINIT: u32 = 0x1;
const BG_BLOCK_UNINIT: u32 = 0x2;
const BG_INODE_ZEROED: u32 = 0x4;

/// One block group as `dumpe2fs` prints it.
#[derive(Debug, PartialEq)]
struct BlockGroup {
    index: u64,
    /// `None` when the descriptor carries a flag this code does not know, so
    /// a rewrite of `bg_flags` could drop it.
    flags: Option<u32>,
    /// First and last block of the group's inode table.
    inode_table: Option<(u64, u64)>,
}

/// The filesystem facts the inode-table marking relies on, from dumpe2fs's
/// header.
#[derive(Debug, PartialEq)]
struct Ext4Geometry {
    block_size: u64,
    blocks: u64,
    groups: u64,
    /// metadata_csum or uninit_bg: only these have lazily initialised inode
    /// tables, and only for these does dumpe2fs print group flags.
    lazy_itables: bool,
}

/// The value of the one header line `key: value`, or `None` when the key is
/// missing or appears more than once. dumpe2fs prints header strings such as
/// the last mount point verbatim, so a second line with the same key is not
/// evidence of the value.
fn header_field<'a>(output: &'a str, key: &str) -> Option<&'a str> {
    let mut values = output
        .lines()
        .filter_map(|line| line.strip_prefix(key)?.strip_prefix(':'))
        .map(str::trim);
    let value = values.next()?;
    values.next().is_none().then_some(value)
}

/// `None` unless every field it reads appears once and the geometry is one
/// ext4 can have.
fn ext4_geometry(header: &str) -> Option<Ext4Geometry> {
    let number = |key| header_field(header, key)?.parse::<u64>().ok();
    let block_size = number("Block size")?;
    let blocks = number("Block count")?;
    let first = number("First block")?;
    let per_group = number("Blocks per group")?;
    if !block_size.is_power_of_two() || !(1024..=65536).contains(&block_size) {
        return None;
    }
    if per_group == 0 || first >= blocks {
        return None;
    }
    let features = header_field(header, "Filesystem features")?;
    let lazy_itables = features
        .split_whitespace()
        .any(|f| f == "metadata_csum" || f == "uninit_bg");
    Some(Ext4Geometry {
        block_size,
        blocks,
        groups: (blocks - first).div_ceil(per_group),
        lazy_itables,
    })
}

/// Every block group in full `dumpe2fs` output, with its flags and inode table.
///
/// `None` unless the output holds exactly one sequence of groups 0..`expected`,
/// in order, each with one inode table. dumpe2fs prints header strings such as
/// the volume name and the last mount point verbatim, so a "Group" or "Inode
/// table at" line outside that sequence is not a descriptor.
fn parse_block_groups(output: &str, expected: u64) -> Option<Vec<BlockGroup>> {
    let mut groups: Vec<BlockGroup> = Vec::new();
    for line in output.lines() {
        if let Some(rest) = line.strip_prefix("Group ") {
            let Some((index, rest)) = rest.split_once(':') else {
                continue;
            };
            let Ok(index) = index.trim().parse::<u64>() else {
                continue; // "Group descriptor size:" and the like
            };
            if index != groups.len() as u64 {
                return None;
            }
            let flags = match rest.split_once('[').and_then(|(_, f)| f.split_once(']')) {
                None => Some(0),
                Some((names, _)) => {
                    names
                        .split(',')
                        .map(str::trim)
                        .try_fold(0, |acc, name| match name {
                            "INODE_UNINIT" => Some(acc | BG_INODE_UNINIT),
                            "BLOCK_UNINIT" => Some(acc | BG_BLOCK_UNINIT),
                            "ITABLE_ZEROED" => Some(acc | BG_INODE_ZEROED),
                            _ => None,
                        })
                }
            };
            groups.push(BlockGroup {
                index,
                flags,
                inode_table: None,
            });
        } else if let Some(rest) = line.trim_start().strip_prefix("Inode table at ") {
            let group = groups.last_mut()?;
            if group.inode_table.is_some() {
                return None;
            }
            let range = rest.split_whitespace().next().unwrap_or("");
            let (a, b) = range.split_once('-').unwrap_or((range, range));
            group.inode_table = Some((a.parse().ok()?, b.parse().ok()?));
        }
    }
    if groups.len() as u64 != expected || groups.iter().any(|g| g.inode_table.is_none()) {
        return None;
    }
    Some(groups)
}

/// Whether every byte of `len` bytes at `offset` in `path` is zero.
fn range_is_zero(path: &Path, offset: u64, len: u64) -> Result<bool> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path)
        .with_context(|| format!("opening {} to read inode tables", path.display()))?;
    file.seek(SeekFrom::Start(offset))?;
    let mut buf = vec![0u8; 1 << 20];
    let mut left = len;
    while left > 0 {
        let n = left.min(buf.len() as u64) as usize;
        file.read_exact(&mut buf[..n])
            .with_context(|| format!("reading {} at {}", path.display(), offset + len - left))?;
        if buf[..n].iter().any(|&b| b != 0) {
            return Ok(false);
        }
        left -= n as u64;
    }
    Ok(true)
}

/// Mark the inode tables `resize2fs` added as zeroed when their bytes are zero.
///
/// resize2fs leaves a new group's inode table unwritten and its descriptor
/// without ITABLE_ZEROED, so the guest kernel's ext4lazyinit thread zeroes it
/// after boot. A snapshot taken before that finishes hands the same unfinished
/// work to every clone, and each clone writes the zeros into its own disk file:
/// 16 MiB per clone in the page cache within 10 s of a restore, on a 4 vCPU
/// Chromium golden whose resize added 16 groups (64 MiB of inode tables).
///
/// A new group's table usually lies in the sparse region `truncate` just
/// added, but with flex_bg resize2fs may place it in free space of the old
/// filesystem, which can hold the bytes of a file that was moved or deleted.
/// So the table is read, and only an all-zero table is marked. A descriptor
/// with a flag this code does not know is left alone. The result must pass
/// `e2fsck -fn`.
async fn mark_added_inode_tables_zeroed(disk_path: &Path, groups_before: u64) -> Result<()> {
    let output = e2fsprogs("dumpe2fs")
        .arg(disk_path.as_os_str())
        .output()
        .await
        .context("running dumpe2fs")?;
    if !output.status.success() {
        bail!(
            "dumpe2fs failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let dump = String::from_utf8_lossy(&output.stdout);
    let geometry = ext4_geometry(&dump).filter(|g| g.lazy_itables);
    let Some((geometry, groups)) =
        geometry.and_then(|g| parse_block_groups(&dump, g.groups).map(|groups| (g, groups)))
    else {
        warn!(
            disk = %disk_path.display(),
            "dumpe2fs output is not one header and one sequence of block groups; \
             inode tables left to the guest"
        );
        return Ok(());
    };
    let (block_size, total) = (geometry.block_size, geometry.groups);

    let mut zeroed = Vec::new();
    let mut not_zero = 0u64;
    for group in groups.iter().filter(|g| g.index >= groups_before) {
        let (Some(flags), Some((first, last))) = (group.flags, group.inode_table) else {
            continue;
        };
        if flags & BG_INODE_ZEROED != 0 || last < first || last >= geometry.blocks {
            continue;
        }
        let path = disk_path.to_path_buf();
        let (offset, len) = (first * block_size, (last - first + 1) * block_size);
        let zero = tokio::task::spawn_blocking(move || range_is_zero(&path, offset, len))
            .await
            .context("joining inode table read")??;
        if zero {
            zeroed.push((group.index, flags | BG_INODE_ZEROED, len));
        } else {
            not_zero += 1;
        }
    }
    if not_zero > 0 {
        info!(
            disk = %disk_path.display(),
            groups = not_zero,
            "added inode tables hold nonzero bytes; the guest zeroes them"
        );
    }
    if zeroed.is_empty() {
        return Ok(());
    }

    let mut script = tempfile::NamedTempFile::new().context("creating debugfs script")?;
    {
        use std::io::Write;
        for (index, flags, _) in &zeroed {
            writeln!(script, "set_bg {index} flags {flags}")?;
            writeln!(script, "set_bg {index} checksum calc")?;
        }
        script.flush()?;
    }
    let output = e2fsprogs("debugfs")
        .arg("-w")
        .arg("-f")
        .arg(script.path())
        .arg(disk_path.as_os_str())
        .output()
        .await
        .context("running debugfs")?;
    if !output.status.success() {
        bail!(
            "debugfs failed marking inode tables zeroed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    // debugfs reports a failed command on stderr and still exits 0, so the
    // result is checked: the filesystem must be clean and every group marked.
    let check = e2fsprogs("e2fsck")
        .args(["-f", "-n"])
        .arg(disk_path.as_os_str())
        .output()
        .await
        .context("running e2fsck")?;
    if !check.status.success() {
        bail!(
            "e2fsck -fn exited {:?} after marking inode tables zeroed: {}",
            check.status.code(),
            String::from_utf8_lossy(&check.stdout)
        );
    }
    let output = e2fsprogs("dumpe2fs")
        .arg(disk_path.as_os_str())
        .output()
        .await
        .context("running dumpe2fs")?;
    let after =
        parse_block_groups(&String::from_utf8_lossy(&output.stdout), total).unwrap_or_default();
    for (index, _, _) in &zeroed {
        let marked = after
            .iter()
            .any(|g| g.index == *index && g.flags.is_some_and(|f| f & BG_INODE_ZEROED != 0));
        if !output.status.success() || !marked {
            bail!(
                "group {index} of {} is not marked ITABLE_ZEROED after debugfs: {}",
                disk_path.display(),
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }
    info!(
        disk = %disk_path.display(),
        groups = zeroed.len(),
        mib = zeroed.iter().map(|(_, _, len)| len).sum::<u64>() >> 20,
        "marked added inode tables zeroed"
    );
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

/// Free bytes from dumpe2fs -h output: "Free blocks" times "Block size", each read
/// through `header_field`, so a key that is missing or appears twice (a header string
/// such as the last mount point printed verbatim can carry a forged line) fails the
/// resize instead of growing the disk by the wrong amount.
fn ext4_free_bytes(header: &str) -> Result<u64> {
    let number = |key: &str| -> Result<u64> {
        let value = header_field(header, key).with_context(|| {
            format!("'{key}' is missing from dumpe2fs output or appears more than once")
        })?;
        value
            .parse::<u64>()
            .with_context(|| format!("parsing dumpe2fs '{key}' value '{value}'"))
    };
    let block_size = number("Block size")?;
    if !block_size.is_power_of_two() || !(1024..=65536).contains(&block_size) {
        bail!("dumpe2fs 'Block size' {block_size} is not one ext4 can have");
    }
    let free_blocks = number("Free blocks")?;
    free_blocks
        .checked_mul(block_size)
        .with_context(|| format!("free space overflows: {free_blocks} blocks of {block_size}"))
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

    /// A counted disk of at most REFLINK_MAX_EXTENTS extents is reflinked; a larger one, or
    /// one whose extents could not be counted, gets the subvolume snapshot.
    #[test]
    fn a_disk_with_few_extents_is_reflinked() {
        let limit = REFLINK_MAX_EXTENTS;
        assert!(reflink_is_cheaper(Some(0), limit));
        assert!(reflink_is_cheaper(Some(684), limit));
        assert!(reflink_is_cheaper(Some(limit), limit));
        assert!(!reflink_is_cheaper(Some(limit + 1), limit));
        assert!(!reflink_is_cheaper(None, limit));
        // the override: a number replaces the limit, anything else leaves it
        assert_eq!(reflink_max_extents(None), REFLINK_MAX_EXTENTS);
        assert_eq!(reflink_max_extents(Some("0")), 0);
        assert_eq!(reflink_max_extents(Some(" 12\n")), 12);
        assert_eq!(reflink_max_extents(Some("many")), REFLINK_MAX_EXTENTS);
        assert!(!reflink_is_cheaper(
            Some(684),
            reflink_max_extents(Some("0"))
        ));
        let source = include_str!("disk.rs");
        let create = &source[source.find("pub async fn create_cow_disk").unwrap()..];
        let subvolume = create.find("if let Some(base_dir) = self").unwrap();
        assert!(
            create[subvolume..subvolume + 260].contains("!reflink_is_cheaper(base_extents, limit)"),
            "create_cow_disk takes the subvolume snapshot only for disks not cheaper to reflink"
        );
        // the count covers every disk the reflink path would copy, the extra disks too
        assert!(
            create[..subvolume].contains(".and_then(|d| snapshot_disks_extents(d, limit))"),
            "the reflink decision counts the snapshot's extra disks as well as its root disk"
        );
    }

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

    /// `snapshot create --memory-dir` leaves the memory file outside the snapshot directory,
    /// behind a `memory.bin` link. Removing the directory through fcvm (a replaced
    /// generation, a failed create) must remove that file, or every sleep leaks the guest's
    /// memory size in the memory directory.
    #[tokio::test]
    async fn removing_a_snapshot_removes_its_linked_memory_file() {
        let root = tempfile::tempdir().unwrap();
        let memory_dir = root.path().join("shm");
        let snapshot = root.path().join("snap");
        std::fs::create_dir_all(&snapshot).unwrap();
        let link = snapshot.join("memory.bin");
        let memory_dir = prepare_memory_dir(&memory_dir, &root.path().join("snapshots")).unwrap();
        let target = linked_memory_target(&memory_dir);
        create_linked_memory_file(&target, &link).unwrap();
        assert_eq!(std::fs::read_link(&link).unwrap(), target);
        std::fs::write(&link, b"guest memory").unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"guest memory");
        remove_snapshot_dir(&snapshot).await.unwrap();
        assert!(!snapshot.exists());
        assert!(
            !target.exists(),
            "the linked memory file outlived its snapshot"
        );
        assert_eq!(std::fs::read_dir(&memory_dir).unwrap().count(), 0);
    }

    /// A snapshot directory reached through a symbolic link (an alias of another snapshot)
    /// is removed as the link alone: the snapshot it names keeps its memory file.
    #[tokio::test]
    async fn removing_an_alias_of_a_snapshot_keeps_the_snapshots_memory() {
        let root = tempfile::tempdir().unwrap();
        let live = root.path().join("live");
        std::fs::create_dir_all(&live).unwrap();
        let memory_dir =
            prepare_memory_dir(&root.path().join("shm"), &root.path().join("snapshots")).unwrap();
        let target = linked_memory_target(&memory_dir);
        create_linked_memory_file(&target, &live.join("memory.bin")).unwrap();
        let alias = root.path().join("alias");
        std::os::unix::fs::symlink(&live, &alias).unwrap();
        remove_snapshot_dir(&alias).await.unwrap();
        assert!(!alias.exists());
        assert!(target.exists(), "the aliased snapshot lost its memory file");
    }

    /// A linked memory file that cannot be removed fails the removal, and the link that is the
    /// only reference to the file stays.
    #[test]
    fn a_linked_memory_file_that_cannot_be_removed_keeps_its_link() {
        let root = tempfile::tempdir().unwrap();
        let snapshot = root.path().join("snap");
        std::fs::create_dir_all(&snapshot).unwrap();
        let memory_dir =
            prepare_memory_dir(&root.path().join("shm"), &root.path().join("snapshots")).unwrap();
        let target = linked_memory_target(&memory_dir);
        let link = snapshot.join("memory.bin");
        create_linked_memory_file(&target, &link).unwrap();
        let refused = |_: &Path| Err(std::io::Error::from_raw_os_error(libc::EACCES));
        let error = remove_linked_memory_file_with(&snapshot, &refused).unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::EACCES));
        assert!(std::fs::symlink_metadata(&link).is_ok() && target.exists());
    }

    /// A memory directory others could write, or one inside the snapshot directory, is
    /// refused; a new one is made closed to group and others.
    #[test]
    fn a_memory_directory_must_be_private_and_outside_the_snapshots() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let snapshots = root.path().join("snapshots");
        std::fs::create_dir_all(&snapshots).unwrap();
        let fresh = prepare_memory_dir(&root.path().join("a/b"), &snapshots).unwrap();
        assert_eq!(
            std::fs::metadata(&fresh).unwrap().permissions().mode() & 0o777,
            0o700
        );
        let open = root.path().join("open");
        std::fs::create_dir(&open).unwrap();
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o1777)).unwrap();
        let error = prepare_memory_dir(&open, &snapshots)
            .unwrap_err()
            .to_string();
        assert!(error.contains("group and others cannot write"), "{error}");
        // a parent others can write, not sticky, lets them rename the directory away
        let shared = root.path().join("shared");
        std::fs::create_dir(&shared).unwrap();
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o777)).unwrap();
        let error = prepare_memory_dir(&shared.join("mem"), &snapshots)
            .unwrap_err()
            .to_string();
        assert!(error.contains("lets another user replace it"), "{error}");
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o1777)).unwrap();
        prepare_memory_dir(&shared.join("mem"), &snapshots).expect("a sticky parent is safe");
        let inside = prepare_memory_dir(&snapshots.join("tag/memory"), &snapshots)
            .unwrap_err()
            .to_string();
        assert!(inside.contains("inside the snapshot directory"), "{inside}");
    }

    /// A snapshot whose linked memory file is gone (a tmpfs cleared by a reboot) is refused
    /// with that reason; one whose memory file is present, linked or not, is not.
    #[test]
    fn a_restore_of_a_snapshot_whose_memory_is_gone_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let snapshot = root.path().join("snap");
        std::fs::create_dir_all(&snapshot).unwrap();
        let link = snapshot.join("memory.bin");
        let memory_dir =
            prepare_memory_dir(&root.path().join("shm"), &root.path().join("snapshots")).unwrap();
        let target = linked_memory_target(&memory_dir);
        create_linked_memory_file(&target, &link).unwrap();
        ensure_memory_file_present(&link, "potato-1").unwrap();
        std::fs::remove_file(&target).unwrap();
        let error = ensure_memory_file_present(&link, "potato-1")
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("memory file of snapshot potato-1 is gone"),
            "{error}"
        );
        let plain = root.path().join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        std::fs::write(plain.join("memory.bin"), b"m").unwrap();
        ensure_memory_file_present(&plain.join("memory.bin"), "p").unwrap();
    }

    /// Only a file fcvm made is removed through the link: a snapshot directory whose
    /// memory.bin links anywhere else loses the link, not the file it points to.
    #[tokio::test]
    async fn a_link_to_a_file_fcvm_did_not_make_is_left_alone() {
        let root = tempfile::tempdir().unwrap();
        let other = root.path().join("precious.bin");
        std::fs::write(&other, b"keep").unwrap();
        let snapshot = root.path().join("snap");
        std::fs::create_dir_all(&snapshot).unwrap();
        std::os::unix::fs::symlink(&other, snapshot.join("memory.bin")).unwrap();
        remove_snapshot_dir(&snapshot).await.unwrap();
        assert!(!snapshot.exists());
        assert_eq!(std::fs::read(&other).unwrap(), b"keep");
    }

    /// A snapshot's disk directory must hold disk.raw and every extra disk it records.
    #[test]
    fn a_missing_extra_disk_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("disk.raw"), b"disk").unwrap();
        std::fs::write(dir.path().join("data.raw"), b"data").unwrap();
        let keep = ["data.raw".to_string(), "gone.raw".to_string()];
        assert_eq!(missing_disks(dir.path(), &keep), ["gone.raw"]);
        std::fs::remove_file(dir.path().join("disk.raw")).unwrap();
        assert_eq!(missing_disks(dir.path(), &keep[..1]), ["disk.raw"]);
    }

    /// A disk directory that is not a subvolume is not snapshotted; the caller reflinks.
    #[tokio::test]
    async fn a_plain_disk_directory_is_not_snapshotted() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("disks");
        std::fs::create_dir(&src).unwrap();
        std::fs::write(src.join("rootfs.raw"), b"disk").unwrap();
        let dest = dir.path().join("snap").join("disk");
        let taken = snapshot_disk_subvolume(&src, &dest, std::ffi::OsStr::new("rootfs.raw"), &[])
            .await
            .unwrap();
        assert!(!taken);
        assert!(!dest.exists());
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

#[cfg(test)]
mod resize_inode_table_tests {
    use super::*;
    use std::process::Command;

    fn run(cmd: &mut Command) -> std::process::Output {
        let out = cmd.env("LC_ALL", "C").output().expect("spawn");
        assert!(
            out.status.success(),
            "{cmd:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        out
    }

    fn groups(path: &Path) -> Vec<BlockGroup> {
        let out = run(Command::new("dumpe2fs").arg(path));
        let out = String::from_utf8_lossy(&out.stdout);
        parse_block_groups(&out, ext4_geometry(&out).unwrap().groups)
            .expect("one complete group sequence")
    }

    fn unzeroed(path: &Path) -> Vec<u64> {
        groups(path)
            .iter()
            .filter(|g| g.flags.unwrap_or(0) & BG_INODE_ZEROED == 0)
            .map(|g| g.index)
            .collect()
    }

    /// An ext4 image of `size` with every inode table written, 32 MiB groups.
    fn image(dir: &Path, size: &str) -> PathBuf {
        let path = dir.join("rootfs.raw");
        run(Command::new("truncate").args(["-s", size]).arg(&path));
        let mut mkfs = Command::new("mkfs.ext4");
        mkfs.args([
            "-q",
            "-F",
            "-b",
            "4096",
            "-g",
            "8192",
            "-E",
            "lazy_itable_init=0",
        ]);
        run(mkfs.arg(&path));
        assert_eq!(
            unzeroed(&path),
            Vec::<u64>::new(),
            "precondition: a fully zeroed image"
        );
        path
    }

    fn assert_clean(path: &Path) {
        let out = Command::new("e2fsck")
            .args(["-f", "-n"])
            .arg(path)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "e2fsck -fn: {}",
            String::from_utf8_lossy(&out.stdout)
        );
    }

    /// RED ON MAIN: resize2fs left every group it added without ITABLE_ZEROED,
    /// so the guest's ext4lazyinit zeroed them after boot, and a golden
    /// snapshotted before it finished made each clone write the zeros again
    /// (16 of 96 groups unmarked on a 4 vCPU Chromium golden).
    ///
    /// The base is 16 groups, one flex group, as fcvm's 10 GiB layer2 rootfs is
    /// a whole number of them, so the added groups' tables lie in the region
    /// truncate made.
    #[tokio::test]
    async fn resize_marks_the_inode_tables_it_added_zeroed() {
        crate::utils::ensure_sbin_on_path();
        let dir = tempfile::tempdir().unwrap();
        let path = image(dir.path(), "512M");
        let before = groups(&path).len();

        ensure_free_space(&path, "1G", 0).await.unwrap();

        let after = groups(&path);
        assert!(after.len() > before, "the resize added no groups");
        assert_eq!(unzeroed(&path), Vec::<u64>::new());
        assert_clean(&path);
    }

    /// A group whose new inode table lands on blocks that hold old bytes keeps
    /// its descriptor as resize2fs wrote it. With flex_bg, resize2fs puts the
    /// tables of groups that join a partly filled flex group into that flex
    /// group's free space, and free blocks keep whatever a deleted or moved
    /// file left there. Here the free blocks are filled with 0xff first.
    #[tokio::test]
    async fn an_added_inode_table_over_old_bytes_is_left_unmarked() {
        use std::io::{Seek, SeekFrom, Write};
        crate::utils::ensure_sbin_on_path();
        let dir = tempfile::tempdir().unwrap();
        let path = image(dir.path(), "64M");
        let dump = run(Command::new("dumpe2fs").arg(&path));
        let dump = String::from_utf8_lossy(&dump.stdout);
        let mut file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        for line in dump.lines() {
            // Only a group's own list, indented; the unindented header line
            // of the same name holds a count, not a block.
            let Some(ranges) = line.strip_prefix("  Free blocks: ") else {
                continue;
            };
            for range in ranges.split(',').map(str::trim).filter(|r| !r.is_empty()) {
                let (a, b) = range.split_once('-').unwrap_or((range, range));
                let (a, b): (u64, u64) = (a.parse().unwrap(), b.parse().unwrap());
                file.seek(SeekFrom::Start(a * 4096)).unwrap();
                file.write_all(&vec![0xffu8; ((b - a + 1) * 4096) as usize])
                    .unwrap();
            }
        }
        drop(file);
        assert_clean(&path); // the fill wrote only free blocks
        let before = groups(&path).len() as u64;

        ensure_free_space(&path, "256M", 0).await.unwrap();

        let mut over_old_bytes = 0;
        for group in groups(&path).iter().filter(|g| g.index >= before) {
            let (first, last) = group.inode_table.unwrap();
            let zero = range_is_zero(&path, first * 4096, (last - first + 1) * 4096).unwrap();
            let marked = group.flags.unwrap() & BG_INODE_ZEROED != 0;
            assert_eq!(
                marked, zero,
                "group {}: marked={marked}, table all zero={zero}",
                group.index
            );
            over_old_bytes += usize::from(!zero);
        }
        assert!(
            over_old_bytes > 0,
            "no added inode table landed on old bytes, so this proves nothing"
        );
        assert_clean(&path);
    }

    /// A filesystem without metadata_csum or uninit_bg has no lazily
    /// initialised inode tables, and dumpe2fs prints no group flags for it, so
    /// the check after marking could never see the flag and refused the resize.
    #[tokio::test]
    async fn a_filesystem_without_descriptor_checksums_still_grows() {
        crate::utils::ensure_sbin_on_path();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rootfs.raw");
        run(Command::new("truncate").args(["-s", "128M"]).arg(&path));
        run(Command::new("mkfs.ext4")
            .args([
                "-q",
                "-F",
                "-b",
                "4096",
                "-g",
                "8192",
                "-O",
                "^metadata_csum,^uninit_bg",
            ])
            .arg(&path));
        let before = groups(&path).len();

        ensure_free_space(&path, "256M", 0).await.unwrap();

        assert!(groups(&path).len() > before, "the resize added no groups");
        assert_clean(&path);
    }

    /// dumpe2fs prints header strings such as the last mount point verbatim, so
    /// a "Group" line can appear before the real descriptors. Only one
    /// complete sequence of groups 0..n is accepted.
    #[test]
    fn a_group_line_outside_the_descriptor_sequence_is_refused() {
        let real = "Group 0: (Blocks 0-8191) csum 0x1 [ITABLE_ZEROED]\n  \
                    Inode table at 37-548 (+37)\n\
                    Group 1: (Blocks 8192-16383) csum 0x2 [INODE_UNINIT]\n  \
                    Inode table at 549-1060 (bg #0 + 549)\n";
        assert_eq!(parse_block_groups(real, 2).map(|g| g.len()), Some(2));
        let injected = format!(
            "Last mounted on:          /x\nGroup 1: [INODE_UNINIT]\nInode table at 60000\n{real}"
        );
        assert_eq!(parse_block_groups(&injected, 2), None);
        assert_eq!(parse_block_groups(real, 3), None, "a missing group");
        let skipped = real.replace("Group 1:", "Group 2:");
        assert_eq!(
            parse_block_groups(&skipped, 2),
            None,
            "a group number out of sequence; marking writes to the group by number"
        );
        let doubled = real.replace(
            "Inode table at 549",
            "Inode table at 1-1\n  Inode table at 549",
        );
        assert_eq!(
            parse_block_groups(&doubled, 2),
            None,
            "two tables for one group"
        );
    }

    /// A header field that appears twice comes partly from a string dumpe2fs
    /// printed verbatim, such as the last mount point, so it is not trusted:
    /// a forged "Block size: 0" made every inode table read as zero bytes.
    #[test]
    fn a_header_field_that_appears_twice_is_refused() {
        let header = "Last mounted on:          /\n\
                      Block count:              131072\n\
                      First block:              0\n\
                      Block size:               4096\n\
                      Blocks per group:         32768\n\
                      Filesystem features:      has_journal extent metadata_csum\n";
        assert_eq!(
            ext4_geometry(header),
            Some(Ext4Geometry {
                block_size: 4096,
                blocks: 131072,
                groups: 4,
                lazy_itables: true
            })
        );
        for forged in [
            "Block size: 0\n",
            "Filesystem features: uninit_bg\n",
            "Block count: 9\n",
        ] {
            let injected = header.replacen("/\n", &format!("/x\n{forged}"), 1);
            assert_eq!(ext4_geometry(&injected), None, "{forged:?}");
        }
        let zero = header.replace(
            "Block size:               4096",
            "Block size:               0",
        );
        assert_eq!(ext4_geometry(&zero), None, "a block size ext4 cannot have");
    }

    /// The free-space reading that sizes the resize takes each field once: a forged
    /// "Free blocks" or "Block size" line before the real one, from a header string
    /// dumpe2fs prints verbatim, used to be the value read (#1546).
    #[test]
    fn free_space_refuses_a_header_field_that_appears_twice() {
        let header = "Last mounted on:          /\n\
                      Block count:              131072\n\
                      Free blocks:              1000\n\
                      Block size:               4096\n";
        assert_eq!(ext4_free_bytes(header).unwrap(), 4096 * 1000);
        for forged in ["Free blocks: 99999999\n", "Block size: 65536\n"] {
            let injected = header.replacen("/\n", &format!("/x\n{forged}"), 1);
            let error = ext4_free_bytes(&injected).expect_err(forged);
            assert!(error.to_string().contains("more than once"), "{error}");
        }
        let missing = header.replace("Free blocks:              1000\n", "");
        assert!(ext4_free_bytes(&missing).is_err());
        let zero = header.replace(
            "Block size:               4096",
            "Block size:               0",
        );
        assert!(
            ext4_free_bytes(&zero).is_err(),
            "a block size ext4 cannot have"
        );
    }

    /// e2fsprogs translates the group headings and the "Inode table at" label,
    /// so a translated locale made the parser find nothing and mark nothing.
    #[test]
    fn e2fsprogs_commands_run_in_the_c_locale() {
        let cmd = e2fsprogs("dumpe2fs");
        let envs: Vec<_> = cmd.as_std().get_envs().collect();
        assert!(
            envs.contains(&(
                std::ffi::OsStr::new("LC_ALL"),
                Some(std::ffi::OsStr::new("C"))
            )),
            "{envs:?}"
        );
    }

    #[test]
    fn dumpe2fs_groups_parse_with_their_flags_and_inode_tables() {
        let out = "Group descriptor size:    64\n\
                   Group 0: (Blocks 0-32767) csum 0xdccc [INODE_UNINIT, ITABLE_ZEROED]\n  \
                   Inode table at 2643-3154 (+2643)\n\
                   Group 1: (Blocks 32768-65535) csum 0x0001\n  \
                   Inode table at 3155-3666 (bg #0 + 3155)\n\
                   Group 2: (Blocks 65536-98303) csum 0x0002 [INODE_UNINIT, SOMETHING_NEW]\n  \
                   Inode table at 3667-4178 (bg #0 + 3667)\n";
        assert_eq!(
            parse_block_groups(out, 3).unwrap(),
            vec![
                BlockGroup {
                    index: 0,
                    flags: Some(BG_INODE_UNINIT | BG_INODE_ZEROED),
                    inode_table: Some((2643, 3154))
                },
                BlockGroup {
                    index: 1,
                    flags: Some(0),
                    inode_table: Some((3155, 3666))
                },
                BlockGroup {
                    index: 2,
                    flags: None,
                    inode_table: Some((3667, 4178))
                },
            ]
        );
    }
}
