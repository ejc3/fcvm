//! The store of `--pmem` images: one immutable copy of each image generation under
//! `<data_dir>/pmem`, which is the file Firecracker maps.
//!
//! A VM, a snapshot or a clone never maps the image the user named. `podman run` copies
//! it here first, and every VM and snapshot of that generation maps the copy, so a later
//! write to the user's file reaches none of them and the next run copies the new
//! generation.
//!
//! An entry is `<data_dir>/pmem/<64 hex>.img`, named by sha256 over a domain tag, the
//! image's canonical path and its identity from `fstat` of the descriptor that is copied
//! (inode, length, modification and change time, `storage::pmem::image_identity`), so
//! the name and the bytes describe the same file. An entry appears only through
//! `link(temp, entry)` and is never replaced, renamed, linked again or opened for
//! writing; it is mode 0400 and owned by the invoking user. A run uses an entry, one it
//! publishes or finds and one a snapshot it restores names, only when the entry belongs to
//! the run's own uid or, under sudo, to the invoking user with that user's group or the
//! run's (`check_entry_owner`): those are the owners whose mode 0400 file Firecracker can
//! open inside a VM's holder namespace, under sudo too. Any other entry (in a data_dir two
//! users share, the other user's) is refused instead of handed to Firecracker. Nothing
//! deletes entries yet.
//!
//! A copy is published only if it is consistent. The source is fsynced before the copy
//! and its identity compared before and after it: after the fsync any write through a
//! shared mapping faults and moves the change time (measured on btrfs: without the fsync
//! a second write to an already dirtied page moved no timestamp in 200 of 200 trials,
//! with it 0 of 200). fcvm refuses an image on tmpfs, ramfs and hugetlbfs, where a write
//! through a shared mapping has no such fault, on FUSE, NFS, SMB and 9p, where the
//! kernel serves the size and times from a cache that a write through another mount or
//! client need not update, and on overlayfs, whose statfs type does not name the layer
//! that holds the file, which can be tmpfs or a network filesystem. Those are the
//! filesystems it recognizes; an image on another one with the same problem (Ceph, AFS,
//! Lustre, GPFS) is copied, and its copy check can miss a write. On a kernel or filesystem
//! without multigrain timestamps the change time has the filesystem's timestamp
//! granularity, so a write within one tick of the image's previous change keeps its
//! identity: the copy check misses it, and so does the next run's generation.
//!
//! The store is per data_dir, like the snapshots that will reference its entries: flock
//! is authoritative only within one kernel, and assets_dir is shared across nesting
//! levels over fuse-pipe. Ingest holds `.lock` shared from creating its temp until it
//! removes it, so a collector that takes it exclusively can remove `tmp/*` safely. An
//! ingest that is killed leaves its temp in `tmp/`; nothing removes temps or entries yet.
//!
//! A run opens the store directory and `tmp` once, refusing a symlink or another file in
//! place of either, and reaches everything it creates, links, removes or locks there
//! through those descriptors. Under sudo, root works in a directory a user owns, where a
//! name can be replaced between two lookups of a path, so a path would let that user send
//! root's writes and ownership changes anywhere. A run hands the invoking user only the
//! directories and the lock file it created itself, and a directory only after checking
//! through its descriptor that it is still the one the run's mkdirat made. Under sudo a new
//! directory is made under a temporary name and renamed onto its own only once fstat shows
//! it belongs to the invoking user with its final mode, so a rootless run of that user never
//! finds one that root still owns.

use anyhow::{anyhow, bail, ensure, Context, Result};
use nix::errno::Errno;
use nix::fcntl::{AtFlags, Flock, FlockArg, OFlag, RenameFlags};
use nix::sys::stat::Mode;
use nix::sys::statfs::FsType;
use nix::unistd::{UnlinkatFlags, User};
use sha2::{Digest, Sha256};
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use tracing::{debug, info, warn};

use crate::state::types::PmemDevice;

/// Domain tag hashed ahead of the source path, so a change to how entries are named
/// changes every name.
const NAME_DOMAIN: &[u8] = b"fcvm-pmem-v1";

/// Mode of every published entry: the owner reads it, nobody writes it. Firecracker opens
/// a read-only pmem file without write access and maps it PROT_READ, so read is all it
/// needs; a private image stays private on the shared default data_dir.
const ENTRY_MODE: u32 = 0o400;

/// Mode a store directory is created with. Only its owner can write it, so nobody else can
/// have moved a directory with this mode into place, and the umask's group and other bits
/// do not change it. `finish_created_dir` then gives it the mode a plain mkdir would.
const CREATED_DIR_MODE: u32 = 0o700;

/// The setgid bit, which a directory inherits from a setgid parent.
const SETGID: u32 = libc::S_ISGID;

/// Statfs magics from linux/magic.h that libc does not export.
const RAMFS_MAGIC: FsType = FsType(0x8584_58f6);
const SMB2_MAGIC_NUMBER: FsType = FsType(0xfe53_4d42);
const CIFS_MAGIC_NUMBER: FsType = FsType(0xff53_4d42);
const V9FS_MAGIC: FsType = FsType(0x0102_1997);

/// A write through a shared mapping changes no timestamp on these, even after an fsync.
const MEMORY_REASON: &str =
    "a write through a shared mapping there changes no timestamp, even after an fsync";
/// The kernel serves a FUSE file's size and times from what it cached, and keeps the
/// change time of a regular file local: fcvm's own writeback-cache mounts keep the size
/// and modification time a guest first saw.
const FUSE_REASON: &str = "the kernel serves a FUSE file's size and times from its cache, so \
                           a write through another mount or by the filesystem's server need \
                           not change them";
/// A network client caches attributes, so another client's write can be invisible.
const NETWORK_REASON: &str = "a client caches the file's attributes, so a write by another \
                              client need not change the size and times fcvm reads";
/// statfs on an overlayfs file reports overlayfs, so fcvm cannot check the layer under it.
const OVERLAY_REASON: &str = "statfs reports overlayfs, not the layer that holds the file, \
                              and that layer can be tmpfs or a network filesystem";

/// Filesystems fcvm recognizes whose timestamps cannot show it a write to an image, or that
/// hide the filesystem that holds it: statfs magic, name and why. FUSE_SUPER_MAGIC also
/// covers fuseblk and virtiofs. Others with the same problem (Ceph, AFS, Lustre, GPFS) are
/// not listed and pass.
const UNVERIFIABLE_FILESYSTEMS: &[(FsType, &str, &str)] = &[
    (nix::sys::statfs::TMPFS_MAGIC, "tmpfs", MEMORY_REASON),
    (RAMFS_MAGIC, "ramfs", MEMORY_REASON),
    (
        nix::sys::statfs::HUGETLBFS_MAGIC,
        "hugetlbfs",
        MEMORY_REASON,
    ),
    (nix::sys::statfs::FUSE_SUPER_MAGIC, "FUSE", FUSE_REASON),
    (nix::sys::statfs::NFS_SUPER_MAGIC, "NFS", NETWORK_REASON),
    (nix::sys::statfs::SMB_SUPER_MAGIC, "SMB", NETWORK_REASON),
    (SMB2_MAGIC_NUMBER, "SMB2", NETWORK_REASON),
    (CIFS_MAGIC_NUMBER, "CIFS", NETWORK_REASON),
    (V9FS_MAGIC, "9p", NETWORK_REASON),
    (
        nix::sys::statfs::OVERLAYFS_SUPER_MAGIC,
        "overlayfs",
        OVERLAY_REASON,
    ),
];

/// How a temp got the image's bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CopyMethod {
    /// FICLONE: the entry shares the source's extents until the source is rewritten.
    Reflink,
    /// pread/pwrite over the source's data runs, so holes stay holes.
    Bytes,
}

/// The pmem image store of one data_dir.
#[derive(Debug)]
pub struct PmemStore {
    /// `<data_dir>/pmem`, canonical, so an entry is recognized by its parent directory
    /// whatever symlinks the configured data_dir goes through.
    dir: PathBuf,
    /// The store directory, opened without following a symlink.
    pmem: File,
    /// `tmp` under the store directory, opened the same way.
    tmp: File,
    /// The user who ran fcvm under sudo: what the store creates is handed to them, and this
    /// run can use their entries (`check_entry_owner`).
    invoker: Option<User>,
}

impl PmemStore {
    /// Open the store under `data_dir`, creating it and its `tmp` directory if needed and
    /// refusing a symlink or another file in place of either, and a directory that took the
    /// place of one it created. A directory that was already there is used as it is and
    /// keeps its owner.
    ///
    /// Run as root under sudo, it hands the directories it creates to the invoking user, as
    /// the other stores do, so a later rootless run of that user can still create its temps
    /// there. Each is made under a temporary name, checked, given its final mode and handed
    /// over, and renamed onto its own name only once fstat shows it is the invoker's; the
    /// store directory gets its `tmp` before that. A rootless run racing this one therefore
    /// finds either nothing or a directory it can use. When another run publishes a
    /// directory first, this run removes its own and opens that one, and it removes its own
    /// too when any other step fails, unless the directory fails the check or cannot be
    /// opened. A crash in between leaves an empty directory
    /// under the temporary name, which nothing removes.
    ///
    /// Each directory it opens, one it creates or publishes and one it finds, is synced into
    /// the directory that holds it before it returns, so a host crash cannot remove the store,
    /// or its `tmp`, while a snapshot that names an entry in it survives. A directory another
    /// run made a moment earlier may not be synced yet.
    ///
    /// The rename uses RENAME_NOREPLACE, which the Linux NFS client refuses with EINVAL and a
    /// FUSE server without RENAME2 cannot do, and the hand-over needs a filesystem where root
    /// can chown. Where either is missing, a run under sudo cannot create the store: one run
    /// without sudo, with the same data_dir, creates its directories in place, and later runs
    /// under sudo use them. Without sudo nothing is handed over and the directories are
    /// created at their own names.
    pub fn open(data_dir: &Path) -> Result<Self> {
        Self::open_with(data_dir, crate::setup::sudo_invoker(), CreateSteps::REAL)
    }

    /// `open` for the sudo invoker `invoker`, calling `steps` while it creates directories.
    fn open_with(data_dir: &Path, invoker: Option<&User>, steps: CreateSteps<'_>) -> Result<Self> {
        std::fs::create_dir_all(data_dir)
            .with_context(|| format!("creating data directory {}", data_dir.display()))?;
        let data = open_data_dir(data_dir)?
            .with_context(|| format!("data directory {} vanished", data_dir.display()))?;
        let pmem_path = data_dir.join("pmem");
        // A store directory this call creates gets its tmp before it is published.
        let (pmem, _) =
            open_or_create_dir(&data, "pmem", &pmem_path, invoker, steps, |pmem, at| {
                open_or_create_dir(pmem, "tmp", &at.join("tmp"), invoker, steps, leave_empty)
                    .map(drop)
            })?;
        // tmp is opened by name either way: a store directory this call did not create can
        // still lack it, because another run created that directory in place and has not
        // made its tmp yet.
        let tmp_path = pmem_path.join("tmp");
        let (tmp, _) = open_or_create_dir(&pmem, "tmp", &tmp_path, invoker, steps, leave_empty)?;
        let dir = canonical_store_dir(data_dir, &pmem, Some(&tmp))?;
        Ok(Self {
            dir,
            pmem,
            tmp,
            invoker: invoker.cloned(),
        })
    }

    /// The canonical path of the store directory under `data_dir`, or `None` when there is
    /// none. Creates nothing, so a restore check leaves a data_dir without a store as it
    /// found it. A symlink or another file in its place is refused, as `open` refuses it.
    pub fn existing_dir(data_dir: &Path) -> Result<Option<PathBuf>> {
        let Some(data) = open_data_dir(data_dir)? else {
            return Ok(None);
        };
        let Some(pmem) = open_existing_dir(&data, "pmem", &data_dir.join("pmem"))? else {
            return Ok(None);
        };
        canonical_store_dir(data_dir, &pmem, None).map(Some)
    }

    /// The store directory, canonical.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The name of the entry a source generation is published as.
    fn entry_name(source: &Path, identity: &str) -> String {
        use std::os::unix::ffi::OsStrExt;
        let mut hasher = Sha256::new();
        hasher.update(NAME_DOMAIN);
        hasher.update([0u8]);
        hasher.update(source.as_os_str().as_bytes());
        hasher.update([0u8]);
        hasher.update(identity.as_bytes());
        format!("{}.img", hex::encode(hasher.finalize()))
    }

    /// Whether `path` names an entry of this store (`is_entry_of`).
    pub fn is_entry(&self, path: &Path) -> bool {
        is_entry_of(&self.dir, path)
    }

    /// The path Firecracker is given for `device`: its store entry. Anything outside the
    /// store is refused, so a VM never maps the file the user named.
    pub fn attach_path(&self, device: &PmemDevice) -> Result<PathBuf> {
        let path = PathBuf::from(&device.path);
        ensure!(
            self.is_entry(&path),
            "refusing to attach pmem image {} (from {}, mounted at {}): it is not an entry of \
             the pmem image store {}",
            device.path,
            device.source,
            device.mount_path,
            self.dir.display()
        );
        Ok(path)
    }

