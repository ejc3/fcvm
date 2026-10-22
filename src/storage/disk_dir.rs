//! Content identity of the extra disks a launch attaches, for the snapshot key.
//!
//! A pre-start snapshot keeps its own copy of every `--disk-dir` image and a cache hit
//! restores that copy, so the key has to name what the directory held, not where it was.
//! The key carries a hash of a read-only `--disk-dir`'s tree, taken in place, so a cache
//! hit copies nothing. Only a cold boot builds an image, and it builds it from a private
//! copy whose hash must equal the key's, so a directory that changes during the launch
//! cannot put new content under an old key.
//! A read-only `--disk` is attached in place, so its key carries the image's file
//! identity, which a rewrite cannot present again.

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};

/// What a launch resolved its `--disk` and `--disk-dir` specs to, once, before the
/// snapshot key is computed. Both lists are parallel to their specs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResolvedExtraDisks {
    /// Per `--disk` spec: the image's file identity when the disk is read-only.
    pub disk_identities: Vec<Option<String>>,
    /// Per `--disk-dir` spec: its content hash and staging path when it is read-only.
    pub disk_dirs: Vec<Option<StagedDiskDir>>,
}

/// A read-only `--disk-dir` resolved for this launch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagedDiskDir {
    /// The directory a cold boot builds the image from: the private copy that
    /// [`DiskDirStaging::stage`] makes, not the host directory.
    pub source: PathBuf,
    /// SHA-256 of the directory's tree (see [`hash_tree`]), the value the key carries.
    pub content_hash: String,
}

/// Split `HOST:GUEST[:ro]` into its host path, guest path and read-only flag, the way
/// the attach step reads it. None when the spec has no guest path.
fn split_spec(spec: &str) -> Option<(&str, &str, bool)> {
    let (rest, read_only) = match spec.strip_suffix(":ro") {
        Some(rest) => (rest, true),
        None => (spec, false),
    };
    let (host, guest) = rest.split_once(':')?;
    Some((host, guest, read_only))
}

/// The snapshot key's extra-disk list: `--disk`, then `--disk-dir`, then `--nfs`, in
/// device order. A resolved read-only `--disk` carries its image's identity beside the
/// spec, and a resolved read-only `--disk-dir` carries its content hash in place of the
/// host path, so a re-synced directory with the same content keeps its key.
pub fn snapshot_key_extra_disks(
    disk: &[String],
    disk_dir: &[String],
    nfs: &[String],
    resolved: Option<&ResolvedExtraDisks>,
) -> Vec<String> {
    let disks = disk.iter().enumerate().map(|(i, spec)| {
        match resolved
            .and_then(|r| r.disk_identities.get(i))
            .and_then(Option::as_ref)
        {
            Some(identity) => format!("{spec} identity={identity}"),
            None => spec.clone(),
        }
    });
    let dirs = disk_dir.iter().enumerate().map(|(i, spec)| {
        let staged = resolved
            .and_then(|r| r.disk_dirs.get(i))
            .and_then(Option::as_ref);
        match (staged, split_spec(spec)) {
            (Some(staged), Some((_, guest, _))) => {
                format!("sha256={}:{guest}:ro", staged.content_hash)
            }
            _ => spec.clone(),
        }
    });
    disks.chain(dirs).chain(nfs.iter().cloned()).collect()
}

/// The private copies of one launch's read-only `--disk-dir` specs, under one root,
/// made only for a cold boot. Dropping it removes the root.
#[derive(Debug)]
pub struct DiskDirStaging {
    root: PathBuf,
    /// (host directory, staged copy) per read-only spec that was staged.
    staged: Vec<(PathBuf, StagedDiskDir)>,
    present: bool,
}

