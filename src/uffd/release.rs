//! Releasing idle snapshots' memory files from the page cache when a memory server starts.
//!
//! A memory server reads its snapshot's recorded working set through the page cache, and in
//! copy mode it copies each page into the clone. When the page cache still holds another
//! snapshot's memory file that recent restores used, the kernel keeps those pages and evicts
//! the pages the new restore has just read, which are then read again. Measured on a host
//! with 236 GB of memory and two snapshots, A and B, of a 128 GiB guest whose recorded sets
//! are 40 to 42 GiB (#1066):
//!
//! | restore | page cache before it | guest ACK | healthy | read from disk |
//! |---|---|---|---|---|
//! | A | B's memory file, in use for the previous hour | 180 s | 7m20s | 465 GiB |
//! | B | A's memory file, just in use | 58 s | 4m20s | 254 GiB |
//! | A | B's memory file dropped first | 65 s | 2m02s | 112 GiB |
//! | B | A's memory file dropped first | 59 s | 1m51s | 87 GiB |
//!
//! A memory file that is not in the page cache costs its next restore about 40 s (60 s to
//! guest ACK with none of it there, 24 s straight afterwards), so keeping an idle snapshot
//! cached is worth much less than it can cost the next restore of another one. A server that
//! starts therefore asks the kernel to drop the memory file of every other snapshot that
//! nothing is reading.
//!
//! **In use is a lock on the memory file, not a line in fcvm's state.** Whoever reads a
//! snapshot's memory file for a VM holds a shared `flock` on it for as long as it reads: a
//! memory server ([`mark_in_use`], taken before the server maps the file) and a restore whose
//! VMM maps the file itself ([`keep_in_use_until_exit`]). A pass takes the lock exclusively,
//! without waiting, around each drop. So a pass either finds a reader and leaves the file
//! alone, or holds the file while it drops it and a reader that starts meanwhile waits those
//! milliseconds and then reads. Two servers that start together cannot drop each other's
//! file, which a scan of the state directory allowed: a server publishes its state after it
//! has started reading. The kernel releases the lock when its holder dies, however it dies,
//! and a snapshot that a running VM was only created from, or is the diff base of, has no
//! reader and is released.
//!
//! `POSIX_FADV_DONTNEED` drops the clean pages of a file that no process has mapped. A
//! snapshot in use is skipped whole all the same: the pages its server has read ahead and
//! not touched yet are not mapped, and dropping them would make that server read them again.
//!
//! Not measured: minor mode, hugepage snapshots, and a host whose memory holds both recorded
//! sets and both clones at once.

use nix::fcntl::{posix_fadvise, PosixFadviseAdvice};
use std::fs::File;
use std::io::ErrorKind;
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;
use tracing::{info, warn};

/// The memory file's name in a snapshot's directory.
const MEMORY_FILE: &str = "memory.bin";

/// How long a reader waits between attempts while a pass holds its memory file.
const IN_USE_RETRY: Duration = Duration::from_millis(10);

/// The memory files this process keeps in use until it exits.
static KEPT_IN_USE: Mutex<Vec<File>> = Mutex::new(Vec::new());

/// Mark an open memory file in use for as long as `file`, or a duplicate of it, stays open.
///
/// The mark is a shared `flock`. A release pass holds the file exclusively only around one
/// `posix_fadvise` call, so the wait here is that call's length.
pub async fn mark_in_use(file: &File) -> std::io::Result<()> {
    loop {
        // Fully qualified: `std::fs::File` has inherent `try_lock_*` methods with another
        // error type, and inherent methods win over trait methods.
        match fs2::FileExt::try_lock_shared(file) {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                tokio::time::sleep(IN_USE_RETRY).await;
            }
            Err(error) => return Err(error),
        }
    }
}

/// Keep the memory file at `memory_path` in use until this process exits.
///
/// For a restore whose VMM maps the memory file itself: the fcvm process lives exactly as
/// long as its VM, so the mark needs no owner to pass around. Best effort: a file that
/// cannot be marked is logged, and the cost is that a server started for another snapshot
/// may drop this one's cached pages.
pub async fn keep_in_use_until_exit(memory_path: &Path) {
    let marked = async {
        let file = File::open(memory_path)?;
        mark_in_use(&file).await?;
        Ok::<File, std::io::Error>(file)
    }
    .await;
    match marked {
        Ok(file) => KEPT_IN_USE
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(file),
        Err(error) => warn!(
            target: "uffd",
            memory = %memory_path.display(),
            error = %error,
            "could not mark the snapshot's memory file in use: a server that starts for              another snapshot may drop its cached pages"
        ),
    }
}