    /// Check that an existing entry is what this store publishes: a regular file, mode
    /// 0400, of the image's length, owned by a user whose entry Firecracker can open
    /// (`check_entry_owner`), and readable by this process. Anything else is refused and
    /// left alone, since a snapshot may name that inode. An entry another user published
    /// under a shared data_dir is refused here, under sudo too: root could read it, but
    /// Firecracker in a VM's holder namespace could not.
    pub fn check_entry(&self, entry: &Path, len: Option<u64>) -> Result<std::fs::Metadata> {
        let metadata = std::fs::symlink_metadata(entry)
            .with_context(|| format!("reading pmem store entry {}", entry.display()))?;
        let mode = metadata.permissions().mode() & 0o7777;
        ensure!(
            metadata.is_file() && mode == ENTRY_MODE && len.is_none_or(|len| metadata.len() == len),
            "pmem store entry {} is not a mode 0400 regular file{}, so fcvm did not publish \
             it; remove it and run again",
            entry.display(),
            len.map(|len| format!(" of {len} bytes"))
                .unwrap_or_default()
        );
        crate::storage::pmem::check_image_metadata(&metadata, entry)?;
        check_entry_owner(
            entry,
            (metadata.uid(), metadata.gid()),
            &self.dir,
            RunIds::of_run(self.invoker.as_ref()),
        )?;
        let euid = nix::unistd::geteuid().as_raw();
        if let Err(errno) = nix::unistd::faccessat(
            nix::fcntl::AT_FDCWD,
            entry,
            nix::unistd::AccessFlags::R_OK,
            AtFlags::AT_EACCESS | AtFlags::AT_SYMLINK_NOFOLLOW,
        ) {
            return Err(errno).with_context(|| {
                format!(
                    "checking that uid {euid} can read pmem store entry {}",
                    entry.display()
                )
            });
        }
        Ok(metadata)
    }

    /// Return the entry holding the image at `source` (a canonical path) as it is now,
    /// copying it into the store first unless that generation is already published. The store
    /// directory is synced before the entry is returned, one this run published or one it
    /// found, so a host crash cannot remove an entry a VM or a snapshot of this run names.
    pub fn ingest(&self, source: &Path) -> Result<PathBuf> {
        self.ingest_with(source, || Ok(()), &sync_parent)
    }

    /// `ingest`, running `after_copy` between the copy and the second fstat of the source and
    /// syncing the store directory through `sync` (`SyncStep`).
    fn ingest_with(
        &self,
        source: &Path,
        after_copy: impl FnOnce() -> Result<()>,
        sync: SyncStep<'_>,
    ) -> Result<PathBuf> {
        let started = std::time::Instant::now();
        // O_NONBLOCK keeps a FIFO named by mistake from blocking the open; it has no
        // effect on a regular file, and anything else is refused below.
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(source)
            .with_context(|| format!("opening --pmem image {}", source.display()))?;
        refuse_unverifiable_filesystem(&file, source)?;
        let before = file
            .metadata()
            .with_context(|| format!("reading --pmem image {}", source.display()))?;
        crate::storage::pmem::check_image_metadata(&before, source)?;
        let identity = crate::storage::pmem::image_identity(&before);
        let name = Self::entry_name(source, &identity);
        let entry = self.dir.join(&name);
        if std::fs::symlink_metadata(&entry).is_ok() {
            self.check_entry(&entry, Some(before.len()))?;
            // Another run links an entry before it syncs the store directory, so the name
            // found here need not survive a crash yet.
            make_name_durable(sync, &self.pmem, &entry)?;
            info!(
                source = %source.display(),
                entry = %entry.display(),
                "using the pmem store's copy of this image generation"
            );
            return Ok(entry);
        }

        sync_source(&file, source)?;
        let _lock = self.lock_shared()?;
        let temp = TempFile::create(&self.tmp, &self.dir.join("tmp"))?;
        let out = &temp.file;
        let method = copy_image(&file, out, before.len()).with_context(|| {
            format!(
                "copying --pmem image {} into {}",
                source.display(),
                temp.path.display()
            )
        })?;
        after_copy()?;
        let after = file
            .metadata()
            .with_context(|| format!("reading --pmem image {}", source.display()))?;
        let identity_after = crate::storage::pmem::image_identity(&after);
        ensure!(
            identity_after == identity,
            "--pmem image {} changed while fcvm copied it (inode:length:mtime:ctime {identity} \
             then, {identity_after} now); stop writing it and run again",
            source.display()
        );

        out.set_permissions(std::fs::Permissions::from_mode(ENTRY_MODE))
            .with_context(|| format!("making {} read-only", temp.path.display()))?;
        crate::setup::give_store_fd_to(out, &temp.path, self.invoker.as_ref());
        out.sync_all()
            .with_context(|| format!("syncing {}", temp.path.display()))?;
        match nix::unistd::linkat(
            &self.tmp,
            temp.name.as_str(),
            &self.pmem,
            name.as_str(),
            AtFlags::empty(),
        ) {
            Ok(()) => {}
            // Another run published this generation first: use its inode, never replace it.
            // The sync below makes its name durable, which that run may not have done yet.
            Err(Errno::EEXIST) => {
                self.check_entry(&entry, Some(before.len()))?;
            }
            Err(errno) => {
                return Err(errno).with_context(|| format!("publishing {}", entry.display()))
            }
        }
        make_name_durable(sync, &self.pmem, &entry)?;
        drop(temp);
        let allocated = std::fs::metadata(&entry)
            .map(|m| m.blocks() * 512)
            .unwrap_or(0);
        info!(
            source = %source.display(),
            entry = %entry.display(),
            method = ?method,
            bytes = before.len(),
            allocated,
            elapsed_ms = started.elapsed().as_millis() as u64,
            "copied --pmem image into the pmem store"
        );
        Ok(entry)
    }

    /// Hold `.lock` shared. Ingest holds it from creating its temp until it removes it.
    fn lock_shared(&self) -> Result<Flock<File>> {
        let path = self.dir.join(".lock");
        let (file, created) = open_lock(&self.pmem, &path)?;
        if created {
            crate::setup::give_store_fd_to(&file, &path, self.invoker.as_ref());
        }
        Flock::lock(file, FlockArg::LockShared)
            .map_err(|(_, errno)| errno)
            .with_context(|| format!("locking {}", path.display()))
    }
}

/// Whether `path` names an entry of the store directory `dir`: its parent is `dir` and its
/// name is 64 lowercase hex digits and `.img`. Compared as written, so callers pass
/// canonical paths.
pub fn is_entry_of(dir: &Path, path: &Path) -> bool {
    path.parent() == Some(dir)
        && path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_suffix(".img"))
            .is_some_and(|hex| {
                hex.len() == 64 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
            })
}

/// The ids that decide which store entries a run can use: the run's real uid and gid, which
/// a VM's holder namespace maps at 0, and in a root run the sudo invoker's uid and gid.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RunIds {
    pub(crate) own: (u32, u32),
    pub(crate) invoker: Option<(u32, u32)>,
}

impl RunIds {
    /// This process's real uid and gid, with the sudo invoker `invoker`.
    pub(crate) fn of_run(invoker: Option<&User>) -> Self {
        Self {
            own: (
                nix::unistd::getuid().as_raw(),
                nix::unistd::getgid().as_raw(),
            ),
            invoker: invoker.map(|user| (user.uid.as_raw(), user.gid.as_raw())),
        }
    }
}

/// Refuse the store entry `entry`, owned by `owner` (uid, gid), unless Firecracker in a VM of
/// the run `run` can open it (`entry_owner_is_mapped`). Publishing, finding and restoring an
/// entry all ask here, so they cannot disagree. `store_dir` names the store in messages. The
/// message says why: the invoker's entry with a group the namespace does not map, root's
/// entry in a run without root, another user's entry in a root run without a sudo invoker,
/// or another user's entry.
pub(crate) fn check_entry_owner(
    entry: &Path,
    owner: (u32, u32),
    store_dir: &Path,
    run: RunIds,
) -> Result<()> {
    if entry_owner_is_mapped(owner, run.own, run.invoker) {
        return Ok(());
    }
    let (owner_uid, owner_gid) = owner;
    let (uid, gid) = run.own;
    if let Some((invoker_uid, invoker_gid)) = run.invoker.filter(|(id, _)| *id == owner_uid) {
        bail!(
            "pmem store entry {} belongs to the sudo invoker, uid {invoker_uid}, with gid \
             {owner_gid}: Firecracker in a VM's namespace can open that user's entry only with \
             gid {invoker_gid} or {gid}, the groups the namespace maps. A setgid directory or \
             a run under another group gave it this one; use a data_dir of your own without \
             setgid directories (FCVM_DATA_DIR or paths.data_dir)",
            entry.display()
        );
    }
    if owner_uid == 0 {
        bail!(
            "pmem store entry {} belongs to root, so Firecracker as uid {uid} could not open \
             it: a run as root published it and did not hand it to a sudo invoker (it had \
             none, or the hand-over failed, which fcvm only logs). Remove it and run again",
            entry.display()
        );
    }
    if uid == 0 && run.invoker.is_none() {
        bail!(
            "uid 0 cannot use pmem store entry {}, which belongs to uid {owner_uid}: this run \
             as root has no sudo invoker (SUDO_USER is unset), so a VM's holder namespace maps \
             only root, where Firecracker cannot open another user's mode 0400 file. Run fcvm \
             as uid {owner_uid}, under sudo or without it",
            entry.display()
        );
    }
    bail!(
        "uid {uid} cannot use pmem store entry {}, which belongs to uid {owner_uid}: the pmem \
         image store {} under this data_dir holds another user's copies, which Firecracker \
         could not open. Use a data_dir of your own (FCVM_DATA_DIR or paths.data_dir)",
        entry.display(),
        store_dir.display()
    );
}

/// Whether Firecracker can open a mode 0400 store entry owned by `owner` (uid, gid) in a run
/// whose real uid and gid are `own`, with the sudo invoker `invoker` (uid, gid).
///
/// Firecracker runs as the run's uid, so an entry of that uid is readable through its owner
/// bits whatever its group. Any other owner needs the capability that overrides file
/// permissions, which inside a VM's holder namespace applies only to a file whose uid and gid
/// the namespace maps. The rule asks the holder's own maps (`holder_maps_id` in
/// commands/common.rs), the entry's uid against the uid map and its gid against the gid map.
/// A root run's holder maps the run's ids and the invoker's, so an entry of the invoker is
/// readable only with the invoker's group or the run's, and a run without an invoker can use
/// only its own entries. A bridged root run starts Firecracker in no user namespace, where
/// root could read any entry; the rule is the same there, so a run can use an entry in every
/// network mode or in none.
pub(crate) fn entry_owner_is_mapped(
    owner: (u32, u32),
    own: (u32, u32),
    invoker: Option<(u32, u32)>,
) -> bool {
    use crate::commands::common::holder_maps_id;
    let (uid, gid) = owner;
    uid == own.0
        || (holder_maps_id(own.0, invoker.map(|(id, _)| id), uid)
            && holder_maps_id(own.1, invoker.map(|(_, id)| id), gid))
}

/// Flags that open a store directory: only a directory, and never through a symlink.
fn directory_flags() -> OFlag {
    OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC
}

/// Open the configured data_dir, following symlinks in its path, or `None` when it does
/// not exist.
fn open_data_dir(data_dir: &Path) -> Result<Option<File>> {
    match nix::fcntl::open(
        data_dir,
        OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC,
        Mode::empty(),
    ) {
        Ok(fd) => Ok(Some(File::from(fd))),
        Err(Errno::ENOENT) => Ok(None),
        Err(errno) => {
            Err(errno).with_context(|| format!("opening data directory {}", data_dir.display()))
        }
    }
}

/// Open the directory `name` under `parent`, or `None` when nothing is there. A symlink or
/// another file there is refused. `path` names it in messages.
fn open_existing_dir(parent: &File, name: &str, path: &Path) -> Result<Option<File>> {
    match nix::fcntl::openat(parent, name, directory_flags(), Mode::empty()) {
        Ok(fd) => Ok(Some(File::from(fd))),
        Err(Errno::ENOENT) => Ok(None),
        Err(Errno::ELOOP | Errno::ENOTDIR) => bail!(
            "{} is not a directory (a symlink or another file is there); the pmem image \
             store is made of directories fcvm creates, so remove it and run again",
            path.display()
        ),
        Err(errno) => Err(errno).with_context(|| format!("opening {}", path.display())),
    }
}

/// What `PmemStore::open_with` calls while it creates the store's directories, so a test can
/// look at what is visible then, act as another process, or make a step fail.
#[derive(Clone, Copy)]
struct CreateSteps<'a> {
    /// Called with the path of a directory mkdirat just made, before the run opens it.
    created: &'a dyn Fn(&Path),
    /// Gives a directory this run made, at the path given, its final mode.
    set_mode: &'a dyn Fn(&File, &Path, u32) -> std::io::Result<()>,
    /// Hands a directory this run made, at the path given, to the sudo invoker.
    hand_over: &'a dyn Fn(&File, &Path, &User),
    /// Called with a name's path just before a directory that is checked, populated, has its
    /// final mode and belongs to the invoker is renamed onto that name.
    publishing: &'a dyn Fn(&Path),
    /// Syncs the directory that holds a store directory this run opened, created or found.
    sync_parent: SyncStep<'a>,
}

