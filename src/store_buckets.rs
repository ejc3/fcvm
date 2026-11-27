//! A per-start entry in a store directory, spread over 256 buckets that each store makes once.
//!
//! Every call that creates a directory entry (a mkdir, an open with O_CREAT, a rename's
//! target) takes the directory's inode lock for writing, and on btrfs the holder can wait out
//! a transaction commit (every clone's disk is a subvolume snapshot, which ends in one). So
//! the starts that write into one directory take turns, and the start rate is bounded by how
//! many starts share each directory. A directory that holds one entry per VM, or per owner of
//! host resources, is split into 256 buckets named by the first byte of sha256 of the
//! entry's key, so a bucket is shared by one start in 256.
//!
//! What a start costs is a lookup: [`ensure`] looks the bucket up and does nothing else when
//! it exists, and [`open`] opens it `O_PATH`, which does not call the filesystem's open
//! (opening a directory on btrfs takes its inode lock). The first start of a store that finds
//! its bucket missing takes an exclusive flock on the parent, looks again, and makes every
//! missing bucket: each under a name of its own, given the parent's owner and permission
//! bits, then renamed into place without replacing one another process made first
//! (RENAME_NOREPLACE). So no start finds a bucket it cannot use (root and a user share a data
//! directory), and correctness does not rest on the flock, which does not cross a FUSE map in
//! a nested guest: there it costs duplicate work, never a wrong bucket.
//!
//! The flock and every other call that needs an open file are on a descriptor opened for
//! reading, never on an `O_PATH` one (flock and fsync fail on those with EBADF).
//!
//! Callers: the VMs' runtime directories (`vm-disks/<b>/<vm_id>`, `crate::paths`) and the
//! owner records of host resources and their locks (`reclaim/<euid>/<b>/<id>.*`,
//! `crate::reclaim`).

use anyhow::{Context, Result};
use nix::fcntl::{AtFlags, OFlag};
use nix::sys::stat::Mode;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::fs::MetadataExt;
use std::path::Path;

/// How many buckets a store directory is split into.
pub const COUNT: usize = 256;

/// The bucket of `key`: the first byte of sha256(key) as two lowercase hex digits. A pure
/// function of the key, so whoever knows the key finds the entry again.
pub fn name(key: &str) -> String {
    use sha2::Digest;
    format!("{:02x}", sha2::Sha256::digest(key.as_bytes())[0])
}

