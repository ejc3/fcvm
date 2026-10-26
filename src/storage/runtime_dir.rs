//! A VM's runtime directory (`vm-disks/<vm_id>`) as a mount target for its clones.
//!
//! A snapshot names its disks at the source VM's paths (`vm-disks/<source>/disks/...`).
//! A clone's VMM reaches its own disks through those paths only because its mount
//! namespace binds the clone's directory over `vm-disks/<source>`, and it opens them
//! when the snapshot is loaded. When the source VM's teardown removes its directory,
//! the kernel detaches every mount on it in every other namespace, so a clone whose VMM
//! is spawned but has not loaded the snapshot yet finds no disk at all.
//!
//! The lock that closes that window is a `flock` on the directory itself:
//!
//! | who | when | lock |
//! |-----|------|------|
//! | a restoring clone | from before its VMM is spawned until the snapshot load returns | shared, on each source directory its redirect binds over |
//! | the directory's own VM | around the removal of its directory at teardown | exclusive |
//!
//! A clone that waited behind a teardown holds a lock on a directory that no longer has
//! a name, so it checks the inode it locked against the path and starts again.
//!
//! A directory a restore recreates, after its VM is gone, is only a mountpoint: empty in
//! the host's namespace, with no VM of its own to remove it. The lease that ends last
//! removes it (`MountTargetLease`'s drop), once it can take the lock exclusively without
//! waiting and the directory is still the empty one it leased. A clone whose VMM keeps
//! resolving its vsock socket through the redirect (Cloud Hypervisor, and Firecracker
//! before 1.16.0) holds the lease on such a directory for its whole life
//! (`MountTargetLease::after_load`), so another clone's release cannot remove it from
//! under it.

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tracing::{debug, info, warn};

const POLL_INTERVAL: Duration = Duration::from_millis(100);
const WAIT_LOG_INTERVAL: Duration = Duration::from_secs(5);

/// A teardown removes its directory once, so a lease that finds its directory replaced
/// more often than this is not racing a teardown.
const MAX_LEASE_ATTEMPTS: usize = 8;

/// Shared locks on the source VM directories a restoring VMM's mount redirect binds
/// over. Their VMs' teardowns cannot remove them until this is dropped.
///
/// Dropping it removes each leased directory that was only a mountpoint, unless another
/// lease still holds it (see the module documentation).
#[must_use = "the source directories are protected only while the lease is held"]
pub struct MountTargetLease {
    dirs: Vec<LeasedDir>,
}

struct LeasedDir {
    path: PathBuf,
    file: std::fs::File,
    /// Empty when it was leased: recreated as a mount target after its VM was gone. A
    /// live VM's runtime directory always holds its disks.
    mountpoint_only: bool,
}

impl MountTargetLease {
    /// What a clone keeps once its VMM has opened the disks the snapshot names.
    ///
    /// A VMM that also resolves its vsock socket through the redirect for its whole life
    /// (`vsock_through_redirect`) keeps the directories that are only mountpoints, so no
    /// other clone's release removes them while it runs; the caller holds the returned
    /// lease until the clone exits. A live VM's directory is released either way, so that
    /// VM's teardown is not held up. Everything released here is removed if it was only a
    /// mountpoint and no other lease holds it.
    pub fn after_load(mut self, vsock_through_redirect: bool) -> Option<MountTargetLease> {
        if !vsock_through_redirect {
            return None;
        }
        let (kept, released): (Vec<_>, Vec<_>) =
            self.dirs.drain(..).partition(|dir| dir.mountpoint_only);
        drop(MountTargetLease { dirs: released });
        (!kept.is_empty()).then_some(MountTargetLease { dirs: kept })
    }
}

impl Drop for MountTargetLease {
    fn drop(&mut self) {
        for dir in self.dirs.drain(..) {
            remove_if_unused(dir);
        }
    }
}