impl CreateSteps<'static> {
    /// The steps `PmemStore::open` passes: no hooks, and the real mode change and hand-over.
    const REAL: Self = Self {
        created: &ignore_path,
        set_mode: &set_dir_mode,
        hand_over: &hand_dir_to,
        publishing: &ignore_path,
        sync_parent: &sync_parent,
    };
}

fn ignore_path(_: &Path) {}

/// Syncs the directory given, which holds the path given, so that name survives a host
/// crash: `sync_parent`, except in tests. A sync guards against a crash between the name
/// appearing and the directory reaching the disk, which a unit test cannot cause, so the
/// tests pass a step that records each call and check which directories were synced, when,
/// and that a failed sync fails the run.
type SyncStep<'a> = &'a dyn Fn(&File, &Path) -> std::io::Result<()>;

/// fsync the directory `parent`, so the name `child` it holds survives a host crash.
fn sync_parent(parent: &File, _child: &Path) -> std::io::Result<()> {
    parent.sync_all()
}

/// Make the name `child` durable in the directory `parent` that holds it, through `sync`,
/// before anything relies on it: a VM that maps an entry, or a snapshot whose metadata names
/// one, must not outlive the name in a host crash.
fn make_name_durable(sync: SyncStep<'_>, parent: &File, child: &Path) -> Result<()> {
    sync(parent, child)
        .with_context(|| format!("syncing the directory that holds {}", child.display()))
}

/// Give the directory `dir` the mode `mode`.
fn set_dir_mode(dir: &File, _: &Path, mode: u32) -> std::io::Result<()> {
    dir.set_permissions(std::fs::Permissions::from_mode(mode))
}

/// Hand the directory `dir`, at `path`, to `invoker`. A failure is only logged
/// (`setup::give_store_fd_to`); `hand_over_new_dir` checks the result.
fn hand_dir_to(dir: &File, path: &Path, invoker: &User) {
    crate::setup::give_store_fd_to(dir, path, Some(invoker));
}

/// The `populate` of a directory that holds nothing when it is published.
fn leave_empty(_: &File, _: &Path) -> Result<()> {
    Ok(())
}

/// Open the directory `name` under `parent`, creating it first if it is absent, and report
/// whether this call created it, so only a directory the run created is handed back. A
/// symlink or another file there is refused, and so is a directory that took the place of
/// the one this call created (`finish_created_dir`). `populate` runs on a directory this call
/// created, with the path it has then, before the directory is returned.
///
/// The directory is synced into `parent` before it is returned, whether this call created it
/// or found it (`make_name_durable`), so a host crash cannot remove it once the run has put
/// anything in it. A found one needs the sync too: the run that made it syncs `parent` only
/// after the name appears there, and may not have done so yet.
///
/// With a sudo `invoker` the directory is made under a temporary name and published at
/// `name` only once it belongs to the invoker (`open_or_publish_dir`). When another run
/// publishes `name` first, that directory is opened instead and reported as not created.
/// Without one it is made at `name` (`create_dir_in_place`).
fn open_or_create_dir(
    parent: &File,
    name: &str,
    path: &Path,
    invoker: Option<&User>,
    steps: CreateSteps<'_>,
    populate: impl FnOnce(&File, &Path) -> Result<()>,
) -> Result<(File, bool)> {
    let (dir, created) = match invoker {
        Some(invoker) => open_or_publish_dir(parent, name, path, invoker, steps, populate)?,
        None => create_dir_in_place(parent, name, path, steps, populate)?,
    };
    make_name_durable(steps.sync_parent, parent, path)?;
    Ok((dir, created))
}

/// `open_or_create_dir` for the sudo invoker `invoker`: open the directory at `name`, or
/// publish a new one there (`publish_new_dir`), or open the one another run published there
/// first.
fn open_or_publish_dir(
    parent: &File,
    name: &str,
    path: &Path,
    invoker: &User,
    steps: CreateSteps<'_>,
    populate: impl FnOnce(&File, &Path) -> Result<()>,
) -> Result<(File, bool)> {
    if let Some(dir) = open_existing_dir(parent, name, path)? {
        return Ok((dir, false));
    }
    if let Some(dir) = publish_new_dir(parent, name, path, invoker, steps, populate)? {
        return Ok((dir, true));
    }
    let dir = open_existing_dir(parent, name, path)?
        .with_context(|| format!("{} was removed while fcvm opened it", path.display()))?;
    Ok((dir, false))
}

/// `open_or_create_dir` without an invoker: mkdirat at `name`, then open it. Nothing is
/// handed back, so a directory put at the name in between is used as found
/// (`finish_created_dir` without its check).
fn create_dir_in_place(
    parent: &File,
    name: &str,
    path: &Path,
    steps: CreateSteps<'_>,
    populate: impl FnOnce(&File, &Path) -> Result<()>,
) -> Result<(File, bool)> {
    let mode = Mode::from_bits_truncate(CREATED_DIR_MODE);
    let created = match nix::sys::stat::mkdirat(parent, name, mode) {
        Ok(()) => true,
        Err(Errno::EEXIST) => false,
        Err(errno) => return Err(errno).with_context(|| format!("creating {}", path.display())),
    };
    if created {
        (steps.created)(path);
    }
    let dir = open_existing_dir(parent, name, path)?
        .with_context(|| format!("{} was removed while fcvm opened it", path.display()))?;
    if created {
        finish_created_dir(parent, &dir, path, false, steps).map_err(FinishError::into_error)?;
        populate(&dir, path)?;
    }
    Ok((dir, created))
}

/// Create the directory `name` under `parent` for the sudo invoker `invoker` and publish it,
/// or return `None` when another run published `name` first.
///
/// The directory is made under a temporary name next to `name`, checked
/// (`finish_created_dir`), populated, handed to the invoker and checked to be theirs
/// (`hand_over_new_dir`), and only then renamed onto `name` with RENAME_NOREPLACE, which never
/// replaces what another run published (`rename_new_dir`). A rootless run of the invoker
/// therefore never finds a directory at `name` that root still owns with mode 0700, which it
/// could neither open nor write. The descriptor returned is of the directory this run made;
/// `canonical_store_dir` refuses the store if another directory took its name meanwhile.
///
/// A directory this run made and did not publish is removed again (`discard_unpublished_dir`),
/// whichever step failed, reading the umask and the fstat included. One that fails the check,
/// or that cannot be opened, is left as it was found: it may not be the one this run made.
fn publish_new_dir(
    parent: &File,
    name: &str,
    path: &Path,
    invoker: &User,
    steps: CreateSteps<'_>,
    populate: impl FnOnce(&File, &Path) -> Result<()>,
) -> Result<Option<File>> {
    let temp_name = format!(".{name}-creating-{}", uuid::Uuid::new_v4().simple());
    let temp_path = path.with_file_name(&temp_name);
    let mode = Mode::from_bits_truncate(CREATED_DIR_MODE);
    nix::sys::stat::mkdirat(parent, temp_name.as_str(), mode)
        .with_context(|| format!("creating {}", temp_path.display()))?;
    (steps.created)(&temp_path);
    let dir = open_existing_dir(parent, &temp_name, &temp_path)?
        .with_context(|| format!("{} was removed while fcvm created it", temp_path.display()))?;
    let published = match finish_created_dir(parent, &dir, &temp_path, true, steps) {
        Err(FinishError::NotCreated(error)) => return Err(error),
        Err(FinishError::Failed(error)) => Err(error),
        Ok(()) => populate(&dir, &temp_path)
            .and_then(|()| hand_over_new_dir(&dir, &temp_path, path, invoker, steps))
            .and_then(|()| {
                (steps.publishing)(path);
                rename_new_dir(parent, &temp_name, &temp_path, name, path)
            }),
    };
    match published {
        Ok(true) => Ok(Some(dir)),
        Ok(false) => {
            discard_unpublished_dir(parent, &temp_name, &dir, &temp_path);
            Ok(None)
        }
        Err(error) => {
            discard_unpublished_dir(parent, &temp_name, &dir, &temp_path);
            Err(error)
        }
    }
}

/// Hand the directory `dir`, made at `temp_path` to be published at `path`, to the sudo
/// invoker, then check through the descriptor that it is theirs. A failed chown is only
/// logged, and a directory published while root still owns it with mode 0700 would keep
/// every rootless run of the invoker out of the store.
fn hand_over_new_dir(
    dir: &File,
    temp_path: &Path,
    path: &Path,
    invoker: &User,
    steps: CreateSteps<'_>,
) -> Result<()> {
    (steps.hand_over)(dir, temp_path, invoker);
    let metadata = dir
        .metadata()
        .with_context(|| format!("reading {}", temp_path.display()))?;
    let wanted = (invoker.uid.as_raw(), invoker.gid.as_raw());
    ensure!(
        (metadata.uid(), metadata.gid()) == wanted,
        "could not hand {} to the sudo invoker {} (uid {}, gid {}): it still belongs to uid \
         {}, gid {}, so fcvm did not publish it as {}. Where root cannot chown, a run under \
         sudo cannot create the pmem image store: one run without sudo, with the same \
         data_dir, creates it in place, and later runs under sudo use it",
        temp_path.display(),
        invoker.name,
        wanted.0,
        wanted.1,
        metadata.uid(),
        metadata.gid(),
        path.display()
    );
    Ok(())
}

/// Rename the directory `temp_name` under `parent` onto `name` without replacing anything,
/// and report whether it was renamed: `false` when another run published `name` first.
/// `temp_path` and `path` name the two in messages.
fn rename_new_dir(
    parent: &File,
    temp_name: &str,
    temp_path: &Path,
    name: &str,
    path: &Path,
) -> Result<bool> {
    match nix::fcntl::renameat2(
        parent,
        temp_name,
        parent,
        name,
        RenameFlags::RENAME_NOREPLACE,
    ) {
        Ok(()) => Ok(true),
        Err(Errno::EEXIST) => Ok(false),
        Err(Errno::EINVAL) => Err(anyhow!(
            "cannot create {}: its filesystem refuses a rename that does not replace \
             (RENAME_NOREPLACE; the Linux NFS client and a FUSE server without RENAME2 do), \
             which fcvm needs to create the pmem image store under sudo. One run without sudo, \
             with the same data_dir, creates the store in place, and later runs under sudo use \
             it",
            path.display()
        )),
        Err(errno) => Err(errno)
            .with_context(|| format!("renaming {} onto {}", temp_path.display(), path.display())),
    }
}

/// Remove the directory `name` under `parent`, which `dir` holds and this run made but did not
/// publish, with the directories `populate` made in it. Only empty directories are removed. A
/// failure is logged and leaves the directory, which nothing removes later.
fn discard_unpublished_dir(parent: &File, name: &str, dir: &File, path: &Path) {
    let removed = (|| -> Result<()> {
        for child in entries(dir, path)? {
            let child = child?;
            nix::unistd::unlinkat(dir, child.as_os_str(), UnlinkatFlags::RemoveDir)
                .with_context(|| format!("removing {}", path.join(&child).display()))?;
        }
        nix::unistd::unlinkat(parent, name, UnlinkatFlags::RemoveDir)
            .with_context(|| format!("removing {}", path.display()))
    })();
    if let Err(error) = removed {
        warn!(
            dir = %path.display(),
            error = format!("{error:#}"),
            "could not remove a pmem store directory this run did not publish"
        );
    }
}