impl DiskDirStaging {
    /// Resolve `disk` and `disk_dir` for the snapshot key. Each read-only `--disk-dir`
    /// is hashed in place and copies nothing; [`stage`](Self::stage) copies it to
    /// `root/<index>` when a cold boot needs the image.
    pub fn resolve(
        root: &Path,
        disk: &[String],
        disk_dir: &[String],
    ) -> Result<(Self, ResolvedExtraDisks)> {
        let mut staging = Self {
            root: root.to_path_buf(),
            staged: Vec::new(),
            present: false,
        };
        let mut resolved = ResolvedExtraDisks::default();
        for spec in disk {
            let identity = match split_spec(spec) {
                Some((host, _, true)) => Some(
                    crate::utils::file_identity(Path::new(host))
                        .with_context(|| format!("stat --disk image {host} for snapshot key"))?,
                ),
                _ => None,
            };
            resolved.disk_identities.push(identity);
        }
        for (i, spec) in disk_dir.iter().enumerate() {
            let staged = match split_spec(spec) {
                Some((host, _, true)) => {
                    let host = PathBuf::from(host);
                    if !host.is_dir() {
                        bail!(
                            "Source directory does not exist or is not a directory: {}",
                            host.display()
                        );
                    }
                    // A HOST_DIR given as a symlink names its target: hashing and
                    // `cp -a` would otherwise both take the link itself, and the staged
                    // copy would be a link back to the mutable original, or dangle.
                    let host = std::fs::canonicalize(&host)
                        .with_context(|| format!("resolving --disk-dir {}", host.display()))?;
                    let content_hash = hash_tree(&host).with_context(|| {
                        format!("hashing --disk-dir {} for snapshot key", host.display())
                    })?;
                    let resolved = StagedDiskDir {
                        source: root.join(i.to_string()),
                        content_hash,
                    };
                    staging.staged.push((host, resolved.clone()));
                    Some(resolved)
                }
                _ => None,
            };
            resolved.disk_dirs.push(staged);
        }
        Ok((staging, resolved))
    }

    /// Remove the copies, and the root's parent when that leaves it empty (the data
    /// directory of a VM whose launch failed before creating anything else there).
    pub fn remove(&mut self) {
        if self.present {
            if let Err(error) = std::fs::remove_dir_all(&self.root) {
                if error.kind() != std::io::ErrorKind::NotFound {
                    tracing::warn!(root = %self.root.display(), %error, "removing --disk-dir staging");
                }
            }
            if let Some(parent) = self.root.parent() {
                // Fails, as intended, while the VM's data directory holds anything else.
                let _ = std::fs::remove_dir(parent);
            }
            self.present = false;
        }
    }

    /// Copy each read-only host directory to its staging path and hash the copy, for a
    /// cold boot to build the image from. Fails if a copy's hash is not the one the
    /// snapshot key carries, because an image built from it would sit under that key.
    pub fn stage(&mut self) -> Result<()> {
        if self.present || self.staged.is_empty() {
            return Ok(());
        }
        self.present = true;
        for (host, staged) in &self.staged {
            let copy = stage_one(host, &staged.source)?;
            if copy.content_hash != staged.content_hash {
                bail!(
                    "--disk-dir {} changed during the launch (content {} when the snapshot key \
                     was computed, {} now); run again",
                    host.display(),
                    staged.content_hash,
                    copy.content_hash
                );
            }
        }
        Ok(())
    }
}

impl Drop for DiskDirStaging {
    fn drop(&mut self) {
        self.remove();
    }
}

/// Copy `host` to `dest` (which must not exist) and hash the copy.
fn stage_one(host: &Path, dest: &Path) -> Result<StagedDiskDir> {
    if !host.is_dir() {
        bail!(
            "Source directory does not exist or is not a directory: {}",
            host.display()
        );
    }
    copy_tree(host, dest)?;
    let content_hash = hash_tree(dest)
        .with_context(|| format!("hashing staged copy of --disk-dir {}", host.display()))?;
    tracing::info!(
        host = %host.display(),
        staging = %dest.display(),
        %content_hash,
        "staged read-only --disk-dir"
    );
    Ok(StagedDiskDir {
        source: dest.to_path_buf(),
        content_hash,
    })
}