/// Remove `dir` if it was leased as a bare mountpoint, no other lease holds it, and it is
/// still that directory and still empty. Never waits: a directory another lease holds is
/// left to whichever lease ends last.
fn remove_if_unused(dir: LeasedDir) {
    if !dir.mountpoint_only || fs2::FileExt::try_lock_exclusive(&dir.file).is_err() {
        return;
    }
    let same = match (dir.file.metadata(), std::fs::metadata(&dir.path)) {
        (Ok(locked), Ok(named)) => (locked.dev(), locked.ino()) == (named.dev(), named.ino()),
        _ => false,
    };
    let empty =
        same && std::fs::read_dir(&dir.path).is_ok_and(|mut entries| entries.next().is_none());
    if !empty {
        return;
    }
    match std::fs::remove_dir(&dir.path) {
        Ok(()) => debug!(dir = %dir.path.display(), "removed a recreated mount target"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => warn!(
            dir = %dir.path.display(),
            error = %error,
            "could not remove a recreated mount target"
        ),
    }
}

/// Lease every directory in `dirs`, creating any that is missing (a mount target must
/// exist).
pub async fn lease_mount_targets(dirs: &[PathBuf]) -> Result<MountTargetLease> {
    let mut held = MountTargetLease {
        dirs: Vec::with_capacity(dirs.len()),
    };
    for dir in dirs {
        let file = lease_one(dir).await?;
        let mountpoint_only = std::fs::read_dir(dir)
            .with_context(|| format!("reading mount target {}", dir.display()))?
            .next()
            .is_none();
        held.dirs.push(LeasedDir {
            path: dir.clone(),
            file,
            mountpoint_only,
        });
    }
    Ok(held)
}

async fn lease_one(dir: &Path) -> Result<std::fs::File> {
    lease_one_with(dir, |dir| std::fs::create_dir_all(dir)).await
}

/// `create` is `std::fs::create_dir_all` outside tests.
async fn lease_one_with(
    dir: &Path,
    mut create: impl FnMut(&Path) -> std::io::Result<()>,
) -> Result<std::fs::File> {
    for _ in 0..MAX_LEASE_ATTEMPTS {
        match create(dir) {
            Ok(()) => {}
            // mkdir found the directory, and a teardown removed it before the check that
            // it is one.
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("creating mount target {}", dir.display()))
            }
        }
        let file = match std::fs::File::open(dir) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("opening mount target {}", dir.display()))
            }
        };
        lock(&file, dir, false).await?;
        let locked = file
            .metadata()
            .with_context(|| format!("reading the locked mount target {}", dir.display()))?;
        match std::fs::metadata(dir) {
            Ok(named) if (named.dev(), named.ino()) == (locked.dev(), locked.ino()) => {
                debug!(dir = %dir.display(), "leased mount target");
                return Ok(file);
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("reading mount target {}", dir.display()))
            }
        }
        debug!(dir = %dir.display(), "mount target was removed while leasing it; retrying");
    }
    anyhow::bail!(
        "mount target {} was removed {MAX_LEASE_ATTEMPTS} times while it was being leased",
        dir.display()
    )
}

/// Remove a VM's runtime directory once no restore that binds over it still has to open
/// its disks. A missing directory is not an error.
pub async fn remove_runtime_dir(dir: &Path) -> Result<()> {
    let file = match std::fs::File::open(dir) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("opening runtime directory {}", dir.display()))
        }
    };
    lock(&file, dir, true).await?;
    match tokio::fs::remove_dir_all(dir).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("removing runtime directory {}", dir.display()))
        }
    }
}