/// Check that `dir`, opened at the name `parent`'s mkdirat just created, is still the
/// directory that mkdirat made, then give it the mode a plain mkdir gives: 0777 less the
/// umask, and the setgid bit if it inherited one.
///
/// Between the mkdirat and the open, whoever owns `parent` can put another directory at
/// the name. Under sudo that is the invoking user, and the run would hand them, as one it
/// created, any directory they can write but do not own, such as another user's shared
/// sticky directory. The directory mkdirat made is owned by the effective uid, has the
/// effective gid or the parent's (a setgid parent or a grpid mount gives the parent's),
/// has `CREATED_DIR_MODE` less the umask, a setgid bit only under a setgid parent, and no
/// entries. Anything else is refused.
///
/// The check runs only with `verify`, which `publish_new_dir` sets: it hands the directory
/// to the sudo invoker. `create_dir_in_place` runs without an invoker, where nothing is
/// chowned, so a directory put there in between grants nobody anything, and a filesystem
/// that gives a new directory another owner (fuse-pipe in a nested guest, NFS with
/// root_squash) keeps working.
///
/// A directory that fails the check is `FinishError::NotCreated`. Failing to read the umask,
/// the directory, its parent or its entries, or to set the mode through `steps.set_mode`, is
/// `FinishError::Failed`.
fn finish_created_dir(
    parent: &File,
    dir: &File,
    path: &Path,
    verify: bool,
    steps: CreateSteps<'_>,
) -> std::result::Result<(), FinishError> {
    let changed = |why: String| {
        FinishError::NotCreated(anyhow!(
            "{} changed while fcvm created it: {why}. fcvm has not used it or handed it to \
             anyone; find out what put it there before running again",
            path.display()
        ))
    };
    let umask = process_umask().map_err(FinishError::Failed)?;
    let metadata = dir
        .metadata()
        .with_context(|| format!("reading {}", path.display()))
        .map_err(FinishError::Failed)?;
    let mode = metadata.mode() & 0o7777;
    if verify {
        let parent_metadata = parent
            .metadata()
            .with_context(|| format!("reading the parent of {}", path.display()))
            .map_err(FinishError::Failed)?;
        let euid = nix::unistd::geteuid().as_raw();
        if metadata.uid() != euid {
            return Err(changed(format!(
                "it is owned by uid {}, not {euid}",
                metadata.uid()
            )));
        }
        let egid = nix::unistd::getegid().as_raw();
        if metadata.gid() != egid && metadata.gid() != parent_metadata.gid() {
            return Err(changed(format!(
                "its group is gid {}, neither {egid} nor its parent's {}",
                metadata.gid(),
                parent_metadata.gid()
            )));
        }
        let expected = CREATED_DIR_MODE & !umask;
        let setgid_allowed = parent_metadata.mode() & SETGID;
        if mode & !SETGID != expected || mode & SETGID & !setgid_allowed != 0 {
            return Err(changed(format!(
                "its mode is {mode:04o}, not the {expected:04o} fcvm created it with"
            )));
        }
        let first = entries(dir, path)
            .and_then(|mut listing| listing.next().transpose())
            .map_err(FinishError::Failed)?;
        if let Some(entry) = first {
            return Err(changed(format!("it holds {entry:?}")));
        }
    }
    let final_mode = (0o777 & !umask) | (mode & SETGID);
    (steps.set_mode)(dir, path, final_mode)
        .with_context(|| format!("setting the mode of {} to {final_mode:04o}", path.display()))
        .map_err(FinishError::Failed)
}

/// Why `finish_created_dir` failed.
enum FinishError {
    /// The directory at the name is not the one mkdirat made, so the run leaves it as found.
    NotCreated(anyhow::Error),
    /// Reading the umask, the directory, its parent or its entries, or setting its mode,
    /// failed.
    Failed(anyhow::Error),
}

impl FinishError {
    fn into_error(self) -> anyhow::Error {
        match self {
            Self::NotCreated(error) | Self::Failed(error) => error,
        }
    }
}

/// The process's umask, from the `Umask:` line of /proc/self/status. umask(2) reads it only
/// by replacing it, which would change the mode of files other threads create meanwhile.
fn process_umask() -> Result<u32> {
    let status =
        std::fs::read_to_string("/proc/self/status").context("reading /proc/self/status")?;
    let field = status
        .lines()
        .find_map(|line| line.strip_prefix("Umask:"))
        .context("/proc/self/status has no Umask line")?;
    u32::from_str_radix(field.trim(), 8).with_context(|| format!("parsing the umask {field:?}"))
}

/// The entries of the directory `dir` other than `.` and `..`, read as they are needed
/// through a new descriptor of the same directory, so the listing is of the directory this
/// run holds.
fn entries<'a>(dir: &File, path: &'a Path) -> Result<impl Iterator<Item = Result<OsString>> + 'a> {
    let listing = nix::dir::Dir::openat(dir, ".", directory_flags(), Mode::empty())
        .with_context(|| format!("opening {} to list it", path.display()))?;
    Ok(listing.into_iter().filter_map(move |entry| match entry {
        Ok(entry) => {
            let name = entry.file_name().to_bytes();
            (name != b"." && name != b"..").then(|| Ok(OsStr::from_bytes(name).to_owned()))
        }
        Err(errno) => Some(Err(errno).with_context(|| format!("listing {}", path.display()))),
    }))
}

/// The canonical path of the store directory `pmem`, opened under `data_dir`. The path is
/// checked against the descriptor, so it names the directory this run holds, and so is the
/// path of its `tmp` when the descriptor `tmp` is given.
fn canonical_store_dir(data_dir: &Path, pmem: &File, tmp: Option<&File>) -> Result<PathBuf> {
    let dir = data_dir
        .canonicalize()
        .with_context(|| format!("resolving data directory {}", data_dir.display()))?
        .join("pmem");
    check_held(&dir, pmem, "the pmem image store")?;
    if let Some(tmp) = tmp {
        check_held(
            &dir.join("tmp"),
            tmp,
            "the pmem image store's temp directory",
        )?;
    }
    Ok(dir)
}

/// Check that `path`, not followed if it is a symlink, is the directory `held`. `what` names
/// it in messages.
fn check_held(path: &Path, held: &File, what: &str) -> Result<()> {
    let at_path = std::fs::symlink_metadata(path)
        .with_context(|| format!("reading {what} {}", path.display()))?;
    let held = held
        .metadata()
        .with_context(|| format!("reading {what} {}", path.display()))?;
    ensure!(
        (at_path.dev(), at_path.ino()) == (held.dev(), held.ino()),
        "{what} {} was replaced while fcvm opened it; run again",
        path.display()
    );
    Ok(())
}

/// Open `.lock` in the store directory `dir`, creating it if it is absent, and report
/// whether this call created it. Only a lock this call created is handed back: one that was
/// already there can be a hard link to a file its planter does not own. Anything but a
/// regular file is refused, and O_NONBLOCK keeps a FIFO from blocking the open.
fn open_lock(dir: &File, path: &Path) -> Result<(File, bool)> {
    let flags = OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK | OFlag::O_CLOEXEC;
    let (fd, created) = match nix::fcntl::openat(
        dir,
        ".lock",
        flags | OFlag::O_CREAT | OFlag::O_EXCL,
        Mode::from_bits_truncate(0o644),
    ) {
        Ok(fd) => (fd, true),
        Err(Errno::EEXIST) => {
            let fd = nix::fcntl::openat(dir, ".lock", flags, Mode::empty())
                .with_context(|| format!("opening {}", path.display()))?;
            (fd, false)
        }
        Err(errno) => return Err(errno).with_context(|| format!("creating {}", path.display())),
    };
    let file = File::from(fd);
    let metadata = file
        .metadata()
        .with_context(|| format!("reading {}", path.display()))?;
    ensure!(
        metadata.is_file(),
        "{} is not a regular file; remove it and run again",
        path.display()
    );
    Ok((file, created))
}

/// A temp under `tmp/`, created and removed through the store's descriptor for `tmp`.
/// After publication its name is a second link to the entry, and removing it leaves the
/// entry.
struct TempFile<'a> {
    dir: &'a File,
    name: String,
    /// The temp's path, for messages.
    path: PathBuf,
    file: File,
}

impl<'a> TempFile<'a> {
    /// Create a new temp, mode 0600, in `dir`, whose path is `dir_path`.
    fn create(dir: &'a File, dir_path: &Path) -> Result<Self> {
        let name = uuid::Uuid::new_v4().simple().to_string();
        let path = dir_path.join(&name);
        let fd = nix::fcntl::openat(
            dir,
            name.as_str(),
            OFlag::O_RDWR | OFlag::O_CREAT | OFlag::O_EXCL | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
            Mode::from_bits_truncate(0o600),
        )
        .with_context(|| format!("creating {}", path.display()))?;
        Ok(Self {
            dir,
            name,
            path,
            file: File::from(fd),
        })
    }
}

impl Drop for TempFile<'_> {
    fn drop(&mut self) {
        let removed =
            nix::unistd::unlinkat(self.dir, self.name.as_str(), UnlinkatFlags::NoRemoveDir);
        if let Err(error) = removed {
            warn!(temp = %self.path.display(), %error, "could not remove pmem store temp");
        }
    }
}

/// Refuse an image on one of the filesystems in `UNVERIFIABLE_FILESYSTEMS`, whose
/// timestamps cannot show fcvm a write, so the store could not tell a torn copy or a new
/// generation.
fn refuse_unverifiable_filesystem(file: &File, source: &Path) -> Result<()> {
    let kind = nix::sys::statfs::fstatfs(file)
        .with_context(|| {
            format!(
                "reading the filesystem of --pmem image {}",
                source.display()
            )
        })?
        .filesystem_type();
    let Some((name, reason)) = unverifiable_filesystem(kind) else {
        return Ok(());
    };
    bail!(
        "--pmem image {} is on {name}, where fcvm cannot tell whether its copy is consistent: \
         {reason}. Copy the image to a local disk filesystem and pass that copy.",
        source.display()
    )
}

/// The name of the filesystem of type `kind` (statfs `f_type`) and why its timestamps
/// cannot show fcvm a write to an image, when it is in `UNVERIFIABLE_FILESYSTEMS`.
pub fn unverifiable_filesystem(kind: FsType) -> Option<(&'static str, &'static str)> {
    UNVERIFIABLE_FILESYSTEMS
        .iter()
        .find(|(magic, _, _)| *magic == kind)
        .map(|(_, name, reason)| (*name, *reason))
}

/// The directory the tests put source images under: /mnt/fcvm-btrfs, which every test
/// environment mounts. The system temp directory can be tmpfs or overlayfs (the CI
/// container's /tmp is overlayfs), where the store refuses an image. Panics unless the store
/// accepts the directory's filesystem, so a wrong environment fails here and not as a refusal
/// in the middle of a test.
#[cfg(test)]
pub(crate) fn test_image_dir() -> &'static Path {
    let dir = Path::new("/mnt/fcvm-btrfs");
    let kind = nix::sys::statfs::statfs(dir)
        .unwrap_or_else(|error| panic!("statfs {}: {error}", dir.display()))
        .filesystem_type();
    if let Some((name, _)) = unverifiable_filesystem(kind) {
        panic!(
            "{} is on {name}, where the pmem store refuses an image",
            dir.display()
        );
    }
    dir
}

/// fsync the source, so any later write through a shared mapping faults and moves its
/// change time. A filesystem without fsync (squashfs, iso9660) is accepted only when it is
/// mounted read-only, since then nothing can write the image.
fn sync_source(file: &File, source: &Path) -> Result<()> {
    let Err(error) = file.sync_all() else {
        return Ok(());
    };
    let read_only = || {
        nix::sys::statvfs::fstatvfs(file)
            .map(|vfs| vfs.flags().contains(nix::sys::statvfs::FsFlags::ST_RDONLY))
            .unwrap_or(false)
    };
    if fsync_error_is_benign(error.raw_os_error(), read_only) {
        return Ok(());
    }
    Err(error).with_context(|| format!("syncing --pmem image {}", source.display()))
}

/// Whether an fsync error leaves the copy safe: EINVAL or EROFS on a read-only mount.
fn fsync_error_is_benign(errno: Option<i32>, read_only: impl FnOnce() -> bool) -> bool {
    matches!(errno, Some(libc::EINVAL | libc::EROFS)) && read_only()
}

/// Give `dst` the bytes of `src`, `len` long: FICLONE when the filesystem can share the
/// extents, otherwise a copy of the data runs.
fn copy_image(src: &File, dst: &File, len: u64) -> Result<CopyMethod> {
    let mut result = ficlone(src, dst);
    // btrfs refuses a clone between a nodatacow file and a copy-on-write one with EINVAL.
    // The temp is still empty, so it can take the source's flag and the clone run again.
    if matches!(&result, Err(error) if error.raw_os_error() == Some(libc::EINVAL))
        && match_nodatacow(src, dst)
    {
        result = ficlone(src, dst);
    }
    let Err(error) = result else {
        return Ok(CopyMethod::Reflink);
    };
    match error.raw_os_error() {
        // Another filesystem, or one that cannot clone these files.
        Some(libc::EXDEV | libc::EOPNOTSUPP | libc::EINVAL | libc::ENOTTY) => {}
        _ => return Err(error).context("FICLONE"),
    }
    copy_data_runs(src, dst, len)?;
    Ok(CopyMethod::Bytes)
}