/// Whether `name` is a bucket's name (two lowercase hex digits).
pub fn is_name(name: &str) -> bool {
    name.len() == 2
        && name
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Every bucket's name, in order.
pub fn names() -> impl Iterator<Item = String> {
    (0..=u8::MAX).map(|byte| format!("{byte:02x}"))
}

/// Whether the directory `parent` holds a directory named `name`, without following a link.
fn exists(parent: &impl AsFd, name: &str) -> nix::Result<bool> {
    match nix::sys::stat::fstatat(parent, name, AtFlags::AT_SYMLINK_NOFOLLOW) {
        Ok(stat) => Ok(stat.st_mode & libc::S_IFMT == libc::S_IFDIR),
        Err(nix::errno::Errno::ENOENT) => Ok(false),
        Err(errno) => Err(errno),
    }
}

/// Make sure the directory `parent` (open, `O_PATH` or not, at `parent_path`) holds the
/// bucket `bucket`. Returns how many buckets this call made: 0 when it exists, which costs
/// one lookup, and all the missing ones otherwise ([`self`]).
pub fn ensure_at(parent: &impl AsFd, parent_path: &Path, bucket: &str) -> Result<usize> {
    if exists(parent, bucket).with_context(|| at(parent_path, bucket))? {
        return Ok(0);
    }
    // A descriptor that can be locked: the one given may be O_PATH.
    let lockable: OwnedFd = nix::fcntl::openat(
        parent,
        ".",
        OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC,
        Mode::empty(),
    )
    .with_context(|| format!("opening {}", parent_path.display()))?;
    let lockable = nix::fcntl::Flock::lock(
        std::fs::File::from(lockable),
        nix::fcntl::FlockArg::LockExclusive,
    )
    .map_err(|(_, errno)| errno)
    .with_context(|| format!("locking {}", parent_path.display()))?;
    if exists(&*lockable, bucket).with_context(|| at(parent_path, bucket))? {
        return Ok(0);
    }
    let owner = lockable
        .metadata()
        .with_context(|| format!("reading {}", parent_path.display()))?;
    let mut made = 0;
    for name in names() {
        if exists(&*lockable, &name).with_context(|| at(parent_path, &name))? {
            continue;
        }
        make(&lockable, &name, &owner)
            .with_context(|| format!("making the bucket {}", parent_path.join(&name).display()))?;
        made += 1;
    }
    Ok(made)
}

/// [`ensure_at`] by path. `parent` is made only when it is missing: a mkdir of a directory
/// that exists fails with EEXIST but still takes the lock of the directory it would be made
/// in, for writing, which every start would then queue on.
pub fn ensure(parent: &Path, bucket: &str) -> Result<usize> {
    let open = || {
        nix::fcntl::open(
            parent,
            OFlag::O_PATH | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC,
            Mode::empty(),
        )
    };
    let fd = match open() {
        Ok(fd) => fd,
        Err(nix::errno::Errno::ENOENT) => {
            crate::paths::ensure_dir(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
            open().with_context(|| format!("opening {}", parent.display()))?
        }
        Err(errno) => return Err(errno).with_context(|| format!("opening {}", parent.display())),
    };
    ensure_at(&fd, parent, bucket)
}

/// The bucket `bucket` of `parent`, opened `O_PATH` without following a link: a descriptor
/// for `fstat` and as the directory of `*at` calls. `None` when there is none.
pub fn open(parent: &impl AsFd, bucket: &str) -> nix::Result<Option<OwnedFd>> {
    match nix::fcntl::openat(
        parent,
        bucket,
        OFlag::O_PATH | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
        Mode::empty(),
    ) {
        Ok(fd) => Ok(Some(fd)),
        Err(nix::errno::Errno::ENOENT) => Ok(None),
        Err(errno) => Err(errno),
    }
}

/// One bucket made under a name of its own, given the parent's owner and mode, then renamed
/// into place unless another process put one there first.
fn make(parent: &std::fs::File, name: &str, owner: &std::fs::Metadata) -> Result<()> {
    let staged = format!(".{name}.{}", uuid::Uuid::new_v4().simple());
    nix::sys::stat::mkdirat(parent, staged.as_str(), Mode::from_bits_truncate(0o700))
        .with_context(|| format!("creating {staged}"))?;
    let made = (|| -> Result<()> {
        // SAFETY: geteuid has no preconditions.
        if unsafe { libc::geteuid() } == 0 {
            nix::unistd::fchownat(
                parent,
                staged.as_str(),
                Some(nix::unistd::Uid::from_raw(owner.uid())),
                Some(nix::unistd::Gid::from_raw(owner.gid())),
                AtFlags::AT_SYMLINK_NOFOLLOW,
            )
            .context("giving the bucket to its parent's owner")?;
        }
        nix::sys::stat::fchmodat(
            parent,
            staged.as_str(),
            Mode::from_bits_truncate(owner.mode() & 0o7777),
            nix::sys::stat::FchmodatFlags::FollowSymlink,
        )
        .context("giving the bucket its parent's permission bits")?;
        match nix::fcntl::renameat2(
            parent,
            staged.as_str(),
            parent,
            name,
            nix::fcntl::RenameFlags::RENAME_NOREPLACE,
        ) {
            Ok(()) => Ok(()),
            // Another process put the bucket there first.
            Err(nix::errno::Errno::EEXIST) => Ok(()),
            Err(errno) => Err(errno).context("renaming the bucket into place"),
        }
    })();
    let _ = nix::unistd::unlinkat(
        parent,
        staged.as_str(),
        nix::unistd::UnlinkatFlags::RemoveDir,
    );
    made
}

fn at(parent: &Path, name: &str) -> String {
    format!("looking up {}", parent.join(name).display())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn store() -> (tempfile::TempDir, std::path::PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let parent = root.path().join("records");
        std::fs::create_dir(&parent).unwrap();
        (root, parent)
    }

    fn buckets_in(parent: &Path) -> (usize, Vec<String>) {
        let mut buckets = 0;
        let mut other = Vec::new();
        for entry in std::fs::read_dir(parent).unwrap() {
            let name = entry.unwrap().file_name().to_string_lossy().into_owned();
            if is_name(&name) {
                buckets += 1;
            } else {
                other.push(name);
            }
        }
        (buckets, other)
    }

    /// The first start of a store makes all 256 buckets, and the starts beside it make none:
    /// a new store's first wave does not make its buckets one start at a time under the
    /// parent's inode lock. Through the missing-bucket path, so the flock is taken on a
    /// descriptor that can be locked although the parent is given `O_PATH`.
    #[test]
    fn the_first_start_of_a_store_makes_every_bucket_and_the_others_none() {
        let (_root, parent) = store();
        let made: Vec<usize> = std::thread::scope(|scope| {
            let starts: Vec<_> = (0..32u8)
                .map(|start| {
                    let parent = &parent;
                    scope.spawn(move || {
                        ensure(parent, &format!("{:02x}", start.wrapping_mul(8))).unwrap()
                    })
                })
                .collect();
            starts
                .into_iter()
                .map(|start| start.join().unwrap())
                .collect()
        });
        let (buckets, other) = buckets_in(&parent);
        assert_eq!(buckets, COUNT, "every bucket exists after the first wave");
        assert!(other.is_empty(), "staging directories left: {other:?}");
        assert_eq!(
            made.iter().sum::<usize>(),
            COUNT,
            "each bucket was made once: {made:?}"
        );
        assert_eq!(
            made.iter().filter(|&&n| n > 0).count(),
            1,
            "one start made the buckets and the others none: {made:?}"
        );
        assert_eq!(
            ensure(&parent, "7f").unwrap(),
            0,
            "a made store makes nothing"
        );
    }

    /// A bucket has its parent's permission bits, not the umask's.
    #[test]
    fn a_bucket_has_the_permission_bits_of_its_parent() {
        let (_root, parent) = store();
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o1777)).unwrap();
        ensure(&parent, "7f").unwrap();
        let mode = std::fs::metadata(parent.join("7f"))
            .unwrap()
            .permissions()
            .mode()
            & 0o7777;
        assert_eq!(mode, 0o1777);
    }

    /// An open bucket is a descriptor for fstat and for `*at` calls: a file made through it
    /// lands in the bucket.
    #[test]
    fn an_open_bucket_serves_fstat_and_at_calls() {
        let (_root, parent) = store();
        let fd = nix::fcntl::open(
            &parent,
            OFlag::O_PATH | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC,
            Mode::empty(),
        )
        .unwrap();
        assert!(open(&fd, "3c").unwrap().is_none(), "no bucket yet");
        ensure_at(&fd, &parent, "3c").unwrap();
        let bucket = open(&fd, "3c").unwrap().unwrap();
        assert!(nix::sys::stat::fstat(&bucket).unwrap().st_mode & libc::S_IFMT == libc::S_IFDIR);
        let file: OwnedFd = nix::fcntl::openat(
            &bucket,
            "x.lock",
            OFlag::O_CREAT | OFlag::O_WRONLY | OFlag::O_CLOEXEC,
            Mode::from_bits_truncate(0o600),
        )
        .unwrap();
        drop(file);
        assert!(parent.join("3c/x.lock").is_file());
    }

    #[test]
    fn a_name_is_two_hex_digits_of_the_key() {
        assert_eq!(name("vm-0123456789abcdef0123456789abcdef").len(), 2);
        assert!(is_name(&name("anything")));
        assert!(!is_name("7") && !is_name("7F") && !is_name("vm"));
        assert_eq!(names().count(), COUNT);
    }
}