/// `cp -a --reflink=auto host dest`: a reflink copy where the filesystem allows one.
fn copy_tree(host: &Path, dest: &Path) -> Result<()> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating --disk-dir staging {}", parent.display()))?;
    }
    let output = std::process::Command::new("cp")
        .args(["-a", "--reflink=auto", "--no-target-directory", "--"])
        .arg(host)
        .arg(dest)
        .output()
        .context("running cp for --disk-dir staging")?;
    if !output.status.success() {
        bail!(
            "copying --disk-dir {} to {} failed: {}",
            host.display(),
            dest.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// SHA-256 over a sorted walk of `root`. Each entry contributes its path relative to
/// `root`, file type, permission bits, the extended attributes this process can list
/// (names and values, sorted; `cp -a` and `mkfs.ext4 -d` carry them into the image),
/// then a regular file's bytes, a symlink's target, or a device's number. Timestamps are left out, so a directory recreated with the
/// same content hashes the same. The hash covers what lands in the image: uid and gid
/// enter it only when fcvm runs as root, because a non-root fcvm's copy, and so its
/// image, belongs to the copier whatever the source's owners.
pub fn hash_tree(root: &Path) -> Result<String> {
    hash_tree_with(root, nix::unistd::geteuid().is_root())
}

/// [`hash_tree`] with the decision whether the copy keeps the source's owners passed in.
fn hash_tree_with(root: &Path, keep_owners: bool) -> Result<String> {
    let mut hasher = Sha256::new();
    hash_entry(root, Path::new(""), keep_owners, &mut hasher)?;
    Ok(format!("{:x}", hasher.finalize()))
}

/// The extended attributes of `path` (not following a symlink) that this process can
/// list and read, sorted by name. A filesystem without xattr support has none.
fn extended_attributes(path: &Path) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes())
        .with_context(|| format!("NUL in {}", path.display()))?;
    let unsupported = |error: &std::io::Error| error.raw_os_error() == Some(libc::ENOTSUP);
    // Sized read with a retry: the list or a value can grow between the two calls.
    let read = |call: &dyn Fn(*mut libc::c_char, usize) -> isize| loop {
        let size = call(std::ptr::null_mut(), 0);
        if size < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let mut buf = vec![0u8; size as usize];
        let got = call(buf.as_mut_ptr().cast(), buf.len());
        if got >= 0 {
            buf.truncate(got as usize);
            return Ok(buf);
        }
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ERANGE) {
            return Err(error);
        }
    };
    let names = match read(&|buf, len| {
        // SAFETY: c_path is NUL-terminated; buf is null with len 0 or valid for len bytes.
        unsafe { libc::llistxattr(c_path.as_ptr(), buf, len) }
    }) {
        Ok(names) => names,
        Err(error) if unsupported(&error) => return Ok(Vec::new()),
        Err(error) => {
            return Err(error).with_context(|| format!("listing xattrs of {}", path.display()))
        }
    };
    let mut xattrs = Vec::new();
    for name in names.split(|b| *b == 0).filter(|n| !n.is_empty()) {
        let c_name = std::ffi::CString::new(name).expect("split on NUL");
        let value = read(&|buf, len| {
            // SAFETY: both strings are NUL-terminated; buf is null with len 0 or valid
            // for len bytes.
            unsafe { libc::lgetxattr(c_path.as_ptr(), c_name.as_ptr(), buf.cast(), len) }
        })
        .with_context(|| {
            format!(
                "reading xattr {} of {}",
                String::from_utf8_lossy(name),
                path.display()
            )
        })?;
        xattrs.push((name.to_vec(), value));
    }
    xattrs.sort();
    Ok(xattrs)
}