/// FICLONE `src` into `dst`.
fn ficlone(src: &File, dst: &File) -> std::io::Result<()> {
    // SAFETY: both descriptors are open for the duration of the call.
    let rc = unsafe { libc::ioctl(dst.as_raw_fd(), libc::FICLONE as _, src.as_raw_fd()) };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Set or clear the nodatacow flag of the empty `dst` to match `src`, and report whether
/// it changed. The flag can only change while a file is empty, and a temp inherits it
/// from a store directory marked `chattr +C`. `copy_image` calls this only after btrfs
/// refused a clone, so a byte copy (from another filesystem, for one) keeps the flag the
/// store directory gives its files.
fn match_nodatacow(src: &File, dst: &File) -> bool {
    let (Some(src_flags), Some(dst_flags)) = (inode_flags(src), inode_flags(dst)) else {
        return false;
    };
    let wanted = (dst_flags & !FS_NOCOW_FL) | (src_flags & FS_NOCOW_FL);
    if wanted == dst_flags {
        return false;
    }
    match set_inode_flags(dst, wanted) {
        Ok(()) => true,
        Err(error) => {
            debug!(%error, "could not match the pmem store temp's nodatacow flag to its source");
            false
        }
    }
}

/// Copy the data runs of `src` into `dst` at the same offsets with pread and pwrite, then
/// set the length, so holes stay holes. All-zero 4 KiB blocks inside a run are not written
/// either, so the copy stays sparse when the source reports every block as data.
/// copy_file_range is not used: it returns EXDEV between two superblocks, which is the case
/// this exists for.
fn copy_data_runs(src: &File, dst: &File, len: u64) -> Result<()> {
    let mut buffer = vec![0u8; 1 << 20];
    let mut offset = 0u64;
    while offset < len {
        let Some(data) = seek(src, offset, libc::SEEK_DATA)? else {
            break;
        };
        if data >= len {
            break;
        }
        let hole = seek(src, data, libc::SEEK_HOLE)?.unwrap_or(len).min(len);
        let mut position = data;
        while position < hole {
            let chunk = ((hole - position) as usize).min(buffer.len());
            src.read_exact_at(&mut buffer[..chunk], position)
                .with_context(|| format!("reading at {position}"))?;
            write_nonzero_blocks(dst, &buffer[..chunk], position)?;
            position += chunk as u64;
        }
        offset = hole;
    }
    dst.set_len(len).context("setting the copy's length")?;
    Ok(())
}

/// The unit in which the byte copy looks for all-zero blocks to leave unwritten.
const ZERO_BLOCK: usize = 4096;

/// Write the blocks of `data`, which belongs at `offset`, that hold a non-zero byte, one
/// write per run of them. An all-zero block is left unwritten: the temp starts empty, so it
/// reads back as zeros once `set_len` gives the copy its length.
fn write_nonzero_blocks(dst: &File, data: &[u8], offset: u64) -> Result<()> {
    let mut run: Option<usize> = None;
    for (index, block) in data.chunks(ZERO_BLOCK).enumerate() {
        let start = index * ZERO_BLOCK;
        let zero = block.iter().all(|byte| *byte == 0);
        match (zero, run) {
            (false, None) => run = Some(start),
            (true, Some(first)) => {
                write_run(dst, &data[first..start], offset + first as u64)?;
                run = None;
            }
            _ => {}
        }
    }
    if let Some(first) = run {
        write_run(dst, &data[first..], offset + first as u64)?;
    }
    Ok(())
}

fn write_run(dst: &File, bytes: &[u8], offset: u64) -> Result<()> {
    dst.write_all_at(bytes, offset)
        .with_context(|| format!("writing at {offset}"))
}

/// `FS_NOCOW_FL` from linux/fs.h, which libc does not export: the file is written in place
/// instead of copy-on-write (btrfs nodatacow, `chattr +C`).
const FS_NOCOW_FL: libc::c_int = 0x0080_0000;

/// The inode flags of `file` (FS_IOC_GETFLAGS), or `None` on a filesystem without them.
fn inode_flags(file: &File) -> Option<libc::c_int> {
    let mut flags: libc::c_int = 0;
    // SAFETY: FS_IOC_GETFLAGS writes one int to `flags`, which outlives the call.
    let rc = unsafe { libc::ioctl(file.as_raw_fd(), libc::FS_IOC_GETFLAGS as _, &mut flags) };
    (rc == 0).then_some(flags)
}

/// Set the inode flags of `file` (FS_IOC_SETFLAGS).
fn set_inode_flags(file: &File, flags: libc::c_int) -> std::io::Result<()> {
    // SAFETY: FS_IOC_SETFLAGS reads one int from `flags`, which outlives the call.
    let rc = unsafe { libc::ioctl(file.as_raw_fd(), libc::FS_IOC_SETFLAGS as _, &flags) };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// lseek with SEEK_DATA or SEEK_HOLE; `None` when no data follows `offset` (ENXIO).
fn seek(file: &File, offset: u64, whence: libc::c_int) -> Result<Option<u64>> {
    // SAFETY: lseek on an open descriptor.
    let position = unsafe { libc::lseek(file.as_raw_fd(), offset as libc::off_t, whence) };
    if position >= 0 {
        return Ok(Some(position as u64));
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ENXIO) {
        return Ok(None);
    }
    Err(error).with_context(|| format!("lseek to {offset}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use nix::sys::statfs::{statfs, TMPFS_MAGIC};
    use nix::unistd::{Gid, Uid};
    use std::cell::RefCell;

    const MIB: u64 = 1024 * 1024;

    /// A user with these ids, to pass as the sudo invoker. Handing a directory to it is an
    /// fchown, which needs no passwd entry.
    fn user(uid: u32, gid: u32) -> User {
        User {
            name: format!("fixture-{uid}"),
            passwd: std::ffi::CString::new("x").unwrap(),
            uid: Uid::from_raw(uid),
            gid: Gid::from_raw(gid),
            gecos: std::ffi::CString::default(),
            dir: PathBuf::from("/nonexistent"),
            shell: PathBuf::from("/bin/false"),
        }
    }

    /// The user this test runs as, as the invoker: handing a directory to it changes nothing
    /// a test checks, so the sudo path runs without root.
    fn this_user() -> User {
        user(
            nix::unistd::geteuid().as_raw(),
            nix::unistd::getegid().as_raw(),
        )
    }

    /// The names in the directory `dir`, sorted.
    fn names_in(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    fn random(len: usize) -> Vec<u8> {
        use rand::RngCore;
        let mut bytes = vec![0u8; len];
        rand::thread_rng().fill_bytes(&mut bytes);
        bytes
    }

    fn fs_type(path: &Path) -> FsType {
        statfs(path).unwrap().filesystem_type()
    }

    fn dev(path: &Path) -> u64 {
        std::fs::metadata(path).unwrap().dev()
    }

    /// A `len` byte image under `dir` whose first MiB is random and the rest a hole.
    fn image_in(dir: &Path, len: u64) -> PathBuf {
        let path = dir.join("image.ext4");
        let file = File::create(&path).unwrap();
        file.set_len(len).unwrap();
        file.write_all_at(&random(MIB as usize), 0).unwrap();
        path.canonicalize().unwrap()
    }

    /// The store's entries and temps.
    fn contents(store: &PmemStore) -> (Vec<PathBuf>, Vec<PathBuf>) {
        let list = |dir: &Path| {
            let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .collect();
            paths.sort();
            paths
        };
        let entries = list(store.dir())
            .into_iter()
            .filter(|path| store.is_entry(path))
            .collect();
        (entries, list(&store.dir().join("tmp")))
    }

    /// The copy from another filesystem keeps the image's holes. copy_file_range returns
    /// EXDEV between two superblocks, so a fallback built on it fails here.
    #[test]
    fn the_byte_copy_keeps_holes_from_dev_shm_into_the_data_dir() {
        let shm = tempfile::tempdir_in("/dev/shm").unwrap();
        assert_eq!(
            fs_type(shm.path()),
            TMPFS_MAGIC,
            "/dev/shm is not tmpfs here, so this test would not copy across filesystems"
        );
        let data = tempfile::tempdir().unwrap();
        assert_ne!(
            dev(shm.path()),
            dev(data.path()),
            "the source and the copy must be on different filesystems"
        );
        const LEN: u64 = 256 * MIB;
        let source = File::options()
            .read(true)
            .write(true)
            .create_new(true)
            .open(shm.path().join("image"))
            .unwrap();
        source.set_len(LEN).unwrap();
        let (first, second) = (random(MIB as usize), random(MIB as usize));
        source.write_all_at(&first, 0).unwrap();
        source.write_all_at(&second, 128 * MIB).unwrap();
        let copy = File::options()
            .read(true)
            .write(true)
            .create_new(true)
            .open(data.path().join("copy"))
            .unwrap();

        let method = copy_image(&source, &copy, LEN).expect("copying from /dev/shm");
        assert_eq!(method, CopyMethod::Bytes);
        copy.sync_all().unwrap();
        let metadata = copy.metadata().unwrap();
        assert_eq!(metadata.len(), LEN);
        let mut read = vec![0u8; MIB as usize];
        copy.read_exact_at(&mut read, 0).unwrap();
        assert!(read == first, "the first data run differs");
        copy.read_exact_at(&mut read, 128 * MIB).unwrap();
        assert!(read == second, "the second data run differs");
        copy.read_exact_at(&mut read, 64 * MIB).unwrap();
        assert!(read.iter().all(|byte| *byte == 0), "a hole reads non-zero");
        let allocated = metadata.blocks() * 512;
        assert!(
            allocated < 4 * MIB,
            "the copy allocates {allocated} bytes for 2 MiB of data in a {LEN} byte image"
        );
    }

    /// A nodatacow (`chattr +C`) source on the store's btrfs is still cloned: btrfs refuses
    /// a clone between a nodatacow file and a copy-on-write one, so the temp has to get the
    /// flag while it is empty.
    #[test]
    fn a_nodatacow_source_on_the_same_btrfs_is_cloned() {
        let dir = tempfile::tempdir_in("/mnt/fcvm-btrfs").expect("temp dir on the btrfs store");
        assert_eq!(
            fs_type(dir.path()),
            nix::sys::statfs::BTRFS_SUPER_MAGIC,
            "/mnt/fcvm-btrfs is not btrfs here, so this test would not cover a clone"
        );
        let source = File::options()
            .read(true)
            .write(true)
            .create_new(true)
            .open(dir.path().join("image"))
            .unwrap();
        let flags = inode_flags(&source).expect("btrfs has inode flags");
        set_inode_flags(&source, flags | FS_NOCOW_FL).expect("chattr +C on an empty file");
        assert_ne!(inode_flags(&source).unwrap() & FS_NOCOW_FL, 0);
        const LEN: u64 = 4 * MIB;
        source.set_len(LEN).unwrap();
        let bytes = random(MIB as usize);
        source.write_all_at(&bytes, 0).unwrap();
        let copy = File::options()
            .read(true)
            .write(true)
            .create_new(true)
            .open(dir.path().join("copy"))
            .unwrap();

        let method = copy_image(&source, &copy, LEN).unwrap();
        assert_eq!(
            method,
            CopyMethod::Reflink,
            "the nodatacow source was copied byte by byte"
        );
        let mut read = vec![0u8; MIB as usize];
        copy.read_exact_at(&mut read, 0).unwrap();
        assert!(read == bytes, "the clone differs");
        assert_eq!(copy.metadata().unwrap().len(), LEN);
    }

    /// The listed filesystems, whose timestamps cannot show a write to an image, are refused
    /// by their statfs magic from linux/magic.h, and local disk filesystems are accepted.
    /// FUSE covers fuseblk and virtiofs, which report the same magic. overlayfs is refused
    /// because statfs on it does not name the layer that holds the file.
    #[test]
    fn the_listed_filesystems_are_refused_and_local_disks_accepted() {
        for (magic, name) in [
            (0x0102_1994, "tmpfs"),
            (0x8584_58f6, "ramfs"),
            (0x9584_58f6, "hugetlbfs"),
            (0x6573_5546, "FUSE"),
            (0x6969, "NFS"),
            (0x517b, "SMB"),
            (0xfe53_4d42, "SMB2"),
            (0xff53_4d42, "CIFS"),
            (0x0102_1997, "9p"),
            (0x794c_7630, "overlayfs"),
        ] {
            assert_eq!(
                unverifiable_filesystem(FsType(magic)).map(|(name, _)| name),
                Some(name),
                "{name} ({magic:#x})"
            );
        }
        for (magic, name) in [
            (0xef53, "ext4"),
            (0x9123_683e, "btrfs"),
            (0x5846_5342, "XFS"),
        ] {
            assert_eq!(
                unverifiable_filesystem(FsType(magic)),
                None,
                "{name} ({magic:#x}) was refused"
            );
        }
    }

    /// An image on tmpfs is refused before anything is copied.
    #[test]
    fn a_tmpfs_source_is_refused() {
        let shm = tempfile::tempdir_in("/dev/shm").unwrap();
        assert_eq!(
            fs_type(shm.path()),
            TMPFS_MAGIC,
            "/dev/shm is not tmpfs here, so this test would not exercise the refusal"
        );
        let source = image_in(shm.path(), 2 * MIB);
        let data = tempfile::tempdir().unwrap();
        let store = PmemStore::open(data.path()).unwrap();
        let error = format!(
            "{:#}",
            store
                .ingest(&source)
                .expect_err("an image on tmpfs was copied")
        );
        assert!(error.contains("is on tmpfs"), "{error}");
        assert_eq!(contents(&store), (vec![], vec![]));
    }

    /// A process that dirtied a page of the image through a shared mapping before the copy
    /// rewrites it after the copy. Without the fsync before the copy that second write moves
    /// no timestamp, so the copy would be published under the generation it no longer
    /// matches.
    #[test]
    fn a_shared_mapping_write_after_the_copy_is_refused() {
        // A disk filesystem, where an fsync write-protects the image's pages.
        let disk = tempfile::tempdir_in(test_image_dir()).unwrap();
        // A store on tmpfs makes the copy a byte copy: FICLONE fails across filesystems,
        // and unlike a btrfs clone a read does not write the image back first.
        let shm = tempfile::tempdir_in("/dev/shm").unwrap();
        assert_ne!(dev(disk.path()), dev(shm.path()));
        let source = image_in(disk.path(), 4 * MIB);
        let file = File::options()
            .read(true)
            .write(true)
            .open(&source)
            .unwrap();
        // SAFETY: the test owns the image and nothing truncates it while it is mapped.
        let mut map = unsafe { memmap2::MmapMut::map_mut(&file).unwrap() };
        map[..4096].fill(1);
        let store = PmemStore::open(shm.path()).unwrap();
        let result = store.ingest_with(
            &source,
            || {
                map[..4096].fill(2);
                Ok(())
            },
            &sync_parent,
        );
        let error = format!(
            "{:#}",
            result.expect_err("a copy older than the image was published")
        );
        assert!(error.contains("changed while fcvm copied it"), "{error}");
        assert_eq!(contents(&store), (vec![], vec![]));
    }

    /// Entries are mode 0400, so their owner cannot open one for writing either.
    #[test]
    fn entries_are_read_only_for_their_owner() {
        let disk = tempfile::tempdir_in(test_image_dir()).unwrap();
        let data = tempfile::tempdir().unwrap();
        let source = image_in(disk.path(), 2 * MIB);
        let store = PmemStore::open(data.path()).unwrap();
        let entry = store.ingest(&source).unwrap();
        let mode = std::fs::metadata(&entry).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode, 0o400, "{} has mode {mode:o}", entry.display());
        if !nix::unistd::geteuid().is_root() {
            let error = File::options()
                .read(true)
                .write(true)
                .open(&entry)
                .expect_err("the owner opened an entry for writing");
            assert_eq!(error.raw_os_error(), Some(libc::EACCES), "{error}");
        }
    }

    /// When another run publishes the same generation while this one copies, this run uses
    /// that entry: publication never replaces an inode a VM or snapshot may already name.
    #[test]
    fn a_racing_publication_keeps_the_first_inode() {
        let disk = tempfile::tempdir_in(test_image_dir()).unwrap();
        let data = tempfile::tempdir().unwrap();
        let source = image_in(disk.path(), 2 * MIB);
        let store = PmemStore::open(data.path()).unwrap();
        let mut first = None;
        let entry = store
            .ingest_with(
                &source,
                || {
                    let published = store.ingest(&source)?;
                    first = Some(std::fs::metadata(&published)?.ino());
                    Ok(())
                },
                &sync_parent,
            )
            .unwrap();
        let first = first.expect("the racing ingest did not run");
        assert_eq!(
            std::fs::metadata(&entry).unwrap().ino(),
            first,
            "publication replaced the entry the other run published"
        );
        assert_eq!(contents(&store), (vec![entry], vec![]));
    }

    /// In a data_dir two users share, the entry of a generation can belong to the other user,
    /// whose mode 0400 keeps this one's Firecracker from opening it. Ingest refuses it rather
    /// than hand Firecracker a path it cannot open. Only root can give a file away, so
    /// `podman unshare` hands the entry to this user's first subordinate uid. A root run is
    /// refused the same entry, though root itself could read it;
    /// `a_root_run_refuses_an_entry_another_user_owns` covers that by giving the entry away
    /// with chown, so this test runs only without root.
    #[test]
    fn an_entry_another_user_owns_is_refused() {
        if nix::unistd::geteuid().is_root() {
            eprintln!("skipping: a_root_run_refuses_an_entry_another_user_owns covers root");
            return;
        }
        let disk = tempfile::tempdir_in(test_image_dir()).unwrap();
        let data = tempfile::tempdir().unwrap();
        let source = image_in(disk.path(), 2 * MIB);
        let store = PmemStore::open(data.path()).unwrap();
        let entry = store.ingest(&source).unwrap();
        let status = std::process::Command::new("podman")
            .args(["unshare", "chown", "1:1"])
            .arg(&entry)
            .status()
            .expect("running podman unshare");
        assert!(
            status.success(),
            "podman unshare chown {} failed: {status}",
            entry.display()
        );
        let owner = std::fs::metadata(&entry).unwrap().uid();
        assert_ne!(
            owner,
            nix::unistd::geteuid().as_raw(),
            "{} still belongs to this user",
            entry.display()
        );
        let error = format!(
            "{:#}",
            store
                .ingest(&source)
                .expect_err("an entry this user cannot read was used")
        );
        assert!(error.contains("another user's copies"), "{error}");
        assert!(error.contains(&entry.display().to_string()), "{error}");
    }

    /// A run uses its own entries whatever their group and, under sudo, the invoker's entries
    /// with the invoker's group or the run's: the ids a VM's holder namespace maps, which are
    /// all Firecracker can open a mode 0400 entry as.
    #[test]
    fn only_entries_firecracker_can_open_are_usable() {
        let rootless = (1000, 1000);
        assert!(
            entry_owner_is_mapped((1000, 1000), rootless, None),
            "a run without root refused its own entry"
        );
        assert!(
            entry_owner_is_mapped((1000, 3000), rootless, None),
            "a run without root refused its own entry with another group"
        );
        assert!(
            !entry_owner_is_mapped((2000, 2000), rootless, None),
            "a run without root used another user's entry"
        );
        let root = (0, 0);
        let invoker = Some((1000, 1000));
        assert!(
            entry_owner_is_mapped((0, 0), root, invoker),
            "a root run refused its own entry"
        );
        assert!(
            entry_owner_is_mapped((1000, 1000), root, invoker),
            "a root run refused the invoker's entry"
        );
        assert!(
            entry_owner_is_mapped((1000, 0), root, invoker),
            "a root run refused the invoker's entry with root's group"
        );
        assert!(
            !entry_owner_is_mapped((2000, 2000), root, invoker),
            "a root run with an invoker used another user's entry"
        );
        assert!(
            !entry_owner_is_mapped((1000, 3000), root, invoker),
            "a root run used the invoker's entry with a group its namespace does not map"
        );
        assert!(
            !entry_owner_is_mapped((2000, 2000), root, None),
            "a root run without an invoker used another user's entry"
        );
    }

    /// A root run refuses an entry of a user outside its VM's holder namespace, though root
    /// itself can read it: Firecracker in that namespace could not open it.
    #[cfg(feature = "privileged-tests")]
    #[test]
    fn a_root_run_refuses_an_entry_another_user_owns() {
        assert!(
            nix::unistd::geteuid().is_root(),
            "privileged tests run as root"
        );
        const OTHER: u32 = 3000;
        let invoker = crate::setup::sudo_invoker().map(|user| user.uid.as_raw());
        let sudo_uid = std::env::var("SUDO_UID")
            .ok()
            .and_then(|uid| uid.parse::<u32>().ok());
        assert!(
            invoker != Some(OTHER) && sudo_uid != Some(OTHER),
            "uid {OTHER} invoked this run, so its entries are usable; pick another fixture uid"
        );
        let disk = tempfile::tempdir_in(test_image_dir()).unwrap();
        let data = tempfile::tempdir().unwrap();
        let source = image_in(disk.path(), 2 * MIB);
        let store = PmemStore::open(data.path()).unwrap();
        let entry = store.ingest(&source).unwrap();
        nix::unistd::chown(
            &entry,
            Some(Uid::from_raw(OTHER)),
            Some(Gid::from_raw(OTHER)),
        )
        .unwrap();
        let error = format!(
            "{:#}",
            store
                .ingest(&source)
                .expect_err("a root run used an entry of a user outside its namespace")
        );
        assert!(
            error.contains("uid 0 cannot use pmem store entry"),
            "{error}"
        );
        assert!(error.contains(&entry.display().to_string()), "{error}");
    }

    /// Each owner a run cannot use is refused with a message that says why, and the
    /// invoker's entry passes in a root run with an invoker.
    #[test]
    fn each_owner_a_run_cannot_use_is_refused_with_its_reason() {
        let store = Path::new("/data/pmem");
        let entry = store.join(format!("{}.img", "a".repeat(64)));
        let check =
            |owner, own, invoker| check_entry_owner(&entry, owner, store, RunIds { own, invoker });
        let refusal = |owner, own, invoker| {
            format!(
                "{:#}",
                check(owner, own, invoker).expect_err("an entry the run cannot use passed")
            )
        };
        check((1000, 1000), (0, 0), Some((1000, 1000))).expect("the invoker's entry was refused");
        for (owner, own, invoker, reason) in [
            (
                (1000, 3000),
                (0, 0),
                Some((1000, 1000)),
                "belongs to the sudo invoker",
            ),
            ((0, 0), (1000, 1000), None, "belongs to root"),
            ((1000, 1000), (0, 0), None, "no sudo invoker"),
            ((2000, 2000), (1000, 1000), None, "another user's copies"),
            (
                (2000, 2000),
                (0, 0),
                Some((1000, 1000)),
                "another user's copies",
            ),
        ] {
            let error = refusal(owner, own, invoker);
            assert!(
                error.contains(reason) && error.contains(&entry.display().to_string()),
                "owner {owner:?}, run {own:?}, invoker {invoker:?}: {error}"
            );
        }
    }

    /// Firecracker is only ever given a store entry.
    #[test]
    fn attach_refuses_a_path_outside_the_store() {
        let disk = tempfile::tempdir_in(test_image_dir()).unwrap();
        let data = tempfile::tempdir().unwrap();
        let source = image_in(disk.path(), 2 * MIB);
        let store = PmemStore::open(data.path()).unwrap();
        let entry = store.ingest(&source).unwrap();
        let device = |path: &Path| PmemDevice {
            path: path.display().to_string(),
            source: source.display().to_string(),
            mount_path: "/mnt/cache".to_string(),
            identity: String::new(),
        };
        assert_eq!(store.attach_path(&device(&entry)).unwrap(), entry);
        for outside in [
            source.clone(),
            store.dir().join("tmp").join(entry.file_name().unwrap()),
            store.dir().join("cache.img"),
        ] {
            let error = format!(
                "{:#}",
                store
                    .attach_path(&device(&outside))
                    .expect_err("a path outside the store was attached")
            );
            assert!(
                error.contains("not an entry of the pmem image store"),
                "{error}"
            );
        }
    }

    /// A filesystem without fsync is accepted only when mounted read-only.
    #[test]
    fn an_fsync_error_is_accepted_only_on_a_read_only_mount() {
        assert!(fsync_error_is_benign(Some(libc::EINVAL), || true));
        assert!(fsync_error_is_benign(Some(libc::EROFS), || true));
        assert!(!fsync_error_is_benign(Some(libc::EINVAL), || false));
        assert!(!fsync_error_is_benign(Some(libc::EIO), || true));
    }

    /// A symlink at `<data_dir>/pmem` or at its `tmp` is refused, and nothing is created
    /// where it points. A root run creates the store's directories and hands them to the
    /// user who ran sudo, so following a symlink that user planted would make root create
    /// directories wherever it points and hand them over, /etc included.
    #[test]
    fn a_symlink_at_the_store_or_its_temp_directory_is_refused() {
        let elsewhere = tempfile::tempdir().unwrap();
        for at in ["pmem", "pmem/tmp"] {
            let data = tempfile::tempdir().unwrap();
            if at == "pmem/tmp" {
                std::fs::create_dir(data.path().join("pmem")).unwrap();
            }
            std::os::unix::fs::symlink(elsewhere.path(), data.path().join(at)).unwrap();
            let error = format!(
                "{:#}",
                PmemStore::open(data.path())
                    .expect_err("a store reached through a symlink was opened")
            );
            assert!(error.contains("is not a directory"), "{at}: {error}");
            assert_eq!(
                std::fs::read_dir(elsewhere.path()).unwrap().count(),
                0,
                "opening the store created something through the symlink at {at}"
            );
        }
    }

    /// Without a sudo invoker nothing is handed back, so a directory put at the name between
    /// the mkdirat and the open is used as found: a check there would protect no one, and
    /// it would refuse a filesystem that gives a new directory another owner.
    #[test]
    fn a_created_directory_is_checked_only_when_it_will_be_handed_back() {
        let data = tempfile::tempdir().unwrap();
        let parent = File::open(data.path()).unwrap();
        let store = data.path().join("pmem");
        let prepared = data.path().join("prepared");
        std::fs::create_dir(&prepared).unwrap();
        std::fs::write(prepared.join("entry"), b"x").unwrap();
        let swap = |path: &Path| std::fs::rename(&prepared, path).unwrap();
        let steps = CreateSteps {
            created: &swap,
            ..CreateSteps::REAL
        };
        let (_, created) = open_or_create_dir(&parent, "pmem", &store, None, steps, leave_empty)
            .expect("without a hand-back the directory is used as found");
        assert!(
            created,
            "the mkdirat succeeded, so the call created the name"
        );
        assert!(
            store.join("entry").exists(),
            "the swapped-in directory was not the one used"
        );
    }

    /// A copy-on-write source is cloned into a store whose temp directory makes new files
    /// nodatacow (`chattr +C` on the directory): the temp drops the flag it inherited,
    /// since btrfs refuses a clone between a nodatacow file and a copy-on-write one.
    #[test]
    fn a_copy_on_write_source_is_cloned_into_a_nodatacow_directory() {
        let dir = tempfile::tempdir_in("/mnt/fcvm-btrfs").expect("temp dir on the btrfs store");
        assert_eq!(
            fs_type(dir.path()),
            nix::sys::statfs::BTRFS_SUPER_MAGIC,
            "/mnt/fcvm-btrfs is not btrfs here, so this test would not cover a clone"
        );
        let nocow = dir.path().join("nocow");
        std::fs::create_dir(&nocow).unwrap();
        let nocow_dir = File::open(&nocow).unwrap();
        let flags = inode_flags(&nocow_dir).expect("btrfs has inode flags");
        set_inode_flags(&nocow_dir, flags | FS_NOCOW_FL).expect("chattr +C on a directory");
        let source = File::options()
            .read(true)
            .write(true)
            .create_new(true)
            .open(dir.path().join("image"))
            .unwrap();
        assert_eq!(inode_flags(&source).unwrap() & FS_NOCOW_FL, 0);
        const LEN: u64 = 4 * MIB;
        source.set_len(LEN).unwrap();
        let bytes = random(MIB as usize);
        source.write_all_at(&bytes, 0).unwrap();
        let copy = File::options()
            .read(true)
            .write(true)
            .create_new(true)
            .open(nocow.join("copy"))
            .unwrap();
        assert_ne!(
            inode_flags(&copy).unwrap() & FS_NOCOW_FL,
            0,
            "the temp did not inherit nodatacow, so this test would not cover clearing it"
        );

        let method = copy_image(&source, &copy, LEN).unwrap();
        assert_eq!(
            method,
            CopyMethod::Reflink,
            "the copy-on-write source was copied byte by byte"
        );
        let mut read = vec![0u8; MIB as usize];
        copy.read_exact_at(&mut read, 0).unwrap();
        assert!(read == bytes, "the clone differs");
    }

    /// A source whose data runs hold all-zero blocks, here a fully allocated one, still
    /// gives a sparse copy: a filesystem that cannot report holes reports every block as
    /// data, and writing its zeros would allocate them. The first MiB repeats random, zero
    /// and random 4 KiB blocks, so a run of data blocks also ends at a zero block in the
    /// middle of one read and has to be written there. The whole copy is compared.
    #[test]
    fn the_byte_copy_leaves_zero_blocks_unwritten() {
        let shm = tempfile::tempdir_in("/dev/shm").unwrap();
        assert_eq!(
            fs_type(shm.path()),
            TMPFS_MAGIC,
            "/dev/shm is not tmpfs here"
        );
        let data = tempfile::tempdir().unwrap();
        assert_ne!(dev(shm.path()), dev(data.path()));
        const LEN: u64 = 32 * MIB;
        let mut expected = vec![0u8; LEN as usize];
        for (index, block) in expected[..MIB as usize].chunks_mut(ZERO_BLOCK).enumerate() {
            if index % 3 != 1 {
                block.copy_from_slice(&random(ZERO_BLOCK));
            }
        }
        expected[(LEN - MIB) as usize..].copy_from_slice(&random(MIB as usize));
        let source = File::options()
            .read(true)
            .write(true)
            .create_new(true)
            .open(shm.path().join("image"))
            .unwrap();
        source.write_all_at(&expected, 0).unwrap();
        assert!(
            source.metadata().unwrap().blocks() * 512 >= LEN,
            "the source is not fully allocated, so this test would not cover the zero skip"
        );
        assert_eq!(seek(&source, 0, libc::SEEK_HOLE).unwrap(), Some(LEN));
        let copy = File::options()
            .read(true)
            .write(true)
            .create_new(true)
            .open(data.path().join("copy"))
            .unwrap();

        assert_eq!(copy_image(&source, &copy, LEN).unwrap(), CopyMethod::Bytes);
        copy.sync_all().unwrap();
        let metadata = copy.metadata().unwrap();
        assert_eq!(metadata.len(), LEN);
        let mut read = vec![0u8; LEN as usize];
        copy.read_exact_at(&mut read, 0).unwrap();
        let first_difference = read.iter().zip(&expected).position(|(a, b)| a != b);
        assert_eq!(first_difference, None, "the copy differs from the source");
        let allocated = metadata.blocks() * 512;
        assert!(
            allocated < 4 * MIB,
            "the copy allocates {allocated} bytes for under 2 MiB of non-zero data in {LEN} bytes"
        );
    }

    /// The store hands back only what a run created: `open_or_create_dir` and `open_lock`
    /// report whether this call created the directory or the lock, and refuse a symlink
    /// there, with and without an invoker. A lock that is a FIFO is refused, and the open does
    /// not block on it.
    #[test]
    fn the_store_reports_which_directories_and_lock_it_created() {
        let me = this_user();
        for invoker in [Some(&me), None] {
            let data = tempfile::tempdir().unwrap();
            let parent = File::open(data.path()).unwrap();
            let store = data.path().join("pmem");
            let open = || {
                open_or_create_dir(
                    &parent,
                    "pmem",
                    &store,
                    invoker,
                    CreateSteps::REAL,
                    leave_empty,
                )
            };
            let (_, created) = open().unwrap();
            assert!(
                created,
                "invoker {}: a new directory was not reported as created",
                invoker.is_some()
            );
            let (_, created) = open().unwrap();
            assert!(
                !created,
                "invoker {}: an existing directory was reported as created",
                invoker.is_some()
            );
            let other = data.path().join("other");
            std::fs::create_dir(&other).unwrap();
            std::os::unix::fs::symlink(&store, other.join("pmem")).unwrap();
            let other_dir = File::open(&other).unwrap();
            open_or_create_dir(
                &other_dir,
                "pmem",
                &other.join("pmem"),
                invoker,
                CreateSteps::REAL,
                leave_empty,
            )
            .expect_err("a symlink was taken as a store directory");
        }

        let data = tempfile::tempdir().unwrap();
        let parent = File::open(data.path()).unwrap();
        let store = data.path().join("pmem");
        let (dir, _) = open_or_create_dir(
            &parent,
            "pmem",
            &store,
            None,
            CreateSteps::REAL,
            leave_empty,
        )
        .unwrap();
        let lock = store.join(".lock");
        assert!(
            open_lock(&dir, &lock).unwrap().1,
            "a new lock was not reported as created"
        );
        assert!(
            !open_lock(&dir, &lock).unwrap().1,
            "an existing lock was reported as created"
        );

        let other = data.path().join("other");
        std::fs::create_dir(&other).unwrap();
        let other_dir = File::open(&other).unwrap();
        let other_lock = other.join(".lock");
        nix::unistd::mkfifo(&other_lock, Mode::from_bits_truncate(0o600)).unwrap();
        let error = format!(
            "{:#}",
            open_lock(&other_dir, &other_lock).expect_err("a FIFO was taken as the lock")
        );
        assert!(error.contains("not a regular file"), "{error}");
        std::fs::remove_file(&other_lock).unwrap();
        std::os::unix::fs::symlink(&lock, &other_lock).unwrap();
        open_lock(&other_dir, &other_lock).expect_err("a symlink was taken as the lock");
    }

    /// The mode bits of `path`, setuid, setgid and sticky included.
    fn mode_of(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o7777
    }

    /// Create `<data>/pmem` for an invoker, this user, renaming `prepared` onto the directory
    /// mkdirat made, under its temporary name, before the run opens it, as the owner of
    /// `data` can. Returns the result and the temporary name's path.
    fn create_store_dir_swapping_in(
        data: &Path,
        prepared: &Path,
    ) -> (Result<(File, bool)>, PathBuf) {
        let parent = File::open(data).unwrap();
        let created = RefCell::new(None);
        let swap = |path: &Path| {
            std::fs::rename(prepared, path).unwrap();
            *created.borrow_mut() = Some(path.to_path_buf());
        };
        let steps = CreateSteps {
            created: &swap,
            ..CreateSteps::REAL
        };
        let me = this_user();
        let result = open_or_create_dir(
            &parent,
            "pmem",
            &data.join("pmem"),
            Some(&me),
            steps,
            leave_empty,
        );
        let created = created.into_inner().expect("mkdirat made no directory");
        (result, created)
    }

    /// A non-empty directory put in place of the one mkdirat made, before the open, is
    /// refused, not reported as created: under sudo the run would hand it to the invoking
    /// user, who owns the store's parent and can make that swap. This one has the creation
    /// mode and the run's owner, so only its entries tell it from the one mkdirat made.
    #[test]
    fn a_non_empty_directory_swapped_in_after_mkdirat_is_refused() {
        let data = tempfile::tempdir().unwrap();
        let prepared = data.path().join("prepared");
        std::fs::create_dir(&prepared).unwrap();
        std::fs::set_permissions(&prepared, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::write(prepared.join("victim"), b"another user's file").unwrap();
        let (result, swapped) = create_store_dir_swapping_in(data.path(), &prepared);
        let error = format!(
            "{:#}",
            result.expect_err("a non-empty directory swapped in after mkdirat was accepted")
        );
        assert!(error.contains("changed while fcvm created it"), "{error}");
        assert!(error.contains("victim"), "{error}");
        assert_eq!(
            mode_of(&swapped),
            0o700,
            "the refused directory's mode was changed"
        );
        assert!(
            swapped.join("victim").exists(),
            "the refused directory lost its entry"
        );
        assert!(
            !data.path().join("pmem").exists(),
            "the refused directory was published"
        );
    }

    /// An empty directory with mode 1777, a shared sticky directory, put in place of the one
    /// mkdirat made, before the open, is refused: handing it over would let the invoking user
    /// remove other users' files in it.
    #[test]
    fn a_sticky_directory_swapped_in_after_mkdirat_is_refused() {
        let data = tempfile::tempdir().unwrap();
        let prepared = data.path().join("prepared");
        std::fs::create_dir(&prepared).unwrap();
        std::fs::set_permissions(&prepared, std::fs::Permissions::from_mode(0o1777)).unwrap();
        let (result, swapped) = create_store_dir_swapping_in(data.path(), &prepared);
        let error = format!(
            "{:#}",
            result.expect_err("a 1777 directory swapped in after mkdirat was accepted")
        );
        assert!(error.contains("changed while fcvm created it"), "{error}");
        assert!(error.contains("1777"), "{error}");
        assert_eq!(
            mode_of(&swapped),
            0o1777,
            "the refused directory's mode was changed"
        );
        assert!(
            !data.path().join("pmem").exists(),
            "the refused directory was published"
        );
    }

    /// A root run under sudo publishes the store directory and its tmp only once they belong
    /// to the invoker with their final mode. A rootless run of that user racing it never finds
    /// either one still root's with mode 0700, where it could neither open the store nor
    /// create a temp. No directory under a temporary name is left behind.
    #[cfg(feature = "privileged-tests")]
    #[test]
    fn a_sudo_run_publishes_store_directories_only_once_they_are_the_invokers() {
        assert!(
            nix::unistd::geteuid().is_root(),
            "privileged tests run as root"
        );
        const INVOKER: u32 = 2000;
        let invoker = user(INVOKER, INVOKER);
        let data = tempfile::tempdir().unwrap();
        let final_mode = 0o777 & !process_umask().unwrap();
        let finals = [data.path().join("pmem"), data.path().join("pmem/tmp")];
        let found = |path: &Path| match std::fs::symlink_metadata(path) {
            Ok(metadata) => Some((metadata.uid(), metadata.gid(), metadata.mode() & 0o7777)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => panic!("{}: {error}", path.display()),
        };
        let seen = RefCell::new(Vec::new());
        let check = |step: &str, at: &Path| {
            seen.borrow_mut().push(step.to_string());
            for path in &finals {
                if let Some(found) = found(path) {
                    assert_eq!(
                        found,
                        (INVOKER, INVOKER, final_mode),
                        "{step} {}: {} is visible as uid {}, gid {}, mode {:04o} before it is \
                         the invoker's with mode {final_mode:04o}",
                        at.display(),
                        path.display(),
                        found.0,
                        found.1,
                        found.2
                    );
                }
            }
        };
        let created = |at: &Path| check("created", at);
        let publishing = |at: &Path| check("publishing", at);
        let steps = CreateSteps {
            created: &created,
            publishing: &publishing,
            ..CreateSteps::REAL
        };
        let store = PmemStore::open_with(data.path(), Some(&invoker), steps).unwrap();
        assert_eq!(
            seen.into_inner(),
            ["created", "created", "publishing", "publishing"],
            "the store directory and its tmp were not each made and then published"
        );
        for path in &finals {
            assert_eq!(
                found(path),
                Some((INVOKER, INVOKER, final_mode)),
                "{}",
                path.display()
            );
        }
        assert_eq!(names_in(data.path()), ["pmem"]);
        assert_eq!(names_in(&data.path().join("pmem")), ["tmp"]);
        drop(store);
    }

    /// A run under sudo that another run beats to publishing the store directory, and then
    /// its tmp, opens the directories that run published and removes its own, the tmp it had
    /// made inside the losing store directory included.
    #[test]
    fn a_run_that_loses_the_publication_race_opens_the_winners_directories() {
        let data = tempfile::tempdir().unwrap();
        let finals = [data.path().join("pmem"), data.path().join("pmem/tmp")];
        let winners = RefCell::new(Vec::new());
        let publish_first = |to: &Path| {
            if finals.iter().any(|path| path == to) {
                std::fs::create_dir(to).unwrap();
                let ino = std::fs::metadata(to).unwrap().ino();
                winners.borrow_mut().push((to.to_path_buf(), ino));
            }
        };
        let steps = CreateSteps {
            publishing: &publish_first,
            ..CreateSteps::REAL
        };
        let store = PmemStore::open_with(data.path(), Some(&this_user()), steps).unwrap();
        let ino = |dir: &File| dir.metadata().unwrap().ino();
        assert_eq!(
            winners.into_inner(),
            [
                (finals[0].clone(), ino(&store.pmem)),
                (finals[1].clone(), ino(&store.tmp))
            ],
            "the run did not lose both publications and open what the other run published"
        );
        assert_eq!(names_in(data.path()), ["pmem"]);
        assert_eq!(names_in(&finals[0]), ["tmp"]);
    }

    /// A directory whose hand-over to the sudo invoker did not take (a failed chown is only
    /// logged) is removed, not published: published, it would stay root's with mode 0700, and
    /// no rootless run of the invoker could open the store or create a temp in it.
    #[test]
    fn a_directory_the_invoker_did_not_receive_is_not_published() {
        let data = tempfile::tempdir().unwrap();
        let me = this_user();
        let invoker = user(me.uid.as_raw() + 1, me.gid.as_raw() + 1);
        let keep = |_: &File, _: &Path, _: &User| {};
        let steps = CreateSteps {
            hand_over: &keep,
            ..CreateSteps::REAL
        };
        let error = format!(
            "{:#}",
            PmemStore::open_with(data.path(), Some(&invoker), steps)
                .expect_err("a directory the invoker did not receive was published")
        );
        assert!(error.contains("could not hand"), "{error}");
        assert_eq!(
            names_in(data.path()),
            Vec::<String>::new(),
            "a directory the invoker did not receive was left behind"
        );
    }

    /// A directory this run made under a temporary name and could not finish after it passed
    /// the check is removed: a failed mode change of the store directory, and one of its tmp,
    /// which fails the store directory's populate step. Left behind, one would pile up in the
    /// data_dir on every such run under sudo.
    #[test]
    fn a_directory_that_fails_after_its_check_is_removed() {
        for failing in [".pmem-creating-", ".tmp-creating-"] {
            let data = tempfile::tempdir().unwrap();
            let fail = |dir: &File, path: &Path, mode: u32| {
                let name = path.file_name().unwrap().to_string_lossy();
                if name.starts_with(failing) {
                    return Err(std::io::Error::from_raw_os_error(libc::EIO));
                }
                set_dir_mode(dir, path, mode)
            };
            let steps = CreateSteps {
                set_mode: &fail,
                ..CreateSteps::REAL
            };
            let error = format!(
                "{:#}",
                PmemStore::open_with(data.path(), Some(&this_user()), steps)
                    .expect_err("a failed mode change was ignored")
            );
            assert!(error.contains("setting the mode of"), "{failing}: {error}");
            assert_eq!(
                names_in(data.path()),
                Vec::<String>::new(),
                "{failing}: a directory this run made was left behind"
            );
        }
    }

    /// The store refuses a tmp whose name no longer leads to the directory it holds, as it
    /// refuses such a store directory.
    #[test]
    fn the_store_refuses_a_tmp_that_is_not_the_one_it_holds() {
        let data = tempfile::tempdir().unwrap();
        let store = PmemStore::open(data.path()).unwrap();
        canonical_store_dir(data.path(), &store.pmem, Some(&store.tmp))
            .expect("the store's own tmp was refused");
        let tmp = store.dir().join("tmp");
        std::fs::rename(&tmp, store.dir().join("old-tmp")).unwrap();
        std::fs::create_dir(&tmp).unwrap();
        let error = format!(
            "{:#}",
            canonical_store_dir(data.path(), &store.pmem, Some(&store.tmp))
                .expect_err("a tmp the run does not hold was accepted")
        );
        assert!(error.contains("temp directory"), "{error}");
        assert!(error.contains("was replaced"), "{error}");
    }

    /// A directory this call created is reported as created and ends with the mode a
    /// plain mkdir gives in the same parent, 0777 less the umask, with the setgid bit a
    /// setgid parent passes down, whether it was made in place or published for an invoker.
    #[test]
    fn a_directory_this_call_created_ends_with_the_mode_mkdir_gives() {
        let me = this_user();
        for (invoker, setgid_parent) in [
            (None, false),
            (None, true),
            (Some(&me), false),
            (Some(&me), true),
        ] {
            let data = tempfile::tempdir().unwrap();
            if setgid_parent {
                std::fs::set_permissions(data.path(), std::fs::Permissions::from_mode(0o2700))
                    .unwrap();
            }
            let reference = data.path().join("reference");
            std::fs::create_dir(&reference).unwrap();
            assert_eq!(
                mode_of(&reference) & 0o2000 != 0,
                setgid_parent,
                "mkdir did not pass the parent's setgid bit down, so this case tests nothing"
            );
            let parent = File::open(data.path()).unwrap();
            let store = data.path().join("pmem");
            let (dir, created) = open_or_create_dir(
                &parent,
                "pmem",
                &store,
                invoker,
                CreateSteps::REAL,
                leave_empty,
            )
            .unwrap();
            assert!(created, "a new directory was not reported as created");
            assert_eq!(
                dir.metadata().unwrap().permissions().mode() & 0o7777,
                mode_of(&reference),
                "invoker {}, setgid parent {setgid_parent}: the store directory's mode differs \
                 from mkdir's",
                invoker.is_some()
            );
        }
    }

    /// What a sync step was asked to sync: the name that had appeared in the directory, the
    /// directory's inode, and whether the directory held that name then.
    type Synced = (String, u64, bool);

    /// A sync step that records each call in `log` and then syncs.
    fn recording_sync(
        log: &RefCell<Vec<Synced>>,
    ) -> impl Fn(&File, &Path) -> std::io::Result<()> + '_ {
        move |parent: &File, child: &Path| {
            let name = child.file_name().unwrap();
            let held = entries(parent, child)
                .unwrap()
                .any(|held| held.unwrap().as_os_str() == name);
            let ino = parent.metadata()?.ino();
            log.borrow_mut()
                .push((name.to_string_lossy().into_owned(), ino, held));
            sync_parent(parent, child)
        }
    }

    /// A sync step that fails.
    fn failing_sync(_: &File, _: &Path) -> std::io::Result<()> {
        Err(std::io::Error::from_raw_os_error(libc::EIO))
    }

    /// Creating the store syncs `tmp` into the store directory and the store directory into
    /// the data_dir, each after it appears there and before `open` returns, whether it is made
    /// in place or published under sudo; `tmp` is synced again when the run opens it by name.
    /// Without the syncs a host crash can remove the store while a snapshot that names one of
    /// its entries survives. A failed sync fails the open.
    #[test]
    fn creating_the_store_syncs_each_new_directory_into_its_parent() {
        let me = this_user();
        for invoker in [None, Some(&me)] {
            let data = tempfile::tempdir().unwrap();
            let log = RefCell::new(Vec::new());
            let record = recording_sync(&log);
            let steps = CreateSteps {
                sync_parent: &record,
                ..CreateSteps::REAL
            };
            let store = PmemStore::open_with(data.path(), invoker, steps).unwrap();
            let data_ino = std::fs::metadata(data.path()).unwrap().ino();
            let store_ino = store.pmem.metadata().unwrap().ino();
            assert_eq!(
                log.take(),
                [
                    ("tmp".to_string(), store_ino, true),
                    ("pmem".to_string(), data_ino, true),
                    ("tmp".to_string(), store_ino, true)
                ],
                "invoker {}: tmp and the store directory were not each synced, after they \
                 appeared, into the directory that holds them",
                invoker.is_some()
            );
        }
        let data = tempfile::tempdir().unwrap();
        let steps = CreateSteps {
            sync_parent: &failing_sync,
            ..CreateSteps::REAL
        };
        let error = format!(
            "{:#}",
            PmemStore::open_with(data.path(), None, steps).expect_err("a failed sync was ignored")
        );
        assert!(
            error.contains("syncing the directory that holds"),
            "{error}"
        );
    }

    /// Ingest syncs the store directory once before it returns an entry: one it published,
    /// one another run published while it copied, and one it found already there. Another
    /// run links its entry before it syncs the directory, so without the sync a found entry
    /// could vanish in a host crash after this run booted a VM or saved a snapshot that names
    /// it. A failed sync fails the ingest.
    #[test]
    fn ingest_syncs_the_store_directory_before_it_returns_an_entry() {
        let data = tempfile::tempdir().unwrap();
        let store = PmemStore::open(data.path()).unwrap();
        let store_ino = store.pmem.metadata().unwrap().ino();
        let disk = tempfile::tempdir_in(test_image_dir()).unwrap();
        let source = image_in(disk.path(), 2 * MIB);
        let raced_disk = tempfile::tempdir_in(test_image_dir()).unwrap();
        let raced = image_in(raced_disk.path(), 2 * MIB);
        let ingest = |case: &str, source: &Path, after_copy: &dyn Fn() -> Result<()>| {
            let log = RefCell::new(Vec::new());
            let record = recording_sync(&log);
            let entry = store.ingest_with(source, after_copy, &record).unwrap();
            let name = entry.file_name().unwrap().to_string_lossy().into_owned();
            assert_eq!(
                log.take(),
                [(name, store_ino, true)],
                "{case}: ingest did not sync the store directory, holding the entry, once \
                 before it returned the entry"
            );
        };
        ingest("published", &source, &|| Ok(()));
        ingest("lost the race", &raced, &|| store.ingest(&raced).map(drop));
        ingest("found", &source, &|| Ok(()));
        let error = format!(
            "{:#}",
            store
                .ingest_with(&source, || Ok(()), &failing_sync)
                .expect_err("a failed sync of a found entry was ignored")
        );
        assert!(
            error.contains("syncing the directory that holds"),
            "{error}"
        );
    }

    /// A run that finds the store directory, or its tmp, already there syncs it into the
    /// directory that holds it before `open` returns, as it syncs one it creates. Another run
    /// makes a directory before it syncs the parent, so a name found a moment after it
    /// appeared need not survive a host crash yet, while a VM or a snapshot of this run can
    /// name an entry inside it. Covered: both directories found, the store directory found
    /// before the run that made it created its tmp, each with and without an invoker, and a
    /// sudo run that loses both publications and opens the winner's directories.
    #[test]
    fn a_run_that_finds_a_store_directory_syncs_it_into_its_parent() {
        let me = this_user();
        let synced = |data: &Path, store: &PmemStore| {
            [
                (
                    "pmem".to_string(),
                    std::fs::metadata(data).unwrap().ino(),
                    true,
                ),
                (
                    "tmp".to_string(),
                    store.pmem.metadata().unwrap().ino(),
                    true,
                ),
            ]
        };
        for invoker in [None, Some(&me)] {
            for with_tmp in [true, false] {
                let data = tempfile::tempdir().unwrap();
                std::fs::create_dir(data.path().join("pmem")).unwrap();
                if with_tmp {
                    std::fs::create_dir(data.path().join("pmem/tmp")).unwrap();
                }
                let log = RefCell::new(Vec::new());
                let record = recording_sync(&log);
                let steps = CreateSteps {
                    sync_parent: &record,
                    ..CreateSteps::REAL
                };
                let store = PmemStore::open_with(data.path(), invoker, steps).unwrap();
                assert_eq!(
                    log.take(),
                    synced(data.path(), &store),
                    "invoker {}, tmp already there {with_tmp}: the store directory and tmp were \
                     not each synced into the directory that holds them",
                    invoker.is_some()
                );
            }
        }

        let data = tempfile::tempdir().unwrap();
        let finals = [data.path().join("pmem"), data.path().join("pmem/tmp")];
        let publish_first = |to: &Path| {
            if finals.iter().any(|path| path == to) {
                std::fs::create_dir(to).unwrap();
            }
        };
        let log = RefCell::new(Vec::new());
        let record = recording_sync(&log);
        let steps = CreateSteps {
            publishing: &publish_first,
            sync_parent: &record,
            ..CreateSteps::REAL
        };
        let store = PmemStore::open_with(data.path(), Some(&me), steps).unwrap();
        // The first sync is of the tmp this run made inside its own store directory, which it
        // removed when it lost.
        let log = log.take();
        assert!(
            log.ends_with(&synced(data.path(), &store)),
            "a run that lost both publications did not sync the winner's store directory and \
             tmp into the directories that hold them: {log:?}"
        );
    }
}