/// What one pass over the snapshot directory did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Released {
    /// Memory files the kernel was asked to drop.
    pub released: u64,
    /// Memory files left alone because a reader holds them in use.
    pub in_use: u64,
    /// Memory files the kernel refused the lock or the request for.
    pub failed: u64,
}

/// Ask the kernel to drop from the page cache the memory file of every snapshot under
/// `snapshot_dir` that nothing holds in use, except `serving`.
///
/// Best effort: a directory that cannot be read, or a file the kernel refuses, is logged
/// and the restore goes on as it would have without this.
pub fn release_idle_snapshots(snapshot_dir: &Path, serving: &str) -> Released {
    release_with(snapshot_dir, serving, |file| {
        posix_fadvise(file, 0, 0, PosixFadviseAdvice::POSIX_FADV_DONTNEED)
            .map_err(std::io::Error::from)
    })
}

/// [`release_idle_snapshots`] with the kernel call passed in, so a test can refuse it or
/// look at the file while the pass holds it.
fn release_with(
    snapshot_dir: &Path,
    serving: &str,
    mut advise: impl FnMut(&File) -> std::io::Result<()>,
) -> Released {
    let mut done = Released::default();
    let entries = match std::fs::read_dir(snapshot_dir) {
        Ok(entries) => entries,
        Err(error) => {
            if error.kind() != ErrorKind::NotFound {
                warn!(
                    target: "uffd",
                    snapshot_dir = %snapshot_dir.display(),
                    error = %error,
                    "could not list the snapshots to release idle ones from the page cache"
                );
            }
            return done;
        }
    };
    for entry in entries.flatten() {
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if name == serving {
            continue;
        }
        // Anything without a memory file is not one: a lock file beside the snapshots, a
        // disk-only snapshot, a snapshot that is being deleted.
        let Ok(file) = File::open(entry.path().join(MEMORY_FILE)) else {
            continue;
        };
        // Exclusive and without waiting: a reader's shared lock refuses it, and while this
        // pass holds it a reader that starts waits in `mark_in_use`.
        let outcome = match fs2::FileExt::try_lock_exclusive(&file) {
            Ok(()) => {
                let outcome = advise(&file);
                let _ = fs2::FileExt::unlock(&file);
                outcome
            }
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                done.in_use += 1;
                continue;
            }
            Err(error) => Err(error),
        };
        match outcome {
            Ok(()) => done.released += 1,
            Err(error) => {
                done.failed += 1;
                if done.failed == 1 {
                    warn!(
                        target: "uffd",
                        snapshot = %name,
                        error = %error,
                        "the kernel refused to lock or to drop an idle snapshot's memory file; \
                         further refusals in this pass are only counted"
                    );
                }
            }
        }
    }
    info!(
        target: "uffd",
        serving,
        released = done.released,
        in_use = done.in_use,
        failed = done.failed,
        "released the memory files of idle snapshots from the page cache"
    );
    done
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::uffd::warmup::testing::{disk_dir, resident_pages, write_incompressible, MIB};
    use memmap2::{Mmap, MmapOptions};

    const LEN: u64 = 4 * MIB;

    /// A snapshot `name` under `root` whose memory file is `LEN` bytes, every one of them in
    /// the page cache. The mapping is for `mincore` only: no page is touched through it, so
    /// none is mapped and the kernel is free to drop them.
    fn cached_snapshot(root: &Path, name: &str) -> Mmap {
        let dir = root.join(name);
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join(MEMORY_FILE);
        let mut file = File::options()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        write_incompressible(&mut file, LEN).unwrap();
        assert_eq!(std::fs::read(&path).unwrap().len() as u64, LEN);
        // SAFETY: the file is private to this test and is not written again.
        let mmap = unsafe { MmapOptions::new().len(LEN as usize).map(&file) }.unwrap();
        assert!(
            resident_pages(&mmap, 0, LEN) > 0,
            "control: a file that was just written and read is in the page cache"
        );
        mmap
    }

    /// Mechanism: real files, the real page cache, the real posix_fadvise and the real
    /// lock. The idle snapshot's memory file leaves the page cache. The one being served and
    /// the one a reader holds in use keep every page they had.
    #[tokio::test]
    async fn a_pass_drops_idle_snapshots_and_keeps_the_served_one_and_those_in_use() {
        let root = tempfile::tempdir_in(disk_dir()).unwrap();
        let served = cached_snapshot(root.path(), "served");
        let idle = cached_snapshot(root.path(), "idle");
        let named = cached_snapshot(root.path(), "held");
        std::fs::create_dir(root.path().join("no-memory-file")).unwrap();
        std::fs::write(root.path().join("served.lock"), b"").unwrap();
        // A reader of the third snapshot, as a memory server is of its own.
        let reader = File::open(root.path().join("held").join(MEMORY_FILE)).unwrap();
        mark_in_use(&reader).await.unwrap();
        let before = (
            resident_pages(&served, 0, LEN),
            resident_pages(&named, 0, LEN),
        );

        let done = release_idle_snapshots(root.path(), "served");

        assert_eq!(
            done,
            Released {
                released: 1,
                in_use: 1,
                failed: 0
            }
        );
        assert_eq!(
            resident_pages(&idle, 0, LEN),
            0,
            "the idle snapshot's memory file is still in the page cache"
        );
        assert_eq!(
            (
                resident_pages(&served, 0, LEN),
                resident_pages(&named, 0, LEN)
            ),
            before,
            "the served snapshot or the one a reader holds in use lost pages"
        );

        // The reader is gone: the same snapshot is idle and the next pass drops it.
        drop(reader);
        let done = release_idle_snapshots(root.path(), "served");
        assert_eq!(done.in_use, 0);
        assert_eq!(
            resident_pages(&named, 0, LEN),
            0,
            "a snapshot whose reader is gone stayed in the page cache"
        );
    }

    /// The race two servers that start together could lose (#1067): a pass decides a file
    /// is idle, the file's own server starts reading, and the pass then drops what that
    /// server read ahead. The pass holds the file exclusively across the drop, so a reader
    /// cannot get its mark in between, and gets it as soon as the pass has moved on.
    #[test]
    fn a_reader_cannot_start_between_a_pass_finding_a_file_idle_and_dropping_it() {
        let root = tempfile::tempdir_in(disk_dir()).unwrap();
        let _idle = cached_snapshot(root.path(), "idle");
        let path = root.path().join("idle").join(MEMORY_FILE);
        let mut refused_during_the_drop = None;

        let done = release_with(root.path(), "served", |_file| {
            let reader = File::open(&path).unwrap();
            refused_during_the_drop = Some(
                fs2::FileExt::try_lock_shared(&reader)
                    .err()
                    .map(|error| error.kind()),
            );
            Ok(())
        });

        assert_eq!(done.released, 1);
        assert_eq!(
            refused_during_the_drop,
            Some(Some(ErrorKind::WouldBlock)),
            "a reader got its mark while the pass was dropping the file it had found idle"
        );
        let reader = File::open(&path).unwrap();
        fs2::FileExt::try_lock_shared(&reader)
            .expect("the pass kept the file locked after it had moved on");
    }

    /// A file kept in use until this process exits is in use to every pass after that.
    #[tokio::test]
    async fn a_file_kept_in_use_until_exit_is_never_dropped() {
        let root = tempfile::tempdir_in(disk_dir()).unwrap();
        let mapped = cached_snapshot(root.path(), "file-backed");
        let before = resident_pages(&mapped, 0, LEN);

        keep_in_use_until_exit(&root.path().join("file-backed").join(MEMORY_FILE)).await;
        let done = release_idle_snapshots(root.path(), "served");

        assert_eq!(
            done,
            Released {
                released: 0,
                in_use: 1,
                failed: 0
            }
        );
        assert_eq!(resident_pages(&mapped, 0, LEN), before);
    }

    #[test]
    fn a_snapshot_directory_that_does_not_exist_releases_nothing() {
        let root = tempfile::tempdir_in(disk_dir()).unwrap();
        let done = release_idle_snapshots(&root.path().join("missing"), "served");
        assert_eq!(done, Released::default());
    }

    /// A refusal is counted and the pass goes on to the other snapshots.
    #[test]
    fn a_refused_request_is_counted_and_the_other_snapshots_are_still_released() {
        let root = tempfile::tempdir_in(disk_dir()).unwrap();
        let _maps = [
            cached_snapshot(root.path(), "one"),
            cached_snapshot(root.path(), "two"),
            cached_snapshot(root.path(), "three"),
        ];
        let mut asked = 0;
        let done = release_with(root.path(), "served", |_file| {
            asked += 1;
            if asked == 1 {
                Err(std::io::Error::from_raw_os_error(libc::EINVAL))
            } else {
                Ok(())
            }
        });
        assert_eq!(
            done,
            Released {
                released: 2,
                in_use: 0,
                failed: 1
            }
        );
    }
}