fn hash_bytes(hasher: &mut Sha256, bytes: &[u8]) {
    hasher.update((bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

fn hash_entry(path: &Path, relative: &Path, keep_owners: bool, hasher: &mut Sha256) -> Result<()> {
    let md = std::fs::symlink_metadata(path).with_context(|| format!("stat {}", path.display()))?;
    let ft = md.file_type();
    let kind: u8 = if ft.is_dir() {
        b'd'
    } else if ft.is_file() {
        b'f'
    } else if ft.is_symlink() {
        b'l'
    } else if ft.is_block_device() {
        b'b'
    } else if ft.is_char_device() {
        b'c'
    } else if ft.is_fifo() {
        b'p'
    } else if ft.is_socket() {
        b's'
    } else {
        b'?'
    };
    hash_bytes(hasher, relative.as_os_str().as_bytes());
    hasher.update([kind]);
    hasher.update((md.mode() & 0o7777).to_le_bytes());
    if keep_owners {
        hasher.update(md.uid().to_le_bytes());
        hasher.update(md.gid().to_le_bytes());
    }
    let xattrs = extended_attributes(path)?;
    hasher.update((xattrs.len() as u64).to_le_bytes());
    for (name, value) in xattrs {
        hash_bytes(hasher, &name);
        hash_bytes(hasher, &value);
    }
    match kind {
        b'f' => {
            hasher.update(md.len().to_le_bytes());
            let mut file =
                std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
            let copied = std::io::copy(&mut file, hasher)
                .with_context(|| format!("read {}", path.display()))?;
            if copied != md.len() {
                bail!("{} changed size while it was hashed", path.display());
            }
        }
        b'l' => {
            let target =
                std::fs::read_link(path).with_context(|| format!("readlink {}", path.display()))?;
            hash_bytes(hasher, target.as_os_str().as_bytes());
        }
        b'b' | b'c' => hasher.update(md.rdev().to_le_bytes()),
        b'd' => {
            let mut names = std::fs::read_dir(path)
                .with_context(|| format!("read directory {}", path.display()))?
                .map(|entry| entry.map(|e| e.file_name()))
                .collect::<std::io::Result<Vec<_>>>()
                .with_context(|| format!("read directory {}", path.display()))?;
            names.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
            hasher.update((names.len() as u64).to_le_bytes());
            for name in names {
                hash_entry(
                    &path.join(&name),
                    &relative.join(&name),
                    keep_owners,
                    hasher,
                )?;
            }
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_tree(root: &Path, payload: &[u8]) {
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("a.txt"), b"alpha").unwrap();
        std::fs::write(root.join("sub/b.txt"), payload).unwrap();
        std::os::unix::fs::symlink("a.txt", root.join("link")).unwrap();
    }

    fn key(dir: &Path, staging: &Path) -> (String, DiskDirStaging) {
        let specs = vec![format!("{}:/data:ro", dir.display())];
        let (staged, resolved) = DiskDirStaging::resolve(staging, &[], &specs).unwrap();
        let key = snapshot_key_extra_disks(&[], &specs, &[], Some(&resolved)).join("\n");
        (key, staged)
    }

    /// Two trees at one path that differ in one file's bytes get different keys, and the
    /// same content recreated with new timestamps gets the same key.
    #[test]
    fn disk_dir_key_follows_content_not_timestamps() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("data");
        write_tree(&dir, b"bravo");
        let (first, staged) = key(&dir, &tmp.path().join("s1"));
        drop(staged);

        // Same content, every file recreated: new inodes, mtimes and ctimes.
        std::fs::remove_dir_all(&dir).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        write_tree(&dir, b"bravo");
        let (recreated, staged) = key(&dir, &tmp.path().join("s2"));
        drop(staged);
        assert_eq!(
            first, recreated,
            "recreated identical content changed the key"
        );

        std::fs::write(dir.join("sub/b.txt"), b"brav0").unwrap();
        let (changed, _staged) = key(&dir, &tmp.path().join("s3"));
        assert_ne!(first, changed, "a file's new bytes kept the old key");
    }

    /// The image is built from the staging copy, and dropping the staging removes it.
    #[test]
    fn staging_copy_is_the_image_source_and_is_removed() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("data");
        write_tree(&dir, b"bravo");
        let root = tmp.path().join("staging");
        let specs = vec![
            format!("{}:/rw", dir.display()),
            format!("{}:/data:ro", dir.display()),
        ];
        let (mut staged, resolved) = DiskDirStaging::resolve(&root, &[], &specs).unwrap();
        assert_eq!(
            resolved.disk_dirs[0], None,
            "a read-write --disk-dir is not staged"
        );
        let copy = resolved.disk_dirs[1].as_ref().unwrap();
        assert_eq!(copy.source, root.join("1"));
        staged.stage().unwrap();
        // Changing the host directory after staging does not reach the staged copy.
        std::fs::write(dir.join("sub/b.txt"), b"changed").unwrap();
        assert_eq!(
            std::fs::read(copy.source.join("sub/b.txt")).unwrap(),
            b"bravo"
        );
        assert_eq!(hash_tree(&copy.source).unwrap(), copy.content_hash);
        drop(staged);
        assert!(
            !root.exists(),
            "dropping the staging left {}",
            root.display()
        );
        assert!(tmp.path().exists(), "a non-empty parent was removed");
    }

    /// Staging unchanged content succeeds, and staging content changed since the key was
    /// computed fails, before and after a removal.
    #[test]
    fn stage_refuses_content_the_key_does_not_name() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("data");
        write_tree(&dir, b"bravo");
        let root = tmp.path().join("staging");
        let specs = vec![format!("{}:/data:ro", dir.display())];
        let (mut staged, _) = DiskDirStaging::resolve(&root, &[], &specs).unwrap();
        staged.stage().unwrap();
        assert!(root.join("0/sub/b.txt").exists());
        staged.remove();
        assert!(!root.exists());
        std::fs::write(dir.join("sub/b.txt"), b"changed").unwrap();
        let error = staged.stage().unwrap_err();
        assert!(
            format!("{error:#}").contains("changed during the launch"),
            "{error:#}"
        );
    }

    /// A HOST_DIR given as a symlink to a directory, relative or absolute, hashes like
    /// the directory and stages its contents, not the link.
    #[test]
    fn a_symlinked_host_dir_hashes_and_stages_its_target() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("data");
        write_tree(&dir, b"bravo");
        let relative = tmp.path().join("relative-link");
        std::os::unix::fs::symlink("data", &relative).unwrap();
        let absolute = tmp.path().join("absolute-link");
        std::os::unix::fs::symlink(&dir, &absolute).unwrap();
        let direct = {
            let specs = vec![format!("{}:/data:ro", dir.display())];
            let (_s, resolved) =
                DiskDirStaging::resolve(&tmp.path().join("s-direct"), &[], &specs).unwrap();
            resolved.disk_dirs[0].clone().unwrap().content_hash
        };
        for (n, link) in [&relative, &absolute].into_iter().enumerate() {
            let root = tmp.path().join(format!("s{n}"));
            let specs = vec![format!("{}:/data:ro", link.display())];
            let (mut staging, resolved) = DiskDirStaging::resolve(&root, &[], &specs).unwrap();
            let staged = resolved.disk_dirs[0].clone().unwrap();
            assert_eq!(
                staged.content_hash,
                direct,
                "{} hashed unlike its target",
                link.display()
            );
            staging.stage().unwrap();
            let md = std::fs::symlink_metadata(&staged.source).unwrap();
            assert!(
                md.is_dir(),
                "{} staged a link, not the directory",
                link.display()
            );
            assert_eq!(
                std::fs::read(staged.source.join("sub/b.txt")).unwrap(),
                b"bravo"
            );
        }
    }

    /// Set a `user.` extended attribute, or return the error.
    fn set_user_xattr(path: &Path, name: &str, value: &[u8]) -> std::io::Result<()> {
        use std::os::unix::ffi::OsStrExt;
        let path = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        let name = std::ffi::CString::new(name).unwrap();
        // SAFETY: both strings are NUL-terminated and the value slice outlives the call.
        let rc = unsafe {
            libc::lsetxattr(
                path.as_ptr(),
                name.as_ptr(),
                value.as_ptr().cast(),
                value.len(),
                0,
            )
        };
        if rc == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }

    /// Two trees that differ only in an extended attribute hash differently: `cp -a`
    /// and `mkfs.ext4 -d` carry xattrs into the image.
    #[test]
    fn an_extended_attribute_changes_the_hash() {
        // tmpfs takes user xattrs on recent kernels; fall back to the crate's own
        // directory, which is on a disk filesystem, and fail if neither does.
        let candidates = [
            std::env::temp_dir(),
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target"),
        ];
        let mut tried = Vec::new();
        let tmp = candidates
            .iter()
            .find_map(|base| {
                let tmp = tempfile::tempdir_in(base).ok()?;
                let probe = tmp.path().join("probe");
                std::fs::write(&probe, b"").unwrap();
                match set_user_xattr(&probe, "user.fcvm-probe", b"1") {
                    Ok(()) => Some(tmp),
                    Err(error) => {
                        tried.push(format!("{}: {error}", base.display()));
                        None
                    }
                }
            })
            .unwrap_or_else(|| panic!("no directory here takes user xattrs: {tried:?}"));
        let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
        write_tree(&a, b"bravo");
        write_tree(&b, b"bravo");
        assert_eq!(hash_tree(&a).unwrap(), hash_tree(&b).unwrap());
        set_user_xattr(&b.join("sub/b.txt"), "user.fcvm-test", b"x").unwrap();
        assert_ne!(
            hash_tree(&a).unwrap(),
            hash_tree(&b).unwrap(),
            "an extended attribute did not change the hash"
        );
        set_user_xattr(&a.join("sub/b.txt"), "user.fcvm-test", b"y").unwrap();
        assert_ne!(
            hash_tree(&a).unwrap(),
            hash_tree(&b).unwrap(),
            "an extended attribute's value did not change the hash"
        );
    }

    /// Resolving for the key copies nothing: a cache hit restores the snapshot's own
    /// image and must not pay for a copy of the directory.
    #[test]
    fn resolving_copies_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("data");
        write_tree(&dir, b"bravo");
        let root = tmp.path().join("staging");
        let specs = vec![format!("{}:/data:ro", dir.display())];
        let (_staging, resolved) = DiskDirStaging::resolve(&root, &[], &specs).unwrap();
        assert!(resolved.disk_dirs[0].is_some());
        assert!(
            !root.exists(),
            "resolution copied the directory to {}",
            root.display()
        );
    }

    /// What lands in the image decides the hash. A copy made without keeping owners
    /// (a non-root fcvm's) belongs to the copier whatever the source's owners, so the
    /// owners must not enter the hash then, and must when they are kept.
    #[test]
    fn owners_enter_the_hash_only_when_the_copy_keeps_them() {
        use std::os::unix::fs::MetadataExt;
        // A root-owned source, so its owners differ from a non-root copier's.
        let source = Path::new("/etc/skel");
        assert_eq!(
            std::fs::metadata(source).expect("/etc/skel").uid(),
            0,
            "the test needs a root-owned source"
        );
        let tmp = tempfile::tempdir().unwrap();
        let copy = tmp.path().join("copy");
        copy_tree(source, &copy).unwrap();
        if nix::unistd::geteuid().is_root() {
            // Root keeps the owners; give the copy others, as a non-root copy would have.
            fn chown_tree(path: &Path) {
                std::os::unix::fs::lchown(path, Some(65534), Some(65534)).unwrap();
                if std::fs::symlink_metadata(path).unwrap().is_dir() {
                    for entry in std::fs::read_dir(path).unwrap() {
                        chown_tree(&entry.unwrap().path());
                    }
                }
            }
            chown_tree(&copy);
        }
        assert_ne!(
            std::fs::metadata(&copy).unwrap().uid(),
            0,
            "the copy kept the source's owner"
        );
        assert_eq!(
            hash_tree_with(source, false).unwrap(),
            hash_tree_with(&copy, false).unwrap(),
            "owners that do not reach the image changed the hash"
        );
        assert_ne!(
            hash_tree_with(source, true).unwrap(),
            hash_tree_with(&copy, true).unwrap(),
            "owners kept in the image did not change the hash"
        );
    }

    /// A read-only --disk image rewritten in place gets a different key.
    #[test]
    fn disk_image_rewritten_in_place_changes_the_key() {
        let tmp = tempfile::tempdir().unwrap();
        let image = tmp.path().join("disk.img");
        std::fs::write(&image, b"one").unwrap();
        let file = std::fs::File::options().write(true).open(&image).unwrap();
        file.set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1 << 30))
            .unwrap();
        drop(file);
        let specs = vec![format!("{}:/mnt:ro", image.display())];
        let key = || {
            let (_staged, resolved) =
                DiskDirStaging::resolve(&tmp.path().join("s"), &specs, &[]).unwrap();
            snapshot_key_extra_disks(&specs, &[], &[], Some(&resolved))
        };
        let before = key();
        std::fs::write(&image, b"two").unwrap();
        assert_ne!(before, key(), "an in-place rewrite kept the key");
    }

    /// Without a resolution the list is the raw specs, in device order.
    #[test]
    fn unresolved_key_is_the_raw_specs() {
        let list = snapshot_key_extra_disks(
            &["/d:/a:ro".to_string()],
            &["/x:/b:ro".to_string()],
            &["/n:/c".to_string()],
            None,
        );
        assert_eq!(list, ["/d:/a:ro", "/x:/b:ro", "/n:/c"]);
    }
}
