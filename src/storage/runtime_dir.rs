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
//! a name, so it checks the inode it locked against the path and starts again (the
//! directory it recreates is only a mountpoint, and nobody else removes it).

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tracing::{debug, info};

const POLL_INTERVAL: Duration = Duration::from_millis(100);
const WAIT_LOG_INTERVAL: Duration = Duration::from_secs(5);

/// A teardown removes its directory once, so a lease that finds its directory replaced
/// more often than this is not racing a teardown.
const MAX_LEASE_ATTEMPTS: usize = 8;

/// Shared locks on the source VM directories a restoring VMM's mount redirect binds
/// over. Their VMs' teardowns cannot remove them until this is dropped.
#[must_use = "the source directories are protected only while the lease is held"]
pub struct MountTargetLease {
    _dirs: Vec<std::fs::File>,
}

/// Lease every directory in `dirs`, creating any that is missing (a mount target must
/// exist).
pub async fn lease_mount_targets(dirs: &[PathBuf]) -> Result<MountTargetLease> {
    let mut held = Vec::with_capacity(dirs.len());
    for dir in dirs {
        held.push(lease_one(dir).await?);
    }
    Ok(MountTargetLease { _dirs: held })
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
        let leased = lease._dirs[0].metadata().unwrap();
        assert!(leased.nlink() > 0, "the lease holds a removed directory");
        assert_eq!(leased.ino(), ino(&source));
    }
}