async fn lock(file: &std::fs::File, dir: &Path, exclusive: bool) -> Result<()> {
    let started = Instant::now();
    let mut next_log = Duration::ZERO;
    loop {
        // Fully qualified: std::fs::File has inherent try_lock_* methods with another
        // error type, and inherent methods win over trait methods.
        let result = if exclusive {
            fs2::FileExt::try_lock_exclusive(file)
        } else {
            fs2::FileExt::try_lock_shared(file)
        };
        match result {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if next_log == Duration::ZERO {
                    failpoint::note("lock.wait", dir.display());
                }
                let waited = started.elapsed();
                if waited >= next_log {
                    if exclusive {
                        info!(
                            dir = %dir.display(),
                            waited_ms = waited.as_millis() as u64,
                            "VM teardown waiting for clone restores that bind over its runtime directory"
                        );
                    } else {
                        info!(
                            dir = %dir.display(),
                            waited_ms = waited.as_millis() as u64,
                            "clone restore waiting for the teardown of the VM it was snapshotted from"
                        );
                    }
                    next_log = waited + WAIT_LOG_INTERVAL;
                }
                tokio::time::sleep(POLL_INTERVAL).await;
            }
            Err(error) => {
                return Err(error).with_context(|| format!("locking {}", dir.display()));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ino(path: &Path) -> u64 {
        std::fs::metadata(path).unwrap().ino()
    }

    #[tokio::test]
    async fn teardown_waits_for_a_lease_then_removes_the_directory() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        std::fs::create_dir_all(source.join("disks")).unwrap();

        let lease = lease_mount_targets(std::slice::from_ref(&source))
            .await
            .unwrap();
        let removal = tokio::spawn({
            let source = source.clone();
            async move { remove_runtime_dir(&source).await }
        });
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(
            !removal.is_finished() && source.join("disks").is_dir(),
            "the teardown removed a directory a restore still binds over"
        );

        drop(lease);
        tokio::time::timeout(Duration::from_secs(5), removal)
            .await
            .expect("the teardown did not finish after the lease was dropped")
            .unwrap()
            .unwrap();
        assert!(!source.exists());
        remove_runtime_dir(&source).await.unwrap();
    }

    /// A restore of a snapshot whose VM is gone recreates the VM's directory as a
    /// mountpoint. The lease that ends last removes it; until then it stays, because
    /// another restore may still have to open its disks through it.
    ///
    /// RED BEFORE THE FIX (issue 1187): nothing removed a recreated directory.
    #[tokio::test]
    async fn the_last_lease_on_a_recreated_mount_target_removes_it() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let first = lease_mount_targets(std::slice::from_ref(&source))
            .await
            .unwrap();
        let second = lease_mount_targets(std::slice::from_ref(&source))
            .await
            .unwrap();
        assert!(first.after_load(false).is_none());
        assert!(source.is_dir(), "removed while another lease held it");
        drop(second);
        assert!(
            !source.exists(),
            "the last lease left the recreated directory"
        );
    }

    /// A VMM that reaches its vsock socket through the redirect keeps a recreated
    /// directory for its life, so a clone that no longer needs it cannot remove it
    /// first; its own exit removes it.
    #[tokio::test]
    async fn a_clone_with_vsock_through_the_redirect_keeps_the_recreated_directory() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let redirected = lease_mount_targets(std::slice::from_ref(&source))
            .await
            .unwrap()
            .after_load(true)
            .expect("a recreated directory is kept by a clone that needs the redirect");
        let overridden = lease_mount_targets(std::slice::from_ref(&source))
            .await
            .unwrap();
        assert!(overridden.after_load(false).is_none());
        assert!(
            source.is_dir(),
            "removed under a clone that still resolves through it"
        );
        drop(redirected);
        assert!(!source.exists());
    }

    /// A live VM's directory holds its disks. A lease never removes it, and a clone
    /// that needs the redirect for vsock does not keep it, so its VM's teardown is
    /// not held up.
    #[tokio::test]
    async fn a_lease_never_removes_or_keeps_a_live_vm_directory() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        std::fs::create_dir_all(source.join("disks")).unwrap();
        let lease = lease_mount_targets(std::slice::from_ref(&source))
            .await
            .unwrap();
        assert!(lease.after_load(true).is_none());
        assert!(source.join("disks").is_dir());
        // Released: the teardown's exclusive lock is free.
        let teardown = std::fs::File::open(&source).unwrap();
        fs2::FileExt::try_lock_exclusive(&teardown).unwrap();
    }

    /// `create_dir_all` answers `mkdir`'s EEXIST by checking that the path is a
    /// directory, and returns AlreadyExists when a teardown removed it in between.
    #[tokio::test]
    async fn a_lease_retries_a_directory_removed_while_it_was_created() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let mut raced = false;
        let leased = lease_one_with(&source, |dir| {
            if !raced {
                raced = true;
                return Err(std::io::ErrorKind::AlreadyExists.into());
            }
            std::fs::create_dir_all(dir)
        })
        .await
        .expect("a mount target removed during its creation failed the lease");
        assert_eq!(leased.metadata().unwrap().ino(), ino(&source));
    }

    #[tokio::test]
    async fn a_lease_that_waited_behind_a_teardown_holds_the_recreated_directory() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        std::fs::create_dir_all(&source).unwrap();
        // The teardown's exclusive lock, taken by hand so the removal can be ordered
        // between the lease's open and its lock.
        let teardown = std::fs::File::open(&source).unwrap();
        fs2::FileExt::try_lock_exclusive(&teardown).unwrap();
        let lease = tokio::spawn({
            let source = source.clone();
            async move { lease_mount_targets(&[source]).await }
        });
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(
            !lease.is_finished(),
            "the lease did not wait for the teardown"
        );
        std::fs::remove_dir(&source).unwrap();
        drop(teardown);

        let lease = tokio::time::timeout(Duration::from_secs(5), lease)
            .await
            .expect("the lease did not finish after the teardown")
            .unwrap()
            .unwrap();
        // The removed directory's inode number can be reused for the new one, so compare
        // the link count: a removed directory has none.
        let leased = lease.dirs[0].file.metadata().unwrap();
        assert!(leased.nlink() > 0, "the lease holds a removed directory");
        assert_eq!(leased.ino(), ino(&source));
    }
}
