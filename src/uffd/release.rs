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
//! starts therefore asks the kernel to drop the memory file of every other snapshot that no
//! live fcvm process names.
//!
//! `POSIX_FADV_DONTNEED` drops the clean pages of a file that no process has mapped. A
//! snapshot some live process names is skipped whole all the same: the pages its server has
//! read ahead and not touched yet are not mapped, and dropping them would make that server
//! read them again.
//!
//! Not measured: minor mode, hugepage snapshots, and a host whose memory holds both recorded
//! sets and both clones at once.

use nix::fcntl::{posix_fadvise, PosixFadviseAdvice};
use std::collections::HashSet;
use std::fs::File;
use std::path::Path;
use tracing::{info, warn};

/// The memory file's name in a snapshot's directory.
const MEMORY_FILE: &str = "memory.bin";

/// What one pass over the snapshot directory did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Released {
    /// Memory files the kernel was asked to drop.
    pub released: u64,
    /// Memory files left alone because a live process names their snapshot.
    pub in_use: u64,
    /// Memory files the kernel refused the request for.
    pub failed: u64,
}

/// Ask the kernel to drop from the page cache the memory file of every snapshot under
/// `snapshot_dir`, except `serving` and the snapshots in `in_use`.
///
/// Best effort: a directory that cannot be read, or a file the kernel refuses, is logged
/// and the restore goes on as it would have without this.
pub fn release_idle_snapshots(
    snapshot_dir: &Path,
    serving: &str,
    in_use: &HashSet<String>,
) -> Released {
    release_with(snapshot_dir, serving, in_use, |file| {
        posix_fadvise(file, 0, 0, PosixFadviseAdvice::POSIX_FADV_DONTNEED)
            .map_err(std::io::Error::from)
    })
}

/// [`release_idle_snapshots`] with the kernel call passed in, so a test can refuse it.
fn release_with(
    snapshot_dir: &Path,
    serving: &str,
    in_use: &HashSet<String>,
    mut advise: impl FnMut(&File) -> std::io::Result<()>,
) -> Released {
    let mut done = Released::default();
    let entries = match std::fs::read_dir(snapshot_dir) {
        Ok(entries) => entries,
        Err(error) => {
            if error.kind() != std::io::ErrorKind::NotFound {
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
        if in_use.contains(&name) {
            done.in_use += 1;
            continue;
        }
        match advise(&file) {
            Ok(()) => done.released += 1,
            Err(error) => {
                done.failed += 1;
                if done.failed == 1 {
                    warn!(
                        target: "uffd",
                        snapshot = %name,
                        error = %error,
                        "the kernel refused to drop an idle snapshot's memory file from the \
                         page cache; further refusals in this pass are only counted"
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

    /// Mechanism: real files, the real page cache, the real posix_fadvise. The idle
    /// snapshot's memory file leaves the page cache. The one being served and the one a live
    /// process names keep every page they had.
    #[test]
    fn a_pass_drops_idle_snapshots_and_keeps_the_served_one_and_those_in_use() {
        let root = tempfile::tempdir_in(disk_dir()).unwrap();
        let served = cached_snapshot(root.path(), "served");
        let idle = cached_snapshot(root.path(), "idle");
        let named = cached_snapshot(root.path(), "named");
        std::fs::create_dir(root.path().join("no-memory-file")).unwrap();
        std::fs::write(root.path().join("served.lock"), b"").unwrap();
        let before = (
            resident_pages(&served, 0, LEN),
            resident_pages(&named, 0, LEN),
        );

        let done =
            release_idle_snapshots(root.path(), "served", &HashSet::from(["named".to_string()]));

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
            "the served snapshot or the one a live process names lost pages"
        );
    }

    #[test]
    fn a_snapshot_directory_that_does_not_exist_releases_nothing() {
        let root = tempfile::tempdir_in(disk_dir()).unwrap();
        let done = release_idle_snapshots(&root.path().join("missing"), "served", &HashSet::new());
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
        let done = release_with(root.path(), "served", &HashSet::new(), |_file| {
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
